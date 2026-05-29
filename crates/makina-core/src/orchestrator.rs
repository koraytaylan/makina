//! The real, core-backed [`Api`] implementation.
//!
//! [`CoreApi`] is the orchestrator's outward-facing surface — the concrete type
//! the TUI binds to via `Arc<dyn Api>`.  It implements the full command set:
//!
//! ```text
//!   execute(OpenRun{path})
//!       │ read file (tokio::fs) → interpret → register (status = Pending)
//!       ▼ broadcast Event::RunOpened
//!   execute(StartRun{run})
//!       │ tokio::spawn(run_graph(graph, …, RunControl{ sink→broadcast, … }))
//!       ▼ status = Running; the Supervisor scheduler drives the graph in the
//!         background, emitting live TaskStateChanged / TaskIterationsUpdated /
//!         RunStatusChanged / AgentExchange events as it goes.
//!   execute(PauseRun{run})   → set pause flag (stop launching NEW tasks)
//!   execute(CancelRun{run})  → cancel token (abort + worktree teardown)
//! ```
//!
//! plus the read queries ([`Api::runs`] / [`Api::run`]) and the live
//! [`Api::subscribe`] stream.
//!
//! # Execution model (task 31: run-control)
//!
//! Execution lives behind injected dependencies so the deterministic TUI path
//! and the e2e ACP path are interchangeable: a `backend: Arc<dyn AgentBackend>`,
//! a [`WorktreeManager`] (repo root + base branch), and a [`Config`].  On
//! `StartRun`, `CoreApi` spawns a background tokio task that runs the
//! Supervisor's [`run_graph`] over the Run's **shared graph**
//! (`Arc<tokio::sync::Mutex<TaskGraph>>`), wired to an [`EventSink`] that
//! forwards every engine [`Event`] to the same broadcast `subscribe()` reads.
//! The graph is shared between the background scheduler (which mutates it) and
//! the read queries (which snapshot it), so `run()`/`runs()` reflect live state.
//!
//! ## Pause semantics (MVP)
//!
//! `PauseRun` sets a cooperative pause flag: the scheduler stops launching NEW
//! task drivers; in-flight tasks finish.  Resume = `StartRun` again, which
//! clears the flag and spawns a fresh `run_graph` that continues launching ready
//! tasks (already-`Done` tasks are skipped, their dependents unlock).  This is
//! the documented "stop launching new tasks" MVP semantics.
//!
//! ## Cancel semantics
//!
//! `CancelRun` cancels the run's [`CancellationToken`]: the scheduler stops
//! launching and `abort_all()`s in-flight drivers — each aborted driver's
//! `DriverGuard` still tears down its worktree + spokes (no leak).  The Run's
//! status is set to `Failed` (cancelled) and that status is emitted; the
//! background task's own terminal status emission is suppressed for a cancelled
//! run (see [`run_graph`]) so it cannot overwrite the cancelled status.
//!
//! # Locking discipline
//!
//! The Runs registry lives behind a `std::sync::Mutex`.  The lock is **never
//! held across an `.await`**: every handler locks, reads/mutates the registry
//! (cloning out the `Arc` graph handle / the cancel+pause handle), drops the
//! guard, and only then awaits I/O, locks the (separate) `tokio::sync::Mutex`
//! graph, or broadcasts an event.
//!
//! # Seams
//!
//! | Concern | Status here | Owning task |
//! |---------|-------------|-------------|
//! | `OpenRun` → interpret → register → broadcast | implemented | task 28 |
//! | `runs()` / `run()` / `subscribe()` | implemented | task 28 |
//! | `StartRun` / `PauseRun` / `CancelRun` driving the Supervisor | **implemented** | this task (31) |
//! | model-backed interpreter + real ACP backend | injected, not wired | e2e (task 33) |

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;

use crate::actors::{EventSink, RunControl, run_graph};
use crate::api::{
    Api, ApiError, Command, CommandOutcome, Event, EventStream, RunId, RunStatus, RunView, TaskView,
};
use crate::audit::{AuditRegistry, NoopAuditRegistry};
use crate::backend::AgentBackend;
use crate::config::Config;
use crate::interpreter::TaskListInterpreter;
use crate::task::TaskGraph;
use crate::worktree::WorktreeManager;

// ── Constants ────────────────────────────────────────────────────────────────────

/// Fallback slug used when a task-list path has no file stem (e.g. a bare `/`
/// or an OS string that cannot be decoded to UTF-8).  Both `open_run` and
/// `start_run` use this value so it is defined once here.
const SLUG_FALLBACK: &str = "task-list";

// ── Broadcast capacity ──────────────────────────────────────────────────────────

/// Capacity of the event broadcast channel.
///
/// A subscriber that lags by more than this many events drops the oldest ones;
/// the [`Api::subscribe`] stream maps such lag errors away so it stays
/// infallible (see [`CoreApi::subscribe`]).  An executing Run emits a steady
/// stream of `TaskStateChanged` / `AgentExchange` events, so this is sized
/// generously to absorb bursts (e.g. a multi-task run streaming chunks) while
/// keeping memory bounded.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

// ── Run handle (task 31) ──────────────────────────────────────────────────────

/// The control handles for a background-executing Run.
///
/// Stored in the [`RunEntry`] once `StartRun` spawns the scheduler so that
/// `PauseRun` / `CancelRun` can signal the running task.
struct RunHandle {
    /// Cancellation signal for the background scheduler.  `CancelRun` cancels it
    /// (scheduler aborts + cleans up); also cancelled if a fresh `StartRun`
    /// supersedes a still-running task (defensive).
    cancel: CancellationToken,
    /// Cooperative pause flag.  `PauseRun` sets it `true` (scheduler stops
    /// launching new tasks); a fresh `StartRun` clears it before resuming.
    pause: Arc<AtomicBool>,
}

// ── Registry entry ──────────────────────────────────────────────────────────────

/// One open Run as tracked by the orchestrator's in-memory registry.
///
/// Holds the interpreted [`TaskGraph`] behind an `Arc<tokio::sync::Mutex<…>>`
/// (so the background scheduler and the read queries share one source of truth),
/// the backing file path, the aggregate [`RunStatus`], and — once started — the
/// [`RunHandle`] used to pause/cancel the background execution.
struct RunEntry {
    /// Path to the task-list file this Run was opened from.
    task_list_path: PathBuf,
    /// The interpreted task graph, shared with the background scheduler.  Reads
    /// (`run`/`runs`) lock it briefly to snapshot; the scheduler mutates it as
    /// tasks progress.
    graph: Arc<AsyncMutex<TaskGraph>>,
    /// Aggregate status.  Starts [`RunStatus::Pending`]; driven by `StartRun`
    /// (→ `Running`), `PauseRun` (→ `Paused`), `CancelRun` (→ `Failed`), and the
    /// background task's terminal emission (→ `Completed`/`Failed`).
    status: RunStatus,
    /// The background execution's control handle.  `None` until `StartRun`.
    handle: Option<RunHandle>,
}

/// Project a snapshot graph + metadata into the view-level [`RunView`].
///
/// Pulled out as a free function because [`RunEntry`] no longer holds the graph
/// inline (it is behind an async mutex); callers snapshot the graph first, then
/// build the view from the clone — keeping the registry lock and the graph lock
/// strictly separate.
fn build_view(id: RunId, task_list_path: PathBuf, status: RunStatus, graph: &TaskGraph) -> RunView {
    let tasks = graph
        .tasks
        .iter()
        .map(|task| TaskView {
            id: (&task.id).into(),
            title: task.title.clone(),
            state: task.state.into(),
            gate_iterations: task.gate_iterations,
            review_iterations: task.review_iterations,
            depends_on: task.depends_on.iter().map(Into::into).collect(),
        })
        .collect();

    RunView {
        id,
        task_list_path,
        status,
        tasks,
    }
}

