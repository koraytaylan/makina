//! Domain `Supervisor` actor — the coordination hub of the star topology.
//!
//! # Role
//!
//! The domain `Supervisor` is the **hub** of the star topology.  All other role
//! actors (Planner, Developer, Reviewer) are spokes that communicate only with
//! this hub — never with each other.
//!
//! Note: this is *not* [`crate::supervision::RootSupervisor`] (the fault-tolerance
//! root).  The `Supervisor` here is a domain-level coordinator that the
//! `RootSupervisor` manages as one of its supervised children.
//!
//! # The develop → review loop (task 21)
//!
//! The Supervisor drives each ready task end-to-end via a **sequential async
//! flow** (request/reply `ask` against the spokes), which is simpler and cleaner
//! than event-driven message ping-pong for a single-task-at-a-time loop:
//!
//! ```text
//! pick ready task
//!   --DependenciesSatisfied--> Ready
//!   create worktree (WorktreeManager::create)
//!   --Dispatched--> InProgress
//!   loop:
//!     ask Developer.Develop{task, worktree, feedback}   (feedback=None first time)
//!     --GatesPassed--> InReview        [seam: task 22 inserts the gate loop here]
//!     ask Reviewer.Review{task, worktree}
//!       Approve  --ReviewerApproved--> Done             [seam: task 23 squash-merges BEFORE teardown]
//!                 WorktreeManager::remove ; next ready task
//!       Reject{feedback} --ReviewerRejected--> InProgress
//!                 review_iterations += 1 ; relay feedback ; re-dispatch (bounded retry)
//! ```
//!
//! Every state change goes through [`crate::state_machine::transition`]; the
//! Supervisor keeps each [`Task::state`] in the held graph updated as the source
//! of truth.
//!
//! ## Deferred seams (do NOT implement here)
//!
//! - **Gates** (task 22): the Developer hand-back transitions directly
//!   `InProgress → InReview` via `GatesPassed`; no gate commands run.  Task 22
//!   inserts the gate-iteration loop at the marked seam.
//! - **Squash-merge** (task 23): on approve the Supervisor does NOT merge — it
//!   only transitions to `Done` and tears down the worktree.  Task 23 inserts the
//!   squash-merge at the marked seam, BEFORE the teardown.
//! - **Concurrency** (task 24): tasks run strictly sequentially (one at a time).
//!   No parallel scheduler is built here.
//! - **Termination caps** (task 25): the reject→retry loop uses a SIMPLE bounded
//!   retry ([`MAX_REVIEWER_ITERATIONS`]) only so tests can't infinite-loop.  The
//!   REAL configurable caps (gate/reviewer iteration + wall-clock) come from
//!   [`crate::config::Config`] in task 25 at the marked seam.
//!
//! # Messages
//!
//! - [`SetTaskGraph`] — stores the current task graph; reply `()`.
//! - [`SetSpokes`] — injects the Developer/Reviewer refs (post-spawn wiring that
//!   breaks the hub↔spoke construction cycle); reply `()`.
//! - [`RunReadyTasks`] — drives every currently-ready task to a terminal state
//!   sequentially; reply [`RunReport`].
//! - [`TaskGraphSnapshot`] — returns the current graph for introspection/testing.

use kameo::{actor::ActorRef, message::Context};

use crate::state_machine::{TaskEvent, transition};
use crate::task::{Task, TaskGraph, TaskId, TaskState};
use crate::worktree::WorktreeManager;

use super::developer::{Develop, Developer};
use super::reviewer::{Review, ReviewVerdict, Reviewer};

// ── Constants ───────────────────────────────────────────────────────────────────

/// Simple bounded retry limit for the reject→re-develop loop.
///
/// This is a **placeholder safety bound**, NOT the real termination cap.  It
/// exists only so an integration test cannot infinite-loop if a misconfigured
/// backend always rejects.  Task 25 (`termination-caps`) replaces this with the
/// configurable `Config::caps.reviewer_iterations` (plus the gate-iteration and
/// wall-clock caps) and emits the `ReviewCapReached` FSM event on exhaustion.
pub const MAX_REVIEWER_ITERATIONS: u32 = 8;

// ── Actor ─────────────────────────────────────────────────────────────────────

