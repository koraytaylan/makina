//! Shared integration-test helpers for `makina-core`.
//!
//! Include this module in any integration test file with:
//! ```ignore
//! mod common;
//! ```
//!
//! # Contents
//!
//! - **Builders**: [`sample_task`], [`sample_graph`], [`noop_backend`] — construct
//!   well-formed domain objects for use in assertions without boilerplate.
//!
//! - **Lifecycle-driver simulator**: [`drive_task_to_done`] and
//!   [`drive_task_with_reject_loop`].  These are **test scaffolding** that simulate
//!   the orchestration loop using the FSM and `NoopBackend`.  They are NOT the
//!   production Supervisor loop — that is owned by **task 21 (develop-review-loop)**.
//!   Future integration tests (task 21+) may reuse these helpers or supersede them
//!   with real actor-based drivers.

use std::path::PathBuf;

use chrono::Utc;
use futures::StreamExt;

use makina_core::backend::noop::NoopBackend;
use makina_core::backend::{AgentBackend, Prompt, ResponseEvent, SessionConfig};
use makina_core::state_machine::{IllegalTransition, TaskEvent, transition};
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};

// ── Builders ──────────────────────────────────────────────────────────────────

/// Build a minimal [`Task`] with the given `id` and `deps`.
///
/// All timestamp fields are set to `Utc::now()`.  `state` starts at `New`,
/// iteration counters at `0`.
pub fn sample_task(id: impl Into<String>, deps: Vec<&str>) -> Task {
    let now = Utc::now();
    Task {
        id: TaskId::new(id),
        title: "Sample task".to_string(),
        description: "A task used by integration tests.".to_string(),
        done_when: "The test asserts the expected final state.".to_string(),
        depends_on: deps.into_iter().map(TaskId::new).collect(),
        section: None,
        state: TaskState::New,
        gate_iterations: 0,
        review_iterations: 0,
        created_at: now,
        updated_at: now,
        started_at: None,
        finished_at: None,
        failure_reason: None,
    }
}

/// Build a [`TaskGraph`] with the given `slug` and list of `tasks`.
///
/// Does NOT call `validate()`; callers that need a validated graph should call
/// `graph.validate().unwrap()` themselves.
pub fn sample_graph(slug: impl Into<String>, tasks: Vec<Task>) -> TaskGraph {
    TaskGraph {
        slug: slug.into(),
        tasks,
    }
}

/// Build a [`NoopBackend`] pre-loaded with two canned responses:
/// 1. `"developer output"` — returned for the first (developer) prompt.
/// 2. `"reviewer approved"` — returned for the second (reviewer) prompt.
///
/// If a test exercises the reject loop, supply a longer response list via
/// `NoopBackend::with_responses` directly.
pub fn noop_backend() -> NoopBackend {
    NoopBackend::with_responses(vec!["developer output".into(), "reviewer approved".into()])
}

/// Build a [`SessionConfig`] suitable for test use.
///
/// Uses `/tmp/makina-test` as the working directory and a generic system prompt.
pub fn test_session_config() -> SessionConfig {
    SessionConfig {
        working_dir: PathBuf::from("/tmp/makina-test"),
        system_prompt: "You are a test agent.".to_string(),
        mode: None,
        model: None,
        effort: None,
        extra: None,
        task_id: None,
        run_id: "test-run".into(),
    }
}

// ── Stream helpers ────────────────────────────────────────────────────────────

/// Drain a response stream, concatenating all [`ResponseEvent::TextChunk`] text
/// until [`ResponseEvent::TurnComplete`].
///
/// Panics on any `Err` item in the stream (test infrastructure; real code must handle errors).
pub async fn drain_response(stream: makina_core::backend::ResponseStream) -> String {
    let mut text = String::new();
    let mut events = stream;
    while let Some(item) = events.next().await {
        match item.expect("unexpected Err item in noop stream") {
            ResponseEvent::TextChunk { text: chunk } => text.push_str(&chunk),
            // Side-channel events do not contribute to the assembled answer.
            ResponseEvent::ThoughtChunk { .. }
            | ResponseEvent::ToolCall { .. }
            | ResponseEvent::ToolCallUpdate { .. }
            | ResponseEvent::CurrentModeUpdate { .. } => {}
            ResponseEvent::TurnComplete { .. } => break,
        }
    }
    text
}