// ── Shared inner state ──────────────────────────────────────────────────────────

/// The shared, mutable orchestrator state behind an `Arc`.
///
/// Pulled out of [`CoreApi`] into an `Arc<CoreState>` so the **background
/// execution task** (`StartRun`'s spawned scheduler) can hold a clone and update
/// the registry's run status when the run reaches a terminal state — `CoreApi`'s
/// `&self` methods cannot be borrowed by a `'static` spawned future, but a
/// cloned `Arc<CoreState>` can.
struct CoreState {
    /// The interpreter used to turn task-list source text into a [`TaskGraph`].
    interpreter: Arc<dyn TaskListInterpreter>,

    /// The agent backend cloned into each background scheduler (and from there
    /// into every per-task Developer/Reviewer).  Injected so the deterministic
    /// `NoopBackend` (tests/TUI) and the ACP backend (e2e) are interchangeable.
    backend: Arc<dyn AgentBackend>,

    /// Worktree/branch lifecycle manager (repo root + base branch) handed to the
    /// background scheduler.
    worktree_manager: WorktreeManager,

    /// Resolved runtime config (gates, caps, concurrency, base branch).
    config: Config,

    /// The Runs registry: `RunId` → [`RunEntry`].  A `BTreeMap` keeps iteration
    /// order stable (ascending `RunId`, i.e. insertion order) for [`Api::runs`].
    runs: Mutex<BTreeMap<u64, RunEntry>>,

    /// Monotonic allocator for fresh [`RunId`]s.  First id is `1`.
    next_id: AtomicU64,

    /// Broadcast sender for the live event stream.  Each [`Api::subscribe`] call
    /// derives an independent receiver; the background scheduler's [`EventSink`]
    /// forwards engine events into this same sender.
    event_tx: broadcast::Sender<Event>,

    /// Audit registry: the Supervisor calls this to associate each task's
    /// `working_dir` with its run/slug/task context before dispatching a driver.
    /// The registry is backed by [`crate::audit::JsonlAuditSink`] in production
    /// and [`crate::audit::NoopAuditRegistry`] in tests.
    audit_registry: Arc<dyn AuditRegistry>,
}

impl CoreState {
    /// Allocate the next monotonic [`RunId`].
    fn alloc_id(&self) -> RunId {
        RunId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Build an [`EventSink`] that forwards every engine [`Event`] into the
    /// broadcast channel `subscribe()` reads.
    ///
    /// `send` only errors when there are no live receivers, which is fine — the
    /// TUI may not be subscribed yet; events are best-effort.  The events the
    /// engine produces are already tagged with the correct `RunId` (the
    /// scheduler/drivers stamp it from the `RunControl`), so this is a thin pass.
    fn make_sink(&self) -> EventSink {
        let tx = self.event_tx.clone();
        Arc::new(move |event: Event| {
            let _ = tx.send(event);
        })
    }

    /// Record the final aggregate status of a run **after** its background
    /// scheduler returns, unless the run was cancelled (Cancel owns that status).
    ///
    /// Derives `Completed` (all tasks `Done`) or `Failed` (otherwise — a failed
    /// task, or a paused run that stopped launching) from the live graph, writes
    /// it into the registry, and clears the run's handle (it is no longer
    /// executing).  Called only from the background task; `run_graph` has already
    /// broadcast the matching `RunStatusChanged` event, so this just keeps the
    /// registry's snapshot status consistent with what the TUI was told.
    async fn finalize_run_status(&self, run: RunId) {
        // Snapshot the graph handle + whether this run was cancelled, under the
        // registry lock; drop the guard before awaiting the graph lock.
        let (graph, cancelled) = {
            let runs = self.runs.lock().expect("runs registry mutex poisoned");
            match runs.get(&run.0) {
                Some(entry) => {
                    let cancelled = entry
                        .handle
                        .as_ref()
                        .map(|h| h.cancel.is_cancelled())
                        .unwrap_or(false);
                    (Arc::clone(&entry.graph), cancelled)
                }
                None => return, // run was removed; nothing to finalize.
            }
        };
        if cancelled {
            return; // Cancel set the status explicitly; do not overwrite it.
        }
        // Derive the terminal status from the live task states.
        let status = {
            let g = graph.lock().await;
            let all_done = g
                .tasks
                .iter()
                .all(|t| t.state == crate::task::TaskState::Done);
            if all_done {
                RunStatus::Completed
            } else {
                RunStatus::Failed
            }
        }; // graph guard dropped before re-taking the registry lock.

        let mut runs = self.runs.lock().expect("runs registry mutex poisoned");
        if let Some(entry) = runs.get_mut(&run.0) {
            // Only finalize if the run is still in the executing state we set on
            // start (Running).  A concurrent Pause/Cancel/Start may have moved it
            // on; respect that.
            if entry.status == RunStatus::Running {
                entry.status = status;
            }
            // The scheduler has finished; the handle is spent.
            entry.handle = None;
        }
    }
}

// ── CoreApi ─────────────────────────────────────────────────────────────────────

/// The real, core-backed orchestrator [`Api`].
///
/// Construct with [`CoreApi::new`], injecting:
/// - a [`TaskListInterpreter`] (the deterministic
///   `EdgeInferrer::new(StructuredTextInterpreter)` for the TUI; a
///   `ModelInterpreter` for the e2e),
/// - the agent `backend` (`NoopBackend` in tests; the ACP backend in the e2e),
/// - a [`WorktreeManager`] (repo root + base branch), and
/// - a resolved [`Config`] (gates + caps + concurrency).
///
/// # Concurrency
///
/// `CoreApi` is `Send + Sync` and all methods take `&self`, so it can be shared
/// across the TUI's async tasks as `Arc<dyn Api>`.  Internal mutable state lives
/// in an `Arc<CoreState>` (so background tasks can update it too) behind a
/// `std::sync::Mutex` that is never held across an `.await`.
pub struct CoreApi {
    /// Shared mutable state (also cloned into background execution tasks).
    state: Arc<CoreState>,
}

impl CoreApi {
    /// Create a new `CoreApi` with all execution dependencies injected.
    ///
    /// The TUI passes the deterministic interpreter, a `NoopBackend` (or the ACP
    /// backend), a `WorktreeManager` pointed at the repo, and a resolved
    /// `Config`.  Tests pass `NoopBackend` + a temp-repo `WorktreeManager` + a
    /// trivial `Config`.
    ///
    /// `audit_registry` is the seam the Supervisor uses to register each task's
    /// worktree context before dispatching a driver.  Pass
    /// `Arc::new(NoopAuditRegistry)` (the default, available via
    /// [`CoreApi::new`]) for tests that do not need the ledger; pass
    /// `Arc<JsonlAuditSink>` in production (where `main.rs` wires the same
    /// `Arc` into both `AcpBackend::with_audit_sink` and here).
    pub fn new(
        interpreter: Arc<dyn TaskListInterpreter>,
        backend: Arc<dyn AgentBackend>,
        worktree_manager: WorktreeManager,
        config: Config,
    ) -> Self {
        Self::with_audit_registry(
            interpreter,
            backend,
            worktree_manager,
            config,
            Arc::new(NoopAuditRegistry),
        )
    }

