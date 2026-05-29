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
//!       Approve  squash_merge(task/{id} → develop)      (task 23 — BEFORE teardown)
//!                 Merged   --ReviewerApproved--> Done ; WorktreeManager::remove
//!                 Conflict develop already restored clean by merger ;
//!                          --ReviewCapReached--> Failed ; teardown   (safe-fail; agent-reconcile seam)
//!       Reject{feedback} --ReviewerRejected--> InProgress
//!                 review_iterations += 1 ; relay feedback ; re-develop+gate (bounded retry)
//! ```
//!
//! Every state change goes through [`crate::state_machine::transition`]; the
//! Supervisor keeps each [`Task::state`] in the shared graph updated as the
//! source of truth.
//!
//! ## Gates (task 22 — implemented)
//!
//! Between the Developer hand-back and review, the work iterates against the
//! configured gates ([`crate::config::Config::gates`]) via
//! [`develop_until_gates_pass`].  On a gate failure the FSM self-loops
//! (`InProgress --GateFailed--> InProgress`), the failing gate's output is fed
//! back to the Developer, and ALL gates re-run; the work only advances to the
//! Reviewer (`GatesPassed`) once every gate exits `0`.  A per-task
//! gate-iteration cap (`config.caps.gate_iterations`) moves the task to `Failed`
//! (`GateCapReached`) on exhaustion.
//!
//! **Placement choice**: the architecture frames gates as "Developer-side"; the
//! MVP implements them **Supervisor-coordinated** (the driver runs the gates and
//! re-dispatches the Developer with the failure output).  The agent still does
//! the fixing; the gate EXECUTION is the reusable [`crate::gate::GateRunner`].
//!
//! ## Squash-merge (task 23 — implemented)
//!
//! On approval the driver squash-merges `task/{id}` into the base branch via
//! [`SquashMerger::squash_merge`], **before** tearing down the worktree.  A clean
//! merge lands the task's work as ONE squashed commit on `develop`, then the FSM
//! advances `InReview --ReviewerApproved--> Done` and the worktree is removed.  A
//! straggler **conflict** is reconciled rather than hard-failed-into-corruption:
//! the merger restores `develop` to a clean state (its hard invariant), and the
//! driver drives the task to a SAFE terminal `Failed` (via `ReviewCapReached`)
//! without ever leaving `develop` broken.
//!
//! ## Concurrency (task 24 — implemented)
//!
//! The Supervisor runs **multiple tasks in parallel**, one Developer/Reviewer
//! pair per task, up to `config.concurrency`.  The sequential `run_ready_tasks`
//! of task 21 is replaced by a [`scheduler`] that launches a [`task_driver`] per
//! ready task on a [`tokio::task::JoinSet`], capped by a
//! [`tokio::sync::Semaphore`].  *Within-task* logic (FSM, dev+gate loop, review
//! loop, squash-merge, worktree lifecycle) is unchanged — concurrency is purely
//! **across** tasks (the architecture's "concurrency is across tasks, not within
//! one").  See the [`scheduler`] / [`task_driver`] docs for the full design,
//! lock discipline, and deadlock-freedom argument.
//!
//! ## Deferred seams (do NOT implement here)
//!
//! - **Termination caps** (task 25): the **gate** cap is already enforced from
//!   `config.caps.gate_iterations` (task 22).  The **reviewer** reject→retry
//!   loop still uses a SIMPLE bounded retry ([`MAX_REVIEWER_ITERATIONS`]) only so
//!   tests can't infinite-loop; task 25 replaces it with
//!   `config.caps.reviewer_iterations` and adds the wall-clock cap, unifying all
//!   caps.  Concurrency keeps each cap **per task** (each driver counts its own
//!   task's iterations under the graph lock — see [`task_driver`]).
//! - **Run control** (task 31): pause/cancel is not implemented; the
//!   [`scheduler`] leaves a documented cancellation seam (drop the `JoinSet` /
//!   close the semaphore) but does not act on it.
//!
//! # Messages
//!
//! - [`SetTaskGraph`] — stores the current task graph; reply `()`.
//! - [`SetSpokes`] — injects the concurrency dependencies (the `RootSupervisor`
//!   ref, the hub's own ref, and the shared backend) the scheduler uses to spawn
//!   a per-task Developer/Reviewer pair; reply `()`.
//! - [`RunReadyTasks`] — drives every ready task to a terminal state, running up
//!   to `config.concurrency` in parallel; reply [`RunReport`].
//! - [`TaskGraphSnapshot`] — returns the current graph for introspection/testing.

use std::path::Path;
use std::sync::Arc;

use kameo::actor::ActorRef;
use kameo::message::Context;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;

use crate::backend::AgentBackend;
use crate::config::Config;
use crate::gate::{GateOutcome, GateRunner};
use crate::merge::{MergeOutcome, SquashMerger};
use crate::state_machine::{TaskEvent, transition};
use crate::supervision::{RestartConfig, RootSupervisor};
use crate::task::{Task, TaskGraph, TaskId, TaskState};
use crate::worktree::WorktreeManager;

use super::developer::{Develop, Developer, DeveloperArgs};
use super::reviewer::{Review, ReviewVerdict, Reviewer, ReviewerArgs};

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
/// Holds the active [`TaskGraph`], the shared resources used to drive tasks
/// (worktree manager, gate runner, squash merger, config), and — after
/// [`SetSpokes`] — the dependencies needed to spawn a per-task Developer/Reviewer
/// pair (the `RootSupervisor` ref, the hub's own ref, and the agent backend).
/// Spawnable as a supervised child of [`crate::supervision::RootSupervisor`].
pub struct Supervisor {
    /// The active task graph — the source of truth for scheduling and state.
    ///
    /// `None` until [`SetTaskGraph`] is received.  During a [`RunReadyTasks`]
    /// run the graph is temporarily moved into a shared
    /// `Arc<tokio::sync::Mutex<TaskGraph>>` so the concurrent drivers can update
    /// it without holding `&mut self`; the final graph is moved back here when
    /// the run completes (so [`TaskGraphSnapshot`] keeps working).
    graph: Option<TaskGraph>,

