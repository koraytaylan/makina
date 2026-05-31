//! TUI consumption surface: commands, queries, and the live event stream.
//!
//! This module defines **the only interface the TUI is allowed to depend on**.
//! All orchestration logic lives inside `makina-core`; the TUI issues
//! [`Command`]s, reads [`RunView`]s via [`Api::runs`] / [`Api::run`], and
//! subscribes to a live [`EventStream`] — nothing more.
//!
//! # Design principles
//!
//! * **Maximalist core, thin shell.** No orchestration logic leaks into the
//!   TUI.  The TUI receives canned snapshots (DTOs) and events; it never touches
//!   internal actors or the state machine directly.
//!
//! * **Decoupled from `backend` internals.** The view-level [`ExchangeEvent`]
//!   and [`AgentRole`] mirror *concepts* from [`crate::backend`] but are
//!   defined independently here.  `api` MUST NOT re-export or depend on
//!   `crate::backend` types in any public signature.
//!
//! * **Minimal surface.** Only commands, queries, and events that the TUI
//!   actually needs for the MVP (open / control a Run, observe its progress,
//!   and stream live agent exchanges).  No pagination, filtering, or auth.
//!
//! # Dependency direction
//!
//! ```text
//!   makina (TUI)
//!       └─ depends on ──► makina_core::api   (this module)
//!                                │
//!                                │   (no arrow back to backend or actors)
//!                         makina_core internals
//! ```

use std::path::PathBuf;
use std::pin::Pin;

use async_trait::async_trait;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};

// Re-export ingestion report types so TUI (and other consumers) can import them
// from `makina_core::api` alongside the other view types (RunView, TaskView, …).
pub use crate::ingestion::{IngestionIssue, IngestionReport, IssueSeverity, IssueSource};

// ── Identifier newtypes ───────────────────────────────────────────────────────

/// Opaque numeric identifier for an open Run.
///
/// A `RunId` is assigned by the orchestrator when a Run is opened via
/// [`Command::OpenRun`] and remains stable until the Run is dropped from
/// memory.  It is NOT persisted across process restarts; treat it as a
/// session-scoped handle.
///
/// Represented as a `u64` because monotonic counters are collision-free,
/// trivially comparable, and serde-friendly.  A UUID would be overkill for
/// an in-process session key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunId(pub u64);

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "run:{}", self.0)
    }
}

/// Identifier for an individual task within a Run.
///
/// Corresponds to the kebab-case slug used in `.tasks/{slug}.json` files
/// (e.g. `"core-api-surface"`, `"setup-workspace"`).  The orchestrator treats
/// these as opaque strings; uniqueness is enforced by the task-list file.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

impl TaskId {
    /// Convenience constructor.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Map the domain [`crate::task::TaskId`] onto the view-level [`TaskId`].
///
/// The two newtypes are intentionally distinct (see the type docs): the domain
/// type is the persisted source of truth, the view type is what the TUI sees.
/// This conversion is the single, documented bridge used by the orchestrator
/// when projecting a [`crate::task::TaskGraph`] into [`RunView`]s.
impl From<crate::task::TaskId> for TaskId {
    fn from(id: crate::task::TaskId) -> Self {
        TaskId(id.0)
    }
}

impl From<&crate::task::TaskId> for TaskId {
    fn from(id: &crate::task::TaskId) -> Self {
        TaskId(id.0.clone())
    }
}

// ── View / DTO types ──────────────────────────────────────────────────────────

/// The lifecycle state of a single task, as seen by the TUI.
///
/// This is a **view mirror** of the internal task state machine
/// ([`crate::task::TaskState`]).  The `From<crate::task::TaskState>` conversion
/// below is the documented bridge from the domain enum to this view enum.
///
/// # State meanings
///
/// | Variant | Meaning |
/// |---------|---------|
/// | `New` | Registered but not yet ready (unresolved dependencies). |
/// | `Ready` | All dependencies satisfied; waiting for an available agent slot. |
/// | `InProgress` | A Developer agent is actively working on this task. |
/// | `InReview` | Implementation complete; a Reviewer agent is checking the work. |
/// | `Done` | Accepted by the Reviewer (or auto-approved). |
/// | `Failed` | Permanently failed after exhausting retry / gate limits. |
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Registered; waiting for dependencies to become [`TaskState::Done`].
    New,
    /// All dependencies satisfied; eligible to be picked up by a Developer.
    Ready,
    /// A Developer agent is actively working.
    InProgress,
    /// Developer finished; a Reviewer agent is evaluating the output.
    InReview,
    /// Task accepted — work complete.
    Done,
    /// Task permanently failed (gate/review limit exceeded or fatal error).
    Failed,
    /// A prerequisite failed, so the task was never run.
    Skipped,
}

