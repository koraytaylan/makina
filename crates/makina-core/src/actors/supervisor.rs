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
//!     develop_until_gates_pass(task, worktree, feedback):     (task 22)
//!       loop:
//!         ask Developer.Develop{task, worktree, feedback}     (feedback=None first time)
//!         run gates in worktree
//!           Passed       --GatesPassed--> InReview ; break
//!           Failed{..}   --GateFailed--> InProgress (self-loop) ; gate_iterations += 1
//!                        if gate_iterations >= caps.gate_iterations:
//!                          --GateCapReached--> Failed ; teardown ; task fails
//!                        else: feedback = gate output ; re-dispatch
//!     ask Reviewer.Review{task, worktree}
//!       Approve  --ReviewerApproved--> Done             [seam: task 23 squash-merges BEFORE teardown]
//!                 WorktreeManager::remove ; next ready task
//!       Reject{feedback} --ReviewerRejected--> InProgress
//!                 review_iterations += 1 ; relay feedback ; re-develop+gate (bounded retry)
//! ```
//!
//! Every state change goes through [`crate::state_machine::transition`]; the
//! Supervisor keeps each [`Task::state`] in the held graph updated as the source
//! of truth.
//!
//! ## Gates (task 22 — implemented)
//!
//! Between the Developer hand-back and review, the work iterates against the
//! configured gates ([`crate::config::Config::gates`]) via
//! [`Supervisor::develop_until_gates_pass`].  On a gate failure the FSM
//! self-loops (`InProgress --GateFailed--> InProgress`), the failing gate's
//! output is fed back to the Developer, and ALL gates re-run; the work only
//! advances to the Reviewer (`GatesPassed`) once every gate exits `0`.  A
//! per-task gate-iteration cap (`config.caps.gate_iterations`) moves the task to
//! `Failed` (`GateCapReached`) on exhaustion.
//!
//! **Placement choice**: the architecture frames gates as "Developer-side"; the
//! MVP implements them **Supervisor-coordinated** (the Supervisor runs the gates
//! and re-dispatches the Developer with the failure output).  The agent still
//! does the fixing; the gate EXECUTION is the reusable [`crate::gate::GateRunner`].
//! This keeps FSM ownership in the Supervisor (consistent with task 21).
//!
//! ## Deferred seams (do NOT implement here)
//!
//! - **Squash-merge** (task 23): on approve the Supervisor does NOT merge — it
//!   only transitions to `Done` and tears down the worktree.  Task 23 inserts the
//!   squash-merge at the marked seam, BEFORE the teardown.
//! - **Concurrency** (task 24): tasks run strictly sequentially (one at a time).
//!   No parallel scheduler is built here.
//! - **Termination caps** (task 25): the **gate** cap is already enforced from
//!   `config.caps.gate_iterations` (task 22, above).  The **reviewer** reject→retry
//!   loop still uses a SIMPLE bounded retry ([`MAX_REVIEWER_ITERATIONS`]) only so
//!   tests can't infinite-loop; task 25 replaces it with
//!   `config.caps.reviewer_iterations` and adds the wall-clock cap, unifying all
//!   caps.  Task 22 deliberately does NOT touch the reviewer/wall-clock caps.
//!
//! # Messages
//!
//! - [`SetTaskGraph`] — stores the current task graph; reply `()`.
//! - [`SetSpokes`] — injects the Developer/Reviewer refs (post-spawn wiring that
//!   breaks the hub↔spoke construction cycle); reply `()`.
//! - [`RunReadyTasks`] — drives every currently-ready task to a terminal state
//!   sequentially; reply [`RunReport`].
//! - [`TaskGraphSnapshot`] — returns the current graph for introspection/testing.

use std::path::Path;

use kameo::{actor::ActorRef, message::Context};

