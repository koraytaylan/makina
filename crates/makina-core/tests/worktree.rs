//! Integration tests for [`makina_core::worktree::WorktreeManager`].
//!
//! # Acceptance criterion
//!
//! Task 20 "done when": dispatching a task creates the worktree/branch, and
//! completion removes them.  This file is the acceptance test suite.
//!
//! # Test isolation strategy
//!
//! All tests create a **fresh temporary git repository** via `tempfile::tempdir()`.
//! The real Makina repository is NEVER touched.  Each test:
//!
//! 1. Creates a temp dir.
//! 2. Initialises a bare-minimum git repo inside it (`git init`, user config,
//!    initial commit, ensures a `develop` branch exists).
//! 3. Constructs a [`WorktreeManager`] pointing at that temp repo.
//! 4. Calls `create` / `remove` and asserts the effects.
//!
//! Using real `git` (available in CI and development) means we test the actual
//! git integration, not a mock — while still being deterministic and having no
//! network access.

use std::process::Command;

use makina_core::worktree::{WorktreeError, WorktreeManager};

// ── Temp-repo helper ──────────────────────────────────────────────────────────

/// Create a minimal git repository in a new temporary directory.
///
/// Steps performed:
/// 1. `git init`
/// 2. `git config user.email test@example.com`
/// 3. `git config user.name "Test User"`
/// 4. Create an empty initial commit so HEAD and `develop` exist.
/// 5. Rename the default branch to `develop` (handles repos where git defaults
///    to `main` or `master`).
///
/// Returns the temp dir (must be kept alive for the duration of the test).
fn setup_temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let path = dir.path();

    // git init
    run_git(path, &["init"]);

    // Configure identity so commits work.
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test User"]);

    // Create an initial commit so HEAD is valid.
    // We need at least one commit for `git worktree add -b <branch> <base>` to work.
    run_git(path, &["commit", "--allow-empty", "-m", "Initial commit"]);

    // Ensure the branch is named `develop` regardless of git's init.defaultBranch.
    // First, check what the current branch name is.
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
/// Panics with a helpful message on failure.
fn run_git(path: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn git {:?}: {e}", args));
    assert!(
        status.success(),
        "git {:?} in {:?} exited with {:?}",
        args,
        path,
        status.code()
    );
}