    /// Like [`CoreApi::new`] but with an explicit [`AuditRegistry`].
    ///
    /// Use this in production to inject the `JsonlAuditSink` so the Supervisor
    /// can register each task's worktree context and audit entries are routed to
    /// `.tasks/{slug}/audit.jsonl`.
    pub fn with_audit_registry(
        interpreter: Arc<dyn TaskListInterpreter>,
        backend: Arc<dyn AgentBackend>,
        worktree_manager: WorktreeManager,
        config: Config,
        audit_registry: Arc<dyn AuditRegistry>,
    ) -> Self {
        let (event_tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            state: Arc::new(CoreState {
                interpreter,
                backend,
                worktree_manager,
                config,
                runs: Mutex::new(BTreeMap::new()),
                next_id: AtomicU64::new(1),
                event_tx,
                audit_registry,
            }),
        }
    }

    /// Implement the `OpenRun` command: read → interpret → register → broadcast.
    ///
    /// Lock discipline: the file read and interpretation happen with **no lock
    /// held**; the registry lock is taken only for the brief insert, then
    /// dropped before the broadcast.
    async fn open_run(&self, task_list_path: PathBuf) -> Result<CommandOutcome, ApiError> {
        // 1. Read the task-list file (no lock held — this awaits).
        let text = tokio::fs::read_to_string(&task_list_path)
            .await
            .map_err(|e| ApiError::InvalidCommand {
                reason: format!(
                    "could not read task list `{}`: {e}",
                    task_list_path.display()
                ),
            })?;

        // 2. Derive the slug from the file stem (fallback to the whole name).
        let slug = task_list_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(SLUG_FALLBACK)
            .to_string();

        // 3. Interpret the source into a TaskGraph (no lock held — this awaits).
        let graph = self
            .state
            .interpreter
            .interpret(&slug, &text)
            .await
            .map_err(|e| ApiError::InvalidCommand {
                reason: format!("could not interpret task list `{slug}`: {e}"),
            })?;

        // 3a. Seed-persist the freshly-interpreted graph so the artifact exists
        //     immediately (before StartRun).  Best-effort: a failure only warns;
        //     opening a run must not break because the disk is unwritable.
        let repo_root = self.state.worktree_manager.repo_root.clone();
        if let Err(e) = crate::persist::persist_graph(&graph, &repo_root).await {
            tracing::warn!(
                slug = %slug,
                error = %e,
                "seed-persist failed for freshly-opened run; continuing without artifact",
            );
        }

        // 4. Allocate an id and register the Run.  Lock → insert → DROP guard
        //    before any further await/broadcast.
        let id = self.state.alloc_id();
        {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            runs.insert(
                id.0,
                RunEntry {
                    task_list_path: task_list_path.clone(),
                    graph: Arc::new(AsyncMutex::new(graph)),
                    status: RunStatus::Pending,
                    handle: None,
                },
            );
        } // guard dropped here

        // 5. Broadcast RunOpened (lock no longer held).
        let _ = self.state.event_tx.send(Event::RunOpened {
            run: id,
            task_list_path,
        });

        Ok(CommandOutcome::RunOpened { run: id })
    }

    /// Implement `StartRun`: spawn the Supervisor scheduler in the background.
    ///
    /// Looks up the Run's shared graph, builds a fresh [`RunControl`] (a sink
    /// wired to the broadcast, a cleared pause flag, a fresh cancel token),
    /// records the handle, sets the status `Running`, then `tokio::spawn`s
    /// [`run_graph`] over the graph and returns promptly.  The scheduler emits
    /// the live events the TUI observes; this method does not block on execution.
    ///
    /// Resume: re-issuing `StartRun` on a paused Run clears the pause flag and
    /// spawns a fresh scheduler that continues launching ready tasks (done tasks
    /// are skipped).
    fn start_run(&self, run: RunId) -> Result<CommandOutcome, ApiError> {
        // Take everything we need out of the registry under ONE lock, then drop
        // the guard before spawning (no lock across the spawn / await boundary).
        let (graph, cancel, pause, run_slug) = {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;

            // If a previous handle exists (e.g. resuming a paused run), cancel its
            // (already-finished or paused) scheduler defensively and replace it.
            if let Some(old) = entry.handle.take() {
                old.cancel.cancel();
            }

            let cancel = CancellationToken::new();
            let pause = Arc::new(AtomicBool::new(false));
            entry.handle = Some(RunHandle {
                cancel: cancel.clone(),
                pause: Arc::clone(&pause),
            });
            entry.status = RunStatus::Running;

            // Derive the slug here — inside the same lock — so we don't need a
            // second lock acquisition below.
            let slug = entry
                .task_list_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(SLUG_FALLBACK)
                .to_string();

            // Return the pieces the background task needs.
            (Arc::clone(&entry.graph), cancel, pause, slug)
        }; // registry guard dropped here.

        // Build the per-run control (sink → broadcast, pause flag, cancel token).
        let control = RunControl {
            run,
            sink: self.state.make_sink(),
            pause,
            cancel,
        };

        // Clone the static execution deps + the shared state for the background
        // task (so it can finalize the registry status when the scheduler ends).
        let worktree_manager = self.state.worktree_manager.clone();
        let config = self.state.config.clone();
        let backend = Arc::clone(&self.state.backend);
        let audit_registry = Arc::clone(&self.state.audit_registry);
        let state = Arc::clone(&self.state);

        // Spawn the scheduler.  It emits RunStatusChanged{Running} at the start
        // and the aggregate terminal status at the end (unless cancelled).  We do
        // not await it — execution proceeds in the background; the TUI observes
        // via subscribe().  When it returns we finalize the registry status so a
        // later `run()`/`runs()` snapshot reflects Completed/Failed.  The
        // JoinHandle is detached: the run drives itself to terminal + cleans up.
        tokio::spawn(async move {
            let _ = run_graph(
                graph,
                worktree_manager,
                config,
                backend,
                control,
                audit_registry,
                run_slug,
            )
            .await;
            state.finalize_run_status(run).await;
        });

        Ok(CommandOutcome::Acknowledged)
    }

    /// Implement `PauseRun`: stop launching NEW tasks (in-flight finish).
    ///
    /// Sets the cooperative pause flag on the Run's handle (if running) and the
    /// status to `Paused`, then broadcasts `RunStatusChanged{Paused}`.  See the
    /// module-level pause-semantics note.
    fn pause_run(&self, run: RunId) -> Result<CommandOutcome, ApiError> {
        {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;
            if let Some(handle) = entry.handle.as_ref() {
                handle.pause.store(true, Ordering::SeqCst);
            }
            entry.status = RunStatus::Paused;
        } // guard dropped before broadcast.
        let _ = self.state.event_tx.send(Event::RunStatusChanged {
            run,
            status: RunStatus::Paused,
        });
        Ok(CommandOutcome::Acknowledged)
    }

    /// Implement `CancelRun`: abort the scheduler + clean up; status → Failed.
    ///
    /// Cancels the Run's [`CancellationToken`] (the scheduler aborts in-flight
    /// drivers; each `DriverGuard` tears down its worktree/spokes — no leak),
    /// sets the status to `Failed` (cancelled), and broadcasts the change.  The
    /// Run is kept in the registry (its final state is observable) rather than
    /// dropped — the TUI can still inspect what completed before cancellation.
    fn cancel_run(&self, run: RunId) -> Result<CommandOutcome, ApiError> {
        {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;
            if let Some(handle) = entry.handle.take() {
                // Cancel the background scheduler; it aborts + cleans up.  We do
                // NOT drop the handle's cancel state from the entry before the
                // scheduler observes it — `cancel.cancel()` is sticky, so the
                // background task's `finalize_run_status` still sees a cancelled
                // token via the (now-removed) handle?  No: we removed the handle,
                // so finalize won't see it.  That is fine: finalize only
                // overwrites a `Running` status, and we set `Failed` here, so it
                // is a no-op.  Cancel's status wins.
                handle.cancel.cancel();
                // Clearing the pause flag is harmless and avoids a stuck flag if
                // the run is somehow resumed; cancel takes precedence anyway.
                handle.pause.store(false, Ordering::SeqCst);
            }
            entry.status = RunStatus::Failed;
        } // guard dropped before broadcast.
        let _ = self.state.event_tx.send(Event::RunStatusChanged {
            run,
            status: RunStatus::Failed,
        });
        Ok(CommandOutcome::Acknowledged)
    }