    /// Worktree/branch lifecycle manager.
    ///
    /// `None` until provided via [`SupervisorArgs`].  The Supervisor owns all
    /// worktree create/remove calls (architecture invariant: only the Supervisor
    /// manages worktree+branch lifecycle).  `Clone`/shared by reference into
    /// every driver (the manager is safe for concurrent **distinct** task IDs —
    /// see [`WorktreeManager`]).
    worktree_manager: Option<WorktreeManager>,

    /// The `RootSupervisor` ref, injected post-spawn via [`SetSpokes`].
    ///
    /// Used by the scheduler to spawn each task's Developer/Reviewer as
    /// supervised children of the fault-tolerance root.  `None` until wired.
    root: Option<ActorRef<RootSupervisor>>,

    /// The hub's own ref, injected post-spawn via [`SetSpokes`].
    ///
    /// Needed because a per-task Developer/Reviewer's `Args` require an
    /// `ActorRef<Supervisor>` (the star-topology anchor).  `None` until wired.
    self_ref: Option<ActorRef<Supervisor>>,

    /// The shared agent backend, injected post-spawn via [`SetSpokes`].
    ///
    /// Cloned (`Arc`) into every per-task Developer/Reviewer.  `None` until
    /// wired.
    backend: Option<Arc<dyn AgentBackend>>,

    /// The resolved runtime configuration.
    ///
    /// Supplies `config.gates` (gate command lines), `config.caps.gate_iterations`
    /// (the gate cap), and `config.concurrency` (the parallel-task limit the
    /// scheduler enforces).  The whole [`Config`] is injected so task 25 can read
    /// the reviewer/wall-clock caps from the same place.
    config: Config,

    /// Executes the configured gates in a task's worktree.
    ///
    /// Stateless and reused across all tasks/iterations; shared by reference into
    /// every driver.  See [`GateRunner`].
    gate_runner: GateRunner,

    /// Squash-merges an approved task's branch into the base branch (task 23).
    ///
    /// Built from the same `repo_root` + `base_branch` as the
    /// [`WorktreeManager`].  Stateless beyond its config, so it is shared by
    /// reference into every driver — but the *merge step* mutates the single
    /// shared `develop` checkout, so drivers serialize that step behind the
    /// **develop merge lock** (see [`task_driver`]).  See [`SquashMerger`].
    squash_merger: SquashMerger,
}

/// Construction arguments for [`Supervisor`].
///
/// The [`WorktreeManager`] is supplied at spawn time (it is `Clone`, satisfying
/// the `Args: Clone + Sync` bound for supervised children).  The concurrency
/// dependencies (root ref, self ref, backend) are NOT part of `Args` because the
/// Developer/Reviewer need the Supervisor's ref to be constructed — a
/// construction cycle — so they are injected afterwards via [`SetSpokes`].
#[derive(Clone)]
pub struct SupervisorArgs {
    /// The worktree manager the Supervisor uses to create/tear down worktrees.
    pub worktree_manager: WorktreeManager,

    /// The resolved runtime [`Config`].
    ///
    /// Supplies the gate command lines (`config.gates`), the gate-iteration cap
    /// (`config.caps.gate_iterations`), and the concurrency limit
    /// (`config.concurrency`).  `Config` is `Clone`, satisfying the
    /// `Args: Clone + Sync` bound for supervised children.
    pub config: Config,
}

impl kameo::actor::Actor for Supervisor {
    type Args = SupervisorArgs;
    type Error = std::convert::Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // The squash-merger operates in the SAME main repo the worktree manager
        // branches off, merging into the SAME base branch.  Derive it from the
        // worktree manager's config so there is a single source of truth.
        let squash_merger = SquashMerger::new(
            args.worktree_manager.repo_root.clone(),
            args.worktree_manager.base_branch.clone(),
        );
        Ok(Supervisor {
            graph: None,
            worktree_manager: Some(args.worktree_manager),
            root: None,
            self_ref: None,
            backend: None,
            config: args.config,
            gate_runner: GateRunner::new(),
            squash_merger,
        })
    }
}

// ── RunReport ───────────────────────────────────────────────────────────────────

/// Summary returned by [`RunReadyTasks`].
///
/// Reports the terminal outcome for each task the run touched.  Because tasks run
/// **concurrently**, the order of `outcomes` reflects driver *completion* order
/// (not authored order); tests assert on the set of `(id, state)` pairs (and on
/// the final graph snapshot for ordering-independent state), not on positional
/// order.  Each task appears **exactly once** (single dispatch per task — see
/// [`scheduler`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// `(task_id, final_state)` for every task driven to a terminal state during
    /// this run, in driver-completion order.
    pub outcomes: Vec<(TaskId, TaskState)>,
}

// ── DevelopGateOutcome ──────────────────────────────────────────────────────────

/// Result of one [`develop_until_gates_pass`] round.
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

// ── Shared driver context ───────────────────────────────────────────────────────

/// Shared, cheaply-clonable resources handed to every [`task_driver`].
///
/// All fields are `Arc`/`Clone`, so cloning a `DriverContext` per task is cheap
/// and every driver observes the SAME underlying graph, merge lock, semaphore,
/// and git managers.  This is what lets drivers run concurrently while still
/// keeping the graph as the single source of truth.
#[derive(Clone)]
struct DriverContext {
    /// The shared task graph (source of truth for state + iteration counts).
    ///
    /// A `tokio::sync::Mutex` so the lock is async-aware; **the guard is NEVER
    /// held across an `.await`** (see the lock-discipline note on
    /// [`task_driver`]).
    graph: Arc<Mutex<TaskGraph>>,

    /// The **develop merge lock**: serializes the squash-merge step (the only
    /// part of a driver that mutates the single shared `develop` checkout).  A
    /// `Mutex<()>` whose guard is held for the minimal merge span only.
    merge_lock: Arc<Mutex<()>>,

    /// Worktree/branch lifecycle manager (safe for concurrent distinct IDs).
    worktree_manager: WorktreeManager,

