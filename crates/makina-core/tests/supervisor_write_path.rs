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

use async_trait::async_trait;
use chrono::Utc;
use futures::stream;

use makina_core::actors::{
    RunReadyTasks, SetSpokes, SetTaskGraph, Supervisor, SupervisorArgs, TaskGraphSnapshot,
};
use makina_core::api::FailureKind;
use makina_core::backend::noop::NoopBackend;
use makina_core::backend::{
    AgentBackend, AgentSession, BackendError, Prompt, ResponseEvent, ResponseStream, SessionConfig,
};
use makina_core::config::{CapsConfig, Config, GateConfig};
use makina_core::persist::{load_graph, tasks_path};
use makina_core::supervision::{RestartConfig, RootSupervisor};
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};
use makina_core::worktree::WorktreeManager;

// ── FileWritingBackend ─────────────────────────────────────────────────────────
//
// A test-only backend for `merge_conflict_classified_distinctly`.
//
// On `spawn`, writes a file to the task worktree (the session's `working_dir`)
// so the Developer's `git add -A && git commit` captures a real file change on
// the task branch.  It also writes a CONFLICTING version of that file to
// `repo_root` and commits it on `develop` — the base branch — so that when the
// supervisor tries to squash-merge the task branch, both branches have diverged
// from the same base with different content, which guarantees a conflict.
struct FileWritingBackend {
    /// Name of the file to create both in the worktree and on develop.
    file_name: String,
    /// Content written to the file in the task worktree.
    worktree_content: String,
    /// Conflicting content committed on develop (must differ from `worktree_content`).
    develop_content: String,
    /// The base-checkout (develop) directory, so we can commit there too.
    repo_root: std::path::PathBuf,
    /// Canned response text for the agent prompt.
    response: String,
}

impl FileWritingBackend {
    fn new(
        file_name: &str,
        worktree_content: &str,
        develop_content: &str,
        repo_root: std::path::PathBuf,
        response: &str,
    ) -> Self {
        Self {
            file_name: file_name.to_string(),
            worktree_content: worktree_content.to_string(),
            develop_content: develop_content.to_string(),
            repo_root,
            response: response.to_string(),
        }
    }
}

#[async_trait]
impl AgentBackend for FileWritingBackend {
    async fn spawn(&self, config: SessionConfig) -> Result<Box<dyn AgentSession>, BackendError> {
        // 1. Write the task-branch version into the worktree.
        let worktree_path = config.working_dir.join(&self.file_name);
        std::fs::write(&worktree_path, &self.worktree_content).map_err(|e| {
            BackendError::Spawn {
                reason: e.to_string(),
            }
        })?;

        // 2. Write the conflicting version into develop's checkout and commit it.
        //    This ensures develop's HEAD diverges from the task branch's base,
        //    guaranteeing a merge conflict when the supervisor squash-merges.
        let develop_path = self.repo_root.join(&self.file_name);
        std::fs::write(&develop_path, &self.develop_content).map_err(|e| BackendError::Spawn {
            reason: e.to_string(),
        })?;
        let repo_str = self.repo_root.to_string_lossy().to_string();
        let git_args = vec![
            vec!["-C", &repo_str, "add", "-A"],
            vec![
                "-C",
                &repo_str,
                "commit",
                "--allow-empty",
                "-m",
                "conflict: change on develop",
            ],
        ];
        for args in git_args {
            let status = std::process::Command::new("git")
                .args(&args)
                .status()
                .map_err(|e| BackendError::Spawn {
                    reason: e.to_string(),
                })?;
            if !status.success() {
                return Err(BackendError::Spawn {
                    reason: format!("git {:?} failed with {:?}", args, status.code()),
                });
            }
        }

        Ok(Box::new(CannedSession {
            response: self.response.clone(),
        }))
    }
}

struct CannedSession {
    response: String,
}

#[async_trait]
impl AgentSession for CannedSession {
    async fn prompt(&mut self, _prompt: Prompt) -> Result<ResponseStream, BackendError> {
        let events: Vec<Result<ResponseEvent, BackendError>> = vec![
            Ok(ResponseEvent::TextChunk {
                text: self.response.clone(),
            }),
            Ok(ResponseEvent::TurnComplete),
        ];
        Ok(Box::pin(stream::iter(events)))
    }

    async fn terminate(&mut self) -> Result<(), BackendError> {
        Ok(())
    }
}

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
        failure_reason: None,
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
            idle_secs: None,
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
            idle_secs: None,
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

// ── Test 4: task_view_carries_failure_reason ─────────────────────────────────

