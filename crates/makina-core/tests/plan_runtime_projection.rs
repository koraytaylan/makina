use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use chrono::Utc;
use makina_core::checkpoint::{
    CheckpointDisposition, CheckpointIdentity, PlanCheckpoint, RuntimeTaskCheckpoint,
    archive_clean_checkpoint, checkpoint_path, inspect_checkpoint, load_checkpoint,
    overlay_compatible, persist_checkpoint,
};
use makina_core::plan::{
    FilesystemPlanFileSource, PlanCandidate, PlanKey, PlanReservations, load_plan,
};
use makina_core::plan_runtime::ProjectedTaskGraph;
use makina_core::task::{
    AuthoredSeedOutcome, AuthoredTaskMetadata, Task, TaskGraph, TaskId, TaskState,
};

#[test]
fn projection_preserves_authored_identity_order_dependencies_and_metadata() {
    let (repo, plan) = load_fixture();
    let projected = ProjectedTaskGraph::from_document(&plan, Utc::now());
    assert_eq!(projected.plan_dir, plan.key.relative_dir);
    assert_eq!(projected.executable_digest, plan.executable_digest.as_str());
    assert_eq!(projected.graph.tasks.len(), plan.tasks.len());
    for (runtime, source) in projected.graph.tasks.iter().zip(&plan.tasks) {
        assert_eq!(runtime.id.0, source.frontmatter.id.as_str());
        assert_eq!(
            runtime
                .depends_on
                .iter()
                .map(|id| id.0.as_str())
                .collect::<Vec<_>>(),
            source
                .frontmatter
                .depends_on
                .iter()
                .map(|id| id.as_str())
                .collect::<Vec<_>>()
        );
        let authored = &projected.graph.authored[&runtime.id];
        assert_eq!(authored.source_path, source.source_path);
        assert_eq!(authored.gated, source.frontmatter.gated);
        assert_eq!(authored.touches.len(), source.frontmatter.touches.len());
    }
    drop(repo);
}

#[test]
fn compatible_checkpoint_overlays_volatile_state_but_never_json_done() {
    let (_repo, plan) = load_fixture();
    let mut projected = ProjectedTaskGraph::from_document(&plan, Utc::now());
    let id = projected.graph.tasks[0].id.0.clone();
    let checkpoint = PlanCheckpoint {
        schema_version: 1,
        identity: CheckpointIdentity::from_plan(&plan),
        tasks: vec![RuntimeTaskCheckpoint {
            id,
            state: TaskState::Done,
            gate_iterations: 2,
            review_iterations: 3,
        }],
        active_refs: vec![],
        active_worktrees: vec![],
    };
    assert_eq!(
        inspect_checkpoint(&plan, Some(&checkpoint)),
        CheckpointDisposition::Compatible
    );
    overlay_compatible(&mut projected.graph, &checkpoint);
    assert_eq!(projected.graph.tasks[0].state, TaskState::New);
    assert_eq!(projected.graph.tasks[0].gate_iterations, 2);
    assert_eq!(projected.graph.tasks[0].review_iterations, 3);
}

#[test]
fn compatible_checkpoint_cannot_override_authored_non_dispatch_or_fail_closed_states() {
    let (_repo, plan) = load_fixture();
    let protected = [
        (TaskState::Gated, AuthoredSeedOutcome::Gated),
        (TaskState::Blocked, AuthoredSeedOutcome::Blocked),
        (TaskState::Dropped, AuthoredSeedOutcome::Dropped),
        (
            TaskState::New,
            AuthoredSeedOutcome::NeedsInProgressReconciliation,
        ),
        (
            TaskState::New,
            AuthoredSeedOutcome::NeedsLandingVerification,
        ),
    ];
    for (source_state, seed) in protected {
        let mut projected = ProjectedTaskGraph::from_document(&plan, Utc::now());
        let id = projected.graph.tasks[0].id.clone();
        projected.graph.tasks[0].state = source_state;
        let authored: &mut AuthoredTaskMetadata = projected.graph.authored.get_mut(&id).unwrap();
        authored.seed = seed;
        authored.gated = seed == AuthoredSeedOutcome::Gated;
        authored.status = match seed {
            AuthoredSeedOutcome::Blocked => makina_core::plan::AuthoredTaskStatus::Blocked,
            AuthoredSeedOutcome::Dropped => makina_core::plan::AuthoredTaskStatus::Dropped,
            AuthoredSeedOutcome::NeedsInProgressReconciliation => {
                makina_core::plan::AuthoredTaskStatus::InProgress
            }
            AuthoredSeedOutcome::NeedsLandingVerification => {
                makina_core::plan::AuthoredTaskStatus::Done
            }
            _ => makina_core::plan::AuthoredTaskStatus::Planned,
        };
        let checkpoint = PlanCheckpoint {
            schema_version: 1,
            identity: CheckpointIdentity::from_plan(&plan),
            tasks: vec![RuntimeTaskCheckpoint {
                id: id.0,
                state: TaskState::Ready,
                gate_iterations: 7,
                review_iterations: 8,
            }],
            active_refs: vec![],
            active_worktrees: vec![],
        };
        overlay_compatible(&mut projected.graph, &checkpoint);
        assert_eq!(
            projected.graph.tasks[0].state, source_state,
            "seed {seed:?}"
        );
        assert_eq!(projected.graph.tasks[0].gate_iterations, 7);
    }
}