/// Map the domain [`crate::task::TaskState`] onto the view-level [`TaskState`].
///
/// The two enums mirror each other variant-for-variant.  This is the single,
/// documented bridge the orchestrator uses when projecting a
/// [`crate::task::TaskGraph`] into [`RunView`]s; keeping the view enum separate
/// preserves the rule that the TUI never depends on domain types directly.
impl From<crate::task::TaskState> for TaskState {
    fn from(state: crate::task::TaskState) -> Self {
        use crate::task::TaskState as Domain;
        match state {
            Domain::New => TaskState::New,
            Domain::Ready => TaskState::Ready,
            Domain::InProgress => TaskState::InProgress,
            Domain::InReview => TaskState::InReview,
            Domain::Done => TaskState::Done,
            Domain::Failed => TaskState::Failed,
            Domain::Skipped => TaskState::Skipped,
        }
    }
}

/// A snapshot of one task suitable for rendering in the TUI.
///
/// This is a pure DTO; it holds no behaviour.  The TUI renders `state` as a
/// coloured status badge, `gate_iterations` / `review_iterations` as counters,
/// and `depends_on` to draw a dependency graph or indent tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskView {
    /// Unique kebab-case identifier within the Run.
    pub id: TaskId,

    /// Human-readable task title (taken verbatim from the task-list file).
    pub title: String,

    /// Current lifecycle state of the task.
    pub state: TaskState,

    /// Number of times this task has cycled through the Developer → gate →
    /// failed-gate loop.  Displayed as a "retries" counter.
    pub gate_iterations: u32,

    /// Number of times this task has cycled through the Developer → Reviewer →
    /// changes-requested loop.  Displayed as a "reviews" counter.
    pub review_iterations: u32,

    /// IDs of tasks that must reach [`TaskState::Done`] before this task
    /// becomes [`TaskState::Ready`].
    pub depends_on: Vec<TaskId>,
}

/// Aggregate status of a Run from the TUI's perspective.
///
/// The orchestrator derives this by inspecting the [`TaskState`] of every task
/// in the Run:
///
/// * **`Pending`** — Run was opened but [`Command::StartRun`] has not been issued yet.
/// * **`Running`** — At least one task is [`TaskState::InProgress`] or
///   [`TaskState::InReview`]; the orchestrator is actively dispatching agents.
/// * **`Paused`** — [`Command::PauseRun`] was issued; no new agents will be
///   dispatched until [`Command::StartRun`] resumes the Run.
/// * **`Completed`** — All tasks have reached [`TaskState::Done`].
/// * **`Failed`** — At least one task has reached [`TaskState::Failed`] and no
///   tasks are in progress; the run cannot make further progress automatically.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Opened but not yet started.
    Pending,
    /// Actively running — agents are being dispatched.
    Running,
    /// Explicitly paused; resumable via [`Command::StartRun`].
    Paused,
    /// All tasks completed successfully.
    Completed,
    /// One or more tasks permanently failed; run is stalled.
    Failed,
}

/// A snapshot of one Run suitable for rendering in the TUI sidebar and detail
/// panel.
///
/// The `tasks` field embeds the full task list because the TUI always renders
/// all tasks for the focused Run.  For Runs that are not focused, the TUI may
/// use a leaner summary — but that optimisation is deferred; for MVP a single
/// `RunView` with all tasks is sufficient and keeps the API surface small.
///
/// A separate `Api::run(id)` query returns `None` for an unknown `id` rather
/// than an error, following the query-vs-command separation: unknown reads
/// return absence, not failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunView {
    /// Stable handle for this Run within the current session.
    pub id: RunId,

    /// Persistent, sortable run identity (26-char ULID string) minted when the
    /// Run is opened.  Unlike [`RunView::id`] (a session-scoped `u64` handle),
    /// the ULID's lexicographic order matches chronological order, so it is a
    /// stable key that survives across processes.
    pub run_uid: String,

    /// Path to the task-list file that backs this Run (e.g.
    /// `.tasks/my-feature.json`).  Displayed in the TUI title bar.
    pub task_list_path: PathBuf,

    /// The repo directory basename (the final path component of the worktree
    /// manager's `repo_root`).  Used by the TUI to render `{project}/{plan}`
    /// run labels.  Empty when the `repo_root` has no final component.
    pub project: String,

    /// Aggregate status derived from the task states.
    pub status: RunStatus,

    /// Ordered list of all tasks in this Run.  Order matches the task-list
    /// file; the TUI renders them in this order.
    pub tasks: Vec<TaskView>,

    /// Ingestion report (from validate + qualify) computed at `OpenRun` time
    /// and carried on the run.  Enables the TUI to render issues without
    /// re-scanning the source.
    pub report: IngestionReport,
}

