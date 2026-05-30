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
//!     on create failure: --HardError--> Failed ; task fails   (task 25)
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
//!       (dispatch/parse failure) --HardError--> Failed ; teardown   (task 25)
//!       Approve  squash_merge(task/{id} → develop)      (task 23 — BEFORE teardown)
//!                 Merged    --ReviewerApproved--> Done ; WorktreeManager::remove
//!                 Conflict  develop already restored clean by merger ;
//!                           --ReviewCapReached--> Failed ; teardown  (safe-fail; agent-reconcile seam)
//!                 (hard merge err) --HardError--> Failed ; teardown  (task 25)
//!       Reject{feedback}
//!                 if review_iterations + 1 >= caps.reviewer_iterations:   (task 25)
//!                   --ReviewCapReached--> Failed ; teardown ; task fails
//!                 else: --ReviewerRejected--> InProgress ;
//!                   review_iterations += 1 ; relay feedback ; re-develop+gate
//! ```
//!
//! On top of the develop→review loop, the **scheduler** wraps each driver in a
//! per-task `tokio::time::timeout(config.caps.wall_clock_secs)`.  If the deadline
//! fires the driver future is cancelled (its [`DriverGuard`] tears down the
//! worktree + spokes) and the scheduler applies `WallClockCapReached` → `Failed`
//! under the graph lock (task 25).
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
//! ## Termination caps (task 25 — implemented)
//!
//! All three caps come from `config.caps` and each independently drives a task
//! to terminal `Failed`:
//!
//! - **Gate cap** (`caps.gate_iterations`): enforced in [`develop_until_gates_pass`]
//!   (task 22); on exhaustion emits `GateCapReached` (InProgress → Failed).
//! - **Reviewer cap** (`caps.reviewer_iterations`): enforced in [`task_driver`]'s
//!   reject branch.  The decision is made BEFORE the FSM transition: if this
//!   rejection *reaches* the cap (`review_iterations + 1 >= cap`) the driver
//!   emits `ReviewCapReached` (InReview → Failed) and terminates the task
//!   instead of looping back to develop.  This uses `ReviewCapReached` from its
//!   intended state (`InReview`) and replaced the old stand-in constant.
//! - **Wall-clock cap** (`caps.wall_clock_secs`): enforced by the [`scheduler`],
//!   which wraps each driver future in `tokio::time::timeout`.  On elapse the
//!   driver is cancelled (its [`DriverGuard`] cleans up) and the scheduler emits
//!   `WallClockCapReached` (Ready/InProgress/InReview → Failed) under the graph
//!   lock.
//!
//! Concurrency keeps the iteration caps **per task** (each driver counts its own
//! task's iterations under the graph lock — see [`task_driver`]); the wall-clock
//! cap is per task because each driver future has its own timeout.
//!
//! ## Deferred seams (do NOT implement here)
//!
//! - **Run control** (task 31): pause/cancel is not implemented; the
//!   [`scheduler`] leaves a documented cancellation seam (drop the `JoinSet` /
//!   close the semaphore) but does not act on it.
//! - **Idle / heartbeat detection** (FUTURE): only the three caps above exist;
//!   there is no per-step idle timeout.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kameo::actor::ActorRef;
use kameo::message::Context;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::api;
use crate::audit::{AuditRegistry, NoopAuditRegistry};
use crate::backend::AgentBackend;
use crate::config::Config;
use crate::gate::{GateOutcome, GateRunner};
use crate::merge::{MergeOutcome, SquashMerger};
use crate::persist::persist_graph;
use crate::state_machine::{TaskEvent, transition};
use crate::supervision::{RestartConfig, RootSupervisor};
use crate::task::{Task, TaskGraph, TaskId, TaskState};
use crate::worktree::WorktreeManager;

use super::developer::{Develop, Developer, DeveloperArgs};
use super::reviewer::{Review, ReviewVerdict, Reviewer, ReviewerArgs};

// ── Live event emission (task 31: run-control) ──────────────────────────────────

/// A sink for the live [`api::Event`]s the Supervisor scheduler/drivers emit.
///
/// The orchestrator (`CoreApi`) wires this to its broadcast so the TUI observes
/// execution.  It is an `Arc<dyn Fn(api::Event) + Send + Sync>` — a cheap,
/// `Clone`able callback chosen over a concrete `broadcast::Sender` so the engine
/// stays decoupled from *how* events are delivered (the orchestrator can adapt
/// it to a broadcast, an mpsc, or a test recorder).
///
/// Emission is **additive** and best-effort: dropping events (no live receiver)
/// is fine, and the engine NEVER holds the graph lock across an emit (the sink
/// is called only outside the tight locked sections).  See [`RunControl`].
pub type EventSink = Arc<dyn Fn(api::Event) + Send + Sync>;

/// Per-run control + observability bundle threaded through the scheduler.
///
/// Bundles the four things a *controlled* run needs beyond the static driver
/// resources:
///
/// - `run` — the [`api::RunId`] every emitted event is tagged with.
/// - `sink` — where live events go (see [`EventSink`]).
/// - `pause` — when `true`, the scheduler stops launching NEW task drivers
///   (in-flight tasks finish); clearing it and re-running resumes.  (Task 31
///   pause semantics: "stop launching new tasks".)
/// - `cancel` — a [`CancellationToken`]; when cancelled the scheduler stops
///   launching and `abort_all()`s the in-flight `JoinSet` (each aborted
///   driver's [`DriverGuard`] still tears down its worktree/spokes — no leak).
///
/// The `RunReadyTasks` ask path (the task-21–25 tests) uses
/// [`RunControl::silent`]: a no-op sink, never paused, never cancelled — so the
/// existing behavior is byte-for-byte unchanged (events are purely additive).
#[derive(Clone)]
pub struct RunControl {
    /// The Run these events/controls belong to.
    pub run: api::RunId,
    /// Where live [`api::Event`]s are published.
    pub sink: EventSink,
    /// Cooperative pause flag: while `true`, no NEW drivers are launched.
    pub pause: Arc<AtomicBool>,
    /// Cancellation signal: stops launching + aborts in-flight drivers.
    pub cancel: CancellationToken,
}

impl RunControl {
    /// A control that emits nothing, never pauses, and never cancels.
    ///
    /// Used by the `RunReadyTasks` ask path so the engine behaves exactly as it
    /// did before task 31 (events are additive; the scheduler's pause/cancel
    /// checks are inert).
    pub fn silent() -> Self {
        Self {
            run: api::RunId(0),
            sink: Arc::new(|_| {}),
            pause: Arc::new(AtomicBool::new(false)),
            cancel: CancellationToken::new(),
        }
    }

