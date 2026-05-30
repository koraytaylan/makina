//! Integration tests for the **develop → review loop** (task 21) — the core
//! orchestration cycle.
//!
//! # Acceptance criterion
//!
//! Task 21 "done when": *a task runs end-to-end through the loop with the noop
//! backend.*  These tests build the real actor tree (`RootSupervisor` →
//! `Supervisor` hub + `Developer`/`Reviewer` spokes) over a temporary git repo,
//! drive a `TaskGraph` through `RunReadyTasks`, and assert:
//!
//! 1. **Happy path** — a single task reaches `Done`; the worktree/branch were
//!    created during the run and removed after; the Developer and Reviewer were
//!    each prompted (via `recorded_prompts()`).
//! 2. **Reject → approve** — the backend rejects once then approves; the feedback
//!    was relayed to the Developer (its retry prompt contains the feedback),
//!    `review_iterations` incremented, and the task still ends `Done`.
//! 3. **Chain** — task B depends on task A; A finishes `Done`, then B becomes
//!    ready and runs to `Done`.
//!
//! # Test-strategy compliance (see `docs/spec/testing-strategy.md`)
//!
//! - Backend is always `NoopBackend` — no real agent CLI, no model call.
//! - Determinism via `ask`/await — no arbitrary sleeps.
//! - Each test uses a fresh temporary git repo (`tempfile`); the real Makina repo
//!   is never touched.  The temp-repo setup mirrors `tests/worktree.rs`.

use std::process::Command;
use std::sync::Arc;

use chrono::Utc;

use makina_core::actors::{
    RunReadyTasks, SetSpokes, SetTaskGraph, Supervisor, SupervisorArgs, TaskGraphSnapshot,
};
use makina_core::backend::AgentBackend;
use makina_core::backend::noop::NoopBackend;
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::supervision::{RestartConfig, RootSupervisor};
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};
use makina_core::worktree::WorktreeManager;

// ── Temp-repo helper (mirrors tests/worktree.rs) ─────────────────────────────────

/// Create a minimal git repository in a new temporary directory, on a `develop`
/// branch with one initial commit (so `git worktree add -b … develop` works).
///
/// Returns the temp dir (must be kept alive for the duration of the test).
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

/// Run a `git -C {path}` command, asserting it exits 0.
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

/// Return true if `branch` exists in the repo at `path`.
fn branch_exists(path: &std::path::Path, branch: &str) -> bool {
    let output = Command::new("git")
        .args(["-C", &path.to_string_lossy()])
        .args(["branch", "--list", branch])
        .output()
        .expect("git branch --list");
    !String::from_utf8_lossy(&output.stdout).trim().is_empty()
}

// ── Task / graph builders ────────────────────────────────────────────────────────

