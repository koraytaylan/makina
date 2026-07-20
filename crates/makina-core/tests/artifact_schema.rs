//! Regression tests for the checkpoint-only sample artifact.
//!
//! Plan documents remain authoritative; this JSON contains only compatible
//! volatile scheduler and recovery state.

use std::path::PathBuf;

use makina_core::checkpoint::PlanCheckpoint;
use makina_core::task::TaskState;

const SAMPLE_JSON: &str = include_str!("../../../docs/spec/examples/sample-run.tasks.json");

fn sample() -> PlanCheckpoint {
    serde_json::from_str(SAMPLE_JSON)
        .expect("sample-run.tasks.json must deserialize into PlanCheckpoint")
}

#[test]
fn sample_is_a_versioned_plan_checkpoint() {
    let checkpoint = sample();
    assert_eq!(checkpoint.schema_version, 1);
    assert_eq!(
        checkpoint.identity.plan_dir,
        PathBuf::from("docs/plans/0048-per-task-plan-documents")
    );
    assert_eq!(checkpoint.identity.executable_digest.len(), 64);
    assert!(
        checkpoint
            .identity
            .executable_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
}

#[test]
fn identity_names_exact_ordered_tasks_and_source_paths() {
    let checkpoint = sample();
    assert_eq!(
        checkpoint.identity.task_ids,
        ["define-schema", "project-runtime"]
    );
    assert_eq!(
        checkpoint.identity.task_source_paths,
        [
            PathBuf::from("docs/plans/0048-per-task-plan-documents/tasks/0101-define-schema.md"),
            PathBuf::from("docs/plans/0048-per-task-plan-documents/tasks/0201-project-runtime.md"),
        ]
    );
    assert_eq!(
        checkpoint
            .tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        checkpoint
            .identity
            .task_ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
}

#[test]
fn tasks_contain_only_volatile_state_and_counters() {
    let checkpoint = sample();
    assert_eq!(checkpoint.tasks.len(), 2);
    assert_eq!(checkpoint.tasks[0].state, TaskState::Done);
    assert_eq!(checkpoint.tasks[1].state, TaskState::Ready);
    assert!(
        checkpoint
            .tasks
            .iter()
            .all(|task| task.gate_iterations == 0 && task.review_iterations == 0)
    );

    let value: serde_json::Value = serde_json::from_str(SAMPLE_JSON).unwrap();
    for task in value["tasks"].as_array().unwrap() {
        let mut keys = task
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(
            keys,
            ["gate_iterations", "id", "review_iterations", "state"]
        );
    }
}

#[test]
fn sample_contains_no_authored_narrative_dependencies_or_git_truth() {
    let value: serde_json::Value = serde_json::from_str(SAMPLE_JSON).unwrap();
    let forbidden = [
        "title",
        "description",
        "done_when",
        "depends_on",
        "gated",
        "touches",
        "status",
        "merged_as",
        "source_digest",
        "validation_base_oid",
        "landing_oid",
        "final_oid",
    ];
    fn assert_absent(value: &serde_json::Value, forbidden: &[&str]) {
        match value {
            serde_json::Value::Object(map) => {
                for key in forbidden {
                    assert!(
                        !map.contains_key(*key),
                        "checkpoint contains forbidden `{key}`"
                    );
                }
                for child in map.values() {
                    assert_absent(child, forbidden);
                }
            }
            serde_json::Value::Array(values) => {
                for child in values {
                    assert_absent(child, forbidden);
                }
            }
            _ => {}
        }
    }
    assert_absent(&value, &forbidden);
}

#[test]
fn sample_has_no_active_recovery_evidence_and_round_trips() {
    let checkpoint = sample();
    assert!(checkpoint.active_refs.is_empty());
    assert!(checkpoint.active_worktrees.is_empty());
    let encoded = serde_json::to_string(&checkpoint).unwrap();
    assert_eq!(
        serde_json::from_str::<PlanCheckpoint>(&encoded).unwrap(),
        checkpoint
    );
}
