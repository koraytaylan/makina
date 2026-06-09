//! Integration tests for **supervisor-write-path** (plan 0002 §0011).
//!
//! Acceptance criterion: an integration test drives a graph to terminal states
//! against a temp repo and asserts `.tasks/{slug}.json` exists and its final
//! on-disk contents (states, iteration counts, timestamps) match the task-graph
//! snapshot.
//!
//! # Test coverage
//!
//! 1. **Happy-path (→ Done)** — single task runs New → Done; the `.tasks/` file
//!    exists and its on-disk state/timestamps match the supervisor's snapshot.
//! 2. **Gate-cap (→ Failed, GateCapReached)** — always-failing gate exhausts the
//!    cap; the on-disk file records `state: Failed`, non-zero `gate_iterations`,
//!    and a `finished_at` timestamp.
//! 3. **Reviewer-cap (→ Failed, ReviewCapReached)** — always-rejecting reviewer
//!    exhausts the cap; the on-disk file records `state: Failed`, non-zero
//!    `review_iterations`, and a `finished_at` timestamp.
//!
//! # Test-strategy compliance
//!
//! - Backend is always `NoopBackend` — no real agent CLI, no model call.
//! - Each test uses a fresh temporary git repo (`tempfile`).
//! - Determinism via `ask`/await — no arbitrary sleeps.

use std::process::Command;
use std::sync::Arc;

use chrono::Utc;

use makina_core::actors::{
    RunReadyTasks, SetSpokes, SetTaskGraph, Supervisor, SupervisorArgs, TaskGraphSnapshot,
};
use makina_core::backend::AgentBackend;
use makina_core::backend::noop::NoopBackend;
use makina_core::config::{CapsConfig, Config, GateConfig};
use makina_core::persist::{load_graph, tasks_path};
use makina_core::supervision::{RestartConfig, RootSupervisor};
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};
use makina_core::worktree::WorktreeManager;

// ── Temp-repo helpers (mirror the other integration tests) ───────────────────────

/// Create a minimal git repository in a new temporary directory, on a `develop`
/// branch with one initial commit.
fn setup_temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let path = dir.path();

    run_git(path, &["init"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test User"]);
    run_git(path, &["commit", "--allow-empty", "-m", "Initial commit"]);

    // Ensure the branch is named `develop` regardless of init.defaultBranch.
    let current_branch = String::from_utf8(
        Command::new("git")
            .args(["-C", &path.to_string_lossy()])
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .expect("git rev-parse HEAD")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_string();

    if current_branch != "develop" {
        run_git(path, &["branch", "-m", &current_branch, "develop"]);
    }

    dir
}

fn run_git(path: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    assert!(
        status.success(),
        "git {args:?} in {path:?} exited with {:?}",
        status.code()
    );
}

/// Build a `New` task with the given `id` and no dependencies.
fn task(id: &str, done_when: &str) -> Task {
    let now = Utc::now();
    Task {
        id: TaskId::new(id),
        title: format!("Task {id}"),
        description: format!("Implement {id}."),
        done_when: done_when.to_string(),
        depends_on: vec![],
        section: None,
        state: TaskState::New,
        gate_iterations: 0,
        review_iterations: 0,
        created_at: now,
        updated_at: now,
        started_at: None,
        finished_at: None,
    }
}

/// Spawn the actor tree over `repo_root` with the given `backend` and `config`,
/// wire the concurrency deps into the hub via `SetSpokes`, and return
/// `(root, supervisor_ref)`.
async fn build_actor_tree(
    repo_root: std::path::PathBuf,
    backend: Arc<dyn AgentBackend>,
    config: Config,
) -> (
    kameo::actor::ActorRef<RootSupervisor>,
    kameo::actor::ActorRef<Supervisor>,
) {
    let root = RootSupervisor::start();

    let supervisor_ref = RootSupervisor::spawn_child::<Supervisor>(
        &root,
        SupervisorArgs {
            worktree_manager: WorktreeManager::new(repo_root, "develop".into()),
            config,
        },
        RestartConfig::default(),
    )
    .await;

    supervisor_ref
        .ask(SetSpokes {
            root: root.clone(),
            supervisor: supervisor_ref.clone(),
            developer_backend: Arc::clone(&backend),
            reviewer_backend: Arc::clone(&backend),
        })
        .send()
        .await
        .expect("SetSpokes must be accepted");

    (root, supervisor_ref)
}

// ── Test 1: happy path → Done ────────────────────────────────────────────────────

/// A task that runs `New → Done` must produce a `.tasks/{slug}.json` whose
/// on-disk `state` is `"Done"` and whose timestamps + iteration counts match
/// the supervisor's final in-memory snapshot.
#[tokio::test]
async fn persist_file_exists_and_matches_snapshot_after_done() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let slug = "write-path-happy";
    let backend = NoopBackend::with_responses(vec![
        "Implemented the feature.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]);

    // Default config: no gates, so the loop goes straight to review.
    let config = Config::resolve(
        makina_core::config::GlobalConfig::default(),
        makina_core::config::ProjectConfig::default(),
    );

    let (root, supervisor_ref) =
        build_actor_tree(repo_root.clone(), Arc::new(backend), config).await;

    let graph = TaskGraph {
        slug: slug.into(),
        tasks: vec![task("persist-task", "the task is persisted")],
    };
    supervisor_ref
        .ask(SetTaskGraph(graph))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    // Drive the run.
    let report = supervisor_ref
        .ask(RunReadyTasks)
        .send()
        .await
        .expect("RunReadyTasks must succeed");

    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("persist-task"), TaskState::Done)],
        "task must reach Done"
    );

    // ── Assert: the .tasks/{slug}.json file exists ─────────────────────────────
    let artifact_path = tasks_path(&repo_root, slug);
    assert!(
        artifact_path.exists(),
        ".tasks/{slug}.json must exist after the run; path: {artifact_path:?}"
    );

    // ── Assert: the on-disk content matches the supervisor's final snapshot ─────
    let on_disk = load_graph(&repo_root, slug)
        .await
        .expect("load_graph must succeed")
        .expect("artifact must be present");

    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph must be Some");

    // On-disk task must match the in-memory snapshot task.
    let disk_task = on_disk
        .get(&TaskId::new("persist-task"))
        .expect("task must exist on disk");
    let snap_task = snapshot
        .get(&TaskId::new("persist-task"))
        .expect("task must exist in snapshot");

    assert_eq!(
        disk_task.state,
        TaskState::Done,
        "on-disk state must be Done"
    );
    assert_eq!(
        disk_task.state, snap_task.state,
        "on-disk state must match snapshot"
    );
    assert!(
        disk_task.started_at.is_some(),
        "on-disk started_at must be set"
    );
    assert!(
        disk_task.finished_at.is_some(),
        "on-disk finished_at must be set"
    );
    assert_eq!(
        disk_task.started_at, snap_task.started_at,
        "on-disk started_at must match snapshot"
    );
    assert_eq!(
        disk_task.finished_at, snap_task.finished_at,
        "on-disk finished_at must match snapshot"
    );
    assert_eq!(
        disk_task.gate_iterations, snap_task.gate_iterations,
        "gate_iterations must match"
    );
    assert_eq!(
        disk_task.review_iterations, snap_task.review_iterations,
        "review_iterations must match"
    );

    root.kill();
}