// ── Commands ──────────────────────────────────────────────────────────────────

/// A TUI-issued command that mutates orchestrator state.
///
/// Commands follow a request–response pattern: the TUI calls
/// [`Api::execute`] and awaits a [`CommandOutcome`].  The command is applied
/// synchronously from the TUI's perspective (the future resolves only after
/// the orchestrator has acknowledged it).
///
/// # Adding new commands
///
/// Extend this enum and add a corresponding [`CommandOutcome`] variant.
/// Commands MUST NOT embed implementation detail; they carry only what the TUI
/// knows (paths, IDs, user intent).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Load a task-list file and create a new Run backed by it.
    ///
    /// The orchestrator reads `task_list_path`, parses the task list, assigns a
    /// fresh [`RunId`], and returns [`CommandOutcome::RunOpened`].  The Run
    /// starts in [`RunStatus::Pending`]; issue [`Command::StartRun`] to begin
    /// dispatching agents.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::InvalidCommand`] if `task_list_path` does not exist
    /// or cannot be parsed.
    OpenRun {
        /// Absolute or repo-relative path to the `.tasks/{slug}.json` file.
        task_list_path: PathBuf,
    },

    /// Transition a [`RunStatus::Pending`] or [`RunStatus::Paused`] Run to
    /// [`RunStatus::Running`], allowing the orchestrator to dispatch agents.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::UnknownRun`] if `run` does not identify an open Run.
    /// Returns [`ApiError::InvalidCommand`] if the Run is already running,
    /// completed, or failed.
    StartRun {
        /// The Run to start or resume.
        run: RunId,
    },

    /// Transition a [`RunStatus::Running`] Run to [`RunStatus::Paused`].
    ///
    /// In-flight agent turns are allowed to complete; no new agents will be
    /// dispatched until [`Command::StartRun`] is issued.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::UnknownRun`] if `run` does not identify an open Run.
    /// Returns [`ApiError::InvalidCommand`] if the Run is not currently running.
    PauseRun {
        /// The Run to pause.
        run: RunId,
    },

    /// Immediately cancel a Run, stopping all in-flight agents and releasing
    /// resources.
    ///
    /// The Run is removed from the orchestrator's open-run set; subsequent
    /// queries for this [`RunId`] will return `None`.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::UnknownRun`] if `run` does not identify an open Run.
    CancelRun {
        /// The Run to cancel.
        run: RunId,
    },

    /// Re-interpret a [`RunStatus::Pending`] Run from its source file,
    /// bypassing any persisted artifact.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::UnknownRun`] if `run` does not identify an open Run.
    /// Returns [`ApiError::InvalidCommand`] if the Run is not currently pending.
    ReinterpretRun {
        /// The Run to re-interpret.
        run: RunId,
    },
}

/// The successful outcome of a [`Command`] executed via [`Api::execute`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandOutcome {
    /// Returned by [`Command::OpenRun`].  The assigned [`RunId`] can be used
    /// for all subsequent commands and queries targeting this Run.
    RunOpened {
        /// The freshly assigned identifier for the opened Run.
        run: RunId,
    },

    /// Returned by commands that have no specific output to report
    /// ([`Command::StartRun`], [`Command::PauseRun`], [`Command::CancelRun`], [`Command::ReinterpretRun`]).
    ///
    /// The TUI should react to state changes via the [`EventStream`] rather
    /// than polling after receiving `Acknowledged`.
    Acknowledged,
}