    /// The gate runner (stateless; reused across tasks/iterations).
    gate_runner: GateRunner,

    /// The squash-merger (stateless beyond config; the merge step is serialized
    /// by `merge_lock`).
    squash_merger: SquashMerger,

    /// The resolved runtime config (gates, caps, base branch).
    config: Config,

    /// The `RootSupervisor` ref — each driver spawns its task's Developer and
    /// Reviewer as supervised children of this root.
    root: ActorRef<RootSupervisor>,

    /// The hub's own ref — used as the star-topology anchor in the per-task
    /// Developer/Reviewer `Args`.
    supervisor: ActorRef<Supervisor>,

    /// The shared agent backend, cloned into each per-task Developer/Reviewer.
    backend: Arc<dyn AgentBackend>,
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

/// Inject the **concurrency dependencies** into the Supervisor.
///
/// This post-spawn wiring step breaks the hub↔spoke construction cycle.  Rather
/// than a single shared Developer/Reviewer pair (the task-21 design), the
/// concurrent scheduler (task 24) spawns **one Developer/Reviewer per task**, so
/// the hub needs the pieces to do that spawning:
///
/// - `root` — the [`RootSupervisor`] under which per-task spokes are supervised.
/// - `supervisor` — the hub's own ref (the spokes' star-topology anchor).
/// - `backend` — the shared agent backend cloned into each spoke.
///
/// (The message keeps the historical name `SetSpokes` because it still performs
/// the "wire up the spokes" role; it now wires the *means to spawn* per-task
/// spokes instead of a fixed shared pair.)
pub struct SetSpokes {
    /// The fault-tolerance root under which per-task Developer/Reviewer actors
    /// are spawned as supervised children.
    pub root: ActorRef<RootSupervisor>,
    /// The hub's own ref, used as the star-topology anchor in each per-task
    /// Developer/Reviewer's `Args`.
    pub supervisor: ActorRef<Supervisor>,
    /// The shared agent backend cloned into each per-task Developer/Reviewer.
    pub backend: Arc<dyn AgentBackend>,
}

impl kameo::message::Message<SetSpokes> for Supervisor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetSpokes,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.root = Some(msg.root);
        self.self_ref = Some(msg.supervisor);
        self.backend = Some(msg.backend);
    }
}

// ── RunReadyTasks ─────────────────────────────────────────────────────────────

/// Drive every ready task to a terminal state, running up to
/// `config.concurrency` in parallel.
///
/// A task is "ready" once all of its `depends_on` are `Done`.  The Supervisor
/// launches a driver per ready task (capped by a semaphore at
/// `config.concurrency`), and as each completes it re-evaluates readiness and
/// launches more, until every task is terminal or no progress is possible.  A
/// task becoming `Done` can unlock dependents, which the scheduler then picks up.
///
/// # Reply
///
/// [`RunReport`] listing the terminal outcome of each task driven (in
/// driver-completion order — tasks ran concurrently).  On a per-task hard error
/// that task is recorded as `Failed` and the scheduler stops launching new work
/// (in-flight drivers are still awaited), matching the MVP's fail-fast posture.
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

// ── Orchestration entrypoint ────────────────────────────────────────────────────

impl Supervisor {
    /// Drive every ready task to a terminal state, up to `config.concurrency` at
    /// once.
    ///
    /// This wraps the concurrent [`scheduler`].  It moves the held graph into a
    /// shared `Arc<tokio::sync::Mutex<TaskGraph>>` (so the drivers can update it
    /// without holding `&mut self`), runs the scheduler, then moves the final
    /// graph back into `self` so [`TaskGraphSnapshot`] continues to reflect the
    /// terminal state.
    async fn run_ready_tasks(&mut self) -> Result<RunReport, String> {
        // Take the graph out of self into a shared, lockable handle.  We restore
        // it (the SAME, now-mutated graph) before returning, on every path.
        let graph = self.graph.take().ok_or("supervisor has no graph")?;
        let shared_graph = Arc::new(Mutex::new(graph));

        // Build the shared driver context.  Missing wiring is a hard error — the
        // scheduler cannot spawn per-task spokes without it.
        let ctx = match self.driver_context(Arc::clone(&shared_graph)) {
            Ok(ctx) => ctx,
            Err(e) => {
                // Restore the graph before bailing so self stays consistent.
                self.restore_graph(shared_graph).await;
                return Err(e);
            }
        };

        let result = scheduler(ctx, self.config.concurrency).await;

        // Move the (mutated) graph back into self regardless of the run result.
        self.restore_graph(shared_graph).await;

        result
    }

    /// Build the [`DriverContext`] from the Supervisor's wired resources.
    ///
    /// Errors if the concurrency dependencies (root ref / self ref / backend)
    /// were not injected via [`SetSpokes`], or if the worktree manager is absent.
    fn driver_context(&self, graph: Arc<Mutex<TaskGraph>>) -> Result<DriverContext, String> {
        let worktree_manager = self
            .worktree_manager
            .clone()
            .ok_or("supervisor has no worktree manager")?;
        let root = self
            .root
            .clone()
            .ok_or("supervisor has no RootSupervisor ref (call SetSpokes first)")?;
        let supervisor = self
            .self_ref
            .clone()
            .ok_or("supervisor has no self ref (call SetSpokes first)")?;
        let backend = self
            .backend
            .clone()
            .ok_or("supervisor has no backend (call SetSpokes first)")?;

        Ok(DriverContext {
            graph,
            merge_lock: Arc::new(Mutex::new(())),
            worktree_manager,
            gate_runner: self.gate_runner.clone(),
            squash_merger: self.squash_merger.clone(),
            config: self.config.clone(),
            root,
            supervisor,
            backend,
        })
    }

    /// Move the shared graph back into `self.graph`.
    ///
    /// At the point this is called all drivers have completed (the scheduler has
    /// returned), so we are the sole remaining owner; `try_unwrap` succeeds and
    /// avoids an extra clone.  On the off chance another `Arc` clone is still
    /// alive we fall back to cloning the locked graph.
    async fn restore_graph(&mut self, shared_graph: Arc<Mutex<TaskGraph>>) {
        match Arc::try_unwrap(shared_graph) {
            Ok(mutex) => self.graph = Some(mutex.into_inner()),
            Err(arc) => {
                let g = arc.lock().await;
                self.graph = Some(g.clone());
            }
        }
    }
}