/// The domain coordination hub for the multi-agent pipeline.
///
/// Holds the active [`TaskGraph`], the [`WorktreeManager`], and (after
/// [`SetSpokes`]) refs to the Developer and Reviewer it dispatches to.
/// Spawnable as a supervised child of [`crate::supervision::RootSupervisor`].
pub struct Supervisor {
    /// The active task graph — the source of truth for scheduling and state.
    ///
    /// `None` until [`SetTaskGraph`] is received.
    graph: Option<TaskGraph>,

    /// Worktree/branch lifecycle manager.
    ///
    /// `None` until provided via [`SupervisorArgs`].  The Supervisor owns all
    /// worktree create/remove calls (architecture invariant: only the Supervisor
    /// manages worktree+branch lifecycle).
    worktree_manager: Option<WorktreeManager>,

    /// Developer spoke ref, injected post-spawn via [`SetSpokes`].
    developer: Option<ActorRef<Developer>>,

    /// Reviewer spoke ref, injected post-spawn via [`SetSpokes`].
    reviewer: Option<ActorRef<Reviewer>>,
}

/// Construction arguments for [`Supervisor`].
///
/// The [`WorktreeManager`] is supplied at spawn time (it is `Clone`, satisfying
/// the `Args: Clone + Sync` bound for supervised children).  Spoke refs are NOT
/// part of `Args` because the Developer/Reviewer need the Supervisor's ref to be
/// constructed — a construction cycle — so they are injected afterwards via
/// [`SetSpokes`].
#[derive(Clone)]
pub struct SupervisorArgs {
    /// The worktree manager the Supervisor uses to create/tear down worktrees.
    pub worktree_manager: WorktreeManager,
}

impl kameo::actor::Actor for Supervisor {
    type Args = SupervisorArgs;
    type Error = std::convert::Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Supervisor {
            graph: None,
            worktree_manager: Some(args.worktree_manager),
            developer: None,
            reviewer: None,
        })
    }
}

// ── RunReport ───────────────────────────────────────────────────────────────────

/// Summary returned by [`RunReadyTasks`].
///
/// Reports the terminal outcome for each task the run touched, in the order they
/// were driven.  Tests assert on this plus the final graph snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// `(task_id, final_state)` for every task driven to a terminal state during
    /// this run.
    pub outcomes: Vec<(TaskId, TaskState)>,
}

// ── SetTaskGraph ────────────────────────────────────────────────────────────────

/// Store (or replace) the active [`TaskGraph`].
///
/// The Supervisor holds the graph as the source of truth for all scheduling
/// decisions and state mutations.
pub struct SetTaskGraph(pub TaskGraph);

impl kameo::message::Message<SetTaskGraph> for Supervisor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetTaskGraph,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.graph = Some(msg.0);
    }
}

// ── SetSpokes ─────────────────────────────────────────────────────────────────

/// Inject the Developer and Reviewer refs into the Supervisor.
///
/// This post-spawn wiring step breaks the hub↔spoke construction cycle: the
/// spokes need the Supervisor's ref in their `Args`, so the Supervisor must be
/// spawned first; this message then hands the spoke refs back to the hub so it
/// can dispatch work to them.
pub struct SetSpokes {
    /// The Developer the Supervisor dispatches develop turns to.
    pub developer: ActorRef<Developer>,
    /// The Reviewer the Supervisor dispatches review turns to.
    pub reviewer: ActorRef<Reviewer>,
}

impl kameo::message::Message<SetSpokes> for Supervisor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetSpokes,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.developer = Some(msg.developer);
        self.reviewer = Some(msg.reviewer);
    }
}

// ── RunReadyTasks ─────────────────────────────────────────────────────────────

/// Drive every currently-ready task to a terminal state, sequentially.
///
/// A task is "ready" once all of its `depends_on` are `Done`.  The Supervisor
/// repeatedly picks the next ready task and runs it through the full
/// develop→review loop until no `New`/`Ready` task remains (or one fails).  When
/// a task completes `Done`, dependents may become ready, so the run continues
/// until the graph is drained.
///
/// # Reply
///
/// [`RunReport`] listing the terminal outcome of each task driven.  On a
/// per-task hard error the task is left in `Failed` and recorded; the run then
/// stops (a partial failure does not silently continue, matching the MVP's
/// fail-fast posture — richer failure policy is a later concern).
pub struct RunReadyTasks;