use crate::config::Config;
use crate::gate::{GateOutcome, GateRunner};
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

    /// The resolved runtime configuration.
    ///
    /// Task 22 (`gate-runner`) reads `config.gates` (the gate command lines) and
    /// `config.caps.gate_iterations` (the GATE cap).  The whole [`Config`] is
    /// injected (not just those fields) so task 25 (`termination-caps`) can read
    /// the reviewer/wall-clock caps from the same place without re-plumbing.
    config: Config,

    /// Executes the configured gates in a task's worktree.
    ///
    /// Stateless and reused across all tasks/iterations.  See [`GateRunner`] and
    /// the Supervisor-coordinated placement note on [`Supervisor::develop_until_gates_pass`].
    gate_runner: GateRunner,
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

    /// The resolved runtime [`Config`].
    ///
    /// Supplies the gate command lines (`config.gates`) and the gate-iteration
    /// cap (`config.caps.gate_iterations`) the Supervisor's gate loop uses in
    /// task 22.  `Config` is `Clone`, satisfying the `Args: Clone + Sync` bound
    /// for supervised children.  The full config is passed (rather than just the
    /// gate fields) so task 25 can read the other caps without re-plumbing.
    pub config: Config,
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
            config: args.config,
            gate_runner: GateRunner::new(),
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

// ── DevelopGateOutcome ──────────────────────────────────────────────────────────

/// Result of one [`Supervisor::develop_until_gates_pass`] round.
///
/// Either the work passed every gate and is ready for the Reviewer, or the
/// gate-iteration cap fired and the task was already moved to `Failed` (with its
/// worktree torn down).  A hard error is reported separately via `Err` on the
/// helper, not as a variant here.
enum DevelopGateOutcome {
    /// All gates passed; the task is now `InReview` and ready for the Reviewer.
    ReadyForReview,

    /// The gate-iteration cap was reached; the helper has already emitted
    /// `GateCapReached` (→ `Failed`) and torn down the worktree.
    GateCapReached,
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

        // ── Step 3–6: the develop → gate → review loop (bounded retry) ─────────
        let mut feedback: Option<String> = None;
        let terminal_state;

