//! End-to-end integration test: task lifecycle New → Done via the FSM and NoopBackend.
//!
//! This test is the acceptance criterion for task 12 (testing-harness).
//!
//! It uses the helpers in `common/mod.rs` to:
//! 1. Drive a task through the happy path (with one gate self-loop) to `Done`.
//! 2. Assert the exact state path visited.
//! 3. Assert that `recorded_prompts()` shows the developer and reviewer prompts
//!    were sent through the backend.
//!
//! A second test drives the reject-loop variant and verifies its state path and prompts.

mod common;

use makina_core::backend::noop::NoopBackend;
use makina_core::state_machine::is_terminal;
use makina_core::task::TaskState;

// ── Happy-path test ───────────────────────────────────────────────────────────

/// Drive a task from `New` to `Done` through the real `transition` function,
/// using `NoopBackend` for developer and reviewer turns.
///
/// # Asserted path
///
/// ```text
/// New -> Ready -> InProgress -> InProgress -> InReview -> Done
/// ```
/// (The second `InProgress` is the gate self-loop.)
///
/// # Backend participation
///
/// Two prompts are recorded:
/// 1. `"develop: test-task-01"` — the developer turn.
/// 2. `"review: test-task-01"` — the reviewer turn.
#[tokio::test]
async fn task_lifecycle_new_to_done_with_gate_self_loop() {
    // Arrange: a NoopBackend with two canned responses (developer + reviewer).
    let backend = common::noop_backend();

    // Act: drive the task through the FSM.
    let path = common::drive_task_to_done("test-task-01", &backend).await;

    // Assert: final state is Done (terminal).
    let final_state = *path.last().expect("path must be non-empty");
    assert_eq!(final_state, TaskState::Done, "final state must be Done");
    assert!(
        is_terminal(final_state),
        "Done must be a terminal state per is_terminal()"
    );

    // Assert: the exact path visited — including the gate self-loop.
    let expected_path = vec![
        TaskState::New,
        TaskState::Ready,
        TaskState::InProgress,
        TaskState::InProgress, // GateFailed self-loop
        TaskState::InReview,
        TaskState::Done,
    ];
    assert_eq!(
        path, expected_path,
        "state path must match the expected lifecycle sequence"
    );

    // Assert: the noop backend recorded exactly the prompts the simulator sent,
    // in the correct order.
    let prompts = backend.recorded_prompts();
    assert_eq!(
        prompts.len(),
        2,
        "exactly two prompts should have been sent (developer + reviewer)"
    );
    assert_eq!(
        prompts[0], "develop: test-task-01",
        "first prompt must be the developer turn"
    );
    assert_eq!(
        prompts[1], "review: test-task-01",
        "second prompt must be the reviewer turn"
    );
}

// ── Reject-loop test ──────────────────────────────────────────────────────────

/// Drive a task through a reviewer-reject loop (one rejection, then approval).
///
/// # Asserted path
///
/// ```text
/// New -> Ready -> InProgress -> InReview -> InProgress -> InReview -> Done
/// ```
///
/// # Backend participation
///
/// Four prompts are recorded in order:
/// 1. `"develop: reject-loop-task (attempt 1)"`
/// 2. `"review: reject-loop-task (attempt 1)"`  (leads to rejection)
/// 3. `"develop: reject-loop-task (attempt 2)"`  (re-work)
/// 4. `"review: reject-loop-task (attempt 2)"`  (leads to approval)
#[tokio::test]
async fn task_lifecycle_with_reviewer_reject_loop() {
    // Arrange: a backend with four canned responses (two dev turns + two review turns).
    let backend = NoopBackend::with_responses(vec![
        "first developer output".into(),
        "reviewer rejects".into(),
        "second developer output".into(),
        "reviewer approved".into(),
    ]);

    // Act: drive the task through the FSM with the reject loop.
    let path = common::drive_task_with_reject_loop("reject-loop-task", &backend).await;

    // Assert: final state is Done.
    let final_state = *path.last().expect("path must be non-empty");
    assert_eq!(final_state, TaskState::Done, "final state must be Done");

    // Assert: the exact path — one reject loop, then approved.
    let expected_path = vec![
        TaskState::New,
        TaskState::Ready,
        TaskState::InProgress,
        TaskState::InReview,
        TaskState::InProgress, // ReviewerRejected loop back
        TaskState::InReview,   // re-entered review after re-work
        TaskState::Done,
    ];
    assert_eq!(
        path, expected_path,
        "reject-loop path must match the expected sequence"
    );

    // Assert: four prompts were recorded in the correct order.
    let prompts = backend.recorded_prompts();
    assert_eq!(
        prompts.len(),
        4,
        "four prompts should have been sent (2 dev + 2 review)"
    );
    assert!(
        prompts[0].contains("develop") && prompts[0].contains("attempt 1"),
        "first prompt must be the first developer turn"
    );
    assert!(
        prompts[1].contains("review") && prompts[1].contains("attempt 1"),
        "second prompt must be the first reviewer turn"
    );
    assert!(
        prompts[2].contains("develop") && prompts[2].contains("attempt 2"),
        "third prompt must be the second developer turn"
    );
    assert!(
        prompts[3].contains("review") && prompts[3].contains("attempt 2"),
        "fourth prompt must be the second reviewer turn"
    );
}

// ── Builder smoke tests ───────────────────────────────────────────────────────

/// Verify that `sample_task` and `sample_graph` builders produce well-formed
/// domain objects that pass `TaskGraph::validate()`.
#[test]
fn sample_builders_produce_valid_graph() {
    use makina_core::task::TaskId;

    let task_a = common::sample_task("alpha", vec![]);
    let task_b = common::sample_task("beta", vec!["alpha"]);
    let graph = common::sample_graph("smoke-test", vec![task_a, task_b]);

    graph
        .validate()
        .expect("sample_graph with valid deps must pass TaskGraph::validate");

    assert_eq!(graph.slug, "smoke-test");
    assert_eq!(graph.tasks.len(), 2);
    assert_eq!(graph.tasks[1].depends_on, vec![TaskId::new("alpha")]);
}

/// `sample_task` starts in `New` state with zero iteration counters.
#[test]
fn sample_task_starts_new_with_zero_counters() {
    let task = common::sample_task("counter-check", vec![]);
    assert_eq!(task.state, TaskState::New);
    assert_eq!(task.gate_iterations, 0);
    assert_eq!(task.review_iterations, 0);
    assert!(task.started_at.is_none());
    assert!(task.finished_at.is_none());
}