impl kameo::message::Message<RunReadyTasks> for Supervisor {
    type Reply = Result<RunReport, String>;

    async fn handle(
        &mut self,
        _msg: RunReadyTasks,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.run_ready_tasks().await
    }
}

// ── TaskGraphSnapshot ───────────────────────────────────────────────────────────

/// Return a snapshot of the current task graph for introspection and testing.
///
/// Returns `None` if no graph has been set yet.
pub struct TaskGraphSnapshot;

impl kameo::message::Message<TaskGraphSnapshot> for Supervisor {
    type Reply = Option<TaskGraph>;

    async fn handle(
        &mut self,
        _msg: TaskGraphSnapshot,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.graph.clone()
    }
}

// ── Orchestration logic ─────────────────────────────────────────────────────────

impl Supervisor {
    /// Drive every ready task to a terminal state, sequentially.
    ///
    /// Loops: find the next ready task, run it to completion, repeat until no
    /// schedulable (`New`/`Ready`/in-flight) task remains.  Each completed `Done`
    /// task can unlock dependents on the next iteration.
    async fn run_ready_tasks(&mut self) -> Result<RunReport, String> {
        let mut outcomes = Vec::new();

        loop {
            // Pick the next task to run: a `New`/`Ready` task whose deps are all Done.
            let Some(task_id) = self.next_ready_task_id() else {
                break;
            };

            // Run it end-to-end through the develop→review loop.
            let final_state = self.run_single_task(&task_id).await?;
            outcomes.push((task_id, final_state));

            // Fail-fast: if a task failed, stop the run (don't try to continue
            // with a dependency unmet).  Richer failure policy is a later concern.
            if final_state == TaskState::Failed {
                break;
            }
        }

        Ok(RunReport { outcomes })
    }

    /// Find the next task that is eligible to run: state `New` or `Ready` and all
    /// `depends_on` are `Done`.  Returns its [`TaskId`], or `None` if no such task
    /// exists (graph drained or remaining tasks are blocked/terminal).
    fn next_ready_task_id(&self) -> Option<TaskId> {
        let graph = self.graph.as_ref()?;
        graph
            .tasks
            .iter()
            .find(|t| {
                matches!(t.state, TaskState::New | TaskState::Ready)
                    && t.depends_on.iter().all(|dep| {
                        graph
                            .get(dep)
                            .map(|d| d.state == TaskState::Done)
                            .unwrap_or(false)
                    })
            })
            .map(|t| t.id.clone())
    }