        loop {
            // ── Develop + gate loop (task 22) ──────────────────────────────────
            //
            // The Developer makes/fixes changes, then the configured gates run in
            // the worktree.  On a gate failure the FSM self-loops
            // (InProgress --GateFailed--> InProgress) and the failure output is
            // fed back to the Developer to fix; ALL gates then re-run.  This
            // repeats until gates pass (→ InReview) or the gate-iteration cap
            // fires (→ Failed, worktree torn down).  See
            // [`Supervisor::develop_until_gates_pass`].
            match self
                .develop_until_gates_pass(task_id, &worktree.path, feedback.take())
                .await
            {
                Ok(DevelopGateOutcome::ReadyForReview) => {
                    // Gates passed; the task is now InReview.  Fall through to
                    // the Reviewer turn below.
                }
                Ok(DevelopGateOutcome::GateCapReached) => {
                    // The gate cap fired: the helper already moved the task to
                    // Failed and tore down the worktree.
                    terminal_state = TaskState::Failed;
                    break;
                }
                Err(e) => {
                    // Hard error during development (the helper already moved the
                    // task to Failed and tore down the worktree).
                    return Err(e);
                }
            }

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

    /// Run the **develop + gate loop** for one review round (task 22).
    ///
    /// Drives: dispatch the Developer (with `initial_feedback`, which is the
    /// Reviewer's feedback on a re-develop or `None` on the first attempt), then
    /// run the configured gates in the worktree.  On a gate failure the FSM
    /// self-loops (InProgress --GateFailed--> InProgress), `gate_iterations` is
    /// bumped, and the gate's output is fed back to the Developer; **all** gates
    /// then re-run from the top on the next turn (this is how the architecture's
    /// "re-run ALL gates after each fix" is realised — [`GateRunner::run_gates`]
    /// does one pass, and this loop re-invokes it).  Repeats until:
    ///
    /// - **gates pass** → emit `GatesPassed` (InProgress → InReview) and return
    ///   [`DevelopGateOutcome::ReadyForReview`]; or
    /// - **the gate cap is hit** (`gate_iterations >= config.caps.gate_iterations`)
    ///   → emit `GateCapReached` (InProgress → Failed), tear down the worktree,
    ///   and return [`DevelopGateOutcome::GateCapReached`].
    ///
    /// # Placement note (architecture)
    ///
    /// The architecture frames gates as "Developer-side".  For the MVP they are
    /// **Supervisor-coordinated**: the Supervisor runs the gates and re-dispatches
    /// the Developer with the failure output to fix.  The agent still does the
    /// fixing; the gate EXECUTION is the reusable [`GateRunner`].  This keeps FSM
    /// ownership in the Supervisor (consistent with task 21) — a faithful
    /// realization of the requirement.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` on a hard error (Developer dispatch failure, or a
    /// gate command that could not be **launched** at all — distinct from a gate
    /// *failing*).  In the error case the task has already been moved to Failed
    /// (HardError) and the worktree torn down, so the caller just propagates.
    async fn develop_until_gates_pass(
        &mut self,
        task_id: &TaskId,
        worktree_path: &Path,
        initial_feedback: Option<String>,
    ) -> Result<DevelopGateOutcome, String> {
        let mut feedback = initial_feedback;

        loop {
            // ── Developer turn: make (or fix) the changes ──────────────────────
            let task = self.task_clone(task_id)?;
            let developer = self
                .developer
                .clone()
                .ok_or("supervisor has no Developer ref (call SetSpokes first)")?;

            let develop_result = developer
                .ask(Develop {
                    task,
                    worktree: worktree_path.to_path_buf(),
                    feedback: feedback.take(),
                })
                .send()
                .await;

            if let Err(e) = develop_result {
                // Hard error during development → InProgress → Failed (HardError).
                self.apply_event(task_id, TaskEvent::HardError)?;
                self.mark_finished(task_id);
                self.remove_worktree(task_id).await;
                return Err(format!("developer dispatch failed for {task_id}: {e}"));
            }

            // ── Gate turn: run ALL configured gates in the worktree ────────────
            let outcome = self
                .gate_runner
                .run_gates(&self.config.gates, worktree_path)
                .await;

            match outcome {
                Ok(GateOutcome::Passed) => {
                    // All gates passed → advance to review.
                    self.apply_event(task_id, TaskEvent::GatesPassed)?;
                    return Ok(DevelopGateOutcome::ReadyForReview);
                }
                Ok(GateOutcome::Failed {
                    gate,
                    output,
                    exit_code,
                }) => {
                    // A gate failed → self-loop and count the iteration.
                    self.apply_event(task_id, TaskEvent::GateFailed)?;
                    self.increment_gate_iterations(task_id);

                    // ── GATE cap (task 22 owns this one) ───────────────────────
                    //
                    // NOTE: this is the GATE cap only.  The reviewer/wall-clock
                    // caps are task 25's concern and are left untouched.
                    let iterations = self.gate_iterations(task_id)?;
                    if iterations >= self.config.caps.gate_iterations {
                        // GateCapReached: InProgress → Failed (terminal).
                        self.apply_event(task_id, TaskEvent::GateCapReached)?;
                        self.mark_finished(task_id);
                        self.remove_worktree(task_id).await;
                        return Ok(DevelopGateOutcome::GateCapReached);
                    }

                    // Feed the failing gate's output back to the Developer so the
                    // next turn fixes it; then ALL gates re-run from the top.
                    feedback = Some(format!(
                        "Gate `{gate}` failed (exit code {exit_code}):\n{output}\n\
                         Fix the issue so the gate passes."
                    ));
                    // Loop back to a fresh Developer turn.
                }
                Err(e) => {
                    // The gate command could not be LAUNCHED (infrastructure
                    // failure, not a gate result).  Treat as a hard error: we are
                    // in InProgress, so HardError → Failed is legal.
                    self.apply_event(task_id, TaskEvent::HardError)?;
                    self.mark_finished(task_id);
                    self.remove_worktree(task_id).await;
                    return Err(format!("gate launch failed for {task_id}: {e}"));
                }
            }
        }
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

    /// Read a task's gate-iteration count.
    fn gate_iterations(&self, task_id: &TaskId) -> Result<u32, String> {
        self.graph
            .as_ref()
            .and_then(|g| g.get(task_id))
            .map(|t| t.gate_iterations)
            .ok_or_else(|| format!("task {task_id} not found in graph"))
    }

    /// Increment a task's gate-iteration counter (FSM-external bookkeeping, per
    /// the state-machine module's documented division of responsibility — the
    /// FSM emits `GateFailed`, the Supervisor counts the iterations and enforces
    /// the cap).
    fn increment_gate_iterations(&mut self, task_id: &TaskId) {
        if let Ok(task) = self.task_mut(task_id) {
            task.gate_iterations += 1;
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
