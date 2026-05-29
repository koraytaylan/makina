//! The real, core-backed [`Api`] implementation.
//!
//! [`CoreApi`] is the orchestrator's outward-facing surface — the concrete type
//! the TUI binds to via `Arc<dyn Api>`.  This first cut implements the
//! **`OpenRun`** path end-to-end:
//!
//! ```text
//!   execute(OpenRun{path})
//!       │ read file (tokio::fs)
//!       ▼
//!   interpreter.interpret(slug, text)  ──►  TaskGraph
//!       │ allocate RunId
//!       ▼
//!   register Run (status = Pending)
//!       │ broadcast Event::RunOpened
//!       ▼
//!   return CommandOutcome::RunOpened{run}
//! ```
//!
//! plus the read queries ([`Api::runs`] / [`Api::run`]) and the live
//! [`Api::subscribe`] stream.  The `Start`/`Pause`/`Cancel` commands are
//! **deliberate seams** for task 31 (run-control) and the e2e: they update the
//! lightweight registry status where it is safe to do so, but they do **not**
//! drive the Supervisor execution loop yet (see [`CoreApi::execute`]).
//!
//! # Locking discipline
//!
//! The Runs registry lives behind a `std::sync::Mutex`.  The lock is **never
//! held across an `.await`**: every handler locks, reads/mutates the registry,
//! drops the guard, and only then awaits I/O or broadcasts an event.  This keeps
//! the synchronous mutex sound under the async runtime and avoids deadlocks.
//!
//! # Seams
//!
//! | Concern | Status here | Owning task |
//! |---------|-------------|-------------|
//! | `OpenRun` → interpret → register → broadcast | **implemented** | this task (28) |
//! | `runs()` / `run()` / `subscribe()` | **implemented** | this task (28) |
//! | `StartRun` / `PauseRun` / `CancelRun` driving the Supervisor | **seam** | task 31 (run-control) |
//! | model-backed interpreter + real ACP backend | injected, not wired | e2e (task 33) |

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::api::{
    Api, ApiError, Command, CommandOutcome, Event, EventStream, RunId, RunStatus, RunView, TaskView,
};
use crate::interpreter::TaskListInterpreter;
use crate::task::TaskGraph;

// ── Broadcast capacity ──────────────────────────────────────────────────────────

/// Capacity of the event broadcast channel.
///
/// A subscriber that lags by more than this many events drops the oldest ones;
/// the [`Api::subscribe`] stream maps such lag errors away so it stays
/// infallible (see [`CoreApi::subscribe`]).  256 comfortably absorbs the bursts
/// a single `OpenRun` produces while keeping memory bounded.
const EVENT_CHANNEL_CAPACITY: usize = 256;

// ── Registry entry ──────────────────────────────────────────────────────────────

/// One open Run as tracked by the orchestrator's in-memory registry.
///
/// Holds the interpreted [`TaskGraph`] (the source of truth for the Run's task
/// list), the backing file path, and the aggregate [`RunStatus`].  Projected
/// into an [`api::RunView`](crate::api::RunView) on demand by the read queries.
struct RunEntry {
    /// Path to the task-list file this Run was opened from.
    task_list_path: PathBuf,
    /// The interpreted task graph (deterministic or model-backed, depending on
    /// the injected interpreter).
    graph: TaskGraph,
    /// Aggregate status.  Starts [`RunStatus::Pending`]; task 31 will drive the
    /// remaining transitions once execution is wired.
    status: RunStatus,
}

impl RunEntry {
    /// Project this registry entry into the view-level [`RunView`] the TUI sees.
    ///
    /// Maps every [`crate::task::Task`] into a [`TaskView`] via the documented
    /// `From` conversions in [`crate::api`].
    fn to_view(&self, id: RunId) -> RunView {
        let tasks = self
            .graph
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
            task_list_path: self.task_list_path.clone(),
            status: self.status.clone(),
            tasks,
        }
    }
}

// ── CoreApi ─────────────────────────────────────────────────────────────────────

/// The real, core-backed orchestrator [`Api`].
///
/// Construct with [`CoreApi::new`], injecting any
/// [`TaskListInterpreter`] (typically
/// `EdgeInferrer::new(Arc::new(StructuredTextInterpreter::new()))` for the
/// deterministic, no-model path the TUI uses).
///
/// # Concurrency
///
/// `CoreApi` is `Send + Sync` and all methods take `&self`, so it can be shared
/// across the TUI's async tasks as `Arc<dyn Api>`.  Internal mutable state lives
/// behind a `Mutex` that is never held across an `.await`.
pub struct CoreApi {
    /// The interpreter used to turn task-list source text into a [`TaskGraph`].
    /// Injected so the deterministic parser (TUI) and the model-backed
    /// interpreter (e2e) are interchangeable.
    interpreter: std::sync::Arc<dyn TaskListInterpreter>,