    /// Drive a single task from its current state through the full
    /// develop→review loop to a terminal state, returning that terminal state.
    ///
    /// Drives the FSM for every transition and keeps the held graph's
    /// [`Task::state`] (and timestamps / iteration counters) updated.
    async fn run_single_task(&mut self, task_id: &TaskId) -> Result<TaskState, String> {
        // ── Step 1: New → Ready (DependenciesSatisfied) ───────────────────────
        // The caller (next_ready_task_id) guarantees deps are Done.  If the task
        // is already Ready (re-entry), skip this transition.
        let current = self.task_state(task_id)?;
        if current == TaskState::New {
            self.apply_event(task_id, TaskEvent::DependenciesSatisfied)?;
        }

        // ── Step 2: create the worktree, then Ready → InProgress (Dispatched) ──
        let worktree = {
            let mgr = self
                .worktree_manager
                .as_ref()
                .ok_or("supervisor has no worktree manager")?;
            // TODO(task-25): worktree creation failure leaves the task in Ready with
            // no worktree to clean up (nothing leaked), but there is no
            // Ready → Failed FSM path. Task 25 (termination-caps) should add the
            // Ready --HardError--> Failed transition or handle creation failure
            // before Dispatched so the task reaches a terminal Failed state rather
            // than being stuck in Ready.
            mgr.create(&task_id.0)
                .await
                .map_err(|e| format!("worktree create failed for {task_id}: {e}"))?
        };
        self.apply_event(task_id, TaskEvent::Dispatched)?;
        // Record when the Developer first picked up the task.
        self.mark_started(task_id);

        // ── Step 3–6: the develop → review loop (bounded retry) ────────────────
        let mut feedback: Option<String> = None;
        let terminal_state;

        loop {
            // Snapshot the task to hand a stable copy to the spoke.
            let task = self.task_clone(task_id)?;

            // ── Developer turn ─────────────────────────────────────────────────
            let developer = self
                .developer
                .clone()
                .ok_or("supervisor has no Developer ref (call SetSpokes first)")?;

            let develop_result = developer
                .ask(Develop {
                    task: task.clone(),
                    worktree: worktree.path.clone(),
                    feedback: feedback.take(),
                })
                .send()
                .await;

            if let Err(e) = develop_result {
                // Hard error during development → InProgress → Failed (HardError).
                self.apply_event(task_id, TaskEvent::HardError)?;
                self.mark_finished(task_id);
                // Tear down the worktree even on failure (best-effort).
                self.remove_worktree(task_id).await;
                return Err(format!("developer dispatch failed for {task_id}: {e}"));
            }

            // ── Seam: gate-iteration loop (task 22) ────────────────────────────
            //
            // In the real flow the Developer's changes are run against the
            // configured gates here; on failure the FSM self-loops
            // (InProgress --GateFailed--> InProgress) and the Developer iterates,
            // up to `Config::caps.gate_iterations` (task 25).  Gates are FORBIDDEN
            // in this task (task 22 owns shell-command execution), so we transition
            // straight to review as if all gates passed.
            self.apply_event(task_id, TaskEvent::GatesPassed)?;

            // ── Reviewer turn ──────────────────────────────────────────────────
            let reviewer = self
                .reviewer
                .clone()
                .ok_or("supervisor has no Reviewer ref (call SetSpokes first)")?;

            let review_task = self.task_clone(task_id)?;
            let review_result = reviewer
                .ask(Review {
                    task: review_task,
                    worktree: worktree.path.clone(),
                })
                .send()
                .await;

            // On reviewer ask/parse failure, clean up the worktree before
            // propagating the error to prevent git worktree + branch leaks.
            //
            // The task is currently in InReview. We deliberately do NOT attempt
            // an FSM transition here because InReview --HardError--> Failed is
            // illegal (only InReview --ReviewCapReached--> Failed exists).
            //
            // TODO(task-25): task 25 (termination-caps) owns the
            // InReview → Failed hard-error path. It should either add the
            // InReview --HardError--> Failed transition to the FSM, or model
            // reviewer-side failures via ReviewCapReached, so the task reaches
            // a proper terminal Failed state instead of being left stuck in
            // InReview. For now we clean up the resource leak and return the
            // error without forcing an illegal transition.
            let verdict = match review_result {
                Ok(v) => v,
                Err(e) => {
                    self.remove_worktree(task_id).await;
                    return Err(format!("reviewer dispatch failed for {task_id}: {e}"));
                }
            };

            match verdict {
                ReviewVerdict::Approve => {
                    // ── Approve: InReview → Done (ReviewerApproved) ─────────────
                    self.apply_event(task_id, TaskEvent::ReviewerApproved)?;
                    self.mark_finished(task_id);

                    // ── Seam: squash-merge to `develop` (task 23) ───────────────
                    //
                    // The real flow squash-merges `task/{id}` into `develop` HERE,
                    // BEFORE tearing down the worktree (the branch carries the
                    // committed work).  Merging is FORBIDDEN in this task (task 23
                    // owns it); we go straight to teardown.

                    // Tear down the worktree + branch.
                    self.remove_worktree(task_id).await;

                    terminal_state = TaskState::Done;
                    break;
                }
                ReviewVerdict::Reject { feedback: fb } => {
                    // ── Reject: InReview → InProgress (ReviewerRejected) ────────
                    self.apply_event(task_id, TaskEvent::ReviewerRejected)?;
                    self.increment_review_iterations(task_id);

                    // ── Seam: termination caps (task 25) ───────────────────────
                    //
                    // The simple bounded retry below exists ONLY so this loop can
                    // never spin forever in a test.  Task 25 replaces it with the
                    // configurable `Config::caps.reviewer_iterations` (and gate +
                    // wall-clock caps), emitting `ReviewCapReached` →
                    // InReview → Failed instead of the early-return below.
                    let iterations = self.review_iterations(task_id)?;
                    if iterations >= MAX_REVIEWER_ITERATIONS {
                        // Bounded-retry safety stop.  NOTE: the real FSM event here
                        // is `ReviewCapReached` (task 25); we are already in
                        // InProgress after the reject transition, so we use
                        // HardError to reach a terminal Failed state for now.
                        self.apply_event(task_id, TaskEvent::HardError)?;
                        self.mark_finished(task_id);
                        self.remove_worktree(task_id).await;
                        terminal_state = TaskState::Failed;
                        break;
                    }

                    // Relay the feedback to the Developer on the next iteration.
                    feedback = Some(fb);
                    // Loop back to a fresh Developer turn (re-work).
                }
            }
        }

        Ok(terminal_state)
    }