/// Run a `git -C {path}` command and return its trimmed stdout, asserting exit 0.
fn git_stdout(path: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn git {:?}: {e}", args));
    assert!(
        output.status.success(),
        "git {:?} in {:?} exited with {:?}",
        args,
        path,
        output.status.code()
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Return true if the branch exists in the repo at `path`.
fn branch_exists(path: &std::path::Path, branch: &str) -> bool {
    let output = Command::new("git")
        .args(["-C", &path.to_string_lossy()])
        .args(["branch", "--list", branch])
        .output()
        .expect("git branch --list");
    let stdout = String::from_utf8_lossy(&output.stdout);
    !stdout.trim().is_empty()
}

// ── Acceptance test ───────────────────────────────────────────────────────────

/// **Acceptance criterion** — create makes the worktree + branch; remove tears them down.
///
/// Sequence (with plan-scoped naming `{plan_slug}--{task_id}`):
/// 1. `WorktreeManager::new(temp_repo, "develop").create(plan_slug, "sample-task")`
///    - `.makina/worktrees/{plan_slug}--sample-task/` must exist and contain a
///      valid git checkout.
///    - Branch `task/{plan_slug}--sample-task` must exist.
///    - Handle fields must match expectations.
/// 2. `.remove(plan_slug, "sample-task")`
///    - `.makina/worktrees/{plan_slug}--sample-task/` must be gone.
///    - Branch `task/{plan_slug}--sample-task` must be gone.
#[tokio::test]
async fn create_makes_worktree_and_branch_remove_tears_them_down() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let plan_slug = "0003-runtime-and-tui-hardening";
    let mgr = WorktreeManager::new(repo_root.clone(), "develop".into());

    // ── create ────────────────────────────────────────────────────────────────

    let handle = mgr
        .create(plan_slug, "sample-task")
        .await
        .expect("create sample-task must succeed");

    // Check handle fields.
    assert_eq!(handle.task_id, "sample-task");
    assert_eq!(
        handle.branch,
        "task/0003-runtime-and-tui-hardening--sample-task"
    );
    assert_eq!(
        handle.path,
        repo_root
            .join(".makina")
            .join("worktrees")
            .join("0003-runtime-and-tui-hardening--sample-task")
    );

    // Worktree directory must exist and be a git checkout.
    assert!(
        handle.path.exists(),
        "worktree dir {:?} must exist after create",
        handle.path
    );
    assert!(
        handle.path.join(".git").exists() || {
            // Worktrees use a `.git` FILE (not dir) pointing back to the main repo.
            let git_file = handle.path.join(".git");
            git_file.exists()
        },
        "worktree dir {:?} must contain a .git entry",
        handle.path
    );

    // Branch must exist in the repo.
    assert!(
        branch_exists(
            &repo_root,
            "task/0003-runtime-and-tui-hardening--sample-task"
        ),
        "branch task/0003-runtime-and-tui-hardening--sample-task must exist after create"
    );

    // ── remove ────────────────────────────────────────────────────────────────

    mgr.remove(plan_slug, "sample-task")
        .await
        .expect("remove sample-task must succeed");

    // Worktree directory must be gone.
    assert!(
        !handle.path.exists(),
        "worktree dir {:?} must be gone after remove",
        handle.path
    );

    // Branch must be gone.
    assert!(
        !branch_exists(
            &repo_root,
            "task/0003-runtime-and-tui-hardening--sample-task"
        ),
        "branch task/0003-runtime-and-tui-hardening--sample-task must be gone after remove"
    );
}

// ── Robustness: reclaim-on-conflict ──────────────────────────────────────────

/// A stale worktree/branch left by a prior interrupted run must be **reclaimed**,
/// not rejected.
///
/// Sequence:
/// 1. `create(plan_slug, task_id)` once to leave a real worktree + branch behind.
/// 2. Write a sentinel file into the worktree (proves it's the *old* checkout).
/// 3. `create(plan_slug, task_id)` again for the same pair.
///
/// The second `create` must return `Ok` with a fresh checkout at the same path,
/// on a branch newly cut from `base_branch` (so the sentinel is gone) — **never**
/// a [`WorktreeError::GitCommandFailed`].
#[tokio::test]
async fn create_reclaims_a_stale_slot() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let plan_slug = "sample-plan";
    let task_id = "stale-slot";
    let mgr = WorktreeManager::new(repo_root.clone(), "develop".into());

    // ── First create: leaves a real worktree + branch behind. ──────────────────
    let first = mgr
        .create(plan_slug, task_id)
        .await
        .expect("first create must succeed");

    // Write a sentinel into the stale worktree — it must NOT survive the reclaim.
    let sentinel = first.path.join("STALE_SENTINEL");
    std::fs::write(&sentinel, b"leftover from a prior interrupted run")
        .expect("writing sentinel into stale worktree must succeed");
    assert!(sentinel.exists(), "sentinel must exist before reclaim");

    // ── Second create for the SAME pair: must reclaim, not error. ──────────────
    let second = mgr
        .create(plan_slug, task_id)
        .await
        .expect("second create must reclaim the stale slot, not return GitCommandFailed");

    // Same path — the slot is reused, recreated fresh.
    assert_eq!(
        second.path, first.path,
        "reclaimed worktree must live at the same path"
    );
    assert_eq!(
        second.branch,
        format!("task/{plan_slug}--{task_id}"),
        "reclaimed worktree must use the same branch name"
    );

    // Fresh checkout: the sentinel from the prior attempt is gone.
    assert!(
        second.path.exists(),
        "reclaimed worktree dir {:?} must exist",
        second.path
    );
    assert!(
        !sentinel.exists(),
        "sentinel {:?} must be gone — the slot was reset fresh off base_branch",
        sentinel
    );

    // The branch must exist and point at base_branch (no commits of its own yet).
    assert!(
        branch_exists(&repo_root, &format!("task/{plan_slug}--{task_id}")),
        "branch task/{plan_slug}--{task_id} must exist after reclaim"
    );
    let base_head = git_stdout(&repo_root, &["rev-parse", "develop"]);
    let branch_head = git_stdout(
        &repo_root,
        &["rev-parse", &format!("task/{plan_slug}--{task_id}")],
    );
    assert_eq!(
        branch_head, base_head,
        "reclaimed branch must be cut fresh from base_branch (develop)"
    );
}

// ── Robustness: remove non-existent worktree is best-effort ──────────────────

/// Removing a worktree that was never created must not hard-fail.
///
/// The goal state (worktree gone, branch gone) is trivially true, so `remove`
/// should return `Ok(())`.
#[tokio::test]
async fn remove_nonexistent_worktree_is_idempotent() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let mgr = WorktreeManager::new(repo_root, "develop".into());

    // "ghost-task" was never created — remove should succeed anyway.
    mgr.remove("sample-plan", "ghost-task")
        .await
        .expect("remove of non-existent worktree must not fail");
}