/// Build a `New` task with the given `id`, `done_when`, and dependencies.
fn task(id: &str, done_when: &str, deps: &[&str]) -> Task {
    let now = Utc::now();
    Task {
        id: TaskId::new(id),
        title: format!("Task {id}"),
        description: format!("Implement {id}."),
        done_when: done_when.to_string(),
        depends_on: deps.iter().map(|d| TaskId::new(*d)).collect(),
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

// ── Actor-tree builder ───────────────────────────────────────────────────────────

/// Spawn the actor tree over `repo_root` with the given `backend`, wire the
/// concurrency deps into the hub via `SetSpokes`, and return
/// `(root, supervisor_ref)`.
///
/// Under task 24 the hub spawns a Developer/Reviewer pair **per task** itself, so
/// the helper wires the means to do so (root ref, hub ref, shared backend)
/// rather than pre-spawning a shared spoke pair.
async fn build_actor_tree(
    repo_root: std::path::PathBuf,
    backend: Arc<dyn AgentBackend>,
) -> (
    kameo::actor::ActorRef<RootSupervisor>,
    kameo::actor::ActorRef<Supervisor>,
) {
    let root = RootSupervisor::start();

    let supervisor_ref = RootSupervisor::spawn_child::<Supervisor>(
        &root,
        SupervisorArgs {
            worktree_manager: WorktreeManager::new(repo_root, "develop".into()),
            // These task-21 tests configure NO gates, so the gate loop (task 22)
            // is a no-op and the work advances straight to review.
            config: Config::resolve(GlobalConfig::default(), ProjectConfig::default()),
        },
        RestartConfig::default(),
    )
    .await;

    // Post-spawn wiring: hand the per-task-spawn deps to the hub.
    supervisor_ref
        .ask(SetSpokes {
            root: root.clone(),
            supervisor: supervisor_ref.clone(),
            backend: Arc::clone(&backend),
        })
        .send()
        .await
        .expect("SetSpokes must be accepted");

    (root, supervisor_ref)
}

// ── Acceptance test: a task runs end-to-end through the loop ─────────────────────

/// **Acceptance criterion** — one task runs `New → Done` through the full
/// dispatch → develop → hand-back → review → approve loop with the NoopBackend.
///
/// Asserts:
/// - the task reaches `Done` (via the `RunReport` and the graph snapshot);
/// - the worktree directory + `task/{id}` branch existed *during* the run and are
///   gone *after* (proving create-on-dispatch + teardown-on-approve);
/// - the Developer and Reviewer were each prompted exactly once, with prompts
///   that mention the task and request review respectively.
#[tokio::test]
async fn single_task_runs_end_to_end_to_done() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // NoopBackend: 1st prompt (developer) → dev output; 2nd prompt (reviewer) →
    // approve verdict JSON.  Keep a clone to inspect recorded prompts afterwards.
    let backend = NoopBackend::with_responses(vec![
        "Implemented the feature.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]);
    let backend_probe = backend.clone();

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
    )
    .await;

    // Provide a one-task graph.
    let graph = TaskGraph {
        slug: "loop-test".into(),
        tasks: vec![task("build-thing", "the thing builds", &[])],
    };
    supervisor_ref
        .ask(SetTaskGraph(graph))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    // Sanity: the worktree/branch do NOT exist before the run.
    let worktree_path = repo_root
        .join(".makina")
        .join("worktrees")
        .join("build-thing");
    assert!(!worktree_path.exists(), "worktree must not exist pre-run");
    assert!(
        !branch_exists(&repo_root, "task/build-thing"),
        "branch must not exist pre-run"
    );

    // ── Trigger the run ───────────────────────────────────────────────────────
    let report = supervisor_ref
        .ask(RunReadyTasks)
        .send()
        .await
        .expect("RunReadyTasks must drive the loop without a hard error");

    // The run drove exactly one task to Done.
    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("build-thing"), TaskState::Done)],
        "report should show build-thing reached Done"
    );

    // The graph snapshot reflects the final Done state + finished timestamp.
    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph should be Some");
    let done_task = snapshot
        .get(&TaskId::new("build-thing"))
        .expect("task must be present");
    assert_eq!(done_task.state, TaskState::Done, "task must be Done");
    assert!(
        done_task.started_at.is_some(),
        "started_at should be stamped on dispatch"
    );
    assert!(
        done_task.finished_at.is_some(),
        "finished_at should be stamped on completion"
    );
    assert_eq!(
        done_task.review_iterations, 0,
        "no rejections occurred, so review_iterations stays 0"
    );

    // The worktree + branch were torn down after approval.
    assert!(
        !worktree_path.exists(),
        "worktree dir must be gone after the run"
    );
    assert!(
        !branch_exists(&repo_root, "task/build-thing"),
        "branch must be gone after the run"
    );

    // The Developer and Reviewer were each prompted exactly once.
    let prompts = backend_probe.recorded_prompts();
    assert_eq!(
        prompts.len(),
        2,
        "exactly two prompts (developer + reviewer); got {prompts:?}"
    );
    assert!(
        prompts[0].contains("build-thing") && prompts[0].contains("Implement"),
        "developer prompt must describe the task; got: {:?}",
        prompts[0]
    );
    assert!(
        prompts[1].to_lowercase().contains("review"),
        "reviewer prompt must request a review; got: {:?}",
        prompts[1]
    );

    root.kill();
}

// ── Reject → approve test ────────────────────────────────────────────────────────