    /// Emit one event to the sink (best-effort; never panics).
    fn emit(&self, event: api::Event) {
        (self.sink)(event);
    }
}

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
    /// Supplies `config.gates` (gate command lines), all three termination caps
    /// (`config.caps.{gate_iterations, reviewer_iterations, wall_clock_secs}` —
    /// task 25), and `config.concurrency` (the parallel-task limit the scheduler
    /// enforces).  The whole [`Config`] is injected so every cap reads from the
    /// same place.
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
    /// Supplies the gate command lines (`config.gates`), the three termination
    /// caps (`config.caps.{gate_iterations, reviewer_iterations, wall_clock_secs}`),
    /// and the concurrency limit (`config.concurrency`).  `Config` is `Clone`,
    /// satisfying the `Args: Clone + Sync` bound for supervised children.
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
    /// `(task_id, reason)` for every task that reached a `Failed` terminal during
    /// this run (a completed-but-failed run keeps going — see
    /// [`scheduler`] — so the report records *which* tasks failed and *why*).
    /// The `String` is a short failure reason (a driver hard-error message, or a
    /// synthesized cap literal such as `"wall-clock-cap-reached"`).
    pub failed_tasks: Vec<(TaskId, String)>,
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

    /// Per-run control + live-event sink (task 31).  Threaded into the scheduler
    /// (pause/cancel checks) and every [`task_driver`] (event emission +
    /// Developer/Reviewer `AgentExchange`).  The `RunReadyTasks` ask path passes
    /// [`RunControl::silent`] so behavior is unchanged there.
    control: RunControl,

    /// The audit registry: the Supervisor calls this to associate each task's
    /// worktree `working_dir` with its `(run_id, slug, task_id)` context,
    /// enabling the [`crate::audit::JsonlAuditSink`] to route audit entries to
    /// the correct `.tasks/{slug}/audit.jsonl` file.
    ///
    /// The `RunReadyTasks` ask path passes [`crate::audit::NoopAuditRegistry`]
    /// so the ask-path behavior is unchanged.
    audit_registry: Arc<dyn AuditRegistry>,

    /// The task-graph slug (file stem of the task-list file), used as the
    /// sub-directory name under `.tasks/` when routing audit entries.
    ///
    /// Derived by the orchestrator from the task-list path when the run is
    /// started; the `RunReadyTasks` ask path uses an empty string (no-op with
    /// `NoopAuditRegistry`).
    run_slug: String,

    /// The persistent, sortable run identity (26-char ULID string) minted by the
    /// orchestrator when the run is opened, threaded through so the audit ledger
    /// can key entries on a stable cross-process run id.
    ///
    /// The `RunReadyTasks` ask path uses an empty string (no-op with
    /// `NoopAuditRegistry`).
    ///
    /// Consumed by the `AuditRegistry::register` call in `dispatch_task`, which
    /// passes it as the 2nd arg so the audit ledger can key entries on the
    /// stable cross-process run id.
    run_uid: String,

    /// The plan slug (lowercased-kebab of the task-list's parent directory name),
    /// threaded from the orchestrator so per-task worktree calls can plan-scope
    /// their directory + branch names.
    ///
    /// The `RunReadyTasks` ask path uses an empty string.
    ///
    /// Read when building per-task worktree directory + branch names: the
    /// driver passes it to `WorktreeManager::create`/`remove` so the worktree
    /// dir + branch are plan-scoped as `{plan_slug}--{task_id}`.
    plan_slug: String,
}

impl DriverContext {
    /// Emit `TaskStateChanged{run, task, state}` for this run (task 31).
    ///
    /// Maps the domain [`TaskState`] to its [`api::TaskState`] mirror and calls
    /// the control sink.  Called **after** the graph guard is dropped (never
    /// while holding the lock — the no-lock-across-emit rule), reading the state
    /// the just-applied transition produced.  A no-op under the silent control.
    fn emit_task_state(&self, task_id: &TaskId, state: TaskState) {
        self.control.emit(api::Event::TaskStateChanged {
            run: self.control.run,
            task: api::TaskId(task_id.0.clone()),
            state: state.into(),
        });
    }

    /// Emit `TaskIterationsUpdated{run, task, gate, review}` for this run.
    ///
    /// Called after a gate/review counter bump, outside the graph lock.
    fn emit_task_iterations(&self, task_id: &TaskId, gate_iterations: u32, review_iterations: u32) {
        self.control.emit(api::Event::TaskIterationsUpdated {
            run: self.control.run,
            task: api::TaskId(task_id.0.clone()),
            gate_iterations,
            review_iterations,
        });
    }