// ── FSM step helper ───────────────────────────────────────────────────────────

/// Apply `event` to `current` using `state_machine::transition`, recording the
/// result state in `path`, and returning the new state.
///
/// Propagates `IllegalTransition` as a test failure via `unwrap`.
pub fn step(current: TaskState, event: TaskEvent, path: &mut Vec<TaskState>) -> TaskState {
    let next = transition(current, event)
        .unwrap_or_else(|e: IllegalTransition| panic!("FSM step failed: {e}"));
    path.push(next);
    next
}

// ── Lifecycle-driver simulator ────────────────────────────────────────────────

/// Drive a single task from `New` to `Done` through the FSM, using `backend`
/// for the developer and reviewer "turns".
///
/// # Path simulated (happy path with one gate self-loop)
///
/// ```text
/// New
///   --DependenciesSatisfied--> Ready
///   --Dispatched--> InProgress
///   --GateFailed--> InProgress   (one self-loop to exercise gate retry)
///   --GatesPassed--> InReview
///   --ReviewerApproved--> Done
/// ```
///
/// # Returns
///
/// The ordered list of states visited, including the initial `New`.  For the
/// happy path above:
/// `[New, Ready, InProgress, InProgress, InReview, Done]`
///
/// # Noop backend participation
///
/// - **Developer turn**: spawns a session, sends `"develop: <task_id>"`, drains
///   the stream.  The response is discarded; the point is to exercise the
///   `spawn` → `prompt` → `drain` → `terminate` lifecycle and record the prompt.
/// - **Reviewer turn**: spawns a second session, sends `"review: <task_id>"`,
///   drains the stream.  Again, the point is the recording, not the content.
///
/// Callers can assert `backend.recorded_prompts()` contains the expected texts.
///
/// # This is TEST SCAFFOLDING
///
/// This function simulates the orchestration loop for integration-test purposes
/// only.  The real loop (including real actor messaging, gate execution, and
/// state persistence) is implemented in **task 21 (develop-review-loop)** and
/// will supersede this simulator for production code.
pub async fn drive_task_to_done(task_id: &str, backend: &NoopBackend) -> Vec<TaskState> {
    let mut path = vec![TaskState::New];
    let mut state = TaskState::New;

    // New --DependenciesSatisfied--> Ready
    state = step(state, TaskEvent::DependenciesSatisfied, &mut path);
    assert_eq!(state, TaskState::Ready);

    // Ready --Dispatched--> InProgress
    state = step(state, TaskEvent::Dispatched, &mut path);
    assert_eq!(state, TaskState::InProgress);

    // Developer turn: spawn a noop session and send the develop prompt.
    {
        let mut session = backend
            .spawn(test_session_config())
            .await
            .expect("noop backend spawn must succeed");
        let stream = session
            .prompt(Prompt::new(format!("develop: {task_id}")))
            .await
            .expect("noop prompt must succeed");
        let response = drain_response(stream).await;
        assert!(
            !response.is_empty(),
            "developer turn must yield a non-empty response"
        );
        session.terminate().await.expect("terminate must succeed");
    }

    // InProgress --GateFailed--> InProgress  (one gate self-loop)
    state = step(state, TaskEvent::GateFailed, &mut path);
    assert_eq!(state, TaskState::InProgress);

    // InProgress --GatesPassed--> InReview
    state = step(state, TaskEvent::GatesPassed, &mut path);
    assert_eq!(state, TaskState::InReview);

    // Reviewer turn: spawn a noop session and send the review prompt.
    {
        let mut session = backend
            .spawn(test_session_config())
            .await
            .expect("noop backend spawn must succeed");
        let stream = session
            .prompt(Prompt::new(format!("review: {task_id}")))
            .await
            .expect("noop prompt must succeed");
        let response = drain_response(stream).await;
        assert!(
            !response.is_empty(),
            "reviewer turn must yield a non-empty response"
        );
        session.terminate().await.expect("terminate must succeed");
    }

    // InReview --ReviewerApproved--> Done
    state = step(state, TaskEvent::ReviewerApproved, &mut path);
    assert_eq!(state, TaskState::Done);

    path
}