/// Errors returned by [`Api::execute`].
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The specified [`RunId`] does not correspond to any open Run.
    ///
    /// The TUI should refresh its run list and discard the stale handle.
    #[error("unknown run: {run}")]
    UnknownRun {
        /// The unrecognised run identifier.
        run: RunId,
    },

    /// The command is not valid in the current state (e.g. starting a Run that
    /// is already running, or opening a task-list file that does not exist).
    ///
    /// `reason` carries a human-readable explanation suitable for display in
    /// the TUI status bar.
    #[error("invalid command: {reason}")]
    InvalidCommand {
        /// Human-readable explanation.
        reason: String,
    },

    /// An unexpected internal error occurred inside the orchestrator.
    ///
    /// The TUI should display this as a non-fatal error and allow the user to
    /// retry.  The `reason` string should be logged for diagnostics.
    #[error("internal error: {reason}")]
    Internal {
        /// Diagnostic description.
        reason: String,
    },
}

// ── Events ────────────────────────────────────────────────────────────────────

/// The role of the agent involved in an exchange.
///
/// This is a **view-level enum** that mirrors the Developer / Reviewer roles in
/// the internal actor model.  It is intentionally redefined here so that `api`
/// does not depend on internal actor types.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// The agent responsible for implementing the task.
    Developer,
    /// The agent responsible for reviewing the Developer's output.
    Reviewer,
}

/// A single event within a live agent prompt/answer exchange.
///
/// This is a **view-level enum** that mirrors `crate::backend::ResponseEvent`
/// plus the outgoing prompt.  It is defined independently here — `api` MUST NOT
/// import `backend` types in public signatures.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExchangeEvent {
    /// The orchestrator sent a prompt to the agent.
    ///
    /// The TUI renders this as the "user turn" in the exchange panel.
    PromptSent {
        /// Full text of the prompt that was dispatched.
        text: String,
    },

    /// The agent emitted a chunk of its response.
    ///
    /// The TUI accumulates chunks in order to build the full message, appending
    /// each chunk to the current assistant turn without waiting for
    /// [`ExchangeEvent::TurnComplete`].
    ResponseChunk {
        /// A fragment of the agent's response.  Never empty.
        text: String,
    },

    /// The agent has finished its current response turn.
    ///
    /// The TUI should finalise the current assistant message and prepare for
    /// the next [`ExchangeEvent::PromptSent`] event.
    TurnComplete,
}

/// An event emitted by the orchestrator and consumed by the TUI.
///
/// The TUI subscribes once via [`Api::subscribe`] and drives a render loop
/// from the resulting [`EventStream`].  All mutable state the TUI holds (run
/// list, task states, exchange text) MUST be derived from these events; the TUI
/// MUST NOT poll queries in a tight loop.
///
/// # Ordering guarantees
///
/// Events for the same Run are emitted in causal order.  Events across
/// different Runs may be interleaved arbitrarily.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A new Run was successfully opened.
    ///
    /// The TUI should add it to the sidebar and fetch an initial [`RunView`]
    /// via [`Api::run`] if it needs the full snapshot.
    RunOpened {
        /// The identifier of the newly opened Run.
        run: RunId,
        /// Path to the backing task-list file.
        task_list_path: PathBuf,
    },

    /// The aggregate status of a Run changed.
    ///
    /// Emitted when a Run transitions between [`RunStatus`] variants (e.g.
    /// Pending → Running, Running → Paused).
    RunStatusChanged {
        /// The Run whose status changed.
        run: RunId,
        /// The new aggregate status.
        status: RunStatus,
    },

    /// A task's lifecycle state changed.
    ///
    /// Emitted whenever a task advances (or regresses) in the state machine.
    /// The TUI should update the task's status badge without re-fetching the
    /// full [`RunView`].
    TaskStateChanged {
        /// The Run that contains the task.
        run: RunId,
        /// The task whose state changed.
        task: TaskId,
        /// The new state of the task.
        state: TaskState,
    },

    /// A task's iteration counters were updated.
    ///
    /// Emitted when `gate_iterations` or `review_iterations` is incremented.
    /// Kept as a separate event from [`Event::TaskStateChanged`] because
    /// counter updates and state transitions may arrive independently.
    TaskIterationsUpdated {
        /// The Run that contains the task.
        run: RunId,
        /// The task whose counters were updated.
        task: TaskId,
        /// Updated gate-iteration counter.
        gate_iterations: u32,
        /// Updated review-iteration counter.
        review_iterations: u32,
    },

    /// A live agent exchange event from any in-flight task/agent.
    ///
    /// The orchestrator emits `AgentExchange` events for **all** in-flight
    /// tasks and agents; it does **not** filter by any notion of "focus".
    /// Each event carries `run`, `task`, and `role` so that consumers can
    /// identify its source.
    ///
    /// Focus is a **presentation concern** that belongs in the TUI layer, not
    /// in this core API surface.  There is intentionally no focus command in
    /// this interface for the MVP.  The TUI (task 26, tui-scaffold) and the
    /// prompt/answer stream layer (task 30, prompt-answer-stream) MUST filter
    /// these events client-side based on whichever task the user is currently
    /// viewing.
    AgentExchange {
        /// The Run the exchange belongs to.
        run: RunId,
        /// The task whose agent is exchanging.
        task: TaskId,
        /// Which agent role is involved (Developer or Reviewer).
        role: AgentRole,
        /// The specific exchange event (prompt sent / chunk / turn complete).
        event: ExchangeEvent,
    },
}