    /// The Runs registry: `RunId` → [`RunEntry`].  A `BTreeMap` keeps iteration
    /// order stable (ascending `RunId`, i.e. insertion order) for [`Api::runs`].
    runs: Mutex<BTreeMap<u64, RunEntry>>,

    /// Monotonic allocator for fresh [`RunId`]s.  First id is `1`.
    next_id: AtomicU64,

    /// Broadcast sender for the live event stream.  Each [`Api::subscribe`] call
    /// derives an independent receiver from this.
    event_tx: broadcast::Sender<Event>,
}

impl CoreApi {
    /// Create a new `CoreApi` with the given task-list interpreter.
    ///
    /// The TUI passes
    /// `EdgeInferrer::new(Arc::new(StructuredTextInterpreter::new()))` so runs
    /// open without needing a model or auth; the e2e task will inject a
    /// `ModelInterpreter` (over the real ACP backend) instead.
    pub fn new(interpreter: std::sync::Arc<dyn TaskListInterpreter>) -> Self {
        let (event_tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            interpreter,
            runs: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            event_tx,
        }
    }

    /// Allocate the next monotonic [`RunId`].
    fn alloc_id(&self) -> RunId {
        RunId(self.next_id.fetch_add(1, Ordering::Relaxed))
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
            .unwrap_or("task-list")
            .to_string();

        // 3. Interpret the source into a TaskGraph (no lock held — this awaits).
        let graph = self
            .interpreter
            .interpret(&slug, &text)
            .await
            .map_err(|e| ApiError::InvalidCommand {
                reason: format!("could not interpret task list `{slug}`: {e}"),
            })?;

        // 4. Allocate an id and register the Run.  Lock → insert → DROP guard
        //    before any further await/broadcast.
        let id = self.alloc_id();
        {
            let mut runs = self.runs.lock().expect("runs registry mutex poisoned");
            runs.insert(
                id.0,
                RunEntry {
                    task_list_path: task_list_path.clone(),
                    graph,
                    status: RunStatus::Pending,
                },
            );
        } // guard dropped here

        // 5. Broadcast RunOpened (lock no longer held).  `send` only errors when
        //    there are no live receivers, which is fine — the outcome below is
        //    still the authoritative confirmation for the caller.
        let _ = self.event_tx.send(Event::RunOpened {
            run: id,
            task_list_path,
        });

        Ok(CommandOutcome::RunOpened { run: id })
    }

    /// Look up a Run's status, returning [`ApiError::UnknownRun`] if absent.
    ///
    /// Helper shared by the `Start`/`Pause`/`Cancel` seams to validate the id
    /// before they no-op on execution.  Locks, reads, drops the guard.
    fn require_run(&self, run: RunId) -> Result<(), ApiError> {
        let runs = self.runs.lock().expect("runs registry mutex poisoned");
        if runs.contains_key(&run.0) {
            Ok(())
        } else {
            Err(ApiError::UnknownRun { run })
        }
    }
}

