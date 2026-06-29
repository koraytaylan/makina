//! Integration tests for the per-plan integration branch (task 0083), against
//! real git in temporary repositories.
//!
//! # Acceptance criterion
//!
//! Task 0083 "done when": *a run creates and checks out `plan/{plan_slug}` off
//! `base_branch`; task worktrees fork from `plan/{plan_slug}`; an approved task
//! squash-merges into `plan/{plan_slug}`; `base_branch` is unchanged during the
//! run; `repo_root` is restored to `base_branch` at run end; the ask path is
//! unchanged.*
//!
//! These tests prove that:
//!
//! 1. **Plan branch creation** — at run start, `plan/{plan_slug}` is created off
//!    `base_branch` and checked out in `repo_root`.
//! 2. **Fork from plan branch** — task worktrees fork from `plan/{plan_slug}`,
//!    not `base_branch` (the task branch's merge-base with the plan branch is
//!    the plan branch's tip-at-fork).
//! 3. **Squash into plan branch** — an approved task's squash commit lands on
//!    `plan/{plan_slug}`, not `base_branch`.
//! 4. **Base branch untouched** — `base_branch` (develop) HEAD is unchanged
//!    before and after the run.
//! 5. **Repo root restored** — at run end, `repo_root` is checked out back to
//!    `base_branch`.
//! 6. **Ask path unchanged** — with an empty `plan_slug`, the legacy behavior is
//!    preserved (fork from `base_branch`, merge into `base_branch`, no plan
//!    branch created, and the existing `squash_merge.rs` tests still pass).

use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream;

use makina_core::actors::{RunControl, run_graph};
use makina_core::api::RunId;
use makina_core::audit::NoopAuditRegistry;
use makina_core::backend::noop::NoopBackend;
use makina_core::backend::{
    AgentBackend, AgentSession, BackendError, Prompt, ResponseEvent, ResponseStream, SessionConfig,
};
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::interpreter::StructuredTextInterpreter;
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};
use makina_core::worktree::WorktreeManager;

#[derive(Clone)]
struct WritingBackend {
    file_name: String,
    content: String,
    response: String,
}

#[async_trait]
impl AgentBackend for WritingBackend {
    async fn spawn(&self, config: SessionConfig) -> Result<Box<dyn AgentSession>, BackendError> {
        std::fs::write(config.working_dir.join(&self.file_name), &self.content).map_err(|e| {
            BackendError::Spawn {
                reason: e.to_string(),
            }
        })?;
        Ok(Box::new(WritingSession {
            response: self.response.clone(),
        }))
    }
}

struct WritingSession {
    response: String,
}