    // ── Graph mutation helpers ──────────────────────────────────────────────────

    /// Apply an FSM `event` to the task, updating its state in the held graph.
    ///
    /// Every state change in the loop goes through this helper so the FSM is the
    /// single source of truth for legality.  Also bumps `updated_at`.
    fn apply_event(&mut self, task_id: &TaskId, event: TaskEvent) -> Result<(), String> {
        let task = self.task_mut(task_id)?;
        let next = transition(task.state, event)
            .map_err(|e| format!("illegal transition for {task_id}: {e}"))?;
        task.state = next;
        task.updated_at = chrono::Utc::now();
        Ok(())
    }

    /// Read a task's current state.
    fn task_state(&self, task_id: &TaskId) -> Result<TaskState, String> {
        self.graph
            .as_ref()
            .and_then(|g| g.get(task_id))
            .map(|t| t.state)
            .ok_or_else(|| format!("task {task_id} not found in graph"))
    }

    /// Clone a task out of the graph (to hand a stable snapshot to a spoke).
    fn task_clone(&self, task_id: &TaskId) -> Result<Task, String> {
        self.graph
            .as_ref()
            .and_then(|g| g.get(task_id))
            .cloned()
            .ok_or_else(|| format!("task {task_id} not found in graph"))
    }

    /// Mutable access to a task in the held graph.
    fn task_mut(&mut self, task_id: &TaskId) -> Result<&mut Task, String> {
        self.graph
            .as_mut()
            .ok_or_else(|| "supervisor has no graph".to_string())?
            .tasks
            .iter_mut()
            .find(|t| &t.id == task_id)
            .ok_or_else(|| format!("task {task_id} not found in graph"))
    }

    /// Read a task's reviewer-iteration count.
    fn review_iterations(&self, task_id: &TaskId) -> Result<u32, String> {
        self.graph
            .as_ref()
            .and_then(|g| g.get(task_id))
            .map(|t| t.review_iterations)
            .ok_or_else(|| format!("task {task_id} not found in graph"))
    }

    /// Increment a task's reviewer-iteration counter (FSM-external bookkeeping,
    /// per the state-machine module's documented division of responsibility).
    fn increment_review_iterations(&mut self, task_id: &TaskId) {
        if let Ok(task) = self.task_mut(task_id) {
            task.review_iterations += 1;
            task.updated_at = chrono::Utc::now();
        }
    }

    /// Stamp `started_at` when the Developer first picks up the task.
    fn mark_started(&mut self, task_id: &TaskId) {
        if let Ok(task) = self.task_mut(task_id)
            && task.started_at.is_none()
        {
            task.started_at = Some(chrono::Utc::now());
        }
    }

    /// Stamp `finished_at` when the task reaches a terminal state.
    fn mark_finished(&mut self, task_id: &TaskId) {
        if let Ok(task) = self.task_mut(task_id) {
            task.finished_at = Some(chrono::Utc::now());
        }
    }

    /// Best-effort worktree teardown (idempotent; ignores "already gone").
    async fn remove_worktree(&self, task_id: &TaskId) {
        if let Some(mgr) = self.worktree_manager.as_ref() {
            // `remove` is idempotent and treats "not found" as success.
            let _ = mgr.remove(&task_id.0).await;
        }
    }
}