/// Calling `remove` twice on the same task ID must succeed on both calls.
#[tokio::test]
async fn double_remove_is_idempotent() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let mgr = WorktreeManager::new(repo_root, "develop".into());

    // Create and then remove twice.
    mgr.create("sample-plan", "double-remove")
        .await
        .expect("create must succeed");
    mgr.remove("sample-plan", "double-remove")
        .await
        .expect("first remove must succeed");
    mgr.remove("sample-plan", "double-remove")
        .await
        .expect("second remove must succeed (idempotent)");
}

// ── Robustness: invalid task IDs are rejected ─────────────────────────────────

/// `create` with an empty task ID returns `WorktreeError::InvalidTaskId`.
#[tokio::test]
async fn create_with_empty_task_id_errors() {
    let repo_dir = setup_temp_repo();
    let mgr = WorktreeManager::new(repo_dir.path().to_path_buf(), "develop".into());

    let err = mgr
        .create("sample-plan", "")
        .await
        .expect_err("empty id must fail");
    assert!(
        matches!(err, WorktreeError::InvalidTaskId { .. }),
        "expected InvalidTaskId, got {err:?}"
    );
}

/// `create` with a path-traversal task ID returns `WorktreeError::InvalidTaskId`.
#[tokio::test]
async fn create_with_path_traversal_task_id_errors() {
    let repo_dir = setup_temp_repo();
    let mgr = WorktreeManager::new(repo_dir.path().to_path_buf(), "develop".into());

    let err = mgr
        .create("sample-plan", "../../etc/passwd")
        .await
        .expect_err("path-traversal id must fail");
    assert!(
        matches!(err, WorktreeError::InvalidTaskId { .. }),
        "expected InvalidTaskId, got {err:?}"
    );
}

/// `create` with a task ID containing a forward slash returns `InvalidTaskId`.
#[tokio::test]
async fn create_with_slash_in_task_id_errors() {
    let repo_dir = setup_temp_repo();
    let mgr = WorktreeManager::new(repo_dir.path().to_path_buf(), "develop".into());

    let err = mgr
        .create("sample-plan", "foo/bar")
        .await
        .expect_err("slash in id must fail");
    assert!(
        matches!(err, WorktreeError::InvalidTaskId { .. }),
        "expected InvalidTaskId, got {err:?}"
    );
}

/// `create` with an uppercase task ID returns `InvalidTaskId`.
#[tokio::test]
async fn create_with_uppercase_task_id_errors() {
    let repo_dir = setup_temp_repo();
    let mgr = WorktreeManager::new(repo_dir.path().to_path_buf(), "develop".into());

    let err = mgr
        .create("sample-plan", "MyTask")
        .await
        .expect_err("uppercase id must fail");
    assert!(
        matches!(err, WorktreeError::InvalidTaskId { .. }),
        "expected InvalidTaskId, got {err:?}"
    );
}

// ── Two independent task IDs yield two independent worktrees ─────────────────

/// Creating two distinct task IDs produces two independent, non-overlapping
/// worktrees, each with their own branch.
#[tokio::test]
async fn two_distinct_task_ids_yield_independent_worktrees() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let mgr = WorktreeManager::new(repo_root.clone(), "develop".into());

    let plan_slug = "sample-plan";
    let h1 = mgr
        .create(plan_slug, "task-alpha")
        .await
        .expect("create task-alpha");
    let h2 = mgr
        .create(plan_slug, "task-beta")
        .await
        .expect("create task-beta");

    // Both worktree paths must exist and be different.
    assert_ne!(h1.path, h2.path, "worktree paths must differ");
    assert!(h1.path.exists(), "task-alpha worktree must exist");
    assert!(h2.path.exists(), "task-beta worktree must exist");

    // Both branches must exist.
    assert!(branch_exists(&repo_root, "task/sample-plan--task-alpha"));
    assert!(branch_exists(&repo_root, "task/sample-plan--task-beta"));

    // Remove both.
    mgr.remove(plan_slug, "task-alpha")
        .await
        .expect("remove task-alpha");
    mgr.remove(plan_slug, "task-beta")
        .await
        .expect("remove task-beta");

    // Both gone.
    assert!(!h1.path.exists(), "task-alpha worktree must be gone");
    assert!(!h2.path.exists(), "task-beta worktree must be gone");
    assert!(!branch_exists(&repo_root, "task/sample-plan--task-alpha"));
    assert!(!branch_exists(&repo_root, "task/sample-plan--task-beta"));
}