    /// Snapshot a single Run's view, locking the registry then the graph (never
    /// both at once, never the registry lock across the `.await`).
    async fn view_of(&self, id: RunId) -> Option<RunView> {
        // Pull the Arc graph handle + metadata out under the registry lock, then
        // drop the guard before awaiting the (separate) graph mutex.
        let (path, status, graph) = {
            let runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get(&id.0)?;
            (
                entry.task_list_path.clone(),
                entry.status.clone(),
                Arc::clone(&entry.graph),
            )
        }; // registry guard dropped before await.
        let g = graph.lock().await;
        Some(build_view(id, path, status, &g))
    }
}

#[async_trait]
impl Api for CoreApi {
    /// Execute a [`Command`].
    ///
    /// All four commands are implemented:
    /// * [`Command::OpenRun`] reads + interprets the file, registers the Run, and
    ///   broadcasts [`Event::RunOpened`].
    /// * [`Command::StartRun`] spawns the Supervisor scheduler in the background
    ///   (the Run actually executes) and returns [`CommandOutcome::Acknowledged`].
    /// * [`Command::PauseRun`] stops the scheduler launching NEW tasks.
    /// * [`Command::CancelRun`] aborts the scheduler and cleans up.
    async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
        match command {
            Command::OpenRun { task_list_path } => self.open_run(task_list_path).await,
            // Start/Pause/Cancel are synchronous registry+signal operations that
            // spawn/signal the background scheduler; none of them awaits, so they
            // are infallible-to-call and return promptly.
            Command::StartRun { run } => self.start_run(run),
            Command::PauseRun { run } => self.pause_run(run),
            Command::CancelRun { run } => self.cancel_run(run),
        }
    }

    /// Snapshot all open Runs in ascending `RunId` (insertion) order.
    async fn runs(&self) -> Vec<RunView> {
        // Snapshot the (id, path, status, graph-handle) tuples under the registry
        // lock, drop the guard, THEN lock each graph to build its view — so the
        // registry lock is never held across the graph `.await`.
        let entries: Vec<(RunId, PathBuf, RunStatus, Arc<AsyncMutex<TaskGraph>>)> = {
            let runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            runs.iter()
                .map(|(id, entry)| {
                    (
                        RunId(*id),
                        entry.task_list_path.clone(),
                        entry.status.clone(),
                        Arc::clone(&entry.graph),
                    )
                })
                .collect()
        }; // registry guard dropped before any graph await.

        let mut views = Vec::with_capacity(entries.len());
        for (id, path, status, graph) in entries {
            let g = graph.lock().await;
            views.push(build_view(id, path, status, &g));
        }
        views
    }

    /// Snapshot a single Run, or `None` for an unknown id.
    async fn run(&self, id: RunId) -> Option<RunView> {
        self.view_of(id).await
    }

    /// Subscribe to the live event stream.
    ///
    /// Wraps a fresh `broadcast::Receiver` in a [`BroadcastStream`] and maps away
    /// `Lagged` errors so the returned [`EventStream`] stays infallible
    /// (`Item = Event`), as the trait contract requires.  A lagging consumer
    /// silently skips the dropped events rather than seeing a transport error.
    fn subscribe(&self) -> EventStream {
        let rx = self.state.event_tx.subscribe();
        let stream = BroadcastStream::new(rx).filter_map(|result| result.ok());
        Box::pin(stream)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::noop::NoopBackend;
    use crate::config::{Config, GlobalConfig, ProjectConfig};
    use crate::dependency::EdgeInferrer;
    use crate::interpreter::StructuredTextInterpreter;
    // `next()` comes from `tokio_stream::StreamExt`, already in scope via the
    // glob import above (the orchestrator uses it for the broadcast stream).
    use crate::api::{AgentRole, ExchangeEvent, TaskState};
    use std::process::Command as StdCommand;
    use std::sync::Arc;
    use std::time::Duration;

    /// A small, valid structured-text task list used by the OpenRun tests.
    ///
    /// Two tasks under one section; `task-two` explicitly depends on `task-one`.
    const SAMPLE_TASK_LIST: &str = r#"# Sample — Task List

A small task list used to exercise CoreApi::OpenRun.

---

## 0001 — Foundation

### task-one — First task
Create the `lib.rs` entry point for the crate.
- **Depends on:** —
- **Done when:** `lib.rs` is present and the crate compiles.

### task-two — Second task
Add error types to `lib.rs`.
- **Depends on:** task-one
- **Done when:** error types in `lib.rs` have doc-tests that pass.
"#;

    /// A single-task structured-text task list (no dependency) for the execution
    /// tests where we want a minimal graph that runs to completion quickly.
    const ONE_TASK_LIST: &str = r#"# Solo — Task List

A one-task list used to exercise CoreApi::StartRun execution.

---

## 0001 — Foundation

### solo-task — The only task
Do the thing in `lib.rs`.
- **Depends on:** —
- **Done when:** it works.
"#;

    /// Build a `Config` with NO gates (the gate loop is a no-op) so the develop
    /// → review loop advances straight from develop to review — the same config
    /// the task-21–25 integration tests use.
    fn no_gate_config() -> Config {
        Config::resolve(GlobalConfig::default(), ProjectConfig::default())
    }

    /// Create a minimal git repo on a `develop` branch in a fresh tempdir so the
    /// WorktreeManager can create worktrees off it (mirrors the engine tests).
    fn setup_temp_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path();
        run_git(path, &["init"]);
        run_git(path, &["config", "user.email", "test@example.com"]);
        run_git(path, &["config", "user.name", "Test User"]);
        run_git(path, &["commit", "--allow-empty", "-m", "Initial commit"]);
        let branch = String::from_utf8(
            StdCommand::new("git")
                .args(["-C", &path.to_string_lossy()])
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .output()
                .expect("git rev-parse HEAD")
                .stdout,
        )
        .expect("utf8")
        .trim()
        .to_string();
        if branch != "develop" {
            run_git(path, &["branch", "-m", &branch, "develop"]);
        }
        dir
    }

    fn run_git(path: &std::path::Path, args: &[&str]) {
        let status = StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .status()
            .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
        assert!(status.success(), "git {args:?} failed");
    }

    fn branch_exists(path: &std::path::Path, branch: &str) -> bool {
        let output = StdCommand::new("git")
            .args(["-C", &path.to_string_lossy()])
            .args(["branch", "--list", branch])
            .output()
            .expect("git branch --list");
        !String::from_utf8_lossy(&output.stdout).trim().is_empty()
    }

    // ── Gated backend: holds the first developer prompt until released ─────────
    //
    // A deterministic instrument for the pause/cancel tests: it BLOCKS the very
    // first developer prompt on a `Notify` the test controls, so the first task
    // is provably still in-flight while the test issues Pause/Cancel.  All later
    // prompts pass through (developer → "dev output"; reviewer → approve JSON).