/// Drive a task to `Failed` via the gate cap and assert that the domain task's
/// `failure_reason` is `Some(GateCap)` with a non-empty message — which is
/// exactly what the `TaskView.failure_reason` exposes.
#[tokio::test]
async fn task_view_carries_failure_reason() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let slug = "failure-reason-gate";

    // Developer always "succeeds" (noop), but the gate always fails.
    let backend = NoopBackend::with_responses(vec!["dev output".into()]);

    let config = Config {
        gates: vec![GateConfig {
            name: "always-fail".into(),
            command: "false".into(),
            image: None,
        }],
        caps: CapsConfig {
            gate_iterations: 2,
            reviewer_iterations: 3,
            wall_clock_secs: 60,
            idle_secs: None,
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
        tasks: vec![task("reason-task", "the gate passes")],
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
        .expect("RunReadyTasks must return");

    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("reason-task"), TaskState::Failed)],
        "task must reach Failed (gate cap)"
    );

    // Retrieve the final in-memory snapshot and check the failure_reason.
    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph must be Some");

    let snap_task = snapshot
        .get(&TaskId::new("reason-task"))
        .expect("task must exist in snapshot");

    let fr = snap_task
        .failure_reason
        .as_ref()
        .expect("failure_reason must be Some for a Failed task");

    assert_eq!(
        fr.kind,
        FailureKind::GateCap,
        "gate-cap failure must classify as GateCap, got {:?}",
        fr.kind
    );
    assert!(
        !fr.message.is_empty(),
        "failure_reason.message must be non-empty"
    );

    root.kill();
}

// ── Test 5: merge_conflict_classified_distinctly ──────────────────────────────

/// Drive a task to `Failed` via a squash-merge conflict and assert that the
/// task's `failure_reason.kind` is `MergeConflict` — NOT `ReviewCap`.
///
/// Setup:
/// - `develop` carries a seeded file (`shared.txt = "from-develop\n"`).
/// - A `FileWritingBackend` developer writes `"from-task\n"` to the same file
///   in the task worktree; the developer's `git add -A && git commit` captures
///   it as a real (non-empty) commit on the task branch.
/// - The reviewer always approves.
/// - When the supervisor tries to squash-merge, the two versions of `shared.txt`
///   conflict → `MergeOutcome::Conflict` → `TaskEvent::MergeConflict` →
///   `FailureKind::MergeConflict`.
#[tokio::test]
async fn merge_conflict_classified_distinctly() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // Seed a shared file on develop so both branches start from the same base.
    // The FileWritingBackend will write conflicting content to both the task
    // worktree AND develop (via a new commit) so that the squash-merge conflicts.
    std::fs::write(repo_root.join("shared.txt"), "original\n").expect("write shared.txt");
    run_git(&repo_root, &["add", "-A"]);
    run_git(&repo_root, &["commit", "-m", "seed shared.txt"]);

    let slug = "merge-conflict-class";

    // The developer backend writes "from-task\n" to shared.txt in the worktree
    // (so the task branch has a real file change) AND commits "from-develop-v2\n"
    // to develop's checkout (a diverging change on the base branch) so the
    // squash-merge will conflict.
    let developer_backend = Arc::new(FileWritingBackend::new(
        "shared.txt",
        "from-task\n",
        "from-develop-v2\n",
        repo_root.clone(),
        "Implemented the feature.",
    )) as Arc<dyn AgentBackend>;
    let reviewer_backend = Arc::new(NoopBackend::with_responses(vec![
        r#"{"verdict":"approve"}"#.into(),
    ])) as Arc<dyn AgentBackend>;

    let config = Config::resolve(
        makina_core::config::GlobalConfig::default(),
        makina_core::config::ProjectConfig::default(),
    );

    // Build the actor tree manually (separate dev/reviewer backends).
    let root = RootSupervisor::start();
    let supervisor_ref = RootSupervisor::spawn_child::<Supervisor>(
        &root,
        SupervisorArgs {
            worktree_manager: WorktreeManager::new(repo_root.clone(), "develop".into()),
            config,
        },
        RestartConfig::default(),
    )
    .await;
    supervisor_ref
        .ask(SetSpokes {
            root: root.clone(),
            supervisor: supervisor_ref.clone(),
            developer_backend,
            reviewer_backend,
        })
        .send()
        .await
        .expect("SetSpokes must be accepted");

    let graph = TaskGraph {
        slug: slug.into(),
        tasks: vec![task("conflict-task", "no conflict")],
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
        .expect("RunReadyTasks must return");

    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("conflict-task"), TaskState::Failed)],
        "task must reach Failed (merge conflict)"
    );

    // Check the in-memory snapshot for the classified failure reason.
    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph must be Some");

    let snap_task = snapshot
        .get(&TaskId::new("conflict-task"))
        .expect("task must exist in snapshot");

    let fr = snap_task
        .failure_reason
        .as_ref()
        .expect("failure_reason must be Some for a Failed task");

    assert_eq!(
        fr.kind,
        FailureKind::MergeConflict,
        "merge-conflict failure must classify as MergeConflict, not {:?}",
        fr.kind
    );
    assert!(
        !fr.message.is_empty(),
        "failure_reason.message must be non-empty"
    );

    root.kill();
}