/// The Reviewer rejects the first attempt with feedback, then approves the
/// retry.  Asserts:
/// - the feedback was **relayed to the Developer** (the second developer prompt
///   contains the feedback string);
/// - `review_iterations` was incremented to 1;
/// - the task still ends `Done`;
/// - the worktree is torn down afterwards.
#[tokio::test]
async fn reject_then_approve_relays_feedback_and_finishes_done() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // Response cycle (one per prompt, in order):
    //   1. developer  → dev output (attempt 1)
    //   2. reviewer   → REJECT with feedback
    //   3. developer  → dev output (attempt 2, after feedback relay)
    //   4. reviewer   → APPROVE
    let feedback_msg = "add the missing error handling";
    let backend = NoopBackend::with_responses(vec![
        "First attempt.".into(),
        format!(r#"{{"verdict":"reject","feedback":"{feedback_msg}"}}"#),
        "Second attempt addressing feedback.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]);
    let backend_probe = backend.clone();

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
    )
    .await;

    let graph = TaskGraph {
        slug: "reject-test".into(),
        tasks: vec![task("fix-bug", "the bug is fixed", &[])],
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
        .expect("RunReadyTasks must drive the loop without a hard error");

    // Despite the rejection, the task ends Done.
    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("fix-bug"), TaskState::Done)],
        "fix-bug should still reach Done after one reject"
    );

    // review_iterations incremented to 1 (exactly one rejection).
    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph should be Some");
    let t = snapshot
        .get(&TaskId::new("fix-bug"))
        .expect("task must be present");
    assert_eq!(t.state, TaskState::Done, "task must be Done");
    assert_eq!(
        t.review_iterations, 1,
        "exactly one rejection should bump review_iterations to 1"
    );

    // Four prompts: dev1, review1(reject), dev2, review2(approve).
    let prompts = backend_probe.recorded_prompts();
    assert_eq!(
        prompts.len(),
        4,
        "expected 4 prompts (2 dev + 2 review); got {prompts:?}"
    );

    // The retry developer prompt (3rd overall) must carry the reviewer feedback.
    assert!(
        prompts[2].contains(feedback_msg),
        "the developer retry prompt must relay the reviewer feedback; got: {:?}",
        prompts[2]
    );
    // And the first developer prompt must NOT contain the feedback (it preceded it).
    assert!(
        !prompts[0].contains(feedback_msg),
        "the first developer prompt must not contain feedback; got: {:?}",
        prompts[0]
    );

    // Worktree torn down after completion.
    let worktree_path = repo_root.join(".makina").join("worktrees").join("fix-bug");
    assert!(
        !worktree_path.exists(),
        "worktree must be gone after the run"
    );

    root.kill();
}

// ── Chain test: B depends on A ───────────────────────────────────────────────────

/// A two-task chain (B depends on A) shows the scheduler unlocks dependents: A
/// runs first to `Done`, then B becomes ready and runs to `Done`.
#[tokio::test]
async fn dependency_chain_runs_a_then_b() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // Four prompts cycle across the two tasks: devA, reviewA, devB, reviewB.
    // The NoopBackend cycles responses, so [dev, approve] covers both tasks.
    let backend =
        NoopBackend::with_responses(vec!["dev output".into(), r#"{"verdict":"approve"}"#.into()]);

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
    )
    .await;

    // B depends on A; B is authored first to prove ordering is by readiness, not
    // by position in the task list.
    let graph = TaskGraph {
        slug: "chain-test".into(),
        tasks: vec![
            task("task-b", "b is done", &["task-a"]),
            task("task-a", "a is done", &[]),
        ],
    };
    graph.validate().expect("chain graph must validate");
    supervisor_ref
        .ask(SetTaskGraph(graph))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    let report = supervisor_ref
        .ask(RunReadyTasks)
        .send()
        .await
        .expect("RunReadyTasks must drive both tasks without a hard error");

    // A must complete before B (A was the only initially-ready task).
    assert_eq!(
        report.outcomes,
        vec![
            (TaskId::new("task-a"), TaskState::Done),
            (TaskId::new("task-b"), TaskState::Done),
        ],
        "A should finish before B becomes ready"
    );

    // Both tasks ended Done in the graph.
    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph should be Some");
    for id in ["task-a", "task-b"] {
        assert_eq!(
            snapshot.get(&TaskId::new(id)).expect("task present").state,
            TaskState::Done,
            "{id} must be Done"
        );
    }

    // Both worktrees torn down.
    assert!(
        !repo_root
            .join(".makina")
            .join("worktrees")
            .join("task-a")
            .exists()
    );
    assert!(
        !repo_root
            .join(".makina")
            .join("worktrees")
            .join("task-b")
            .exists()
    );

    root.kill();
}