    /// Snapshot the graph under the lock and write it to `.tasks/{slug}.json`
    /// outside the lock (best-effort: logs on failure, never fails the run).
    ///
    /// # Design
    ///
    /// 1. Acquires the graph mutex for the minimal duration needed to clone the
    ///    current state (a tight, non-awaiting critical section).
    /// 2. Releases the lock **before** the async write, keeping the lock-hold
    ///    span minimal (architecture invariant: no `.await` while holding the
    ///    graph lock).
    /// 3. Persists the clone via [`persist_graph`] with `repo_root` derived from
    ///    the worktree manager.
    /// 4. On any I/O error: logs a `tracing::warn!` and returns — the run
    ///    continues unaffected (best-effort persistence).
    async fn persist(&self) {
        // Clone the graph under the lock (tight critical section; no await).
        let snapshot = {
            let g = self.graph.lock().await;
            g.clone()
        };
        // Write outside the lock.
        let repo_root = &self.worktree_manager.repo_root;
        if let Err(e) = persist_graph(&snapshot, repo_root).await {
            tracing::warn!(
                slug = %snapshot.slug,
                error = %e,
                "supervisor: persist_graph failed (best-effort, run continues)"
            );
        }
    }
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
/// driver-completion order — tasks ran concurrently).  A per-task hard error
/// records that task as `Failed` (and transitively `Skipped`s its dependents)
/// but does NOT halt the run (sched-continue-on-failure): the scheduler keeps
/// launching the remaining independent ready tasks. Only a genuine driver
/// *panic* (or a cancel) stops launching new work; in-flight drivers are always
/// awaited.
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
        // scheduler cannot spawn per-task spokes without it.  The `RunReadyTasks`
        // ask path is uncontrolled: no events, never paused/cancelled, no audit
        // registry (noop).
        let ctx = match self.driver_context(
            Arc::clone(&shared_graph),
            RunControl::silent(),
            Arc::new(NoopAuditRegistry),
            String::new(),
            String::new(),
            String::new(),
        ) {
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
    /// `control` carries the per-run event sink + pause/cancel signals (task 31);
    /// pass [`RunControl::silent`] for the uncontrolled ask path.
    /// `audit_registry`, `run_slug`, and `run_uid` are for the audit ledger;
    /// `plan_slug` plan-scopes per-task worktree calls. Pass
    /// `Arc::new(NoopAuditRegistry)` / `String::new()` ×3 for the ask path.
    fn driver_context(
        &self,
        graph: Arc<Mutex<TaskGraph>>,
        control: RunControl,
        audit_registry: Arc<dyn AuditRegistry>,
        run_slug: String,
        run_uid: String,
        plan_slug: String,
    ) -> Result<DriverContext, String> {
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
            control,
            audit_registry,
            run_slug,
            run_uid,
            plan_slug,
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

// ── Public controlled entrypoint (task 31: run-control) ─────────────────────────

/// Run a whole [`TaskGraph`] to terminal states under a [`RunControl`], emitting
/// live [`api::Event`]s and honouring pause/cancel — the entrypoint the
/// orchestrator (`CoreApi`) spawns on a background task.
///
/// This builds a fresh actor tree (a [`RootSupervisor`] plus one [`Supervisor`]
/// hub, wired via [`SetSpokes`]) over the shared graph, then runs the SAME
/// concurrent [`scheduler`] the `RunReadyTasks` ask path uses — only with a real
/// (non-silent) `control` so it publishes events and reacts to pause/cancel.
/// The actor tree is torn down (`root.kill()`) on every exit path.
///
/// Lifecycle events emitted here (the scheduler/drivers emit the per-task ones):
/// - `RunStatusChanged{run, Running}` once, at the start;
/// - `RunStatusChanged{run, Completed|Failed}` at the end, derived from the
///   final graph — UNLESS the run was cancelled (the caller owns the cancelled
///   status so an explicit Cancel shows as `Failed`/cancelled, not `Completed`).
///
/// The graph-lock-never-across-await invariant and the per-task [`DriverGuard`]
/// teardown are unchanged; this only wraps the scheduler with wiring + the two
/// aggregate `RunStatusChanged` emissions.
// The orchestrator threads the full run context (graph + wiring + audit slug +
// run_uid) into this single entrypoint; grouping these into a struct would just
// move the argument list elsewhere without simplifying the call site.
#[allow(clippy::too_many_arguments)]
pub async fn run_graph(
    graph: Arc<Mutex<TaskGraph>>,
    worktree_manager: WorktreeManager,
    config: Config,
    backend: Arc<dyn AgentBackend>,
    control: RunControl,
    audit_registry: Arc<dyn AuditRegistry>,
    run_slug: String,
    run_uid: String,
    plan_slug: String,
) -> Result<RunReport, String> {
    // Open the run-scoped tracing span so every event emitted while driving this
    // graph carries the `run_uid` key. The `makina` binary's per-run file layer
    // reads this field to route events to `.makina/runs/{run_uid}/logs/run.log`
    // (task log-subscriber-file); under any other subscriber it is just an extra
    // field. We `.instrument()` the whole async body (rather than holding an
    // `.entered()` guard) so the future stays `Send` across `.await` points —
    // `EnteredSpan` is `!Send` and this future is `tokio::spawn`ed.
    use tracing::Instrument as _;
    let run_span = tracing::info_span!("run_graph", run_uid = %run_uid);
    run_graph_inner(
        graph,
        worktree_manager,
        config,
        backend,
        control,
        audit_registry,
        run_slug,
        run_uid,
        plan_slug,
    )
    .instrument(run_span)
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_graph_inner(
    graph: Arc<Mutex<TaskGraph>>,
    worktree_manager: WorktreeManager,
    config: Config,
    backend: Arc<dyn AgentBackend>,
    control: RunControl,
    audit_registry: Arc<dyn AuditRegistry>,
    run_slug: String,
    run_uid: String,
    plan_slug: String,
) -> Result<RunReport, String> {
    // Announce the run is now executing.
    control.emit(api::Event::RunStatusChanged {
        run: control.run,
        status: api::RunStatus::Running,
    });

    // Build the actor tree: fault-tolerance root + domain hub, then wire the
    // per-task-spawn deps into the hub (the same shape as the test harness).
    let root = RootSupervisor::start();
    let supervisor_ref = RootSupervisor::spawn_child::<Supervisor>(
        &root,
        SupervisorArgs {
            worktree_manager: worktree_manager.clone(),
            config: config.clone(),
        },
        RestartConfig::default(),
    )
    .await;
    supervisor_ref
        .ask(SetSpokes {
            root: root.clone(),
            supervisor: supervisor_ref.clone(),
            backend: Arc::clone(&backend),
        })
        .send()
        .await
        .map_err(|e| format!("failed to wire supervisor spokes: {e}"))?;

    // Build the driver context directly (we drive the `scheduler` ourselves so
    // we keep ownership of the shared graph for the final status derivation).
    let squash_merger = SquashMerger::new(
        worktree_manager.repo_root.clone(),
        worktree_manager.base_branch.clone(),
    );
    let ctx = DriverContext {
        graph: Arc::clone(&graph),
        merge_lock: Arc::new(Mutex::new(())),
        worktree_manager,
        gate_runner: GateRunner::new(),
        squash_merger,
        config: config.clone(),
        root: root.clone(),
        supervisor: supervisor_ref.clone(),
        backend,
        control: control.clone(),
        audit_registry,
        run_slug,
        run_uid,
        plan_slug,
    };

    let result = scheduler(ctx, config.concurrency).await;

    // Tear down the actor tree (kills the hub + any lingering supervised spokes).
    root.kill();

    // Derive + emit the aggregate terminal status — but NOT when cancelled: a
    // cancelled run's status is owned by the caller (Cancel sets it explicitly),
    // and we must not overwrite it with a misleading Completed/Failed.
    if !control.cancel.is_cancelled() {
        let status = {
            let g = graph.lock().await;
            aggregate_run_status(&g)
        };
        control.emit(api::Event::RunStatusChanged {
            run: control.run,
            status,
        });
    }

    result
}

/// Derive the aggregate [`api::RunStatus`] from the final task states.
///
/// `Completed` iff every task is `Done`; otherwise `Failed` if any task is
/// `Failed`; otherwise `Running` (defensive — a finished scheduler normally
/// leaves only terminal tasks, but a paused/cancelled run may have non-terminal
/// ones, which the caller's explicit status covers).
fn aggregate_run_status(graph: &TaskGraph) -> api::RunStatus {
    let all_done = graph.tasks.iter().all(|t| t.state == TaskState::Done);
    if all_done {
        return api::RunStatus::Completed;
    }
    if graph.tasks.iter().any(|t| t.state == TaskState::Failed) {
        return api::RunStatus::Failed;
    }
    api::RunStatus::Running
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
/// # Wall-clock cap (task 25)
///
/// Each driver future is wrapped in
/// `tokio::time::timeout(Duration::from_secs(config.caps.wall_clock_secs), …)`,
/// so the cap bounds the **whole** per-task lifecycle (worktree create → develop
/// → gate → review → merge → teardown).  If the deadline elapses, the timeout
/// **cancels** the driver future: its [`DriverGuard`] drops, tearing down the
/// worktree + per-task spokes (no leak), and its owned semaphore permit is
/// released.  The scheduler — which is the only place that holds graph access at
/// that point — then applies `WallClockCapReached` to that task under the graph
/// lock, moving it to terminal `Failed`, records the outcome, and continues.
/// (`caps.wall_clock_secs >= 1` is guaranteed by `Config::validate`.)
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
/// # Run control: pause + cancel (task 31)
///
/// The scheduler honours [`DriverContext::control`]:
///
/// - **Pause** (`control.pause == true`): the **fill** phase launches NO new
///   drivers while paused; in-flight drivers keep running and are drained
///   normally.  (Resume = clear the flag and run again — `CoreApi` does this by
///   re-issuing `StartRun`, which spawns a fresh `run_graph`.)  The ask path's
///   silent control never sets this, so that path is unchanged.
/// - **Cancel** (`control.cancel` cancelled): the fill phase stops launching and
///   the in-flight `JoinSet` is `abort_all()`ed.  Each aborted driver drops its
///   permit *and* its [`DriverGuard`] (which still tears down the worktree/spokes
///   — no leak).  The scheduler then drains the aborted joins and returns.  The
///   drain loop also `select!`s on cancellation so a cancel mid-wait is prompt.
async fn scheduler(ctx: DriverContext, concurrency: usize) -> Result<RunReport, String> {
    let semaphore = Arc::new(Semaphore::new(concurrency));
    // Each driver future yields either its terminal `Result<TaskState, String>`
    // OR `None` if the per-task wall-clock timeout elapsed (the inner driver was
    // cancelled — its DriverGuard already cleaned up).  The scheduler turns a
    // timeout into a `WallClockCapReached` → `Failed` transition (task 25).
    let mut join_set: JoinSet<(TaskId, Option<Result<TaskState, String>>)> = JoinSet::new();

    // Per-task wall-clock deadline (task 25).  Validated `>= 1` by Config.
    let wall_clock = Duration::from_secs(ctx.config.caps.wall_clock_secs);

    // IDs currently dispatched to a driver (defensive against double-dispatch;
    // the FSM advance already removes a task from the ready scan).
    let mut in_flight: std::collections::HashSet<TaskId> = std::collections::HashSet::new();

    let mut outcomes: Vec<(TaskId, TaskState)> = Vec::new();
    // `(task_id, reason)` for every task that reaches a `Failed` terminal.  A
    // completed-but-failed run is NOT a hard error (sched-continue-on-failure):
    // the run keeps going, but the report records which tasks failed and why so
    // `RunStatus::Failed` can be reported without halting (sched-run-status-failed).
    let mut failed_tasks: Vec<(TaskId, String)> = Vec::new();
    // A genuine RUN-level fatal error (a driver-future panic; or, defensively, a
    // graph-advance failure).  A per-TASK failure is NOT fatal
    // (sched-continue-on-failure): it is recorded as a `Failed` outcome and the
    // run keeps going.
    let mut fatal_error: Option<String> = None;
    // We stop *launching* new work (but keep draining in-flight) only on a cancel
    // or a genuine driver panic — a task-level failure no longer flips this.
    let mut stop_launching = false;

    // ── Seed persist: write the initial graph snapshot so the file exists from
    // t=0 (best-effort; the lock is released before the write — no await while
    // holding the graph mutex).
    //
    // The graph is guaranteed non-None here: the `take()` guard in the
    // `RunReadyTasks` handler ensures `ctx.graph` is populated before the
    // scheduler is entered.  This call creates `.tasks/{slug}.json` at run
    // start, so the file is present even if every task is skipped or fails
    // immediately.
    ctx.persist().await;

    loop {
        // ── Cancellation: stop launching + abort everything in flight ──────────
        //
        // Checked at the top of each scheduler iteration.  On cancel we stop the
        // fill phase and abort the JoinSet; each aborted driver's DriverGuard
        // still runs (worktree + spokes torn down — no leak).  We then fall
        // through to the drain phase to reap the aborted joins.
        if ctx.control.cancel.is_cancelled() && !stop_launching {
            stop_launching = true;
            join_set.abort_all();
        }

        // ── Fill: launch ready tasks until the cap is hit or none remain ───────
        //
        // Paused runs (task 31) launch NO new drivers — in-flight ones still
        // drain below.  `stop_launching` covers cancel + a driver panic (NOT a
        // task-level failure, which is non-fatal — sched-continue-on-failure).
        let paused = ctx.control.pause.load(Ordering::SeqCst);
        if !stop_launching && !paused {
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
                    let mut picked = next_ready_task_id(&graph, &in_flight);
                    if let Some(ref id) = picked {
                        // Advance New→Ready if needed so the next scan won't
                        // re-pick this task (single dispatch).  Errors here are
                        // impossible for a freshly-picked New/Ready task, but we
                        // surface them defensively.
                        if let Err(e) = advance_to_ready(&mut graph, id) {
                            // Record the (task-level) error but DO NOT stop
                            // launching independent ready tasks
                            // (sched-continue-on-failure): a single task that
                            // could not be advanced must not halt the whole run.
                            // Drop this task from this fill step (do not launch an
                            // un-advanced task) by clearing `picked`; the outer
                            // scheduler loop keeps draining + filling.
                            fatal_error.get_or_insert(e);
                            picked = None;
                        }
                    }
                    picked
                }; // graph guard dropped here, BEFORE we spawn / await anything.

                match next {
                    Some(id) if !stop_launching => {
                        // Emit the New→Ready transition the scheduler just applied
                        // (outside the graph lock — the task is now `Ready`).
                        ctx.emit_task_state(&id, TaskState::Ready);
                        // Persist the New→Ready advance (best-effort; lock already
                        // released above).
                        ctx.persist().await;
                        in_flight.insert(id.clone());
                        let driver_ctx = ctx.clone();
                        let driver_id = id.clone();
                        // Tag this driver's whole future with a `task` span carrying
                        // `task_slug`; the per-task log routing layer (`RunFileLayer`)
                        // keys on that field to fan this task's records out to its
                        // `{task_slug}.log`.  The span is created HERE — in
                        // `scheduler`, where the enclosing `run_graph` span (which
                        // carries `run_uid`) is the current span — so the new `task`
                        // span is parented to it and the routing layer can resolve
                        // BOTH `run_uid` (from the parent) and `task_slug` by walking
                        // the scope.  (Evaluating the macro inside the spawned future
                        // would lose that parent: the `run_graph` span is not current
                        // on the `JoinSet` worker thread.)
                        let task_span = tracing::info_span!("task", task_slug = %driver_id.0);
                        // The permit is MOVED into the future; it drops (releasing
                        // the slot) when the driver completes — on every path,
                        // INCLUDING a wall-clock timeout (the whole future, permit
                        // included, is dropped when `timeout` elapses).
                        join_set.spawn(async move {
                            // `.instrument()` (not `.in_scope()`) because
                            // `task_driver` `.await`s, so the span must persist
                            // across await points.
                            use tracing::Instrument as _;
                            let _permit = permit; // released on completion/cancel/panic.
                            // Bound the WHOLE per-task lifecycle by the wall-clock
                            // cap.  On elapse, `task_driver` is cancelled mid-await:
                            // its DriverGuard drops → worktree + spokes torn down.
                            // `None` signals "timed out" to the scheduler.
                            match tokio::time::timeout(
                                wall_clock,
                                task_driver(&driver_ctx, &driver_id).instrument(task_span),
                            )
                            .await
                            {
                                Ok(result) => (driver_id, Some(result)),
                                Err(_elapsed) => (driver_id, None),
                            }
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

        // Await the next completed driver — but also wake promptly on a cancel so
        // an in-flight wait does not block the abort.  When cancel fires mid-wait
        // we loop back to the top, which aborts the JoinSet, then drains the
        // (now-aborted) joins via this same match on the next iteration.
        let joined = tokio::select! {
            biased;
            _ = ctx.control.cancel.cancelled(), if !ctx.control.cancel.is_cancelled() => {
                // Re-evaluate at the top of the loop (aborts the JoinSet).
                continue;
            }
            j = join_set.join_next() => j,
        };

        match joined {
            Some(Ok((id, Some(Ok(state))))) => {
                in_flight.remove(&id);
                ctx.emit_task_state(&id, state);
                outcomes.push((id.clone(), state));
                // A cap-driven terminal `Failed` surfaces here (gate/review caps
                // return `Ok(Failed)` from the driver).  Like the hard-error and
                // wall-clock arms, transitively `Skipped` its dependents so they do
                // not dangle non-terminal (a non-`Done` dep never unlocks them).
                if state == TaskState::Failed {
                    // Derive WHICH cap fired from the task's iteration counts under
                    // the lock so the report records *why* it failed
                    // (sched-run-status-failed): a task that went through review
                    // (`review_iterations > 0`) surfaced its terminal `Failed` via
                    // the reviewer cap; otherwise the gate-iteration cap fired.
                    let (skipped, reason) = {
                        let mut graph = ctx.graph.lock().await;
                        let reviewed = review_iterations_locked(&graph, &id).unwrap_or(0) > 0;
                        let reason = if reviewed {
                            "review-cap-reached".to_string()
                        } else {
                            "gate-cap-reached".to_string()
                        };
                        let skipped = mark_dependents_skipped(&mut graph, &id);
                        (skipped, reason)
                    };
                    failed_tasks.push((id.clone(), reason));
                    if !skipped.is_empty() {
                        ctx.persist().await;
                        for skipped_id in skipped {
                            ctx.emit_task_state(&skipped_id, TaskState::Skipped);
                            outcomes.push((skipped_id, TaskState::Skipped));
                        }
                    }
                }
                // A Done task may have unlocked dependents → loop to fill again.
            }
            Some(Ok((id, Some(Err(e))))) => {
                // Hard error in a driver: the driver already moved its task to a
                // terminal state (Failed) and tore down its resources where
                // possible.  Read + emit that terminal state so the TUI reflects
                // it (the driver only emits the NON-terminal transitions; the
                // scheduler owns the single terminal emission on every arm).
                in_flight.remove(&id);
                let (terminal, skipped) = {
                    let mut graph = ctx.graph.lock().await;
                    let terminal = task_state_locked(&graph, &id).unwrap_or(TaskState::Failed);
                    // Transitively `Skipped` the failed task's dependents under the
                    // held lock (no .await), then drop the guard before emitting.
                    let skipped = mark_dependents_skipped(&mut graph, &id);
                    (terminal, skipped)
                };
                ctx.emit_task_state(&id, terminal);
                // Persist the dependents' Skipped transitions (best-effort; lock
                // already released above).
                ctx.persist().await;
                // Record the failed task's terminal outcome.  A task-level hard
                // error no longer feeds `fatal_error` nor sets `stop_launching`
                // (sched-continue-on-failure): the task is already `Failed` and
                // its dependents `Skipped`, so the scheduler keeps launching the
                // remaining independent ready tasks and the run completes (a
                // completed-but-failed run, not a hard run-level error).  The
                // driver error `e` is no longer fatal, but it IS recorded as the
                // failure reason on `failed_tasks` (sched-run-status-failed) — only
                // a genuine driver *panic* (the join-error arm) remains fatal.
                if terminal == TaskState::Failed {
                    failed_tasks.push((id.clone(), e));
                }
                outcomes.push((id, terminal));
                for skipped_id in skipped {
                    ctx.emit_task_state(&skipped_id, TaskState::Skipped);
                    outcomes.push((skipped_id, TaskState::Skipped));
                }
            }
            Some(Ok((id, None))) => {
                // ── Wall-clock cap reached (task 25) ────────────────────────────
                //
                // The driver future was cancelled by the per-task timeout; its
                // DriverGuard already tore down the worktree + spokes and its
                // permit was released.  The scheduler holds graph access here, so
                // it applies `WallClockCapReached` → `Failed` under the lock and
                // records the outcome.  A timeout fails ONLY this task; it does
                // NOT stop the scheduler launching independent ready tasks (a
                // timed-out task is not `Done`, so its dependents never unlock).
                in_flight.remove(&id);
                let (final_state, skipped) = {
                    let mut graph = ctx.graph.lock().await;
                    let final_state =
                        match apply_event_locked(&mut graph, &id, TaskEvent::WallClockCapReached) {
                            Ok(()) => {
                                mark_finished_locked(&mut graph, &id);
                                TaskState::Failed
                            }
                            Err(_) => {
                                // The task already reached a terminal state in the
                                // instant before the timeout fired (a benign race):
                                // record its actual terminal state instead.
                                task_state_locked(&graph, &id).unwrap_or(TaskState::Failed)
                            }
                        };
                    // Transitively `Skipped` the failed task's dependents under the
                    // held lock (no .await), then drop the guard before emitting.
                    let skipped = mark_dependents_skipped(&mut graph, &id);
                    (final_state, skipped)
                }; // guard dropped before emit.
                // Persist WallClockCapReached → Failed + dependents' Skipped
                // (best-effort).
                ctx.persist().await;
                ctx.emit_task_state(&id, final_state);
                // Cap failures carry no driver reason string, so synthesize the
                // literal for this arm (sched-run-status-failed).  Guard on
                // `Failed` so a benign race that landed the task on another
                // terminal does not record a spurious wall-clock reason.
                if final_state == TaskState::Failed {
                    failed_tasks.push((id.clone(), "wall-clock-cap-reached".to_string()));
                }
                outcomes.push((id, final_state));
                for skipped_id in skipped {
                    ctx.emit_task_state(&skipped_id, TaskState::Skipped);
                    outcomes.push((skipped_id, TaskState::Skipped));
                }
            }
            Some(Err(join_err)) => {
                // The driver task ended abnormally.  Two cases:
                //  - **Cancelled (task 31)**: we `abort_all()`ed it; this is the
                //    EXPECTED outcome of a cancel, NOT a failure.  Its DriverGuard
                //    ran on the abort unwind (worktree/spokes torn down — no leak).
                //    We simply drop it (no fatal error, no outcome recorded).
                //  - **Panicked**: a genuine bug.  Record a fatal error and stop
                //    launching; remaining drivers still drain.
                if join_err.is_cancelled() {
                    // Expected during cancellation; nothing to record.
                } else {
                    fatal_error.get_or_insert(format!("task driver panicked: {join_err}"));
                    stop_launching = true;
                }
            }
            None => break, // JoinSet drained.
        }
    }

    match fatal_error {
        Some(e) => Err(e),
        None => Ok(RunReport {
            outcomes,
            failed_tasks,
        }),
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
    /// Plan slug this task belongs to, used to plan-scope the worktree
    /// directory + branch during safety-net teardown.
    plan_slug: String,
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
            let plan_slug = self.plan_slug.clone();
            // `tokio::spawn` requires being inside a runtime; the driver always
            // runs inside one (JoinSet task).  Best-effort: remove() is
            // idempotent and treats "not found" as success.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = mgr.remove(&plan_slug, &id).await;
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
/// graph live for the future TUI without serializing the drivers.  Any new
/// graph-mutation block must be followed by `ctx.persist().await` AFTER the lock
/// guard is dropped, so on-disk state stays in sync with in-memory state.
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
        plan_slug: ctx.plan_slug.clone(),
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
    // Persist New→Ready (if the task was New; no-op cost otherwise).
    ctx.persist().await;

    // ── Step 2: create the worktree, then Ready → InProgress (Dispatched) ──────
    //
    // A worktree-create failure happens while the task is still `Ready` (before
    // the Developer is ever dispatched).  `Ready --HardError--> Failed` (task 25)
    // moves it to a terminal state cleanly rather than leaving it stuck `Ready`.
    let worktree = match ctx
        .worktree_manager
        .create(&ctx.plan_slug, &task_id.0)
        .await
    {
        Ok(wt) => wt,
        Err(e) => {
            {
                let mut graph = ctx.graph.lock().await;
                apply_event_locked(&mut graph, task_id, TaskEvent::HardError)?;
                mark_finished_locked(&mut graph, task_id);
            }
            // Persist Ready→Failed (best-effort; lock released above).
            ctx.persist().await;
            // No worktree was created, so there is nothing to remove; mark the
            // guard so it does not attempt a redundant best-effort teardown.
            guard.worktree_removed = true;
            return Err(format!("worktree create failed for {task_id}: {e}"));
        }
    };

    // ── Register the worktree context with the audit registry (task supervisor-audit-writer) ──
    //
    // The transport emits `AuditEntry` records with placeholder `run_id` /
    // `task_id` and the real `working_dir`.  The `JsonlAuditSink` (if wired)
    // uses this registration to enrich and route those entries.  The registry
    // is `NoopAuditRegistry` on the ask path, so this is a no-op there.
    ctx.audit_registry.register(
        worktree.path.clone(),
        ctx.run_uid.clone(),
        ctx.control.run.to_string(),
        ctx.run_slug.clone(),
        task_id.0.clone(),
    );

    {
        let mut graph = ctx.graph.lock().await;
        apply_event_locked(&mut graph, task_id, TaskEvent::Dispatched)?;
        mark_started_locked(&mut graph, task_id);
    } // guard dropped before emit.
    // Persist Ready→InProgress + started_at stamp (best-effort; lock released
    // above).
    ctx.persist().await;
    // Ready → InProgress (an intermediate transition; the scheduler owns the
    // terminal-state emission, the driver owns the intermediate ones — task 31).
    ctx.emit_task_state(task_id, TaskState::InProgress);
    // Additive tracing emission (log-tracing-transition-events): a per-task
    // subscriber captures the state transition.  Does NOT change EventSink
    // behavior — runs alongside `emit_task_state`.
    tracing::info!(
        task = %task_id.0,
        from = ?TaskState::Ready,
        to = ?TaskState::InProgress,
        "task state transition"
    );

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
                // Additive tracing emission (log-tracing-transition-events): the
                // terminal Failed transition (InProgress → Failed via the gate
                // cap).  The scheduler owns the terminal `emit_task_state`; this
                // is the per-task subscriber's record of the transition.
                tracing::info!(
                    task = %task_id.0,
                    from = ?TaskState::InProgress,
                    to = ?TaskState::Failed,
                    "task state transition"
                );
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
                run: ctx.control.run,
                sink: Arc::clone(&ctx.control.sink),
            })
            .send()
            .await;

        // On reviewer ask/parse failure we are in `InReview`.  Task 25 made
        // `InReview --HardError--> Failed` legal, so we drive the task to a
        // terminal `Failed` (instead of leaving it stuck `InReview`), tear down
        // the worktree, and propagate the error.
        let verdict = match review_result {
            Ok(v) => v,
            Err(e) => {
                {
                    let mut graph = ctx.graph.lock().await;
                    apply_event_locked(&mut graph, task_id, TaskEvent::HardError)?;
                    mark_finished_locked(&mut graph, task_id);
                }
                // Persist InReview→Failed (best-effort; lock released above).
                ctx.persist().await;
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
                let branch = format!("task/{}--{task_id}", ctx.plan_slug);
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
                        // Hard (non-conflict) merge failure; develop already
                        // best-effort-restored by the merger.  We are in
                        // `InReview`; task 25 made `InReview --HardError--> Failed`
                        // legal, so drive the task terminal (HardError is reserved
                        // for hard failures; ReviewCapReached stays the reviewer
                        // cap).  Then clean up + propagate.
                        {
                            let mut graph = ctx.graph.lock().await;
                            apply_event_locked(&mut graph, task_id, TaskEvent::HardError)?;
                            mark_finished_locked(&mut graph, task_id);
                        }
                        // Persist InReview→Failed (best-effort; lock released above).
                        ctx.persist().await;
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
                        // Persist InReview→Done + finished_at stamp (best-effort;
                        // lock released above).
                        ctx.persist().await;
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
                        // hard invariant).  The architecture's agent-driven
                        // reconciliation is a documented seam (see task 23 notes);
                        // the MVP drives the task to a SAFE terminal Failed via the
                        // dedicated `MergeConflict` event (InReview → Failed).
                        // This distinguishes it from reviewer-cap exhaustion
                        // (still ReviewCapReached) and hard merge errors (HardError).
                        let _ = details; // surfaced to the seam; logged by a later task.
                        {
                            let mut graph = ctx.graph.lock().await;
                            apply_event_locked(&mut graph, task_id, TaskEvent::MergeConflict)?;
                            mark_finished_locked(&mut graph, task_id);
                        }
                        // Persist InReview→Failed (MergeConflict; best-effort; lock
                        // released above).
                        ctx.persist().await;
                        remove_worktree(ctx, task_id).await;
                        guard.worktree_removed = true;
                        // Additive tracing emission (log-tracing-transition-events):
                        // terminal Failed transition (InReview → Failed via a
                        // merge conflict).
                        tracing::info!(
                            task = %task_id.0,
                            from = ?TaskState::InReview,
                            to = ?TaskState::Failed,
                            "task state transition"
                        );
                        terminal_state = TaskState::Failed;
                        break;
                    }
                }
            }
            ReviewVerdict::Reject { feedback: fb } => {
                // ── Reject: enforce the REVIEWER cap (task 25) ──────────────────
                //
                // The decision is made BEFORE the FSM transition, while the task
                // is still `InReview`.  We count the CURRENT (already-applied)
                // rejections plus this one: if this rejection *reaches* the cap
                // (`review_iterations + 1 >= caps.reviewer_iterations`) we do NOT
                // loop back to develop — we emit `ReviewCapReached`
                // (InReview → Failed) and terminate the task.  This uses
                // `ReviewCapReached` from its intended state (`InReview`).
                //
                // Each driver counts its OWN task's `review_iterations` under the
                // graph lock, so the per-task cap is correct under concurrency.
                let prior_iterations = {
                    let graph = ctx.graph.lock().await;
                    review_iterations_locked(&graph, task_id)?
                };

                if prior_iterations + 1 >= ctx.config.caps.reviewer_iterations {
                    // This rejection reaches the cap → fail the task.  We count
                    // this final rejection in `review_iterations` first (so the
                    // recorded count equals the cap), then transition InReview →
                    // Failed via ReviewCapReached.
                    let (gate_iters, review_iters) = {
                        let mut graph = ctx.graph.lock().await;
                        increment_review_iterations_locked(&mut graph, task_id);
                        apply_event_locked(&mut graph, task_id, TaskEvent::ReviewCapReached)?;
                        mark_finished_locked(&mut graph, task_id);
                        (
                            gate_iterations_locked(&graph, task_id)?,
                            review_iterations_locked(&graph, task_id)?,
                        )
                    }; // guard dropped before emit.
                    // Persist InReview→Failed (ReviewCapReached; best-effort; lock
                    // released above).
                    ctx.persist().await;
                    // Emit the final iteration count (the terminal Failed state is
                    // emitted by the scheduler when this driver returns Ok(Failed)).
                    ctx.emit_task_iterations(task_id, gate_iters, review_iters);
                    remove_worktree(ctx, task_id).await;
                    guard.worktree_removed = true;
                    // Additive tracing emission (log-tracing-transition-events):
                    // terminal Failed transition (InReview → Failed via the
                    // reviewer cap).
                    tracing::info!(
                        task = %task_id.0,
                        from = ?TaskState::InReview,
                        to = ?TaskState::Failed,
                        "task state transition"
                    );
                    terminal_state = TaskState::Failed;
                    break;
                }

                // ── Below the cap: InReview → InProgress (ReviewerRejected) ─────
                // Count the rejection and loop back for a re-work attempt.
                let (gate_iters, review_iters) = {
                    let mut graph = ctx.graph.lock().await;
                    apply_event_locked(&mut graph, task_id, TaskEvent::ReviewerRejected)?;
                    increment_review_iterations_locked(&mut graph, task_id);
                    (
                        gate_iterations_locked(&graph, task_id)?,
                        review_iterations_locked(&graph, task_id)?,
                    )
                }; // guard dropped before emit.
                // Persist InReview→InProgress + bumped review count (best-effort;
                // lock released above).
                ctx.persist().await;
                // InReview → InProgress (intermediate) + the bumped review count.
                ctx.emit_task_state(task_id, TaskState::InProgress);
                // Additive tracing emission (log-tracing-transition-events).
                tracing::info!(
                    task = %task_id.0,
                    from = ?TaskState::InReview,
                    to = ?TaskState::InProgress,
                    "task state transition"
                );
                ctx.emit_task_iterations(task_id, gate_iters, review_iters);

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
                run: ctx.control.run,
                sink: Arc::clone(&ctx.control.sink),
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
            // Persist InProgress→Failed (best-effort; lock released above).
            ctx.persist().await;
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
                // Additive tracing emission (log-tracing-transition-events): the
                // gate-output record for the passing round (counterpart to the
                // `gate failed` event on the Failed arm).
                tracing::info!(
                    task = %task_id.0,
                    "gates passed"
                );
                // All gates passed → advance to review.
                {
                    let mut graph = ctx.graph.lock().await;
                    apply_event_locked(&mut graph, task_id, TaskEvent::GatesPassed)?;
                } // guard dropped before emit.
                // Persist InProgress→InReview (best-effort; lock released above).
                ctx.persist().await;
                // InProgress → InReview (intermediate transition — task 31).
                ctx.emit_task_state(task_id, TaskState::InReview);
                // Additive tracing emission (log-tracing-transition-events).
                tracing::info!(
                    task = %task_id.0,
                    from = ?TaskState::InProgress,
                    to = ?TaskState::InReview,
                    "task state transition"
                );
                return Ok(DevelopGateOutcome::ReadyForReview);
            }
            Ok(GateOutcome::Failed {
                gate,
                output,
                exit_code,
            }) => {
                // Additive tracing emission (log-tracing-transition-events): the
                // gate-output record for the failing gate.  Does NOT change
                // EventSink behavior — runs alongside the existing emissions.
                tracing::info!(
                    task = %task_id.0,
                    gate = %gate,
                    exit_code,
                    "gate failed"
                );
                // A gate failed → self-loop and count the iteration; enforce the
                // per-task GATE cap.
                let (iterations, review_iters) = {
                    let mut graph = ctx.graph.lock().await;
                    apply_event_locked(&mut graph, task_id, TaskEvent::GateFailed)?;
                    increment_gate_iterations_locked(&mut graph, task_id);
                    (
                        gate_iterations_locked(&graph, task_id)?,
                        review_iterations_locked(&graph, task_id)?,
                    )
                }; // guard dropped before emit.
                // Persist InProgress self-loop + bumped gate count (best-effort;
                // lock released above).
                ctx.persist().await;
                // InProgress --GateFailed--> InProgress (self-loop) + the bumped
                // gate count.  The TUI re-affirms InProgress and updates the
                // counter (task 31).
                ctx.emit_task_state(task_id, TaskState::InProgress);
                // Additive tracing emission (log-tracing-transition-events): the
                // InProgress self-loop transition.
                tracing::info!(
                    task = %task_id.0,
                    from = ?TaskState::InProgress,
                    to = ?TaskState::InProgress,
                    "task state transition"
                );
                ctx.emit_task_iterations(task_id, iterations, review_iters);

                if iterations >= ctx.config.caps.gate_iterations {
                    // GateCapReached: InProgress → Failed (terminal).  The terminal
                    // Failed state is emitted by the scheduler when this driver
                    // returns Ok(GateCapReached) → Ok(Failed).
                    {
                        let mut graph = ctx.graph.lock().await;
                        apply_event_locked(&mut graph, task_id, TaskEvent::GateCapReached)?;
                        mark_finished_locked(&mut graph, task_id);
                    }
                    // Persist InProgress→Failed (GateCapReached; best-effort; lock
                    // released above).
                    ctx.persist().await;
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
                // Persist InProgress→Failed (gate launch HardError; best-effort;
                // lock released above).
                ctx.persist().await;
                remove_worktree(ctx, task_id).await;
                return Err(format!("gate launch failed for {task_id}: {e}"));
            }
        }
    }
}

/// Best-effort worktree teardown (idempotent; ignores "already gone").
async fn remove_worktree(ctx: &DriverContext, task_id: &TaskId) {
    // `remove` is idempotent and treats "not found" as success.
    let _ = ctx
        .worktree_manager
        .remove(&ctx.plan_slug, &task_id.0)
        .await;
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

/// Move the transitive dependents of a just-`Failed` task to [`TaskState::Skipped`].
///
/// `depends_on` lists each task's *prerequisites*, so a failed task's dependents
/// are the tasks whose `depends_on` transitively contains `failed_task_id`.  There
/// is no reverse-adjacency helper, so we build the dependent set inline: starting
/// from `failed_task_id`, repeatedly scan `graph.tasks` for any task whose
/// `depends_on` contains an already-collected id (a reverse-edge BFS).
///
/// For each newly found dependent that is **not already terminal** we apply
/// [`TaskEvent::DependencyFailed`] (active → `Skipped`) and stamp `finished_at`,
/// then collect its id.  The `is_terminal` guard keeps the FSM clean —
/// `apply_event_locked` already rejects the event from `Done/Failed/Skipped`.
///
/// Called under the held graph guard (no `.await`).  Returns the ids that were
/// freshly moved to `Skipped`, so the caller can emit + record them after the
/// guard is dropped.  This is required because `next_ready_task_id` needs every
/// dep `== Done`, so a failed task's dependents would otherwise dangle
/// non-terminal forever.
fn mark_dependents_skipped(graph: &mut TaskGraph, failed_task_id: &TaskId) -> Vec<TaskId> {
    // `collected` seeds the reverse-edge frontier with the failed id; `skipped`
    // accumulates only the ids we actually moved to `Skipped` (excludes the
    // failed root, which is already terminal).
    let mut collected: std::collections::HashSet<TaskId> = std::collections::HashSet::new();
    collected.insert(failed_task_id.clone());
    let mut skipped: Vec<TaskId> = Vec::new();

    // Fixed-point scan: keep sweeping the whole graph until a full pass adds no
    // new dependent (handles transitive chains regardless of authored order).
    loop {
        let mut found_new = false;
        let candidates: Vec<TaskId> = graph
            .tasks
            .iter()
            .filter(|t| !collected.contains(&t.id))
            .filter(|t| t.depends_on.iter().any(|dep| collected.contains(dep)))
            .map(|t| t.id.clone())
            .collect();

        for id in candidates {
            collected.insert(id.clone());
            found_new = true;
            // Skip tasks that already reached a terminal state — the FSM (and
            // `apply_event_locked`) would reject `DependencyFailed` for them.
            let state = match task_state_locked(graph, &id) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if crate::state_machine::is_terminal(state) {
                continue;
            }
            if apply_event_locked(graph, &id, TaskEvent::DependencyFailed).is_ok() {
                mark_finished_locked(graph, &id);
                skipped.push(id);
            }
        }

        if !found_new {
            break;
        }
    }

    skipped
}