// ── Concurrent scheduler ────────────────────────────────────────────────────────

/// Drive the whole graph to terminal states, running up to `concurrency` task
/// drivers in parallel.
///
/// # How drivers are launched and awaited
///
/// - A [`tokio::sync::Semaphore`] with `concurrency` permits caps how many
///   drivers run at once.  Each launched driver **owns** a permit
///   (`acquire_owned`) for its whole lifetime; the permit is released when the
///   driver future completes — including on error/failure/panic paths — so a
///   crashed driver never leaks a slot.
/// - A [`tokio::task::JoinSet`] holds the in-flight driver futures.  Each entry
///   is the future returned by [`task_driver`] (joined with its `task_id` so the
///   result can be recorded).
/// - The loop alternately **fills** (launch ready tasks until either the
///   semaphore is exhausted or no ready task remains) and **drains** (await the
///   next completed driver, record its outcome, then loop to fill again — a
///   completed `Done` task may have unlocked dependents).  It exits when the
///   `JoinSet` is empty and no further task is ready.
///
/// # Single dispatch per task
///
/// Before launching, a task is moved out of `New`/`Ready` (its FSM advances and
/// the move is recorded under the graph lock), so the *next* `ready_task_ids`
/// scan will not see it again.  Dispatched IDs are also tracked in an in-flight
/// set as belt-and-suspenders.  Thus each task is dispatched to **exactly one**
/// driver (each driver owns its own Developer — "single Developer per task").
///
/// # Fail-fast
///
/// If a driver reports a hard `Err`, the scheduler stops launching *new* work
/// (it records the failure and lets in-flight drivers finish), then returns the
/// error after the `JoinSet` drains.  A task that terminates `Failed` (a normal
/// terminal outcome, e.g. gate-cap) does NOT stop the scheduler launching
/// *independent* ready tasks — but a task whose dependency failed never becomes
/// ready (its dep is not `Done`), so it is simply left non-terminal, mirroring
/// the task-21 posture that a partial failure does not silently satisfy
/// downstream deps.
///
/// # Cancellation seam (task 31)
///
/// Run-control (pause/cancel) is a later task.  The clean seam here: a cancel
/// signal would (a) stop the fill phase from launching new drivers and (b)
/// `abort_all()` the `JoinSet` (each aborted driver drops its permit *and* its
/// `DriverGuard`, which still tears the worktree/spokes down — see
/// [`task_driver`]).  No cancellation is performed today.
async fn scheduler(ctx: DriverContext, concurrency: usize) -> Result<RunReport, String> {
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut join_set: JoinSet<(TaskId, Result<TaskState, String>)> = JoinSet::new();

    // IDs currently dispatched to a driver (defensive against double-dispatch;
    // the FSM advance already removes a task from the ready scan).
    let mut in_flight: std::collections::HashSet<TaskId> = std::collections::HashSet::new();

    let mut outcomes: Vec<(TaskId, TaskState)> = Vec::new();
    let mut fatal_error: Option<String> = None;
    // Once a fatal error is seen we stop *launching* but keep draining in-flight.
    let mut stop_launching = false;

    loop {
        // ── Fill: launch ready tasks until the cap is hit or none remain ───────
        if !stop_launching {
            loop {
                // Try to grab a permit WITHOUT awaiting while holding the graph
                // lock: acquire the permit first (await), then take the graph
                // lock briefly to find+advance a ready task.  Lock ordering:
                // semaphore-permit BEFORE graph lock; the graph lock is released
                // before the driver future (which may take the merge lock) runs.
                let permit = match Arc::clone(&semaphore).try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => break, // cap reached; go drain.
                };

                // Briefly lock the graph to pick + advance ONE ready task.  No
                // .await occurs while the guard is held.
                let next = {
                    let mut graph = ctx.graph.lock().await;
                    let picked = next_ready_task_id(&graph, &in_flight);
                    if let Some(ref id) = picked {
                        // Advance New→Ready if needed so the next scan won't
                        // re-pick this task (single dispatch).  Errors here are
                        // impossible for a freshly-picked New/Ready task, but we
                        // surface them defensively.
                        if let Err(e) = advance_to_ready(&mut graph, id) {
                            // Put nothing in-flight; record fatal and stop.
                            fatal_error.get_or_insert(e);
                            stop_launching = true;
                        }
                    }
                    picked
                }; // graph guard dropped here, BEFORE we spawn / await anything.

                match next {
                    Some(id) if !stop_launching => {
                        in_flight.insert(id.clone());
                        let driver_ctx = ctx.clone();
                        let driver_id = id.clone();
                        // The permit is MOVED into the future; it drops (releasing
                        // the slot) when the driver completes, on every path.
                        join_set.spawn(async move {
                            let _permit = permit; // released on completion/panic.
                            let result = task_driver(&driver_ctx, &driver_id).await;
                            (driver_id, result)
                        });
                    }
                    _ => {
                        // No ready task (or we just hit a fatal error): release the
                        // permit we speculatively took and stop filling.
                        drop(permit);
                        break;
                    }
                }
            }
        }

        // ── Drain: nothing in flight → we're done (no more work possible) ──────
        if join_set.is_empty() {
            break;
        }

        // Await the next completed driver.  `join_next` yields the JoinSet's
        // results as they finish (completion order).
        match join_set.join_next().await {
            Some(Ok((id, Ok(state)))) => {
                in_flight.remove(&id);
                outcomes.push((id, state));
                // A Done task may have unlocked dependents → loop to fill again.
            }
            Some(Ok((id, Err(e)))) => {
                // Hard error in a driver: the driver already moved its task to a
                // terminal state and tore down its resources where possible.
                in_flight.remove(&id);
                fatal_error.get_or_insert(e);
                stop_launching = true; // stop launching new work; drain the rest.
            }
            Some(Err(join_err)) => {
                // The driver task panicked (or was aborted).  Record a fatal
                // error and stop launching; remaining drivers still drain.  The
                // panicking driver's permit was released by the JoinSet, and its
                // `DriverGuard` ran on unwind (worktree/spokes torn down).
                fatal_error.get_or_insert(format!("task driver panicked: {join_err}"));
                stop_launching = true;
            }
            None => break, // JoinSet drained.
        }
    }

    match fatal_error {
        Some(e) => Err(e),
        None => Ok(RunReport { outcomes }),
    }
}