    /// Number of prompts to block before passing through (the first developer
    /// turn).  After release, all prompts (including the held one) pass through.
    #[derive(Clone)]
    struct GatedBackend {
        release: Arc<tokio::sync::Notify>,
        released: Arc<AtomicBool>,
        prompt_count: Arc<std::sync::atomic::AtomicUsize>,
        approve: String,
    }

    impl GatedBackend {
        fn new() -> (Arc<Self>, Arc<tokio::sync::Notify>) {
            let release = Arc::new(tokio::sync::Notify::new());
            let backend = Arc::new(Self {
                release: Arc::clone(&release),
                released: Arc::new(AtomicBool::new(false)),
                prompt_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                approve: r#"{"verdict":"approve"}"#.to_string(),
            });
            (backend, release)
        }
    }

    #[async_trait]
    impl AgentBackend for GatedBackend {
        async fn spawn(
            &self,
            _config: crate::backend::SessionConfig,
        ) -> Result<Box<dyn crate::backend::AgentSession>, crate::backend::BackendError> {
            Ok(Box::new(GatedSession {
                backend: self.clone(),
                terminated: false,
            }))
        }
    }

    struct GatedSession {
        backend: GatedBackend,
        terminated: bool,
    }

    #[async_trait]
    impl crate::backend::AgentSession for GatedSession {
        async fn prompt(
            &mut self,
            prompt: crate::backend::Prompt,
        ) -> Result<crate::backend::ResponseStream, crate::backend::BackendError> {
            use crate::backend::{BackendError, ResponseEvent};
            if self.terminated {
                return Err(BackendError::Terminated);
            }
            let n = self.backend.prompt_count.fetch_add(1, Ordering::SeqCst);
            // The first prompt (task-1's developer turn) blocks until released.
            if n == 0 && !self.backend.released.load(Ordering::SeqCst) {
                self.backend.release.notified().await;
                self.backend.released.store(true, Ordering::SeqCst);
            }
            let text = if prompt.text.contains("Review the work") {
                self.backend.approve.clone()
            } else {
                "dev output".to_string()
            };
            let events: Vec<Result<ResponseEvent, BackendError>> = vec![
                Ok(ResponseEvent::TextChunk { text }),
                Ok(ResponseEvent::TurnComplete),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
        async fn terminate(&mut self) -> Result<(), crate::backend::BackendError> {
            self.terminated = true;
            Ok(())
        }
    }

    /// Build a `CoreApi` over the deterministic interpreter + a `NoopBackend`
    /// (configured: developer output then approve verdict) + a temp-repo
    /// `WorktreeManager` + a no-gate `Config`.  Returns the api and the temp dir
    /// (keep it alive for the test).
    fn execution_core_api() -> (CoreApi, tempfile::TempDir) {
        let interpreter = Arc::new(EdgeInferrer::new(
            Arc::new(StructuredTextInterpreter::new()),
        ));
        // Cycle: developer output, then approve verdict (covers any task count).
        let backend = Arc::new(NoopBackend::with_responses(vec![
            "Implemented the feature.".into(),
            r#"{"verdict":"approve"}"#.into(),
        ]));
        let repo_dir = setup_temp_repo();
        let wm = WorktreeManager::new(repo_dir.path().to_path_buf(), "develop".into());
        let api = CoreApi::new(interpreter, backend, wm, no_gate_config());
        (api, repo_dir)
    }

    /// Write `contents` to a uniquely-named markdown file in a fresh tempdir and
    /// return both (keep the dir alive for the test's lifetime).
    fn write_task_list(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("sample-feature.md");
        std::fs::write(&path, contents).expect("write task list");
        (dir, path)
    }

    /// Poll `cond` with a bounded deadline (no fixed sleeps); panic with `what`
    /// on timeout.  Mirrors the testing-strategy poll-with-deadline pattern.
    async fn poll_until<F, Fut>(mut cond: F, what: &str)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if cond().await {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting: {what}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // ── OpenRun (task 28 behavior, preserved) ─────────────────────────────────

    /// **Acceptance (task 28): OpenRun interprets the file and creates a Run.**
    #[tokio::test]
    async fn open_run_interprets_file_and_creates_run() {
        let (api, _repo) = execution_core_api();
        let (_dir, path) = write_task_list(SAMPLE_TASK_LIST);

        let mut stream = api.subscribe();

        let outcome = api
            .execute(Command::OpenRun {
                task_list_path: path.clone(),
            })
            .await
            .expect("OpenRun must succeed for a valid task list");

        let run_id = match outcome {
            CommandOutcome::RunOpened { run } => run,
            other => panic!("expected RunOpened, got {other:?}"),
        };
        assert_eq!(run_id, RunId(1), "first run should be allocated id 1");

        let all = api.runs().await;
        assert_eq!(all.len(), 1, "exactly one run should be open");
        let view = &all[0];
        assert_eq!(view.id, run_id);
        assert_eq!(view.task_list_path, path);
        assert_eq!(view.status, RunStatus::Pending, "new run starts Pending");
        assert_eq!(view.tasks.len(), 2, "both tasks must be interpreted");

        let titles: Vec<&str> = view.tasks.iter().map(|t| t.title.as_str()).collect();
        assert!(titles.contains(&"First task"));
        assert!(titles.contains(&"Second task"));
        let task_two = view
            .tasks
            .iter()
            .find(|t| t.id.0 == "task-two")
            .expect("task-two must be present");
        assert!(
            task_two.depends_on.iter().any(|d| d.0 == "task-one"),
            "task-two must depend on task-one; got {:?}",
            task_two.depends_on
        );

        let single = api.run(run_id).await.expect("run(id) must find the run");
        assert_eq!(single.id, run_id);
        assert!(api.run(RunId(999)).await.is_none());

        let ev = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("timed out waiting for RunOpened")
            .expect("stream ended unexpectedly");
        match ev {
            Event::RunOpened {
                run,
                task_list_path,
            } => {
                assert_eq!(run, run_id);
                assert_eq!(task_list_path, path);
            }
            other => panic!("expected RunOpened event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn multiple_open_runs_get_distinct_ids() {
        let (api, _repo) = execution_core_api();
        let (_d1, p1) = write_task_list(SAMPLE_TASK_LIST);
        let (_d2, p2) = write_task_list(SAMPLE_TASK_LIST);

        let id1 = match api
            .execute(Command::OpenRun { task_list_path: p1 })
            .await
            .unwrap()
        {
            CommandOutcome::RunOpened { run } => run,
            other => panic!("unexpected: {other:?}"),
        };
        let id2 = match api
            .execute(Command::OpenRun { task_list_path: p2 })
            .await
            .unwrap()
        {
            CommandOutcome::RunOpened { run } => run,
            other => panic!("unexpected: {other:?}"),
        };

        assert_ne!(id1, id2, "each run must get a distinct id");
        assert_eq!(id1, RunId(1));
        assert_eq!(id2, RunId(2));
        let all = api.runs().await;
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, RunId(1));
        assert_eq!(all[1].id, RunId(2));
    }

    #[tokio::test]
    async fn open_run_with_missing_file_returns_error() {
        let (api, _repo) = execution_core_api();
        let result = api
            .execute(Command::OpenRun {
                task_list_path: PathBuf::from("/no/such/path/definitely-missing.md"),
            })
            .await;

        match result {
            Err(ApiError::InvalidCommand { reason }) => {
                assert!(
                    reason.contains("could not read task list"),
                    "error should explain the read failure; got: {reason}"
                );
            }
            other => panic!("expected InvalidCommand for a missing file, got {other:?}"),
        }
        assert!(api.runs().await.is_empty());
    }

    #[tokio::test]
    async fn open_run_with_invalid_content_returns_error() {
        let (api, _repo) = execution_core_api();
        let bad = r#"# Bad — Task List

Preamble.

---

## 0001 — X

### only-task — The only task
Does a thing.
- **Depends on:** ghost-task
- **Done when:** it works.
"#;
        let (_dir, path) = write_task_list(bad);

        let result = api
            .execute(Command::OpenRun {
                task_list_path: path,
            })
            .await;
        match result {
            Err(ApiError::InvalidCommand { reason }) => {
                assert!(
                    reason.contains("could not interpret task list"),
                    "error should explain the interpretation failure; got: {reason}"
                );
            }
            other => panic!("expected InvalidCommand for invalid content, got {other:?}"),
        }
        assert!(api.runs().await.is_empty());
    }

    #[tokio::test]
    async fn queries_empty_before_any_open() {
        let (api, _repo) = execution_core_api();
        assert!(api.runs().await.is_empty());
        assert!(api.run(RunId(1)).await.is_none());
    }

    // ── Seed-persist (task orchestrator-seed-write) ───────────────────────────

    /// **Acceptance (orchestrator-seed-write):**
    /// Opening a run on a task list in a temp repo with no pre-existing
    /// `.tasks/{slug}.json` creates the file with all tasks in the `new` state,
    /// asserted BEFORE any StartRun command.
    #[tokio::test]
    async fn open_run_seeds_artifact_before_start_run() {
        // Build a CoreApi backed by a fresh temp repo so persist_graph has a
        // real filesystem to write to.
        let (api, repo_dir) = execution_core_api();
        let repo_root = repo_dir.path().to_path_buf();

        // Write a small task-list file (slug will be "seed-test").
        let task_list = r#"# Seed — Task List

A minimal list to verify seed-persist.

---

## 0001 — Foundation

### alpha — Alpha task
Create alpha.
- **Depends on:** —
- **Done when:** alpha done.

### beta — Beta task
Create beta.
- **Depends on:** alpha
- **Done when:** beta done.
"#;
        let task_list_dir = tempfile::tempdir().expect("create tempdir");
        let task_list_path = task_list_dir.path().join("seed-test.md");
        std::fs::write(&task_list_path, task_list).expect("write task list");

        // Verify no artifact exists yet.
        let artifact_path = crate::persist::tasks_path(&repo_root, "seed-test");
        assert!(
            !artifact_path.exists(),
            ".tasks/seed-test.json must not exist before OpenRun"
        );

        // Issue OpenRun — do NOT issue StartRun.
        let outcome = api
            .execute(Command::OpenRun {
                task_list_path: task_list_path.clone(),
            })
            .await
            .expect("OpenRun must succeed");
        assert!(
            matches!(outcome, CommandOutcome::RunOpened { .. }),
            "expected RunOpened, got {outcome:?}"
        );

        // Assert: the artifact now exists on disk.
        assert!(
            artifact_path.exists(),
            ".tasks/seed-test.json must exist immediately after OpenRun, before StartRun"
        );

        // Assert: all tasks are in the `new` state.
        let loaded = crate::persist::load_graph(&repo_root, "seed-test")
            .await
            .expect("load_graph must not error")
            .expect(".tasks/seed-test.json must be loadable");

        assert_eq!(
            loaded.slug, "seed-test",
            "persisted graph slug must match the file stem"
        );
        assert_eq!(loaded.tasks.len(), 2, "both tasks must be persisted");
        for task in &loaded.tasks {
            assert_eq!(
                task.state,
                crate::task::TaskState::New,
                "task `{}` must be in `new` state before StartRun; got {:?}",
                task.id,
                task.state
            );
        }
    }

    // ── StartRun executes the Run (the done-when) ─────────────────────────────

    /// Open a Run, register a subscriber, then drain the event stream into a
    /// shared collector for the lifetime of `task`.  Returns the join handle of
    /// the collector and the shared `Vec<Event>` it appends into.
    fn collect_events(api: &CoreApi) -> (tokio::task::JoinHandle<()>, Arc<Mutex<Vec<Event>>>) {
        let mut stream = api.subscribe();
        let sink = Arc::new(Mutex::new(Vec::<Event>::new()));
        let sink_clone = Arc::clone(&sink);
        let handle = tokio::spawn(async move {
            while let Some(ev) = stream.next().await {
                sink_clone.lock().unwrap().push(ev);
            }
        });
        (handle, sink)
    }

    /// **Acceptance (the done-when): Start executes the Run.**
    ///
    /// Build a `CoreApi` with `NoopBackend` (dev output + approve), a temp-repo
    /// `WorktreeManager`, and a no-gate `Config`; open a one-task Run, subscribe,
    /// `StartRun`, then observe via `subscribe()` that the run executes:
    /// `RunStatusChanged→Running`, `TaskStateChanged` progressions, the run
    /// reaches `Completed` and the task is `Done`, and `AgentExchange` events
    /// (PromptSent + chunks + TurnComplete) were emitted.  Poll with a bounded
    /// deadline — no fixed sleeps.
    // Multi-thread flavor: this test spawns the background scheduler + an actor
    // tree (Developer/Reviewer per task) AND polls the api concurrently.  A
    // dedicated worker pool keeps it deterministic + fast (no single-thread
    // starvation between the poll loop and the background execution).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn start_run_executes_run_and_emits_live_events() {
        let (api, _repo) = execution_core_api();
        let api = Arc::new(api);
        let (_dir, path) = write_task_list(ONE_TASK_LIST);

        let run = match api
            .execute(Command::OpenRun {
                task_list_path: path,
            })
            .await
            .unwrap()
        {
            CommandOutcome::RunOpened { run } => run,
            other => panic!("unexpected: {other:?}"),
        };

        // Subscribe BEFORE starting so we capture every event.
        let (collector, events) = collect_events(&api);

        let outcome = api.execute(Command::StartRun { run }).await.unwrap();
        assert!(
            matches!(outcome, CommandOutcome::Acknowledged),
            "StartRun returns promptly (Acknowledged) — execution is in background"
        );

        // Poll the run state until it reaches Completed with the task Done.
        let api_poll = Arc::clone(&api);
        poll_until(
            || {
                let api = Arc::clone(&api_poll);
                async move {
                    match api.run(run).await {
                        Some(v) => {
                            v.status == RunStatus::Completed
                                && v.tasks.iter().all(|t| t.state == TaskState::Done)
                        }
                        None => false,
                    }
                }
            },
            "run to reach Completed with task Done",
        )
        .await;

        // Give the collector a moment to drain the final events, bounded.
        poll_until(
            || {
                let events = Arc::clone(&events);
                async move {
                    let evs = events.lock().unwrap();
                    evs.iter().any(|e| {
                        matches!(
                            e,
                            Event::RunStatusChanged {
                                status: RunStatus::Completed,
                                ..
                            }
                        )
                    })
                }
            },
            "RunStatusChanged{Completed} to be observed on the stream",
        )
        .await;

        let evs = events.lock().unwrap().clone();
        collector.abort();

        // RunStatusChanged → Running was emitted.
        assert!(
            evs.iter().any(|e| matches!(
                e,
                Event::RunStatusChanged {
                    run: r,
                    status: RunStatus::Running
                } if *r == run
            )),
            "RunStatusChanged{{Running}} must be emitted; got {evs:?}"
        );
        // RunStatusChanged → Completed was emitted.
        assert!(
            evs.iter().any(|e| matches!(
                e,
                Event::RunStatusChanged {
                    run: r,
                    status: RunStatus::Completed
                } if *r == run
            )),
            "RunStatusChanged{{Completed}} must be emitted"
        );
        // TaskStateChanged progressions: at least InProgress, InReview, Done.
        let saw_in_progress = evs.iter().any(|e| {
            matches!(
                e,
                Event::TaskStateChanged {
                    state: TaskState::InProgress,
                    ..
                }
            )
        });
        let saw_in_review = evs.iter().any(|e| {
            matches!(
                e,
                Event::TaskStateChanged {
                    state: TaskState::InReview,
                    ..
                }
            )
        });
        let saw_done = evs.iter().any(|e| {
            matches!(
                e,
                Event::TaskStateChanged {
                    state: TaskState::Done,
                    ..
                }
            )
        });
        assert!(saw_in_progress, "must emit TaskStateChanged→InProgress");
        assert!(saw_in_review, "must emit TaskStateChanged→InReview");
        assert!(saw_done, "must emit TaskStateChanged→Done");

        // AgentExchange: PromptSent + at least one ResponseChunk + TurnComplete.
        let saw_prompt = evs.iter().any(|e| {
            matches!(
                e,
                Event::AgentExchange {
                    role: AgentRole::Developer,
                    event: ExchangeEvent::PromptSent { .. },
                    ..
                }
            )
        });
        let saw_chunk = evs.iter().any(|e| {
            matches!(
                e,
                Event::AgentExchange {
                    event: ExchangeEvent::ResponseChunk { .. },
                    ..
                }
            )
        });
        let saw_turn_complete = evs.iter().any(|e| {
            matches!(
                e,
                Event::AgentExchange {
                    event: ExchangeEvent::TurnComplete,
                    ..
                }
            )
        });
        assert!(saw_prompt, "must emit AgentExchange PromptSent");
        assert!(saw_chunk, "must emit AgentExchange ResponseChunk");
        assert!(saw_turn_complete, "must emit AgentExchange TurnComplete");

        // Final state: status Completed, task Done.
        let view = api.run(run).await.unwrap();
        assert_eq!(view.status, RunStatus::Completed);
        assert!(view.tasks.iter().all(|t| t.state == TaskState::Done));
    }

    /// Unknown run id is rejected by StartRun.
    #[tokio::test]
    async fn start_run_unknown_id_is_rejected() {
        let (api, _repo) = execution_core_api();
        let err = api.execute(Command::StartRun { run: RunId(999) }).await;
        assert!(matches!(err, Err(ApiError::UnknownRun { run: RunId(999) })));
    }

    // ── CancelRun affects the Run ─────────────────────────────────────────────

    /// **Cancel affects the Run — deterministically (the done-when for Cancel).**
    ///
    /// Uses the [`GatedBackend`] so task-a's developer turn is provably still
    /// in-flight (blocked on the gate) when we cancel.  Sequence:
    ///
    /// 1. Start (task-a's developer prompt blocks on the gate; its worktree +
    ///    branch are created first);
    /// 2. poll until task-a is `InProgress` AND its worktree exists (proving the
    ///    driver launched);
    /// 3. Cancel → the scheduler aborts the in-flight driver; its `DriverGuard`
    ///    tears down the worktree + spokes.
    ///
    /// Then assert: status is `Failed` (cancelled), NOT all tasks reach `Done`
    /// (task-b — which depends on the never-finished task-a — stays `New`), and
    /// no worktree/branch leaks (bounded poll for the async teardown).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancel_run_stops_execution_and_cleans_up() {
        let interpreter = Arc::new(EdgeInferrer::new(
            Arc::new(StructuredTextInterpreter::new()),
        ));
        let (backend, _release) = GatedBackend::new();
        let repo_dir = setup_temp_repo();
        let repo_root = repo_dir.path().to_path_buf();
        let wm = WorktreeManager::new(repo_root.clone(), "develop".into());
        let mut config = no_gate_config();
        config.concurrency = 1; // strictly one task at a time.
        let api = Arc::new(CoreApi::new(interpreter, backend, wm, config));

        // A 2-task chain: task-b depends on task-a (so task-b cannot run until
        // task-a is Done — which never happens because we hold + cancel it).
        let source = "# Cancel — Task List\n\nPreamble.\n\n---\n\n## 0001 — S\n\n\
### task-a — Task A\nDoes A in `lib.rs`.\n- **Depends on:** —\n\
- **Done when:** a works.\n\n\
### task-b — Task B\nDoes B in `lib.rs`.\n- **Depends on:** task-a\n\
- **Done when:** b works.\n";
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("cancel-feature.md");
        std::fs::write(&file_path, source).unwrap();

        let run = match api
            .execute(Command::OpenRun {
                task_list_path: file_path,
            })
            .await
            .unwrap()
        {
            CommandOutcome::RunOpened { run } => run,
            other => panic!("unexpected: {other:?}"),
        };

        // 1. Start: task-a's developer prompt blocks on the gate.
        api.execute(Command::StartRun { run }).await.unwrap();

        // 2. Wait until task-a is InProgress AND its worktree exists (the driver
        //    has launched + created the worktree before the held developer turn).
        let worktrees_dir = repo_root.join(".worktrees");
        let api_poll = Arc::clone(&api);
        let wt_a = worktrees_dir.join("task-a");
        poll_until(
            || {
                let api = Arc::clone(&api_poll);
                let wt_a = wt_a.clone();
                async move {
                    let in_progress = api
                        .run(run)
                        .await
                        .map(|v| {
                            v.tasks
                                .iter()
                                .any(|t| t.id.0 == "task-a" && t.state == TaskState::InProgress)
                        })
                        .unwrap_or(false);
                    in_progress && wt_a.exists()
                }
            },
            "task-a to be InProgress with its worktree created",
        )
        .await;

        // 3. Cancel while task-a is held in-flight.
        let outcome = api.execute(Command::CancelRun { run }).await.unwrap();
        assert!(matches!(outcome, CommandOutcome::Acknowledged));

        // The run status reflects cancellation (Failed) immediately.
        let view = api.run(run).await.expect("run still exists after cancel");
        assert_eq!(
            view.status,
            RunStatus::Failed,
            "cancelled run status must be Failed"
        );

        // The chain cannot complete: task-b (depends on the never-finished
        // task-a) must never reach Done, and the run is never Completed.  Assert
        // this holds over a bounded window after the cancel settles.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        loop {
            let v = api.run(run).await.unwrap();
            let b_done = v
                .tasks
                .iter()
                .find(|t| t.id.0 == "task-b")
                .map(|t| t.state == TaskState::Done)
                .unwrap_or(false);
            assert!(!b_done, "task-b must not complete under cancel");
            assert_ne!(
                v.status,
                RunStatus::Completed,
                "cancelled run must not Complete"
            );
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        // No worktree/branch leak: poll the filesystem with a bounded deadline
        // (the DriverGuard teardown / scheduler abort is async + best-effort).
        poll_until(
            || {
                let repo_root = repo_root.clone();
                let worktrees_dir = worktrees_dir.clone();
                async move {
                    let a_gone = !worktrees_dir.join("task-a").exists();
                    let b_gone = !worktrees_dir.join("task-b").exists();
                    let branch_a_gone = !branch_exists(&repo_root, "task/task-a");
                    let branch_b_gone = !branch_exists(&repo_root, "task/task-b");
                    a_gone && b_gone && branch_a_gone && branch_b_gone
                }
            },
            "all worktrees + task branches to be cleaned up after cancel",
        )
        .await;
    }

    /// Cancel of an unknown run id is rejected.
    #[tokio::test]
    async fn cancel_run_unknown_id_is_rejected() {
        let (api, _repo) = execution_core_api();
        let err = api.execute(Command::CancelRun { run: RunId(123) }).await;
        assert!(matches!(err, Err(ApiError::UnknownRun { .. })));
    }

    // ── PauseRun stops launching new tasks ────────────────────────────────────

    /// **Pause stops launching new tasks (documented MVP semantics).**
    ///
    /// Pause a Run BEFORE starting it (the pause flag is set on the handle when
    /// the first task is about to launch).  Here we assert the simpler, robust
    /// invariant the MVP guarantees: `PauseRun` sets the status to `Paused` and
    /// emits `RunStatusChanged{Paused}`, and a paused run does not drive its
    /// tasks to Done.  We then resume via `StartRun` and confirm it completes —
    /// proving pause is a *cooperative stop-launching* flag, not a teardown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pause_run_sets_paused_and_does_not_complete_then_resume_completes() {
        let (api, _repo) = execution_core_api();
        let api = Arc::new(api);
        let (_dir, path) = write_task_list(ONE_TASK_LIST);

        let run = match api
            .execute(Command::OpenRun {
                task_list_path: path,
            })
            .await
            .unwrap()
        {
            CommandOutcome::RunOpened { run } => run,
            other => panic!("unexpected: {other:?}"),
        };

        // Subscribe so we can assert the Paused status change is emitted.
        let (collector, events) = collect_events(&api);

        // Pause before start: status → Paused, event emitted, no execution.
        api.execute(Command::PauseRun { run }).await.unwrap();
        assert_eq!(api.run(run).await.unwrap().status, RunStatus::Paused);

        poll_until(
            || {
                let events = Arc::clone(&events);
                async move {
                    events.lock().unwrap().iter().any(|e| {
                        matches!(
                            e,
                            Event::RunStatusChanged {
                                status: RunStatus::Paused,
                                ..
                            }
                        )
                    })
                }
            },
            "RunStatusChanged{Paused} to be emitted",
        )
        .await;

        // The task must NOT have advanced to Done (no scheduler is running, and a
        // start-while-paused would not launch it).  Assert it is still New.
        let view = api.run(run).await.unwrap();
        assert!(
            view.tasks.iter().all(|t| t.state == TaskState::New),
            "a paused (never-started) run must not advance its tasks; got {:?}",
            view.tasks.iter().map(|t| &t.state).collect::<Vec<_>>()
        );

        // Resume: StartRun clears the pause flag and runs to completion.
        api.execute(Command::StartRun { run }).await.unwrap();
        let api_poll = Arc::clone(&api);
        poll_until(
            || {
                let api = Arc::clone(&api_poll);
                async move {
                    matches!(
                        api.run(run).await.map(|v| v.status),
                        Some(RunStatus::Completed)
                    )
                }
            },
            "resumed run to reach Completed",
        )
        .await;

        let view = api.run(run).await.unwrap();
        assert_eq!(view.status, RunStatus::Completed);
        assert!(view.tasks.iter().all(|t| t.state == TaskState::Done));
        collector.abort();
    }