// ── Stream type alias ─────────────────────────────────────────────────────────

/// A boxed, owned stream of [`Event`]s emitted by the orchestrator.
///
/// The TUI drives this stream from its render/event loop.  The stream is
/// infinite for as long as the orchestrator is running; it ends only when the
/// orchestrator shuts down.
///
/// # Infallibility — deliberate asymmetry with `backend::ResponseStream`
///
/// `EventStream` yields plain [`Event`] values, not `Result<Event, _>`.  This
/// is an intentional departure from `backend::ResponseStream`, which yields
/// `Result` because it models a fallible network/process boundary.
/// `EventStream` is the orchestrator's *outward-facing* view stream: internal
/// errors are absorbed by the orchestrator and surfaced as state changes (e.g.
/// a task transitioning to [`TaskState::Failed`] or a run reaching
/// [`RunStatus::Failed`]); the stream itself simply terminates on shutdown.
/// Consumers therefore do **not** need to handle transport errors on this
/// stream — a `None` from the stream means clean shutdown, not a failure.
///
/// Use `futures::StreamExt` combinators to consume the stream:
///
/// ```ignore
/// use futures::StreamExt;
/// let mut stream = api.subscribe();
/// while let Some(event) = stream.next().await {
///     // update TUI state …
/// }
/// ```
pub type EventStream = Pin<Box<dyn Stream<Item = Event> + Send>>;

// ── The Api trait ─────────────────────────────────────────────────────────────