// ── Test 2: gate-cap → Failed ────────────────────────────────────────────────────

/// A task whose only gate always fails drives the gate-iteration cap; the
/// on-disk file must record `state: Failed`, non-zero `gate_iterations`, and a
/// `finished_at` timestamp.
#[tokio::test]
async fn persist_file_matches_snapshot_after_gate_cap_failed() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let slug = "write-path-gate-cap";

    // The developer always "succeeds" (noop), but the gate always fails.
    // One response entry is enough for all gate iterations: NoopBackend cycles
    // (wraps the index back to 0 on exhaustion), so it never errors regardless
    // of how many times the developer is re-dispatched.
    let backend = NoopBackend::with_responses(vec!["dev output".into()]);

    let config = Config {
        // One gate that always fails (exit 1).
        gates: vec![GateConfig {
            name: "always-fail".into(),
            command: "false".into(),
            image: None,
        }],
        caps: CapsConfig {
            gate_iterations: 2, // fail after 2 gate iterations
            reviewer_iterations: 3,
            wall_clock_secs: 60,
        },
        ..Config::resolve(
            makina_core::config::GlobalConfig::default(),
            makina_core::config::ProjectConfig::default(),
        )
    };

    let (root, supervisor_ref) =
        build_actor_tree(repo_root.clone(), Arc::new(backend), config).await;

    let graph = TaskGraph {
        slug: slug.into(),
        tasks: vec![task("gate-task", "the gate passes")],
    };
    supervisor_ref
        .ask(SetTaskGraph(graph))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    // The run will end with a hard error (gate cap → Failed, driver returns Ok(Failed));
    // RunReadyTasks returns Ok (not all failures are hard errors at the scheduler level).
    let report = supervisor_ref
        .ask(RunReadyTasks)
        .send()
        .await
        .expect("RunReadyTasks returned");

    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("gate-task"), TaskState::Failed)],
        "task must reach Failed (gate cap)"
    );

    // ── Assert: the artifact exists ────────────────────────────────────────────
    let artifact_path = tasks_path(&repo_root, slug);
    assert!(
        artifact_path.exists(),
        ".tasks/{slug}.json must exist after gate-cap run; path: {artifact_path:?}"
    );

    // ── Assert: on-disk content matches the final snapshot ─────────────────────
    let on_disk = load_graph(&repo_root, slug)
        .await
        .expect("load_graph must succeed")
        .expect("artifact must be present");

    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph must be Some");

    let disk_task = on_disk
        .get(&TaskId::new("gate-task"))
        .expect("task must exist on disk");
    let snap_task = snapshot
        .get(&TaskId::new("gate-task"))
        .expect("task must exist in snapshot");

    assert_eq!(
        disk_task.state,
        TaskState::Failed,
        "on-disk state must be Failed"
    );
    assert_eq!(
        disk_task.state, snap_task.state,
        "on-disk state must match snapshot"
    );
    // gate_iterations must be >= 1 (at least one gate cycle ran before the cap).
    assert!(
        disk_task.gate_iterations >= 1,
        "gate_iterations must be >= 1 after gate-cap; got {}",
        disk_task.gate_iterations
    );
    assert_eq!(
        disk_task.gate_iterations, snap_task.gate_iterations,
        "gate_iterations must match snapshot"
    );
    assert!(
        disk_task.finished_at.is_some(),
        "on-disk finished_at must be set after gate-cap"
    );
    assert_eq!(
        disk_task.finished_at, snap_task.finished_at,
        "on-disk finished_at must match snapshot"
    );

    root.kill();
}