    /// **Pause stops launching the SECOND task — deterministically.**
    ///
    /// Uses a [`GatedBackend`] that BLOCKS the first developer prompt on a signal
    /// the test controls, so task-1 is provably still in-flight when we pause.
    /// With `concurrency = 1`, task-2 cannot launch until task-1 finishes — and
    /// once paused, the scheduler's fill phase will not launch task-2 even after
    /// task-1 completes.  Sequence:
    ///
    /// 1. Start (task-1's developer prompt blocks on the gate);
    /// 2. poll until task-1 is observably `InProgress` (so we know it launched);
    /// 3. Pause (sets the stop-launching flag);
    /// 4. release the gate → task-1 finishes;
    /// 5. assert task-2 NEVER leaves `New` within a bounded window, and the run
    ///    never reaches `Completed` — proving "stop launching new tasks".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pause_stops_second_task_from_launching() {
        let interpreter = Arc::new(EdgeInferrer::new(
            Arc::new(StructuredTextInterpreter::new()),
        ));
        let (backend, release) = GatedBackend::new();
        let repo_dir = setup_temp_repo();
        let wm = WorktreeManager::new(repo_dir.path().to_path_buf(), "develop".into());
        let mut config = no_gate_config();
        config.concurrency = 1; // strictly one task at a time.
        let api = Arc::new(CoreApi::new(interpreter, backend, wm, config));