/// Find the next task eligible to run: state `New` or `Ready`, all `depends_on`
/// are `Done`, and it is not already in flight.  Returns its [`TaskId`] or
/// `None`.
///
/// Pure read over the locked graph (the caller holds the guard).  Never awaits.
fn next_ready_task_id(
    graph: &TaskGraph,
    in_flight: &std::collections::HashSet<TaskId>,
) -> Option<TaskId> {
    graph
        .tasks
        .iter()
        .find(|t| {
            matches!(t.state, TaskState::New | TaskState::Ready)
                && !in_flight.contains(&t.id)
                && t.depends_on.iter().all(|dep| {
                    graph
                        .get(dep)
                        .map(|d| d.state == TaskState::Done)
                        .unwrap_or(false)
                })
        })
        .map(|t| t.id.clone())
}

/// Advance a freshly-picked task to `Ready` (if it is still `New`) so the next
/// ready scan will not re-select it.  No-op if already `Ready`.
///
/// Pure mutation over the locked graph (the caller holds the guard).  Never
/// awaits.  Returns `Err` only on an illegal transition (not expected for a
/// New/Ready task) or a missing task.
fn advance_to_ready(graph: &mut TaskGraph, task_id: &TaskId) -> Result<(), String> {
    let state = graph
        .get(task_id)
        .map(|t| t.state)
        .ok_or_else(|| format!("task {task_id} not found in graph"))?;
    if state == TaskState::New {
        apply_event_locked(graph, task_id, TaskEvent::DependenciesSatisfied)?;
    }
    Ok(())
}

// ── Per-task driver ─────────────────────────────────────────────────────────────

/// RAII teardown guard for one task's per-task resources.
///
/// Holds enough to tear down (best-effort) the task's worktree and to `kill` the
/// task's Developer/Reviewer actors.  Teardown runs in `Drop` so that EVERY exit
/// path of [`task_driver`] — `Ok`, `Err`, early-return, or panic/unwind — frees
/// the worktree + branch and stops the per-task spokes (no leaks).
///
/// Worktree removal is async (`WorktreeManager::remove`), but `Drop` is sync; we
/// therefore tear the worktree down explicitly in the driver's terminal paths
/// (where we can `.await`) and use this guard as the **safety net** for the
/// unexpected/early-return/panic paths via a detached best-effort spawn on the
/// current runtime.  The actor `kill()` calls are synchronous and always run in
/// `Drop`.  In the normal (Ok/Err terminal) paths the driver has already awaited
/// `remove` and set `worktree_removed = true`, so Drop only kills the spokes.
struct DriverGuard {
    task_id: String,
    worktree_manager: WorktreeManager,
    developer: Option<ActorRef<Developer>>,
    reviewer: Option<ActorRef<Reviewer>>,
    /// Set to `true` once the driver has already torn the worktree down on a
    /// normal terminal path, so `Drop` does not redundantly try again.
    worktree_removed: bool,
}

impl Drop for DriverGuard {
    fn drop(&mut self) {
        // Always stop the per-task spokes (sync, cheap, idempotent).
        if let Some(dev) = self.developer.take() {
            dev.kill();
        }
        if let Some(rev) = self.reviewer.take() {
            rev.kill();
        }

        // Safety-net worktree teardown for paths that did not already remove it
        // (e.g. an unexpected early return or a panic).  Normal terminal paths
        // set `worktree_removed = true` after awaiting `remove`, so this is a
        // no-op there.  We cannot `.await` in Drop, so schedule a detached
        // best-effort removal on the runtime.
        if !self.worktree_removed {
            let mgr = self.worktree_manager.clone();
            let id = self.task_id.clone();
            // `tokio::spawn` requires being inside a runtime; the driver always
            // runs inside one (JoinSet task).  Best-effort: remove() is
            // idempotent and treats "not found" as success.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = mgr.remove(&id).await;
                });
            }
        }
    }
}