// ── Test 3: reviewer-cap → Failed ───────────────────────────────────────────────

/// A task whose reviewer always rejects drives the reviewer-iteration cap; the
/// on-disk file must record `state: Failed`, non-zero `review_iterations`, and a
/// `finished_at` timestamp.
#[tokio::test]
async fn persist_file_matches_snapshot_after_reviewer_cap_failed() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let slug = "write-path-reviewer-cap";
    let cap: u32 = 2;

    // The reviewer always rejects; the developer always succeeds.  We need enough
    // canned responses: developer × (cap) + reviewer × cap.
    let mut responses = Vec::new();
    for _ in 0..cap {
        responses.push("dev output".into());
        responses.push(r#"{"verdict":"reject","feedback":"nope"}"#.into());
    }
    let backend = NoopBackend::with_responses(responses);

    let config = Config {
        gates: vec![], // no gates — straight to review
        caps: CapsConfig {
            gate_iterations: 5,
            reviewer_iterations: cap,
            wall_clock_secs: 60,
        },
        ..Config::resolve(
            makina_core::config::GlobalConfig::default(),
            makina_core::config::ProjectConfig::default(),
        )
    };

    let (root, supervisor_ref) =
        build_actor_tree(repo_root.clone(), Arc::new(backend), config).await;

    let graph = TaskGraph {
        slug: slug.into(),
        tasks: vec![task("review-task", "the reviewer approves")],
    };
    supervisor_ref
        .ask(SetTaskGraph(graph))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    let report = supervisor_ref
        .ask(RunReadyTasks)
        .send()
        .await
        .expect("RunReadyTasks returned");

    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("review-task"), TaskState::Failed)],
        "task must reach Failed (reviewer cap)"
    );

    // ── Assert: the artifact exists ────────────────────────────────────────────
    let artifact_path = tasks_path(&repo_root, slug);
    assert!(
        artifact_path.exists(),
        ".tasks/{slug}.json must exist after reviewer-cap run; path: {artifact_path:?}"
    );

    // ── Assert: on-disk content matches the final snapshot ─────────────────────
    let on_disk = load_graph(&repo_root, slug)
        .await
        .expect("load_graph must succeed")
        .expect("artifact must be present");

    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph must be Some");

    let disk_task = on_disk
        .get(&TaskId::new("review-task"))
        .expect("task must exist on disk");
    let snap_task = snapshot
        .get(&TaskId::new("review-task"))
        .expect("task must exist in snapshot");

    assert_eq!(
        disk_task.state,
        TaskState::Failed,
        "on-disk state must be Failed"
    );
    assert_eq!(
        disk_task.state, snap_task.state,
        "on-disk state must match snapshot"
    );
    // review_iterations must equal the cap (all rejections were recorded).
    assert_eq!(
        disk_task.review_iterations, cap,
        "review_iterations must equal the cap ({cap}); got {}",
        disk_task.review_iterations
    );
    assert_eq!(
        disk_task.review_iterations, snap_task.review_iterations,
        "review_iterations must match snapshot"
    );
    assert!(
        disk_task.finished_at.is_some(),
        "on-disk finished_at must be set after reviewer-cap"
    );
    assert_eq!(
        disk_task.finished_at, snap_task.finished_at,
        "on-disk finished_at must match snapshot"
    );

    root.kill();
}