#[async_trait]
impl AgentSession for WritingSession {
    async fn prompt(&mut self, _prompt: Prompt) -> Result<ResponseStream, BackendError> {
        let events = vec![
            Ok(ResponseEvent::TextChunk {
                text: self.response.clone(),
            }),
            Ok(ResponseEvent::TurnComplete { usage: None }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }

    async fn terminate(&mut self) -> Result<(), BackendError> {
        Ok(())
    }
}

// ── Temp-repo helpers (mirror tests/squash_merge.rs & tests/develop_review_loop.rs) ──

/// Create a minimal git repository in a new temporary directory, on a `develop`
/// branch with one initial commit.  Returns the temp dir (keep it alive).
fn setup_temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let path = dir.path();

    run_git(path, &["init"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test User"]);
    run_git(path, &["commit", "--allow-empty", "-m", "Initial commit"]);

    // Ensure the branch is named `develop` regardless of init.defaultBranch.
    let current = current_branch(path);
    if current != "develop" {
        run_git(path, &["branch", "-m", &current, "develop"]);
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

/// Run a `git -C {path}` command, returning trimmed stdout (asserting exit 0).
fn git_stdout(path: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} in {path:?} exited with {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Get the current branch name of the repository.
fn current_branch(path: &std::path::Path) -> String {
    git_stdout(path, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// Get the commit count for the current branch.
fn commit_count(path: &std::path::Path) -> usize {
    git_stdout(path, &["rev-list", "--count", "HEAD"])
        .parse()
        .unwrap_or(0)
}

/// Check whether a branch exists in the repository.
fn branch_exists(path: &std::path::Path, branch: &str) -> bool {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["branch", "--list", branch])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn git: {e}"));
    !String::from_utf8_lossy(&output.stdout).trim().is_empty()
}

/// Get the SHA of a branch tip.
fn branch_sha(path: &std::path::Path, branch: &str) -> String {
    git_stdout(path, &["rev-parse", branch])
}

fn git_cached_name_status(path: &std::path::Path) -> String {
    git_stdout(path, &["diff", "--cached", "--name-status"])
}

/// Create a task with the given ID, title, and dependencies.
fn task(id: &str, title: &str, deps: &[&str]) -> Task {
    let now = Utc::now();
    Task {
        id: TaskId::new(id),
        title: title.into(),
        description: format!("test task {id}"),
        done_when: format!("task {id} is done"),
        depends_on: deps.iter().map(|d| TaskId::new(*d)).collect(),
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

/// Build the actor tree for run_graph testing (with RunControl and audit registry).
async fn run_graph_with_plan_slug(
    repo_root: std::path::PathBuf,
    plan_slug: String,
    tasks: Vec<Task>,
    developer_backend: Arc<dyn AgentBackend>,
    reviewer_backend: Arc<dyn AgentBackend>,
) -> makina_core::actors::RunReport {
    run_graph_with_plan_slug_and_config(
        repo_root,
        plan_slug,
        tasks,
        developer_backend,
        reviewer_backend,
        {
            let mut config = Config::resolve(GlobalConfig::default(), ProjectConfig::default());
            // Default to Manual mode so plan branch is not merged (for backward compatibility
            // with tests written before final-merge was implemented).
            config.merge.final_ = makina_core::config::FinalMerge::Manual;
            config
        },
    )
    .await
}

/// Same as run_graph_with_plan_slug but allows specifying the config.
async fn run_graph_with_plan_slug_and_config(
    repo_root: std::path::PathBuf,
    plan_slug: String,
    tasks: Vec<Task>,
    developer_backend: Arc<dyn AgentBackend>,
    reviewer_backend: Arc<dyn AgentBackend>,
    config: Config,
) -> makina_core::actors::RunReport {
    let worktree_manager = WorktreeManager::new(repo_root, "develop".into());
    let graph = Arc::new(tokio::sync::Mutex::new(TaskGraph {
        slug: plan_slug.clone(),
        tasks,
    }));

    let control = RunControl {
        run: RunId(42),
        sink: Arc::new(|_| {}),
        pause: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel: tokio_util::sync::CancellationToken::new(),
    };

    run_graph(
        Arc::clone(&graph),
        worktree_manager,
        config,
        developer_backend,
        reviewer_backend,
        control,
        Arc::new(NoopAuditRegistry),
        "run-slug".into(),
        "run-uid".into(),
        plan_slug,
        Arc::new(StructuredTextInterpreter::new()),
    )
    .await
    .expect("run_graph should succeed")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// **Proves "run creates plan branch and forking/merging targets it".**
///
/// With a real `plan_slug`, the run should:
/// 1. Create `plan/{plan_slug}` off `develop`.
/// 2. Fork task branches from `plan/{plan_slug}`, not `develop`.
/// 3. Squash-merge approved tasks into `plan/{plan_slug}`.
/// 4. Leave `develop` unchanged.
/// 5. Restore `repo_root` checkout to `develop` at run end.
///
/// # Fork-point proof (claim 2)
///
/// To prove that task branches fork from `plan/{plan_slug}` and NOT from
/// `develop`, this test:
///
/// 1. Pre-creates `plan/0030-demo` at the initial commit (simulating a restart
///    scenario where the plan branch already exists at an older tip).
/// 2. Advances `develop` with an extra commit, so `develop` is now **one commit
///    ahead** of `plan/0030-demo`.
/// 3. Runs the scheduler.  `create_plan_branch` is idempotent-on-restart: it
///    finds the branch exists and merely checks it out — it does **not** advance
///    it to `develop`'s new tip.
/// 4. After the run, the squash commit landed on `plan/0030-demo`.  Its parent
///    SHA equals the plan-branch tip **before the squash** — the original
///    initial commit, NOT `develop`'s advanced tip.  This proves the task branch
///    was forked from the plan-branch tip, not from `develop`.
#[tokio::test]
async fn run_creates_plan_branch_and_merges_into_it() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // ── Fork-point setup: pre-create plan/0030-demo at the initial commit ─────
    //
    // We then advance `develop` one commit beyond where the plan branch sits.
    // This lets us verify (step 4 below) that the task branched from the plan
    // branch tip, NOT from develop's more-advanced tip.
    let plan_slug = "0030-demo";
    let plan_branch = format!("plan/{plan_slug}");

    // Capture the plan branch tip BEFORE the run (initial commit SHA).
    run_git(&repo_root, &["branch", &plan_branch]);
    let plan_sha_before = branch_sha(&repo_root, &plan_branch);

    // Advance develop one commit so it is now *ahead* of plan/0030-demo.
    run_git(
        &repo_root,
        &["commit", "--allow-empty", "-m", "extra develop commit"],
    );

    // develop's state BEFORE the run (one commit ahead of plan_sha_before).
    let develop_before = branch_sha(&repo_root, "develop");
    assert_ne!(
        develop_before, plan_sha_before,
        "develop must be ahead of plan branch for the fork-point assertion to be meaningful"
    );

    // 1st prompt (developer) → dev output; 2nd (reviewer) → approve verdict.
    let backend = NoopBackend::with_responses(vec![
        "Implemented the feature.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]);

    let backend_ref = Arc::new(backend) as Arc<dyn AgentBackend>;

    // ── Run the loop with a real plan_slug ────────────────────────────────────
    let task = task("demo-task", "a test task", &[]);
    let report = run_graph_with_plan_slug(
        repo_root.clone(),
        plan_slug.to_string(),
        vec![task],
        Arc::clone(&backend_ref),
        backend_ref,
    )
    .await;

    // ── The task reached Done ─────────────────────────────────────────────────
    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(report.outcomes[0].0, TaskId::new("demo-task"));
    assert_eq!(report.outcomes[0].1, TaskState::Done);

    // ── plan/{plan_slug} was created ──────────────────────────────────────────
    assert!(
        branch_exists(&repo_root, &plan_branch),
        "plan/{plan_slug} branch must exist after the run"
    );

    // ── develop is unchanged ──────────────────────────────────────────────────
    let develop_after = branch_sha(&repo_root, "develop");
    assert_eq!(
        develop_before, develop_after,
        "develop HEAD must be unchanged (no final merge yet)"
    );

    // ── Squashed commit landed on plan/{plan_slug} ────────────────────────────
    let plan_commit_count = git_stdout(&repo_root, &["rev-list", "--count", &plan_branch]);
    // develop has 2 commits (initial + extra); plan branch started with 1.
    // After the squash-merge of the task, plan branch has 2 (initial + squash).
    // Either way, plan branch must have more total commits than develop's
    // baseline of 2 (initial + extra) — OR we simply verify the squash commit
    // subject references the task.
    assert!(
        plan_commit_count.parse::<i32>().unwrap_or(0) >= 2,
        "plan/{plan_slug} must have at least 2 commits (plan tip + squash); got {plan_commit_count}"
    );

    let plan_head = git_stdout(&repo_root, &["log", "-1", "--pretty=%s", &plan_branch]);
    assert!(
        plan_head.contains("demo-task"),
        "squash commit on plan branch must reference the task; got: {plan_head:?}"
    );

    // ── Task branched from plan/{plan_slug}, NOT from develop ─────────────────
    //
    // The squash commit on plan/{plan_slug} has one parent: the plan-branch tip
    // at the moment of the merge (i.e. `plan_sha_before`, the initial commit).
    // If the task had forked from `develop` instead, the squash commit's parent
    // would be `develop_before` (the extra commit).  We assert the squash
    // commit's parent equals `plan_sha_before` (NOT `develop_before`).
    let squash_parent = git_stdout(&repo_root, &["rev-parse", &format!("{plan_branch}^")]);
    assert_eq!(
        squash_parent, plan_sha_before,
        "squash commit's parent must be the plan-branch tip (task forked from \
         plan/{plan_slug}, not from develop)"
    );
    assert_ne!(
        squash_parent, develop_before,
        "squash commit's parent must NOT be develop's tip (that would mean the \
         task forked from develop, not from plan/{plan_slug})"
    );

    // ── repo_root is checked out to develop at run end ────────────────────────
    let checkout_branch = current_branch(&repo_root);
    assert_eq!(
        checkout_branch, "develop",
        "repo_root must be checked out to develop at run end"
    );
}

/// **Proves "ask path with empty plan_slug keeps legacy behavior".**
///
/// When `plan_slug` is empty (the ask path), the run should NOT create a plan
/// branch and should merge directly into `develop` (the original squash_merge.rs
/// behavior). This proves the ask path is unchanged.
#[tokio::test]
async fn ask_path_with_empty_plan_slug_uses_legacy_behavior() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let backend = NoopBackend::with_responses(vec![
        "Implemented the feature.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]);

    let backend_ref = Arc::new(backend) as Arc<dyn AgentBackend>;

    // For the ask path, we expect develop to gain one commit directly (the old
    // behavior before plan branches). We use Squash mode to land the commit
    // directly on develop (since ask path doesn't create a plan branch).
    let mut config = Config::resolve(GlobalConfig::default(), ProjectConfig::default());
    config.merge.final_ = makina_core::config::FinalMerge::Squash;

    let develop_count_before = commit_count(&repo_root);

    // ── Run with empty plan_slug (ask path) ────────────────────────────────────
    let task = task("ask-task", "a test task", &[]);
    let report = run_graph_with_plan_slug_and_config(
        repo_root.clone(),
        String::new(), // Empty plan_slug = ask path
        vec![task],
        Arc::clone(&backend_ref),
        backend_ref,
        config,
    )
    .await;

    // ── The task reached Done ─────────────────────────────────────────────────
    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(report.outcomes[0].0, TaskId::new("ask-task"));
    assert_eq!(report.outcomes[0].1, TaskState::Done);

    // ── No plan branch was created ────────────────────────────────────────────
    assert!(
        !branch_exists(&repo_root, "plan/"),
        "plan/ should not exist"
    );

    // ── develop gained exactly one commit (legacy behavior) ───────────────────
    let develop_count_after = commit_count(&repo_root);
    assert_eq!(
        develop_count_after,
        develop_count_before + 1,
        "exactly one squash commit must land on develop (ask path legacy)"
    );

    let head_subject = git_stdout(&repo_root, &["log", "-1", "--pretty=%s"]);
    assert!(
        head_subject.contains("ask-task"),
        "the squash commit subject must reference the task; got: {head_subject:?}"
    );
}

// ── Final merge tests (task 0084) ──────────────────────────────────────────────

/// **Proves "Squash mode lands one squash commit on base_branch".**
///
/// With all tasks Done and `[merge] final="squash"`, the run should:
/// 1. Create the plan branch and execute all tasks to Done.
/// 2. Land the plan branch onto develop as exactly one new squash commit.
/// 3. Set `plan_branch_left = None` in the report.
#[tokio::test]
async fn final_squash_lands_one_commit_on_base() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // Capture develop's state BEFORE the run.
    let develop_count_before = commit_count(&repo_root);

    let backend = NoopBackend::with_responses(vec![
        "Implemented the feature.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]);
    let backend_ref = Arc::new(backend) as Arc<dyn AgentBackend>;

    let task = task("squash-task", "a test task", &[]);
    let mut config = Config::resolve(GlobalConfig::default(), ProjectConfig::default());
    // Ensure Squash mode is set.
    config.merge.final_ = makina_core::config::FinalMerge::Squash;

    let worktree_manager = WorktreeManager::new(repo_root.clone(), "develop".into());
    let graph = Arc::new(tokio::sync::Mutex::new(TaskGraph {
        slug: "squash-test".into(),
        tasks: vec![task],
    }));

    let control = RunControl {
        run: RunId(42),
        sink: Arc::new(|_| {}),
        pause: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel: tokio_util::sync::CancellationToken::new(),
    };

    let report = run_graph(
        Arc::clone(&graph),
        worktree_manager,
        config,
        Arc::clone(&backend_ref),
        backend_ref,
        control,
        Arc::new(NoopAuditRegistry),
        "run-slug".into(),
        "run-uid".into(),
        "squash-test".into(),
        Arc::new(StructuredTextInterpreter::new()),
    )
    .await
    .expect("run_graph should succeed");

    // ── Task reached Done ────────────────────────────────────────────────────────
    assert_eq!(report.outcomes.len(), 1);
    let failure_reason = {
        let g = graph.lock().await;
        g.tasks[0].failure_reason.clone()
    };
    assert_eq!(
        report.outcomes[0].1,
        TaskState::Done,
        "task should finish; failure_reason={failure_reason:?}"
    );

    // ── Develop gained exactly one new commit ─────────────────────────────────────
    let develop_count_after = commit_count(&repo_root);
    assert_eq!(
        develop_count_after,
        develop_count_before + 1,
        "exactly one squash commit must land on develop"
    );

    // ── plan_branch_left is None ─────────────────────────────────────────────────
    assert_eq!(
        report.plan_branch_left, None,
        "plan_branch_left must be None when squash succeeds"
    );

    // ── The new commit is a squash (one commit, not a merge) ──────────────────────
    let develop_parents = git_stdout(&repo_root, &["rev-parse", "develop^@"]);
    // A squash commit has one parent; a merge has two (separated by space).
    let parent_count = develop_parents.split_whitespace().count();
    assert_eq!(
        parent_count, 1,
        "the new develop HEAD must have exactly one parent (squash, not merge)"
    );
}

/// **Proves "MergeCommit mode creates a merge commit".**
///
/// With all tasks Done and `[merge] final="merge-commit"`, the run should:
/// 1. Create the plan branch and execute all tasks to Done.
/// 2. Land the plan branch onto develop as a merge commit (two parents).
/// 3. Set `plan_branch_left = None` in the report.
#[tokio::test]
async fn final_merge_commit_creates_a_merge_commit() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let backend = NoopBackend::with_responses(vec![
        "Implemented the feature.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]);
    let backend_ref = Arc::new(backend) as Arc<dyn AgentBackend>;

    let task = task("merge-task", "a test task", &[]);
    let mut config = Config::resolve(GlobalConfig::default(), ProjectConfig::default());
    // Set MergeCommit mode.
    config.merge.final_ = makina_core::config::FinalMerge::MergeCommit;

    let worktree_manager = WorktreeManager::new(repo_root.clone(), "develop".into());
    let graph = Arc::new(tokio::sync::Mutex::new(TaskGraph {
        slug: "merge-test".into(),
        tasks: vec![task],
    }));

    let control = RunControl {
        run: RunId(42),
        sink: Arc::new(|_| {}),
        pause: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel: tokio_util::sync::CancellationToken::new(),
    };

    let report = run_graph(
        Arc::clone(&graph),
        worktree_manager,
        config,
        Arc::clone(&backend_ref),
        backend_ref,
        control,
        Arc::new(NoopAuditRegistry),
        "run-slug".into(),
        "run-uid".into(),
        "merge-test".into(),
        Arc::new(StructuredTextInterpreter::new()),
    )
    .await
    .expect("run_graph should succeed");

    // ── Task reached Done ────────────────────────────────────────────────────────
    assert_eq!(report.outcomes.len(), 1);
    let failure_reason = {
        let g = graph.lock().await;
        g.tasks[0].failure_reason.clone()
    };
    assert_eq!(
        report.outcomes[0].1,
        TaskState::Done,
        "task should finish; failure_reason={failure_reason:?}"
    );

    // ── plan_branch_left is None ─────────────────────────────────────────────────
    assert_eq!(
        report.plan_branch_left, None,
        "plan_branch_left must be None when merge-commit succeeds"
    );

    // ── The new commit is a merge (two parents) ──────────────────────────────────
    let develop_parents = git_stdout(&repo_root, &["rev-parse", "develop^@"]);
    // A merge commit has two parents (separated by space); a squash has one.
    let parent_count = develop_parents.split_whitespace().count();
    assert_eq!(
        parent_count, 2,
        "the new develop HEAD must have exactly two parents (merge commit)"
    );

    // ── The commit appears in --merges (proof it is a merge commit) ───────────────
    let merges = git_stdout(&repo_root, &["rev-list", "--merges", "develop"]);
    let develop_head = git_stdout(&repo_root, &["rev-parse", "develop"]);
    assert!(
        merges.contains(&develop_head),
        "the new develop HEAD must appear in git rev-list --merges"
    );
}

/// **Proves "Stage mode copies the plan diff to the main worktree as staged".**
#[tokio::test]
async fn final_stage_leaves_changes_staged_on_base() {
    let _home_guard = makina_core::HOME_ENV_LOCK.lock().await;
    let temp_home = tempfile::tempdir().expect("temp HOME");
    unsafe { std::env::set_var("HOME", temp_home.path()) };

    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let develop_count_before = commit_count(&repo_root);

    let backend = WritingBackend {
        file_name: "staged.txt".into(),
        content: "hello from plan\n".into(),
        response: "Implemented the feature.".into(),
    };
    let reviewer = NoopBackend::with_responses(vec![r#"{"verdict":"approve"}"#.into()]);
    let backend_ref = Arc::new(backend) as Arc<dyn AgentBackend>;
    let reviewer_ref = Arc::new(reviewer) as Arc<dyn AgentBackend>;

    let task = task("stage-task", "a staged task", &[]);
    let mut config = Config::resolve(GlobalConfig::default(), ProjectConfig::default());
    config.merge.final_ = makina_core::config::FinalMerge::Stage;

    let worktree_manager = WorktreeManager::new(repo_root.clone(), "develop".into());
    let graph = Arc::new(tokio::sync::Mutex::new(TaskGraph {
        slug: "stage-test".into(),
        tasks: vec![task],
    }));

    let control = RunControl {
        run: RunId(42),
        sink: Arc::new(|_| {}),
        pause: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel: tokio_util::sync::CancellationToken::new(),
    };

    let report = run_graph(
        Arc::clone(&graph),
        worktree_manager,
        config,
        backend_ref,
        reviewer_ref,
        control,
        Arc::new(NoopAuditRegistry),
        "run-slug".into(),
        "run-uid".into(),
        "stage-test".into(),
        Arc::new(StructuredTextInterpreter::new()),
    )
    .await
    .expect("run_graph should succeed");

    assert_eq!(report.outcomes.len(), 1);
    let failure_reason = {
        let g = graph.lock().await;
        g.tasks[0].failure_reason.clone()
    };
    assert_eq!(
        report.outcomes[0].1,
        TaskState::Done,
        "task should finish; failure_reason={failure_reason:?}"
    );
    assert_eq!(
        report.plan_branch_left, None,
        "stage mode should treat staged changes as the requested final state"
    );
    assert_eq!(
        current_branch(&repo_root),
        "develop",
        "repo_root should end on develop"
    );
    assert_eq!(
        commit_count(&repo_root),
        develop_count_before,
        "stage mode must not create a commit on develop"
    );
    assert_eq!(
        git_cached_name_status(&repo_root),
        "A\tstaged.txt",
        "plan diff should be staged in the main worktree"
    );
    assert_eq!(
        std::fs::read_to_string(repo_root.join("staged.txt")).unwrap(),
        "hello from plan\n"
    );
}

/// **Proves "Manual mode leaves plan branch and reports its name".**
///
/// With all tasks Done and `[merge] final="manual"`, the run should:
/// 1. Create the plan branch and execute all tasks to Done.
/// 2. Leave the plan branch unmerged (do NOT merge into develop).
/// 3. Set `plan_branch_left = Some(branch_name)` in the report.
#[tokio::test]
async fn final_manual_leaves_branch_and_reports_name() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let develop_sha_before = branch_sha(&repo_root, "develop");

    let backend = NoopBackend::with_responses(vec![
        "Implemented the feature.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]);
    let backend_ref = Arc::new(backend) as Arc<dyn AgentBackend>;

    let task = task("manual-task", "a test task", &[]);
    let mut config = Config::resolve(GlobalConfig::default(), ProjectConfig::default());
    // Set Manual mode.
    config.merge.final_ = makina_core::config::FinalMerge::Manual;

    let worktree_manager = WorktreeManager::new(repo_root.clone(), "develop".into());
    let graph = Arc::new(tokio::sync::Mutex::new(TaskGraph {
        slug: "manual-test".into(),
        tasks: vec![task],
    }));

    let control = RunControl {
        run: RunId(42),
        sink: Arc::new(|_| {}),
        pause: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel: tokio_util::sync::CancellationToken::new(),
    };

    let report = run_graph(
        Arc::clone(&graph),
        worktree_manager,
        config,
        Arc::clone(&backend_ref),
        backend_ref,
        control,
        Arc::new(NoopAuditRegistry),
        "run-slug".into(),
        "run-uid".into(),
        "manual-test".into(),
        Arc::new(StructuredTextInterpreter::new()),
    )
    .await
    .expect("run_graph should succeed");

    // ── Task reached Done ────────────────────────────────────────────────────────
    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(report.outcomes[0].1, TaskState::Done);

    // ── develop HEAD is unchanged ────────────────────────────────────────────────
    let develop_sha_after = branch_sha(&repo_root, "develop");
    assert_eq!(
        develop_sha_before, develop_sha_after,
        "develop HEAD must be unchanged in Manual mode"
    );

    // ── plan branch exists ───────────────────────────────────────────────────────
    assert!(
        branch_exists(&repo_root, "plan/manual-test"),
        "plan/manual-test must exist"
    );

    // ── plan_branch_left is Some(branch_name) ────────────────────────────────────
    assert_eq!(
        report.plan_branch_left,
        Some("plan/manual-test".into()),
        "plan_branch_left must be Some(plan/manual-test)"
    );
}

/// **Proves "Any failed task leaves plan branch in every mode".**
///
/// When any task reaches `Failed` (regardless of `[merge] final` mode), the run
/// should:
/// 1. Leave the plan branch unmerged (do NOT touch develop).
/// 2. Set `plan_branch_left = Some(branch_name)` in the report.
///
/// This test uses a rejected task to force a failure.
#[tokio::test]
async fn failed_task_leaves_branch_in_every_mode() {
    // Test all final-merge modes with a forced failure.
    let modes = vec![
        makina_core::config::FinalMerge::Squash,
        makina_core::config::FinalMerge::Stage,
        makina_core::config::FinalMerge::MergeCommit,
        makina_core::config::FinalMerge::Manual,
    ];

    for mode in modes {
        let repo_dir = setup_temp_repo();
        let repo_root = repo_dir.path().to_path_buf();

        let develop_sha_before = branch_sha(&repo_root, "develop");

        // Developer produces output; reviewer REJECTS (reviewer_iterations cap = 1).
        let backend = NoopBackend::with_responses(vec![
            "Implemented the feature.".into(),
            r#"{"verdict":"reject","feedback":"Needs more work"}"#.into(),
        ]);
        let backend_ref = Arc::new(backend) as Arc<dyn AgentBackend>;

        let task = task("fail-task", "a test task", &[]);
        let mut config = Config::resolve(GlobalConfig::default(), ProjectConfig::default());
        config.merge.final_ = mode;
        // Cap the review iterations to 1 so the task fails after rejection.
        config.caps.reviewer_iterations = 1;

        let worktree_manager = WorktreeManager::new(repo_root.clone(), "develop".into());
        let graph = Arc::new(tokio::sync::Mutex::new(TaskGraph {
            slug: format!("failed-{:?}", mode),
            tasks: vec![task],
        }));

        let control = RunControl {
            run: RunId(42),
            sink: Arc::new(|_| {}),
            pause: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cancel: tokio_util::sync::CancellationToken::new(),
        };

        let report = run_graph(
            Arc::clone(&graph),
            worktree_manager,
            config,
            Arc::clone(&backend_ref),
            backend_ref,
            control,
            Arc::new(NoopAuditRegistry),
            "run-slug".into(),
            "run-uid".into(),
            format!("failed-{:?}", mode),
            Arc::new(StructuredTextInterpreter::new()),
        )
        .await
        .expect("run_graph should succeed");

        // ── Task failed ──────────────────────────────────────────────────────────
        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(
            report.outcomes[0].1,
            TaskState::Failed,
            "task must fail for mode {:?}",
            mode
        );

        // ── develop HEAD is unchanged ────────────────────────────────────────────
        let develop_sha_after = branch_sha(&repo_root, "develop");
        assert_eq!(
            develop_sha_before, develop_sha_after,
            "develop HEAD must be unchanged when task fails (mode {:?})",
            mode
        );

        // ── plan branch exists ───────────────────────────────────────────────────
        assert!(
            branch_exists(&repo_root, &format!("plan/failed-{:?}", mode)),
            "plan branch must exist when task fails (mode {:?})",
            mode
        );

        // ── plan_branch_left is Some(branch_name) ────────────────────────────────
        assert_eq!(
            report.plan_branch_left,
            Some(format!("plan/failed-{:?}", mode)),
            "plan_branch_left must be Some when task fails (mode {:?})",
            mode
        );
    }
}
