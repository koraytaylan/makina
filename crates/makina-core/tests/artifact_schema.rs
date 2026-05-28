//! Integration test: the canonical sample artifact round-trips through
//! `TaskGraph` deserialization and passes `TaskGraph::validate()`.
//!
//! This test loads the committed sample from
//! `docs/spec/examples/sample-run.tasks.json` via `include_str!`, which
//! resolves the path at compile time relative to this source file.

use makina_core::task::{TaskGraph, TaskId, TaskState};

/// The canonical sample artifact embedded at compile time.
const SAMPLE_JSON: &str = include_str!("../../../docs/spec/examples/sample-run.tasks.json");

/// Parse the sample artifact and confirm it deserializes without error.
#[test]
fn sample_artifact_deserializes() {
    let graph: TaskGraph = serde_json::from_str(SAMPLE_JSON)
        .expect("sample-run.tasks.json must deserialize into TaskGraph");

    assert_eq!(graph.slug, "sample-run", "slug must match file stem");
    assert_eq!(graph.tasks.len(), 5, "sample must contain exactly 5 tasks");
}

/// `TaskGraph::validate()` must return `Ok` for the sample artifact.
#[test]
fn sample_artifact_passes_validate() {
    let graph: TaskGraph = serde_json::from_str(SAMPLE_JSON).expect("deserialization must succeed");

    graph
        .validate()
        .expect("sample-run.tasks.json must pass TaskGraph::validate()");
}

/// The `in-progress` task must parse its state correctly and have non-zero
/// `gate_iterations`, confirming that the iteration-count field survives
/// the round-trip.
#[test]
fn in_progress_task_has_correct_state_and_gate_iterations() {
    let graph: TaskGraph = serde_json::from_str(SAMPLE_JSON).expect("deserialization must succeed");

    let id = TaskId::new("agent-backend-trait");
    let task = graph
        .get(&id)
        .expect("agent-backend-trait must be present in the sample");

    assert_eq!(
        task.state,
        TaskState::InProgress,
        "agent-backend-trait state must be InProgress"
    );
    assert!(
        task.gate_iterations > 0,
        "agent-backend-trait gate_iterations must be > 0 in the sample"
    );
}

/// A `depends_on` edge must resolve via `TaskGraph::get()`.
#[test]
fn depends_on_edge_resolves_via_get() {
    let graph: TaskGraph = serde_json::from_str(SAMPLE_JSON).expect("deserialization must succeed");

    // `core-api-surface` depends on `workspace-scaffold`.
    let id = TaskId::new("core-api-surface");
    let task = graph.get(&id).expect("core-api-surface must be present");

    assert!(
        !task.depends_on.is_empty(),
        "core-api-surface must have at least one dependency"
    );

    for dep_id in &task.depends_on {
        let resolved = graph.get(dep_id);
        assert!(
            resolved.is_some(),
            "dependency '{dep_id}' of core-api-surface must resolve in the graph"
        );
    }
}

/// The `done` task must have both `started_at` and `finished_at` present.
#[test]
fn done_task_has_start_and_finish_timestamps() {
    let graph: TaskGraph = serde_json::from_str(SAMPLE_JSON).expect("deserialization must succeed");

    let id = TaskId::new("workspace-scaffold");
    let task = graph.get(&id).expect("workspace-scaffold must be present");

    assert_eq!(
        task.state,
        TaskState::Done,
        "workspace-scaffold must be Done"
    );
    assert!(task.started_at.is_some(), "done task must have started_at");
    assert!(
        task.finished_at.is_some(),
        "done task must have finished_at"
    );
}

/// Optional fields (`section`, `started_at`, `finished_at`) are absent (not
/// `null`) on tasks that have not reached those lifecycle points.
#[test]
fn optional_fields_absent_on_new_task() {
    let graph: TaskGraph = serde_json::from_str(SAMPLE_JSON).expect("deserialization must succeed");

    let id = TaskId::new("planner-actor");
    let task = graph.get(&id).expect("planner-actor must be present");

    assert_eq!(task.state, TaskState::New, "planner-actor must be New");
    assert!(
        task.section.is_none(),
        "planner-actor section must be None (omitted in JSON)"
    );
    assert!(
        task.started_at.is_none(),
        "planner-actor started_at must be None (omitted in JSON)"
    );
    assert!(
        task.finished_at.is_none(),
        "planner-actor finished_at must be None (omitted in JSON)"
    );
}

/// The `ready` task must have state `Ready` and no `started_at`.
#[test]
fn ready_task_has_correct_state_and_no_started_at() {
    let graph: TaskGraph = serde_json::from_str(SAMPLE_JSON).expect("deserialization must succeed");

    let id = TaskId::new("core-api-surface");
    let task = graph.get(&id).expect("core-api-surface must be present");

    assert_eq!(
        task.state,
        TaskState::Ready,
        "core-api-surface must be Ready"
    );
    assert!(
        task.started_at.is_none(),
        "ready task must not yet have started_at"
    );
}

/// The `ready` rule holds: every `ready` task has all deps in `done` state.
#[test]
fn ready_tasks_have_all_deps_done() {
    let graph: TaskGraph = serde_json::from_str(SAMPLE_JSON).expect("deserialization must succeed");

    for task in &graph.tasks {
        if task.state == TaskState::Ready {
            for dep_id in &task.depends_on {
                let dep = graph.get(dep_id).expect("dependency must resolve in graph");
                assert_eq!(
                    dep.state,
                    TaskState::Done,
                    "ready task '{}' has dep '{}' in state {:?}, expected Done",
                    task.id,
                    dep_id,
                    dep.state
                );
            }
        }
    }
}

/// The diamond dependency: `planner-actor` depends on both `task-model` and
/// `core-api-surface`; since `core-api-surface` is `ready` (not `done`),
/// `planner-actor` correctly remains `new`.
#[test]
fn diamond_dep_keeps_planner_actor_new() {
    let graph: TaskGraph = serde_json::from_str(SAMPLE_JSON).expect("deserialization must succeed");

    let id = TaskId::new("planner-actor");
    let task = graph.get(&id).expect("planner-actor must be present");

    assert_eq!(task.state, TaskState::New, "planner-actor must be New");
    assert_eq!(
        task.depends_on.len(),
        2,
        "planner-actor must have exactly 2 dependencies (diamond)"
    );

    // Confirm at least one dep is not done (which is why it stays new).
    let any_not_done = task.depends_on.iter().any(|dep_id| {
        graph
            .get(dep_id)
            .map(|d| d.state != TaskState::Done)
            .unwrap_or(false)
    });
    assert!(
        any_not_done,
        "planner-actor should have at least one dep not yet done, keeping it new"
    );
}