/// Drive a single task from its current state through the full develop→review
/// loop to a terminal state, returning that terminal state.
///
/// This is the per-task lifecycle extracted from task 21's `run_single_task`,
/// now operating on the **shared** graph (via the [`DriverContext`]) and using a
/// **per-task** Developer/Reviewer pair (spawned as supervised children of the
/// `RootSupervisor`).  *Within-task* behavior is unchanged from task 21–23: FSM
/// transitions, the dev+gate loop ([`develop_until_gates_pass`]), the review
/// loop, the squash-merge on approve, and worktree create/teardown all happen
/// exactly as before — only the graph access is now lock-guarded and the spokes
/// are task-local.
///
/// # Graph-lock discipline (the deadlock/race surface — read this)
///
/// The shared graph is a `tokio::sync::Mutex<TaskGraph>`.  **The guard is NEVER
/// held across an `.await`.**  Every read/mutation is a tight critical section:
/// acquire → read-or-mutate → drop the guard — and ONLY THEN do we `.await`
/// (spawn a session, run gates, squash-merge).  Concretely, the helpers
/// `task_state_locked`, `apply_event_locked`, `task_clone_locked`, the
/// iteration-count bumps, and the `mark_*` stamps each take the lock, do their
/// synchronous work, and release it before the next await point.  This keeps the
/// graph live for the future TUI without serializing the drivers.
///
/// # Merge-lock span & lock ordering (no deadlock)
///
/// The squash-merge is the only step that mutates the single shared `develop`
/// checkout, so it is wrapped in the **develop merge lock**
/// ([`DriverContext::merge_lock`]).  The lock is taken for the *minimal* span —
/// just the `squash_merge` call — and released immediately after.
///
/// Lock ordering is strictly:
///
/// 1. **semaphore permit** (held by the scheduler for the driver's lifetime),
/// 2. **graph lock** (taken/released in tight non-awaiting sections, NEVER held
///    while awaiting anything),
/// 3. **merge lock** (taken only for the merge, and we do NOT hold the graph
///    lock while awaiting it).
///
/// No driver ever holds the graph lock while trying to acquire the merge lock
/// (we drop the graph guard before the merge), and no code acquires the graph
/// lock *while holding* the merge lock except via the same tight non-awaiting
/// helpers used everywhere (lock → mutate → unlock) — so there is no lock cycle
/// and thus no deadlock.  The semaphore is only ever *awaited* by the scheduler
/// (never by a driver while holding either mutex).
///
/// # Resource release on every path (no leaks)
///
/// A [`DriverGuard`] (RAII) kills the per-task spokes and best-effort-removes the
/// worktree on Drop, covering early-returns/panics.  The normal terminal paths
/// additionally `await` worktree removal explicitly (and mark the guard so it
/// won't double-remove).  The semaphore permit is owned by the spawned future
/// and released when this function returns (Ok or Err) or panics.
async fn task_driver(ctx: &DriverContext, task_id: &TaskId) -> Result<TaskState, String> {
    // ── Spawn this task's OWN Developer + Reviewer (single Developer per task) ─
    //
    // Supervised children of the RootSupervisor, sharing the hub ref (star
    // anchor) and the shared backend Arc.  Torn down by the DriverGuard.
    let developer = RootSupervisor::spawn_child::<Developer>(
        &ctx.root,
        DeveloperArgs {
            supervisor: ctx.supervisor.clone(),
            backend: Arc::clone(&ctx.backend),
        },
        RestartConfig::default(),
    )
    .await;
    let reviewer = RootSupervisor::spawn_child::<Reviewer>(
        &ctx.root,
        ReviewerArgs {
            supervisor: ctx.supervisor.clone(),
            backend: Arc::clone(&ctx.backend),
        },
        RestartConfig::default(),
    )
    .await;

    let mut guard = DriverGuard {
        task_id: task_id.0.clone(),
        worktree_manager: ctx.worktree_manager.clone(),
        developer: Some(developer.clone()),
        reviewer: Some(reviewer.clone()),
        worktree_removed: false,
    };

    // ── Step 1: ensure Ready (New → Ready). ────────────────────────────────────
    // The scheduler already advanced New→Ready before dispatch, but re-entry or
    // a directly-Ready task is handled idempotently here.
    {
        let mut graph = ctx.graph.lock().await;
        let current = task_state_locked(&graph, task_id)?;
        if current == TaskState::New {
            apply_event_locked(&mut graph, task_id, TaskEvent::DependenciesSatisfied)?;
        }
    } // graph guard dropped before any await.

    // ── Step 2: create the worktree, then Ready → InProgress (Dispatched) ──────
    let worktree = ctx
        .worktree_manager
        .create(&task_id.0)
        .await
        .map_err(|e| format!("worktree create failed for {task_id}: {e}"))?;
    {
        let mut graph = ctx.graph.lock().await;
        apply_event_locked(&mut graph, task_id, TaskEvent::Dispatched)?;
        mark_started_locked(&mut graph, task_id);
    }

    // ── Step 3–6: the develop → gate → review loop (bounded retry) ─────────────
    let mut feedback: Option<String> = None;
    let terminal_state;

    loop {
        // ── Develop + gate loop (task 22) ──────────────────────────────────────
        match develop_until_gates_pass(ctx, task_id, &developer, &worktree.path, feedback.take())
            .await
        {
            Ok(DevelopGateOutcome::ReadyForReview) => {
                // Gates passed; task is now InReview. Fall through to the Reviewer.
            }
            Ok(DevelopGateOutcome::GateCapReached) => {
                // The gate cap fired: the helper already moved the task to Failed
                // and tore down the worktree.
                guard.worktree_removed = true;
                terminal_state = TaskState::Failed;
                break;
            }
            Err(e) => {
                // Hard error during development (the helper already moved the task
                // to Failed and tore down the worktree).
                guard.worktree_removed = true;
                return Err(e);
            }
        }

        // ── Reviewer turn ──────────────────────────────────────────────────────
        let review_task = {
            let graph = ctx.graph.lock().await;
            task_clone_locked(&graph, task_id)?
        };
        let review_result = reviewer
            .ask(Review {
                task: review_task,
                worktree: worktree.path.clone(),
            })
            .send()
            .await;

        // On reviewer ask/parse failure, clean up the worktree before propagating.
        // We are in InReview; InReview --HardError--> Failed is illegal (only
        // ReviewCapReached exists), so — as in task 21 — we do NOT force an FSM
        // transition; we clean up the resource and return the error. Task 25 owns
        // the InReview→Failed hard-error path.
        let verdict = match review_result {
            Ok(v) => v,
            Err(e) => {
                remove_worktree(ctx, task_id).await;
                guard.worktree_removed = true;
                return Err(format!("reviewer dispatch failed for {task_id}: {e}"));
            }
        };

        match verdict {
            ReviewVerdict::Approve => {
                // ── Approve: squash-merge `task/{id}` into `develop` (task 23) ──
                //
                // The merge happens HERE — while still in InReview, BEFORE tearing
                // down the worktree and BEFORE the FSM approve transition.
                //
                // ── MERGE SERIALIZATION (task 24) ──────────────────────────────
                //
                // The squash-merge mutates the single shared `develop` checkout in
                // repo_root; two concurrent merges would race on it. We therefore
                // hold the develop MERGE LOCK for exactly this critical section.
                // We do NOT hold the graph lock while awaiting the merge lock (the
                // review_task clone above already released the graph guard), so
                // there is no lock-ordering cycle.
                let branch = format!("task/{task_id}");
                let message = {
                    let graph = ctx.graph.lock().await;
                    squash_commit_message_locked(&graph, task_id)?
                };

                let merge_outcome = {
                    // Minimal critical section: acquire → merge → release.
                    let _merge_guard = ctx.merge_lock.lock().await;
                    ctx.squash_merger.squash_merge(&branch, &message).await
                }; // merge lock released here.

                let merge_outcome = match merge_outcome {
                    Ok(o) => o,
                    Err(e) => {
                        // Hard merge failure; develop already best-effort-restored
                        // by the merger. Clean up + propagate (no illegal FSM move).
                        remove_worktree(ctx, task_id).await;
                        guard.worktree_removed = true;
                        return Err(format!("squash-merge failed for {task_id}: {e}"));
                    }
                };

                match merge_outcome {
                    MergeOutcome::Merged => {
                        // ── Merged: InReview → Done (ReviewerApproved) ──────────
                        {
                            let mut graph = ctx.graph.lock().await;
                            apply_event_locked(&mut graph, task_id, TaskEvent::ReviewerApproved)?;
                            mark_finished_locked(&mut graph, task_id);
                        }
                        // Tear down the worktree + branch (the work has landed).
                        remove_worktree(ctx, task_id).await;
                        guard.worktree_removed = true;
                        terminal_state = TaskState::Done;
                        break;
                    }
                    MergeOutcome::Conflict { details } => {
                        // ── Conflict: reconcile, do NOT corrupt `develop` ───────
                        //
                        // `develop` is ALREADY safely restored by the merger (its
                        // hard invariant). The architecture's agent-driven
                        // reconciliation is a documented seam (see task 23 notes);
                        // the MVP drives the task to a SAFE terminal Failed via
                        // ReviewCapReached (the only legal InReview→Failed event
                        // today). Task 25 adds a dedicated merge-conflict terminal
                        // event + bounded retry budget.
                        let _ = details; // surfaced to the seam; logged by a later task.
                        {
                            let mut graph = ctx.graph.lock().await;
                            apply_event_locked(&mut graph, task_id, TaskEvent::ReviewCapReached)?;
                            mark_finished_locked(&mut graph, task_id);
                        }
                        remove_worktree(ctx, task_id).await;
                        guard.worktree_removed = true;
                        terminal_state = TaskState::Failed;
                        break;
                    }
                }
            }
            ReviewVerdict::Reject { feedback: fb } => {
                // ── Reject: InReview → InProgress (ReviewerRejected) ────────────
                let iterations = {
                    let mut graph = ctx.graph.lock().await;
                    apply_event_locked(&mut graph, task_id, TaskEvent::ReviewerRejected)?;
                    increment_review_iterations_locked(&mut graph, task_id);
                    review_iterations_locked(&graph, task_id)?
                };

                // ── Seam: termination caps (task 25) ────────────────────────────
                //
                // The simple bounded retry exists ONLY so this loop can never spin
                // forever in a test. Task 25 replaces it with
                // config.caps.reviewer_iterations (+ gate + wall-clock caps),
                // emitting ReviewCapReached on exhaustion. Each driver counts its
                // OWN task's iterations (per-task cap under the graph lock), so the
                // cap is correct under concurrency.
                if iterations >= MAX_REVIEWER_ITERATIONS {
                    // Bounded-retry safety stop. We are in InProgress after the
                    // reject transition, so HardError → Failed is legal (the real
                    // event is ReviewCapReached — task 25).
                    {
                        let mut graph = ctx.graph.lock().await;
                        apply_event_locked(&mut graph, task_id, TaskEvent::HardError)?;
                        mark_finished_locked(&mut graph, task_id);
                    }
                    remove_worktree(ctx, task_id).await;
                    guard.worktree_removed = true;
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
/// Identical within-task behavior to task 22 — only adapted for the concurrent
/// driver: it takes the [`DriverContext`] (shared graph + gate runner + config)
/// and the **per-task** Developer ref, and observes the graph-lock discipline
/// (the guard is never held across an `.await`).
///
/// Drives: dispatch the Developer (with `initial_feedback`), run all configured
/// gates in the worktree; on a gate failure self-loop (InProgress
/// --GateFailed--> InProgress), bump `gate_iterations`, feed the failing gate's
/// output back, and re-run all gates; until gates pass (→ `GatesPassed` /
/// InReview, return `ReadyForReview`) or the gate cap fires (→ `GateCapReached` /
/// Failed, worktree torn down, return `GateCapReached`).
///
/// # Errors
///
/// `Err(String)` on a hard error (Developer dispatch failure, or a gate that
/// could not be **launched**).  In the error case the task was already moved to
/// Failed (HardError) and the worktree torn down; the caller just propagates.
async fn develop_until_gates_pass(
    ctx: &DriverContext,
    task_id: &TaskId,
    developer: &ActorRef<Developer>,
    worktree_path: &Path,
    initial_feedback: Option<String>,
) -> Result<DevelopGateOutcome, String> {
    let mut feedback = initial_feedback;

    loop {
        // ── Developer turn: make (or fix) the changes ──────────────────────────
        let task = {
            let graph = ctx.graph.lock().await;
            task_clone_locked(&graph, task_id)?
        };

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
            {
                let mut graph = ctx.graph.lock().await;
                apply_event_locked(&mut graph, task_id, TaskEvent::HardError)?;
                mark_finished_locked(&mut graph, task_id);
            }
            remove_worktree(ctx, task_id).await;
            return Err(format!("developer dispatch failed for {task_id}: {e}"));
        }

        // ── Gate turn: run ALL configured gates in the worktree ────────────────
        let outcome = ctx
            .gate_runner
            .run_gates(&ctx.config.gates, worktree_path)
            .await;

        match outcome {
            Ok(GateOutcome::Passed) => {
                // All gates passed → advance to review.
                {
                    let mut graph = ctx.graph.lock().await;
                    apply_event_locked(&mut graph, task_id, TaskEvent::GatesPassed)?;
                }
                return Ok(DevelopGateOutcome::ReadyForReview);
            }
            Ok(GateOutcome::Failed {
                gate,
                output,
                exit_code,
            }) => {
                // A gate failed → self-loop and count the iteration; enforce the
                // per-task GATE cap.
                let iterations = {
                    let mut graph = ctx.graph.lock().await;
                    apply_event_locked(&mut graph, task_id, TaskEvent::GateFailed)?;
                    increment_gate_iterations_locked(&mut graph, task_id);
                    gate_iterations_locked(&graph, task_id)?
                };

                if iterations >= ctx.config.caps.gate_iterations {
                    // GateCapReached: InProgress → Failed (terminal).
                    {
                        let mut graph = ctx.graph.lock().await;
                        apply_event_locked(&mut graph, task_id, TaskEvent::GateCapReached)?;
                        mark_finished_locked(&mut graph, task_id);
                    }
                    remove_worktree(ctx, task_id).await;
                    return Ok(DevelopGateOutcome::GateCapReached);
                }

                // Feed the failing gate's output back; all gates re-run next turn.
                feedback = Some(format!(
                    "Gate `{gate}` failed (exit code {exit_code}):\n{output}\n\
                     Fix the issue so the gate passes."
                ));
            }
            Err(e) => {
                // The gate command could not be LAUNCHED (infra failure). Treat as
                // a hard error: we are in InProgress, so HardError → Failed.
                {
                    let mut graph = ctx.graph.lock().await;
                    apply_event_locked(&mut graph, task_id, TaskEvent::HardError)?;
                    mark_finished_locked(&mut graph, task_id);
                }
                remove_worktree(ctx, task_id).await;
                return Err(format!("gate launch failed for {task_id}: {e}"));
            }
        }
    }
}

/// Best-effort worktree teardown (idempotent; ignores "already gone").
async fn remove_worktree(ctx: &DriverContext, task_id: &TaskId) {
    // `remove` is idempotent and treats "not found" as success.
    let _ = ctx.worktree_manager.remove(&task_id.0).await;
}

// ── Locked graph helpers (NEVER hold the guard across an .await) ──────────────────
//
// Each of these takes/returns plain values and is called by a caller that holds
// the `tokio::sync::Mutex<TaskGraph>` guard for the duration of the call ONLY —
// the guard is dropped before the caller's next await point.  They are free
// functions (not methods) so they operate on a borrowed `TaskGraph` rather than
// `&mut self`, which is what lets the concurrent drivers share the graph.

/// Apply an FSM `event` to the task, updating its state in the locked graph.
fn apply_event_locked(
    graph: &mut TaskGraph,
    task_id: &TaskId,
    event: TaskEvent,
) -> Result<(), String> {
    let task = task_mut_locked(graph, task_id)?;
    let next = transition(task.state, event)
        .map_err(|e| format!("illegal transition for {task_id}: {e}"))?;
    task.state = next;
    task.updated_at = chrono::Utc::now();
    Ok(())
}

/// Read a task's current state from the locked graph.
fn task_state_locked(graph: &TaskGraph, task_id: &TaskId) -> Result<TaskState, String> {
    graph
        .get(task_id)
        .map(|t| t.state)
        .ok_or_else(|| format!("task {task_id} not found in graph"))
}

/// Build the squash-merge commit message for a task: `task({id}): {title}`.
fn squash_commit_message_locked(graph: &TaskGraph, task_id: &TaskId) -> Result<String, String> {
    let task = graph
        .get(task_id)
        .ok_or_else(|| format!("task {task_id} not found in graph"))?;
    Ok(format!(
        "task({id}): {title}",
        id = task.id,
        title = task.title
    ))
}

/// Clone a task out of the locked graph (to hand a stable snapshot to a spoke).
fn task_clone_locked(graph: &TaskGraph, task_id: &TaskId) -> Result<Task, String> {
    graph
        .get(task_id)
        .cloned()
        .ok_or_else(|| format!("task {task_id} not found in graph"))
}

/// Mutable access to a task in the locked graph.
fn task_mut_locked<'g>(graph: &'g mut TaskGraph, task_id: &TaskId) -> Result<&'g mut Task, String> {
    graph
        .tasks
        .iter_mut()
        .find(|t| &t.id == task_id)
        .ok_or_else(|| format!("task {task_id} not found in graph"))
}

/// Read a task's reviewer-iteration count from the locked graph.
fn review_iterations_locked(graph: &TaskGraph, task_id: &TaskId) -> Result<u32, String> {
    graph
        .get(task_id)
        .map(|t| t.review_iterations)
        .ok_or_else(|| format!("task {task_id} not found in graph"))
}

/// Increment a task's reviewer-iteration counter (FSM-external bookkeeping).
fn increment_review_iterations_locked(graph: &mut TaskGraph, task_id: &TaskId) {
    if let Ok(task) = task_mut_locked(graph, task_id) {
        task.review_iterations += 1;
        task.updated_at = chrono::Utc::now();
    }
}

/// Read a task's gate-iteration count from the locked graph.
fn gate_iterations_locked(graph: &TaskGraph, task_id: &TaskId) -> Result<u32, String> {
    graph
        .get(task_id)
        .map(|t| t.gate_iterations)
        .ok_or_else(|| format!("task {task_id} not found in graph"))
}

/// Increment a task's gate-iteration counter (FSM-external bookkeeping).
fn increment_gate_iterations_locked(graph: &mut TaskGraph, task_id: &TaskId) {
    if let Ok(task) = task_mut_locked(graph, task_id) {
        task.gate_iterations += 1;
        task.updated_at = chrono::Utc::now();
    }
}

/// Stamp `started_at` when the Developer first picks up the task.
fn mark_started_locked(graph: &mut TaskGraph, task_id: &TaskId) {
    if let Ok(task) = task_mut_locked(graph, task_id)
        && task.started_at.is_none()
    {
        task.started_at = Some(chrono::Utc::now());
    }
}

/// Stamp `finished_at` when the task reaches a terminal state.
fn mark_finished_locked(graph: &mut TaskGraph, task_id: &TaskId) {
    if let Ok(task) = task_mut_locked(graph, task_id) {
        task.finished_at = Some(chrono::Utc::now());
    }
}
