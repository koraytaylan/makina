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
    assert_eq!(graph.tasks.len(), 4, "sample must contain exactly 4 tasks");
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

    let task_model_id = TaskId::new("task-model");
    let task = graph
        .get(&task_model_id)
        .expect("task-model must be present in the sample");

    assert_eq!(
        task.state,
        TaskState::InProgress,
        "task-model state must be InProgress"
    );
    assert!(
        task.gate_iterations > 0,
        "task-model gate_iterations must be > 0 in the sample"
    );
}

/// A `depends_on` edge must resolve via `TaskGraph::get()`.
#[test]
fn depends_on_edge_resolves_via_get() {
    let graph: TaskGraph = serde_json::from_str(SAMPLE_JSON).expect("deserialization must succeed");

    // `runtime-artifact-schema` depends on `task-model`.
    let ras_id = TaskId::new("runtime-artifact-schema");
    let ras = graph
        .get(&ras_id)
        .expect("runtime-artifact-schema must be present");

    assert!(
        !ras.depends_on.is_empty(),
        "runtime-artifact-schema must have at least one dependency"
    );

    for dep_id in &ras.depends_on {
        let resolved = graph.get(dep_id);
        assert!(
            resolved.is_some(),
            "dependency '{dep_id}' of runtime-artifact-schema must resolve in the graph"
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

    let id = TaskId::new("runtime-artifact-schema");
    let task = graph
        .get(&id)
        .expect("runtime-artifact-schema must be present");

    assert_eq!(
        task.state,
        TaskState::Ready,
        "runtime-artifact-schema must be Ready"
    );
    assert!(
        task.started_at.is_none(),
        "ready task must not yet have started_at"
    );
}