/// The single interface the TUI depends on for all interaction with the
/// orchestrator.
///
/// # Object safety
///
/// `Api` is designed to be used as `Box<dyn Api>` (or `Arc<dyn Api>`) in the
/// TUI layer.  All async methods are annotated with `#[async_trait]` to
/// preserve object safety.
///
/// # Concurrency
///
/// All methods take `&self` (shared reference), meaning the TUI may call them
/// concurrently from multiple tasks if it chooses to.  Implementors MUST be
/// `Send + Sync`.
///
/// # Contract summary
///
/// | Method | Side effects | Error conditions |
/// |--------|-------------|-----------------|
/// | `execute` | Mutates orchestrator state | [`ApiError`] |
/// | `runs` | Read-only | None — empty `Vec` when no Runs open |
/// | `run` | Read-only | None — returns `None` for unknown id |
/// | `subscribe` | Returns a live stream | Stream ends at shutdown |
#[async_trait]
pub trait Api: Send + Sync {
    /// Issue a command to the orchestrator and await its acknowledgement.
    ///
    /// Commands are processed in order within the same `Api` instance.  The
    /// returned [`CommandOutcome`] confirms what the orchestrator did; side
    /// effects (state changes, new events) are reflected in the
    /// [`EventStream`].
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::UnknownRun`] if a command references a `RunId` that
    /// is not open.  Returns [`ApiError::InvalidCommand`] if the command is not
    /// valid in the current state.  Returns [`ApiError::Internal`] for
    /// unexpected failures.
    async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError>;

    /// Return a snapshot of all currently open Runs.
    ///
    /// Used by the TUI to populate the sidebar.  Returns an empty `Vec` when no
    /// Runs have been opened yet.  The order is stable within a session
    /// (insertion order of `OpenRun` commands).
    ///
    /// This is a point-in-time snapshot; the TUI SHOULD update its local state
    /// from the [`EventStream`] rather than polling this method repeatedly.
    async fn runs(&self) -> Vec<RunView>;

    /// Return a snapshot of a single Run, or `None` if `id` is unknown.
    ///
    /// Returns `None` (not an error) for an unknown [`RunId`] because the TUI
    /// may hold a stale reference that became invalid while the user was
    /// navigating.  The caller is responsible for handling the absent case
    /// gracefully (e.g. deselect the focused Run).
    async fn run(&self, id: RunId) -> Option<RunView>;

    /// Subscribe to the live event stream.
    ///
    /// Each call returns a **new independent stream** starting from the moment
    /// of the call.  Past events are NOT replayed.  The TUI typically calls
    /// this once at startup and fans out events to its render loop.
    ///
    /// The stream ends when the orchestrator shuts down.  Because [`EventStream`]
    /// is infallible (yields [`Event`], not `Result`), a `None` from the stream
    /// always means clean shutdown — consumers do not handle transport errors
    /// here.  See the [`EventStream`] type-alias doc for the full rationale.
    fn subscribe(&self) -> EventStream;
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Compile/contract proof for the `Api` trait and all supporting types.
    //!
    //! This is NOT a real implementation; it is a minimal inline stub that
    //! proves:
    //!   1. The trait is object-safe (`Box<dyn Api>` compiles).
    //!   2. All types derive the required traits (Clone, Debug, serde, etc.).
    //!   3. A straightforward stub can implement `Api`.
    //!   4. `execute` → `OpenRun` → `CommandOutcome::RunOpened` works end-to-end.
    //!   5. `runs()` and `run(id)` return sensible canned data.
    //!   6. `subscribe()` yields synthetic events that can be drained.

    use super::*;
    use futures::StreamExt;
    use futures::stream;
    use std::path::PathBuf;
    use std::sync::Mutex;

    // ── Stub implementation ───────────────────────────────────────────────────

    /// An in-test stub that returns canned data.
    ///
    /// State is a simple shared `Mutex<Vec<RunView>>` so that `execute` can
    /// insert a Run and subsequent `runs()` / `run()` calls can observe it.
    struct StubApi {
        runs: Mutex<Vec<RunView>>,
        next_id: Mutex<u64>,
    }

    impl StubApi {
        fn new() -> Self {
            Self {
                runs: Mutex::new(Vec::new()),
                next_id: Mutex::new(1),
            }
        }

        fn alloc_id(&self) -> RunId {
            let mut n = self.next_id.lock().unwrap();
            let id = RunId(*n);
            *n += 1;
            id
        }
    }

    #[async_trait]
    impl Api for StubApi {
        async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
            match command {
                Command::OpenRun { task_list_path } => {
                    let id = self.alloc_id();
                    let view = RunView {
                        id,
                        run_uid: String::new(),
                        task_list_path,
                        status: RunStatus::Pending,
                        project: String::new(),
                        tasks: vec![TaskView {
                            id: TaskId::new("stub-task"),
                            title: "Stub task".to_string(),
                            state: TaskState::New,
                            gate_iterations: 0,
                            review_iterations: 0,
                            depends_on: vec![],
                        }],
                        report: IngestionReport::default(),
                    };
                    self.runs.lock().unwrap().push(view);
                    Ok(CommandOutcome::RunOpened { run: id })
                }
                Command::StartRun { run } => {
                    let mut runs = self.runs.lock().unwrap();
                    let found = runs.iter_mut().find(|r| r.id == run);
                    match found {
                        Some(r) => {
                            r.status = RunStatus::Running;
                            Ok(CommandOutcome::Acknowledged)
                        }
                        None => Err(ApiError::UnknownRun { run }),
                    }
                }
                Command::PauseRun { run } => {
                    let mut runs = self.runs.lock().unwrap();
                    let found = runs.iter_mut().find(|r| r.id == run);
                    match found {
                        Some(r) => {
                            r.status = RunStatus::Paused;
                            Ok(CommandOutcome::Acknowledged)
                        }
                        None => Err(ApiError::UnknownRun { run }),
                    }
                }
                Command::CancelRun { run } => {
                    let mut runs = self.runs.lock().unwrap();
                    let before = runs.len();
                    runs.retain(|r| r.id != run);
                    if runs.len() < before {
                        Ok(CommandOutcome::Acknowledged)
                    } else {
                        Err(ApiError::UnknownRun { run })
                    }
                }
                Command::ReinterpretRun { run } => {
                    let runs = self.runs.lock().unwrap();
                    if runs.iter().any(|r| r.id == run) {
                        Ok(CommandOutcome::Acknowledged)
                    } else {
                        Err(ApiError::UnknownRun { run })
                    }
                }
            }
        }

        async fn runs(&self) -> Vec<RunView> {
            self.runs.lock().unwrap().clone()
        }

        async fn run(&self, id: RunId) -> Option<RunView> {
            self.runs
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.id == id)
                .cloned()
        }

        fn subscribe(&self) -> EventStream {
            // Emit two synthetic events and end the stream.
            let events = vec![
                Event::RunOpened {
                    run: RunId(1),
                    task_list_path: PathBuf::from(".tasks/stub.json"),
                },
                Event::TaskStateChanged {
                    run: RunId(1),
                    task: TaskId::new("stub-task"),
                    state: TaskState::InProgress,
                },
                Event::AgentExchange {
                    run: RunId(1),
                    task: TaskId::new("stub-task"),
                    role: AgentRole::Developer,
                    event: ExchangeEvent::PromptSent {
                        text: "Implement the feature.".to_string(),
                    },
                },
                Event::AgentExchange {
                    run: RunId(1),
                    task: TaskId::new("stub-task"),
                    role: AgentRole::Developer,
                    event: ExchangeEvent::ResponseChunk {
                        text: "Sure, working on it…".to_string(),
                    },
                },
                Event::AgentExchange {
                    run: RunId(1),
                    task: TaskId::new("stub-task"),
                    role: AgentRole::Developer,
                    event: ExchangeEvent::TurnComplete,
                },
            ];
            Box::pin(stream::iter(events))
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Drive the full lifecycle through `Box<dyn Api>` to prove object-safety.
    async fn open_run(api: &dyn Api, path: &str) -> RunId {
        let outcome = api
            .execute(Command::OpenRun {
                task_list_path: PathBuf::from(path),
            })
            .await
            .expect("OpenRun must succeed");
        match outcome {
            CommandOutcome::RunOpened { run } => run,
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn open_run_returns_run_opened_outcome() {
        let api: Box<dyn Api> = Box::new(StubApi::new());
        let id = open_run(api.as_ref(), ".tasks/feature.json").await;
        assert_eq!(id, RunId(1), "first run should get id 1");
    }

    #[tokio::test]
    async fn runs_query_reflects_opened_run() {
        let api: Box<dyn Api> = Box::new(StubApi::new());
        open_run(api.as_ref(), ".tasks/feature.json").await;

        let all = api.runs().await;
        assert_eq!(all.len(), 1, "one run should be open");
        assert_eq!(all[0].status, RunStatus::Pending);
        assert_eq!(all[0].task_list_path, PathBuf::from(".tasks/feature.json"));
    }

    #[tokio::test]
    async fn run_query_by_id_returns_correct_view() {
        let api: Box<dyn Api> = Box::new(StubApi::new());
        let id = open_run(api.as_ref(), ".tasks/feature.json").await;

        let view = api.run(id).await.expect("run should be found");
        assert_eq!(view.id, id);
        assert_eq!(view.tasks.len(), 1);
        assert_eq!(view.tasks[0].id, TaskId::new("stub-task"));
    }

    #[tokio::test]
    async fn run_query_unknown_id_returns_none() {
        let api: Box<dyn Api> = Box::new(StubApi::new());
        let result = api.run(RunId(999)).await;
        assert!(result.is_none(), "unknown RunId should return None");
    }

    #[tokio::test]
    async fn start_run_transitions_status_to_running() {
        let api: Box<dyn Api> = Box::new(StubApi::new());
        let id = open_run(api.as_ref(), ".tasks/feature.json").await;

        api.execute(Command::StartRun { run: id })
            .await
            .expect("StartRun must succeed");

        let view = api.run(id).await.expect("run should still exist");
        assert_eq!(view.status, RunStatus::Running);
    }

    #[tokio::test]
    async fn pause_run_transitions_status_to_paused() {
        let api: Box<dyn Api> = Box::new(StubApi::new());
        let id = open_run(api.as_ref(), ".tasks/feature.json").await;
        api.execute(Command::StartRun { run: id }).await.unwrap();
        api.execute(Command::PauseRun { run: id })
            .await
            .expect("PauseRun must succeed");

        let view = api.run(id).await.unwrap();
        assert_eq!(view.status, RunStatus::Paused);
    }

    #[tokio::test]
    async fn cancel_run_removes_run_from_list() {
        let api: Box<dyn Api> = Box::new(StubApi::new());
        let id = open_run(api.as_ref(), ".tasks/feature.json").await;

        api.execute(Command::CancelRun { run: id })
            .await
            .expect("CancelRun must succeed");

        assert!(api.run(id).await.is_none(), "cancelled run should be gone");
        assert!(api.runs().await.is_empty(), "runs list should be empty");
    }

    #[tokio::test]
    async fn unknown_run_returns_error() {
        let api: Box<dyn Api> = Box::new(StubApi::new());
        let result = api.execute(Command::StartRun { run: RunId(99) }).await;
        assert!(
            matches!(result, Err(ApiError::UnknownRun { run: RunId(99) })),
            "should return UnknownRun for non-existent id"
        );
    }

    #[tokio::test]
    async fn subscribe_yields_synthetic_events() {
        let api: Box<dyn Api> = Box::new(StubApi::new());
        let mut stream = api.subscribe();

        // Drain all events.
        let mut events: Vec<Event> = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }

        assert_eq!(events.len(), 5, "stub stream should yield exactly 5 events");

        // First event: RunOpened.
        assert!(
            matches!(&events[0], Event::RunOpened { run: RunId(1), .. }),
            "first event should be RunOpened for run 1"
        );

        // Second event: TaskStateChanged → InProgress.
        assert!(
            matches!(
                &events[1],
                Event::TaskStateChanged {
                    state: TaskState::InProgress,
                    ..
                }
            ),
            "second event should be TaskStateChanged to InProgress"
        );

        // Third event: AgentExchange PromptSent.
        assert!(
            matches!(
                &events[2],
                Event::AgentExchange {
                    role: AgentRole::Developer,
                    event: ExchangeEvent::PromptSent { .. },
                    ..
                }
            ),
            "third event should be AgentExchange PromptSent"
        );

        // Fourth event: ResponseChunk.
        assert!(
            matches!(
                &events[3],
                Event::AgentExchange {
                    event: ExchangeEvent::ResponseChunk { .. },
                    ..
                }
            ),
            "fourth event should be ResponseChunk"
        );

        // Fifth event: TurnComplete.
        assert!(
            matches!(
                &events[4],
                Event::AgentExchange {
                    event: ExchangeEvent::TurnComplete,
                    ..
                }
            ),
            "fifth event should be TurnComplete"
        );
    }

    #[tokio::test]
    async fn all_types_are_clone_and_debug() {
        // Exercises the derives rather than just relying on compile-time checks.
        let task = TaskView {
            id: TaskId::new("t1"),
            title: "Test task".to_string(),
            state: TaskState::InProgress,
            gate_iterations: 2,
            review_iterations: 1,
            depends_on: vec![TaskId::new("t0")],
        };
        let task2 = task.clone();
        let dbg = format!("{task2:?}");
        assert!(
            dbg.contains("InProgress"),
            "Debug should mention InProgress"
        );

        let run_view = RunView {
            id: RunId(42),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/x.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![task],
            report: IngestionReport::default(),
        };
        let _run2 = run_view.clone();
        assert!(format!("{run_view:?}").contains("Running"));

        let cmd = Command::OpenRun {
            task_list_path: PathBuf::from(".tasks/x.json"),
        };
        let _cmd2 = cmd.clone();

        let outcome = CommandOutcome::RunOpened { run: RunId(1) };
        let _outcome2 = outcome.clone();
    }

    #[tokio::test]
    async fn multiple_runs_tracked_independently() {
        let api: Box<dyn Api> = Box::new(StubApi::new());

        let id1 = open_run(api.as_ref(), ".tasks/a.json").await;
        let id2 = open_run(api.as_ref(), ".tasks/b.json").await;

        assert_ne!(id1, id2, "each run must receive a distinct id");
        assert_eq!(api.runs().await.len(), 2);

        api.execute(Command::CancelRun { run: id1 }).await.unwrap();
        assert_eq!(api.runs().await.len(), 1);
        assert!(api.run(id2).await.is_some(), "id2 should still be open");
    }
}