#[test]
fn mismatched_checkpoint_with_active_evidence_is_retained() {
    let (_repo, plan) = load_fixture();
    let mut identity = CheckpointIdentity::from_plan(&plan);
    identity.executable_digest = "0".repeat(64);
    let checkpoint = PlanCheckpoint {
        schema_version: 1,
        identity,
        tasks: vec![],
        active_refs: vec!["refs/heads/task/recovery".into()],
        active_worktrees: vec![],
    };
    assert!(matches!(
        inspect_checkpoint(&plan, Some(&checkpoint)),
        CheckpointDisposition::RetainForRecovery { .. }
    ));
}

#[tokio::test]
async fn plan_checkpoint_persists_in_repo_makina_dir() {
    let (repo, plan) = load_fixture();
    let projected = ProjectedTaskGraph::from_document(&plan, Utc::now());
    persist_checkpoint(
        repo.path(),
        CheckpointIdentity::from_plan(&plan),
        &projected.graph,
    )
    .await
    .unwrap();
    let path = checkpoint_path(repo.path(), &plan.key).unwrap();
    assert!(
        path.starts_with(repo.path().join(".makina").join("checkpoints")),
        "checkpoint must be under repo/.makina/checkpoints, got {}",
        path.display()
    );
    assert!(
        load_checkpoint(repo.path(), &plan.key)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn clean_mismatch_can_be_archived_without_overwriting_recovery_evidence() {
    let (repo, plan) = load_fixture();
    let projected = ProjectedTaskGraph::from_document(&plan, Utc::now());
    persist_checkpoint(
        repo.path(),
        CheckpointIdentity::from_plan(&plan),
        &projected.graph,
    )
    .await
    .unwrap();

    let checkpoint = checkpoint_path(repo.path(), &plan.key).unwrap();
    let archive = archive_clean_checkpoint(repo.path(), &plan.key)
        .await
        .unwrap()
        .expect("checkpoint should be archived");
    assert!(!checkpoint.exists());
    assert!(archive.exists());
    assert!(
        load_checkpoint(repo.path(), &plan.key)
            .await
            .unwrap()
            .is_none()
    );
}

/// Regression test: a `Skipped` task in a compatible checkpoint must be
/// un-skipped back to `New` when its dependencies are no longer `Failed` or
/// `Skipped`. Otherwise a task whose dependency was reset (e.g. from
/// `InProgress` to `Ready` by `overlay_compatible`) stays `Skipped` forever,
/// producing the "first task ready, other two skipped" regression.
#[test]
fn compatible_checkpoint_unskips_dependents_when_blocking_task_is_not_failed() {
    let now = Utc::now();
    let mut graph = TaskGraph {
        slug: "test-plan".into(),
        tasks: vec![
            Task {
                id: TaskId::new("root"),
                title: "Root".into(),
                description: String::new(),
                done_when: String::new(),
                depends_on: vec![],
                section: None,
                state: TaskState::New,
                gate_iterations: 0,
                review_iterations: 0,
                created_at: now,
                updated_at: now,
                started_at: None,
                finished_at: None,
                failure_reason: None,
            },
            Task {
                id: TaskId::new("dep-a"),
                title: "Dep A".into(),
                description: String::new(),
                done_when: String::new(),
                depends_on: vec![TaskId::new("root")],
                section: None,
                state: TaskState::New,
                gate_iterations: 0,
                review_iterations: 0,
                created_at: now,
                updated_at: now,
                started_at: None,
                finished_at: None,
                failure_reason: None,
            },
            Task {
                id: TaskId::new("dep-b"),
                title: "Dep B".into(),
                description: String::new(),
                done_when: String::new(),
                depends_on: vec![TaskId::new("dep-a")],
                section: None,
                state: TaskState::New,
                gate_iterations: 0,
                review_iterations: 0,
                created_at: now,
                updated_at: now,
                started_at: None,
                finished_at: None,
                failure_reason: None,
            },
        ],
        authored: BTreeMap::new(),
    };

    // Checkpoint from a prior run where root was InProgress (stuck), dep-a and
    // dep-b were Skipped (because root had Failed at some point during that run,
    // skipping its dependents transitively, then root was retried back to
    // InProgress before the checkpoint was written).
    let checkpoint = PlanCheckpoint {
        schema_version: 1,
        identity: CheckpointIdentity {
            plan_dir: std::path::PathBuf::from("docs/plans/0001-test-plan"),
            executable_digest: "0".repeat(64),
            task_ids: vec!["root".into(), "dep-a".into(), "dep-b".into()],
            task_source_paths: vec![],
        },
        tasks: vec![
            RuntimeTaskCheckpoint {
                id: "root".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
            },
            RuntimeTaskCheckpoint {
                id: "dep-a".into(),
                state: TaskState::Skipped,
                gate_iterations: 0,
                review_iterations: 0,
            },
            RuntimeTaskCheckpoint {
                id: "dep-b".into(),
                state: TaskState::Skipped,
                gate_iterations: 0,
                review_iterations: 0,
            },
        ],
        active_refs: vec![],
        active_worktrees: vec![],
    };

    overlay_compatible(&mut graph, &checkpoint);

    // root: InProgress → Ready (standard overlay reset).
    assert_eq!(
        graph.tasks.iter().find(|t| t.id.0 == "root").unwrap().state,
        TaskState::Ready,
        "root (was InProgress) must be reset to Ready"
    );
    // dep-a: Skipped → New (un-skipped because root is no longer Failed/Skipped).
    assert_eq!(
        graph
            .tasks
            .iter()
            .find(|t| t.id.0 == "dep-a")
            .unwrap()
            .state,
        TaskState::New,
        "dep-a must be un-skipped to New because its dependency (root) is no longer Failed/Skipped"
    );
    // dep-b: Skipped → New (un-skipped because dep-a is no longer Failed/Skipped).
    assert_eq!(
        graph
            .tasks
            .iter()
            .find(|t| t.id.0 == "dep-b")
            .unwrap()
            .state,
        TaskState::New,
        "dep-b must be un-skipped to New because its dependency (dep-a) is no longer Failed/Skipped"
    );
}

/// A `Skipped` task whose dependency is still `Failed` in the checkpoint must
/// STAY `Skipped` (the un-skip must not fire when the blocker is genuinely
/// still failed).
#[test]
fn compatible_checkpoint_keeps_skipped_when_blocking_task_is_still_failed() {
    let now = Utc::now();
    let mut graph = TaskGraph {
        slug: "test-plan".into(),
        tasks: vec![
            Task {
                id: TaskId::new("root"),
                title: "Root".into(),
                description: String::new(),
                done_when: String::new(),
                depends_on: vec![],
                section: None,
                state: TaskState::New,
                gate_iterations: 0,
                review_iterations: 0,
                created_at: now,
                updated_at: now,
                started_at: None,
                finished_at: None,
                failure_reason: None,
            },
            Task {
                id: TaskId::new("dep"),
                title: "Dep".into(),
                description: String::new(),
                done_when: String::new(),
                depends_on: vec![TaskId::new("root")],
                section: None,
                state: TaskState::New,
                gate_iterations: 0,
                review_iterations: 0,
                created_at: now,
                updated_at: now,
                started_at: None,
                finished_at: None,
                failure_reason: None,
            },
        ],
        authored: BTreeMap::new(),
    };

    let checkpoint = PlanCheckpoint {
        schema_version: 1,
        identity: CheckpointIdentity {
            plan_dir: std::path::PathBuf::from("docs/plans/0001-test-plan"),
            executable_digest: "0".repeat(64),
            task_ids: vec!["root".into(), "dep-a".into(), "dep-b".into()],
            task_source_paths: vec![],
        },
        tasks: vec![
            RuntimeTaskCheckpoint {
                id: "root".into(),
                state: TaskState::Failed,
                gate_iterations: 5,
                review_iterations: 0,
            },
            RuntimeTaskCheckpoint {
                id: "dep".into(),
                state: TaskState::Skipped,
                gate_iterations: 0,
                review_iterations: 0,
            },
        ],
        active_refs: vec![],
        active_worktrees: vec![],
    };

    overlay_compatible(&mut graph, &checkpoint);

    // root: Failed stays Failed (the `state => state` arm preserves it).
    assert_eq!(
        graph.tasks.iter().find(|t| t.id.0 == "root").unwrap().state,
        TaskState::Failed,
        "root must stay Failed"
    );
    // dep: Skipped stays Skipped because root is still Failed.
    assert_eq!(
        graph.tasks.iter().find(|t| t.id.0 == "dep").unwrap().state,
        TaskState::Skipped,
        "dep must stay Skipped because its dependency (root) is still Failed"
    );
}

fn load_fixture() -> (tempfile::TempDir, Box<makina_core::plan::PlanDocument>) {
    let repo = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/plan-bundles/valid/0049-Sample"),
        &repo.path().join("docs/plans/0049-Sample"),
    );
    let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    let key = PlanKey::parse("docs/plans/0049-Sample").unwrap();
    let PlanCandidate::Plan(plan) = load_plan(&source, key, &PlanReservations::default()).unwrap()
    else {
        panic!()
    };
    (repo, plan)
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target)
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn restore_home(old: Option<std::ffi::OsString>) {
    if let Some(value) = old {
        unsafe { std::env::set_var("HOME", value) }
    } else {
        unsafe { std::env::remove_var("HOME") }
    }
}