        // Two INDEPENDENT tasks (both ready immediately); concurrency=1 serializes.
        let source = "# PauseTwo — Task List\n\nPreamble.\n\n---\n\n## 0001 — S\n\n\
### first — First task\nDoes first in `a.rs`.\n- **Depends on:** —\n\
- **Done when:** first works.\n\n\
### second — Second task\nDoes second in `b.rs`.\n- **Depends on:** —\n\
- **Done when:** second works.\n";
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("pausetwo.md");
        std::fs::write(&file_path, source).unwrap();

        let run = match api
            .execute(Command::OpenRun {
                task_list_path: file_path,
            })
            .await
            .unwrap()
        {
            CommandOutcome::RunOpened { run } => run,
            other => panic!("unexpected: {other:?}"),
        };

        // 1. Start: task-1's developer prompt blocks on the gate.
        api.execute(Command::StartRun { run }).await.unwrap();

        // 2. Wait until task-1 is observably InProgress (it has launched + its
        //    developer turn is blocked on the gate).
        let api_poll = Arc::clone(&api);
        poll_until(
            || {
                let api = Arc::clone(&api_poll);
                async move {
                    api.run(run)
                        .await
                        .map(|v| v.tasks.iter().any(|t| t.state == TaskState::InProgress))
                        .unwrap_or(false)
                }
            },
            "task-1 to reach InProgress (blocked on the gate)",
        )
        .await;