/// Drive a task through the FSM including a reviewer-reject loop.
///
/// # Path simulated (one reject, then approve)
///
/// ```text
/// New
///   --DependenciesSatisfied--> Ready
///   --Dispatched--> InProgress
///   --GatesPassed--> InReview
///   --ReviewerRejected--> InProgress   (reject loop)
///   --GatesPassed--> InReview          (re-enter review after re-work)
///   --ReviewerApproved--> Done
/// ```
///
/// # Returns
///
/// `[New, Ready, InProgress, InReview, InProgress, InReview, Done]`
///
/// Two developer prompts and two reviewer prompts are sent through `backend`,
/// so `backend.recorded_prompts()` will have 4 entries in order.
///
/// # This is TEST SCAFFOLDING — see [`drive_task_to_done`].
pub async fn drive_task_with_reject_loop(task_id: &str, backend: &NoopBackend) -> Vec<TaskState> {
    let mut path = vec![TaskState::New];
    let mut state = TaskState::New;

    // New --DependenciesSatisfied--> Ready
    state = step(state, TaskEvent::DependenciesSatisfied, &mut path);

    // Ready --Dispatched--> InProgress
    state = step(state, TaskEvent::Dispatched, &mut path);

    // First developer turn
    {
        let mut session = backend
            .spawn(test_session_config())
            .await
            .expect("noop backend spawn must succeed");
        let stream = session
            .prompt(Prompt::new(format!("develop: {task_id} (attempt 1)")))
            .await
            .expect("noop prompt must succeed");
        drain_response(stream).await;
        session.terminate().await.expect("terminate must succeed");
    }

    // InProgress --GatesPassed--> InReview
    state = step(state, TaskEvent::GatesPassed, &mut path);

    // First reviewer turn — will be a reject
    {
        let mut session = backend
            .spawn(test_session_config())
            .await
            .expect("noop backend spawn must succeed");
        let stream = session
            .prompt(Prompt::new(format!("review: {task_id} (attempt 1)")))
            .await
            .expect("noop prompt must succeed");
        drain_response(stream).await;
        session.terminate().await.expect("terminate must succeed");
    }

    // InReview --ReviewerRejected--> InProgress  (reject loop)
    state = step(state, TaskEvent::ReviewerRejected, &mut path);
    assert_eq!(state, TaskState::InProgress);

    // Second developer turn (re-work)
    {
        let mut session = backend
            .spawn(test_session_config())
            .await
            .expect("noop backend spawn must succeed");
        let stream = session
            .prompt(Prompt::new(format!("develop: {task_id} (attempt 2)")))
            .await
            .expect("noop prompt must succeed");
        drain_response(stream).await;
        session.terminate().await.expect("terminate must succeed");
    }

    // InProgress --GatesPassed--> InReview  (re-enter review)
    state = step(state, TaskEvent::GatesPassed, &mut path);

    // Second reviewer turn — will approve
    {
        let mut session = backend
            .spawn(test_session_config())
            .await
            .expect("noop backend spawn must succeed");
        let stream = session
            .prompt(Prompt::new(format!("review: {task_id} (attempt 2)")))
            .await
            .expect("noop prompt must succeed");
        drain_response(stream).await;
        session.terminate().await.expect("terminate must succeed");
    }

    // InReview --ReviewerApproved--> Done
    state = step(state, TaskEvent::ReviewerApproved, &mut path);
    assert_eq!(state, TaskState::Done);

    path
}