#[async_trait]
impl Api for CoreApi {
    /// Execute a [`Command`].
    ///
    /// * [`Command::OpenRun`] is fully implemented here (the heart of this task):
    ///   it reads + interprets the file, registers the Run, and broadcasts
    ///   [`Event::RunOpened`].
    /// * [`Command::StartRun`] / [`Command::PauseRun`] / [`Command::CancelRun`]
    ///   are **seams for task 31 (run-control) and the e2e**.  They validate the
    ///   target `RunId` (returning [`ApiError::UnknownRun`] for an unknown id)
    ///   but **do not drive the Supervisor execution loop** — opening a Run does
    ///   not yet run it.  `Start`/`Pause` record the requested aggregate status
    ///   so the sidebar reflects user intent and broadcast the corresponding
    ///   [`Event::RunStatusChanged`]; `Cancel` removes the Run from the registry.
    ///   Wiring these to real agent dispatch is task 31's job.
    async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
        match command {
            Command::OpenRun { task_list_path } => self.open_run(task_list_path).await,

            // ── Seam: task 31 (run-control) ──────────────────────────────────
            // TODO(task-31): drive Supervisor execution. For now we only record
            // the requested status in the registry so the TUI reflects intent;
            // no agents are dispatched and no task transitions occur.
            Command::StartRun { run } => {
                self.require_run(run)?;
                {
                    let mut runs = self.runs.lock().expect("runs registry mutex poisoned");
                    if let Some(entry) = runs.get_mut(&run.0) {
                        entry.status = RunStatus::Running;
                    }
                } // guard dropped before broadcast
                let _ = self.event_tx.send(Event::RunStatusChanged {
                    run,
                    status: RunStatus::Running,
                });
                Ok(CommandOutcome::Acknowledged)
            }

            // ── Seam: task 31 (run-control) ──────────────────────────────────
            // TODO(task-31): pause Supervisor dispatch. For now we only record
            // the Paused status; there is no in-flight work to actually pause.
            Command::PauseRun { run } => {
                self.require_run(run)?;
                {
                    let mut runs = self.runs.lock().expect("runs registry mutex poisoned");
                    if let Some(entry) = runs.get_mut(&run.0) {
                        entry.status = RunStatus::Paused;
                    }
                } // guard dropped before broadcast
                let _ = self.event_tx.send(Event::RunStatusChanged {
                    run,
                    status: RunStatus::Paused,
                });
                Ok(CommandOutcome::Acknowledged)
            }

            // ── Seam: task 31 (run-control) ──────────────────────────────────
            // TODO(task-31): cancel in-flight agents + release resources. For now
            // we simply drop the Run from the registry (no agents are running).
            Command::CancelRun { run } => {
                let removed = {
                    let mut runs = self.runs.lock().expect("runs registry mutex poisoned");
                    runs.remove(&run.0).is_some()
                }; // guard dropped before returning
                if removed {
                    Ok(CommandOutcome::Acknowledged)
                } else {
                    Err(ApiError::UnknownRun { run })
                }
            }
        }
    }

    /// Snapshot all open Runs in ascending `RunId` (insertion) order.
    async fn runs(&self) -> Vec<RunView> {
        let runs = self.runs.lock().expect("runs registry mutex poisoned");
        runs.iter()
            .map(|(id, entry)| entry.to_view(RunId(*id)))
            .collect()
    }

    /// Snapshot a single Run, or `None` for an unknown id.
    async fn run(&self, id: RunId) -> Option<RunView> {
        let runs = self.runs.lock().expect("runs registry mutex poisoned");
        runs.get(&id.0).map(|entry| entry.to_view(id))
    }

    /// Subscribe to the live event stream.
    ///
    /// Wraps a fresh `broadcast::Receiver` in a [`BroadcastStream`] and maps away
    /// `Lagged` errors so the returned [`EventStream`] stays infallible
    /// (`Item = Event`), as the trait contract requires.  A lagging consumer
    /// silently skips the dropped events rather than seeing a transport error.
    fn subscribe(&self) -> EventStream {
        let rx = self.event_tx.subscribe();
        let stream = BroadcastStream::new(rx).filter_map(|result| result.ok());
        Box::pin(stream)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependency::EdgeInferrer;
    use crate::interpreter::StructuredTextInterpreter;
    // `next()` comes from `tokio_stream::StreamExt`, already in scope via the
    // glob import above (the orchestrator uses it for the broadcast stream).
    use std::sync::Arc;
    use std::time::Duration;

    /// A small, valid structured-text task list used by the OpenRun tests.
    ///
    /// Two tasks under one section; `task-two` explicitly depends on `task-one`.
    /// Both mention `` `lib.rs` `` so the `EdgeInferrer` would also infer the
    /// same edge — exercising the real interpreter + decorator path.
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

    /// Build a `CoreApi` over the deterministic interpreter wrapped in the
    /// `EdgeInferrer` decorator — exactly the wiring `main.rs` uses.
    fn deterministic_core_api() -> CoreApi {
        let interpreter = Arc::new(EdgeInferrer::new(
            Arc::new(StructuredTextInterpreter::new()),
        ));
        CoreApi::new(interpreter)
    }

    /// Write `contents` to a uniquely-named markdown file in a fresh tempdir and
    /// return both (keep the dir alive for the test's lifetime).
    fn write_task_list(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("sample-feature.md");
        std::fs::write(&path, contents).expect("write task list");
        (dir, path)
    }

    // ── Core of the done-when: OpenRun creates a real Run ─────────────────────

    /// **Acceptance (done-when): selecting a task list triggers interpretation
    /// and creates a Run.**
    ///
    /// Writes a sample task-list markdown to a tempfile, executes `OpenRun`, and
    /// asserts: a `RunId` is returned, `runs()`/`run()` expose the Run with the
    /// interpreted tasks (real `StructuredTextInterpreter` + `EdgeInferrer`), the
    /// Run starts `Pending`, and a `RunOpened` event was broadcast.
    #[tokio::test]
    async fn open_run_interprets_file_and_creates_run() {
        let api = deterministic_core_api();
        let (_dir, path) = write_task_list(SAMPLE_TASK_LIST);

        // Subscribe BEFORE executing so we capture the RunOpened broadcast.
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

        // runs() exposes the new Run with the interpreted tasks.
        let all = api.runs().await;
        assert_eq!(all.len(), 1, "exactly one run should be open");
        let view = &all[0];
        assert_eq!(view.id, run_id);
        assert_eq!(view.task_list_path, path);
        assert_eq!(view.status, RunStatus::Pending, "new run starts Pending");
        assert_eq!(view.tasks.len(), 2, "both tasks must be interpreted");

        // The interpreted task identities + the explicit dependency edge survive
        // the domain → view projection.
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

        // run(id) returns the same view; an unknown id returns None.
        let single = api.run(run_id).await.expect("run(id) must find the run");
        assert_eq!(single.id, run_id);
        assert!(api.run(RunId(999)).await.is_none());

        // A RunOpened event was broadcast for this run.
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

    /// Each `OpenRun` gets a distinct, monotonically increasing `RunId`, and the
    /// registry tracks them independently in insertion order.
    #[tokio::test]
    async fn multiple_open_runs_get_distinct_ids() {
        let api = deterministic_core_api();
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
        // Insertion order preserved.
        assert_eq!(all[0].id, RunId(1));
        assert_eq!(all[1].id, RunId(2));
    }

    // ── Error paths ───────────────────────────────────────────────────────────

    /// Opening a path that does not exist returns a clear `ApiError`.
    #[tokio::test]
    async fn open_run_with_missing_file_returns_error() {
        let api = deterministic_core_api();
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
        // No run should have been registered.
        assert!(api.runs().await.is_empty());
    }

    /// A file that exists but does not conform to the structured-text convention
    /// surfaces as a clear `ApiError` (interpretation failure), and no Run is
    /// created.
    #[tokio::test]
    async fn open_run_with_invalid_content_returns_error() {
        let api = deterministic_core_api();
        // A task with a dangling dependency fails interpretation/validation.
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

    // ── Query semantics ───────────────────────────────────────────────────────

    /// `runs()` is empty before any Run is opened; `run(id)` returns `None`.
    #[tokio::test]
    async fn queries_empty_before_any_open() {
        let api = deterministic_core_api();
        assert!(api.runs().await.is_empty());
        assert!(api.run(RunId(1)).await.is_none());
    }

    // ── Start/Pause/Cancel seams (task 31) ────────────────────────────────────

    /// The `StartRun` seam records `Running` and broadcasts the status change,
    /// but does NOT execute the run (task 31).  An unknown id is rejected.
    #[tokio::test]
    async fn start_run_seam_records_status_without_executing() {
        let api = deterministic_core_api();
        let (_dir, path) = write_task_list(SAMPLE_TASK_LIST);
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

        let outcome = api.execute(Command::StartRun { run }).await.unwrap();
        assert!(matches!(outcome, CommandOutcome::Acknowledged));
        // Status reflects intent…
        assert_eq!(api.run(run).await.unwrap().status, RunStatus::Running);
        // …but the tasks were NOT driven anywhere (still New, no agents ran).
        for task in api.run(run).await.unwrap().tasks {
            assert_eq!(
                task.state,
                crate::api::TaskState::New,
                "no task should have advanced — execution is a task-31 seam"
            );
        }

        // Unknown run id is rejected.
        let err = api.execute(Command::StartRun { run: RunId(999) }).await;
        assert!(matches!(err, Err(ApiError::UnknownRun { run: RunId(999) })));
    }

    /// The `CancelRun` seam removes the Run from the registry; an unknown id is
    /// rejected with `UnknownRun`.
    #[tokio::test]
    async fn cancel_run_seam_removes_run() {
        let api = deterministic_core_api();
        let (_dir, path) = write_task_list(SAMPLE_TASK_LIST);
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

        api.execute(Command::CancelRun { run }).await.unwrap();
        assert!(api.run(run).await.is_none(), "cancelled run must be gone");
        assert!(api.runs().await.is_empty());

        let err = api.execute(Command::CancelRun { run }).await;
        assert!(matches!(err, Err(ApiError::UnknownRun { .. })));
    }

    /// `subscribe()` returns an independent stream per call, and lag is mapped
    /// away (the stream yields plain `Event`, never an error).
    #[tokio::test]
    async fn subscribe_is_independent_and_infallible() {
        let api = deterministic_core_api();
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