        // 3. Pause while task-1 is held in-flight.
        api.execute(Command::PauseRun { run }).await.unwrap();
        assert_eq!(api.run(run).await.unwrap().status, RunStatus::Paused);

        // 4. Release the gate → task-1 finishes; the scheduler's fill phase sees
        //    the pause flag and does NOT launch task-2.
        release.notify_waiters();

        // 5. Within a bounded window, task-2 must NEVER leave `New` and the run
        //    must never reach `Completed`.  (task-1 may reach Done; that's fine.)
        let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
        loop {
            let view = api.run(run).await.unwrap();
            let second = view
                .tasks
                .iter()
                .find(|t| t.id.0 == "second")
                .expect("second task present");
            assert_eq!(
                second.state,
                TaskState::New,
                "the SECOND task must not launch while paused; got {:?}",
                second.state
            );
            assert_ne!(
                view.status,
                RunStatus::Completed,
                "a paused run must not reach Completed"
            );
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    /// Pause of an unknown run id is rejected.
    #[tokio::test]
    async fn pause_run_unknown_id_is_rejected() {
        let (api, _repo) = execution_core_api();
        let err = api.execute(Command::PauseRun { run: RunId(7) }).await;
        assert!(matches!(err, Err(ApiError::UnknownRun { .. })));
    }

    // ── subscribe semantics ───────────────────────────────────────────────────

    #[tokio::test]
    async fn subscribe_is_independent_and_infallible() {
        let (api, _repo) = execution_core_api();
        let mut s1 = api.subscribe();
        let mut s2 = api.subscribe();

        let (_dir, path) = write_task_list(SAMPLE_TASK_LIST);
        api.execute(Command::OpenRun {
            task_list_path: path,
        })
        .await
        .unwrap();

        for stream in [&mut s1, &mut s2] {
            let ev = tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .expect("each subscriber must receive the event")
                .expect("stream ended unexpectedly");
            assert!(matches!(ev, Event::RunOpened { .. }));
        }
    }
}
