//! Integration tests for the **squash-merge** of an approved task branch into
//! `develop` (task 23), against real git in temporary repositories.
//!
//! # Acceptance criterion
//!
//! Task 23 "done when": *an approved task lands on `develop` as one squashed
//! commit and its worktree is torn down.*  These tests prove that, plus the hard
//! invariant that a conflict NEVER leaves `develop` broken:
//!
//! 1. **Component happy path** — [`SquashMerger::squash_merge`] of a `task/foo`
//!    branch with two real commits lands exactly ONE new commit on `develop`,
//!    carrying the file content, with the two originals NOT individually present
//!    (it is squashed).
//! 2. **Component conflict safety** — a `task/bar` branch that conflicts with
//!    `develop` yields [`MergeOutcome::Conflict`] AND leaves `develop` clean
//!    (no conflict markers, clean `git status`, `HEAD` unchanged).  This is the
//!    critical invariant.
//! 3. **Loop integration** — the full scheduler loop (NoopBackend, approve
//!    verdict) over a temp repo lands a squashed commit referencing the task on
//!    `develop` and tears the worktree + branch down.
//!
//! # Test-strategy compliance (see `docs/spec/testing-strategy.md`)
//!
//! - Backend is always `NoopBackend` — no real agent CLI, no model call.
//! - Determinism via awaited scheduler completion — no arbitrary sleeps.
//! - Each test uses a fresh temporary git repo (`tempfile`); the real Makina repo
//!   is never touched.  Real `git` is used (available in CI/dev) so we test the
//!   actual git integration, mirroring `tests/worktree.rs` /
//!   `tests/develop_review_loop.rs`.

use std::process::Command;
use std::sync::Arc;

use chrono::Utc;

mod common;

use makina_core::backend::AgentBackend;
use makina_core::backend::noop::NoopBackend;
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::merge::{MergeOutcome, SquashMerger};
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};
use makina_core::test_support::{run_git, setup_temp_repo};

// ── Temp-repo helpers (mirror tests/worktree.rs & tests/develop_review_loop.rs) ──

/// Create a minimal git repository in a new temporary directory, on a `develop`
/// branch with one initial commit.  Returns the temp dir (keep it alive).
/// Run a `git -C {path}` command, asserting it exits 0.
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

/// The current branch name in the repo at `path`.
fn current_branch(path: &std::path::Path) -> String {
    git_stdout(path, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// The current `HEAD` commit SHA in the repo at `path`.
fn head_sha(path: &std::path::Path) -> String {
    git_stdout(path, &["rev-parse", "HEAD"])
}

/// The number of commits reachable from `HEAD` in the repo at `path`.
fn commit_count(path: &std::path::Path) -> usize {
    git_stdout(path, &["rev-list", "--count", "HEAD"])
        .parse()
        .expect("commit count is a number")
}

/// `git status --porcelain` output (empty == clean working tree + index).
fn status_porcelain(path: &std::path::Path) -> String {
    git_stdout(path, &["status", "--porcelain"])
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

/// Write `content` to `path/name`, asserting success.
fn write_file(path: &std::path::Path, name: &str, content: &str) {
    std::fs::write(path.join(name), content).expect("write file");
}

/// Read `path/name` as a string.
fn read_file(path: &std::path::Path, name: &str) -> String {
    std::fs::read_to_string(path.join(name)).expect("read file")
}

// ── Test 1: component happy path — ONE squashed commit on develop ────────────────

/// **Proves "one squashed commit on develop".**
///
/// Setup: a `develop` branch and a `task/foo` branch with TWO real commits that
/// add then change a file.  After `squash_merge("task/foo", msg)`:
/// - `develop` has exactly ONE new commit (its HEAD message == `msg`);
/// - the file content from `task/foo` is present on `develop`;
/// - the two original commit subjects are NOT individually on `develop` (squashed).
#[tokio::test]
async fn squash_merge_lands_one_squashed_commit_on_develop() {
    let repo_dir = setup_temp_repo();
    let repo = repo_dir.path();

    // Build `task/foo` off develop with two commits touching a file.
    run_git(repo, &["checkout", "-b", "task/foo"]);
    write_file(repo, "feature.txt", "line one\n");
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "-m", "task-foo: add feature line one"]);
    write_file(repo, "feature.txt", "line one\nline two\n");
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "-m", "task-foo: add feature line two"]);

    // Back to develop (the main checkout the merger operates in).
    run_git(repo, &["checkout", "develop"]);
    let develop_before = head_sha(repo);
    let count_before = commit_count(repo);

    // ── Squash-merge ────────────────────────────────────────────────────────
    let merger = SquashMerger::new(repo.to_path_buf(), "develop".into());
    let msg = "task(foo): Implement foo";
    let outcome = merger
        .squash_merge("task/foo", msg)
        .await
        .expect("squash_merge must not hard-error");
    assert_eq!(
        outcome,
        MergeOutcome::Merged,
        "a clean merge must be Merged"
    );

    // Exactly ONE new commit on develop.
    assert_eq!(
        commit_count(repo),
        count_before + 1,
        "develop must gain exactly one commit (squashed)"
    );
    assert_ne!(head_sha(repo), develop_before, "develop HEAD must advance");

    // HEAD message is the squash message.
    let head_subject = git_stdout(repo, &["log", "-1", "--pretty=%s"]);
    assert_eq!(head_subject, msg, "HEAD subject must be the squash message");

    // The file content from task/foo is present on develop.
    assert_eq!(
        read_file(repo, "feature.txt"),
        "line one\nline two\n",
        "the task branch's file content must be on develop"
    );

    // The two original commit subjects are NOT individually on develop — it is a
    // squash, not a merge/replay.  (Only the new squash commit references foo.)
    let develop_log = git_stdout(repo, &["log", "--pretty=%s"]);
    assert!(
        !develop_log.contains("task-foo: add feature line one"),
        "the first original commit must NOT be on develop (squashed); log:\n{develop_log}"
    );
    assert!(
        !develop_log.contains("task-foo: add feature line two"),
        "the second original commit must NOT be on develop (squashed); log:\n{develop_log}"
    );

    // Working tree is clean afterwards.
    assert!(
        status_porcelain(repo).is_empty(),
        "develop working tree must be clean after a successful merge"
    );
}

/// A no-op task (empty branch commit, mirroring the NoopBackend path) still lands
/// a commit on `develop` via `--allow-empty` — documenting that design choice.
#[tokio::test]
async fn squash_merge_of_empty_branch_still_lands_a_commit() {
    let repo_dir = setup_temp_repo();
    let repo = repo_dir.path();

    // `task/empty` carries only an empty commit (as the Developer's --allow-empty
    // commit does with the NoopBackend).
    run_git(repo, &["checkout", "-b", "task/empty"]);
    run_git(repo, &["commit", "--allow-empty", "-m", "task-empty: noop"]);
    run_git(repo, &["checkout", "develop"]);
    let count_before = commit_count(repo);

    let merger = SquashMerger::new(repo.to_path_buf(), "develop".into());
    let outcome = merger
        .squash_merge("task/empty", "task(empty): noop task")
        .await
        .expect("squash_merge must not hard-error");

    assert_eq!(
        outcome,
        MergeOutcome::Merged,
        "an empty-diff merge is still Merged (--allow-empty)"
    );
    assert_eq!(
        commit_count(repo),
        count_before + 1,
        "a no-op task still records one commit on develop"
    );
    assert_eq!(
        git_stdout(repo, &["log", "-1", "--pretty=%s"]),
        "task(empty): noop task"
    );
}

// ── Test 2: component conflict safety — develop left CLEAN (the invariant) ────────

/// **The critical invariant.**
///
/// Setup: `develop` and `task/bar` BOTH modify the same line of the same file
/// differently — a real conflict.  After `squash_merge("task/bar", msg)`:
/// - the outcome is [`MergeOutcome::Conflict`];
/// - `develop` is left CLEAN: no conflict markers in files, `git status` clean,
///   and `HEAD` unchanged from before the merge attempt.
#[tokio::test]
async fn squash_merge_conflict_leaves_develop_clean() {
    let repo_dir = setup_temp_repo();
    let repo = repo_dir.path();

    // Seed a shared file on develop, committed.
    write_file(repo, "shared.txt", "original\n");
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "-m", "seed shared.txt"]);

    // task/bar changes the line one way.
    run_git(repo, &["checkout", "-b", "task/bar"]);
    write_file(repo, "shared.txt", "from-task-bar\n");
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "-m", "task-bar: change shared line"]);

    // develop changes the SAME line a different way → guaranteed conflict.
    run_git(repo, &["checkout", "develop"]);
    write_file(repo, "shared.txt", "from-develop\n");
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "-m", "develop: change shared line"]);

    // Snapshot develop's pristine state BEFORE the merge attempt.
    let head_before = head_sha(repo);
    let count_before = commit_count(repo);

    // ── Attempt the squash-merge — must conflict ──────────────────────────────
    let merger = SquashMerger::new(repo.to_path_buf(), "develop".into());
    let outcome = merger
        .squash_merge("task/bar", "task(bar): change shared line")
        .await
        .expect("a conflict must be reported as Ok(Conflict), not a hard error");

    match &outcome {
        MergeOutcome::Conflict { details } => {
            assert!(
                !details.is_empty(),
                "conflict details should carry git's output for reconciliation"
            );
        }
        other => panic!("expected MergeOutcome::Conflict, got {other:?}"),
    }

    // ── INVARIANT: develop is left CLEAN and unchanged ────────────────────────

    // 1. HEAD is unchanged (nothing was committed).
    assert_eq!(
        head_sha(repo),
        head_before,
        "develop HEAD must be unchanged after a conflict (nothing committed)"
    );
    assert_eq!(
        commit_count(repo),
        count_before,
        "develop commit count must be unchanged after a conflict"
    );

    // 2. The working tree + index are clean (no half-staged squash, no markers).
    assert!(
        status_porcelain(repo).is_empty(),
        "develop working tree/index must be CLEAN after a conflict; status:\n{}",
        status_porcelain(repo)
    );

    // 3. The shared file still has develop's content, with NO conflict markers.
    let shared = read_file(repo, "shared.txt");
    assert_eq!(
        shared, "from-develop\n",
        "shared.txt must retain develop's content (the merge did not apply)"
    );
    for marker in ["<<<<<<<", "=======", ">>>>>>>"] {
        assert!(
            !shared.contains(marker),
            "shared.txt must contain NO conflict marker {marker:?}; got:\n{shared}"
        );
    }

    // 4. We are still on the develop branch (the merge did not leave us detached
    //    or mid-merge).
    assert_eq!(current_branch(repo), "develop", "must still be on develop");
}

// ── Test 3: loop integration — approve lands a commit + tears down the worktree ──

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
        failure_reason: None,
    }
}

/// **Proves "approved task lands on develop + worktree torn down".**
///
/// Runs the full scheduler loop with the NoopBackend (approve verdict) over a
/// temp repo.  With the NoopBackend the Developer makes no file change, so its
/// `--allow-empty` commit on `task/{id}` + the (empty) squash still land ONE
/// commit on `develop`.  Asserts:
/// - the task reaches `Done`;
/// - `develop` gained exactly one new commit whose subject references the task;
/// - the worktree directory and `task/{id}` branch were torn down.
#[tokio::test]
async fn approve_squash_merges_to_develop_and_tears_down_worktree() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // 1st prompt (developer) → dev output; 2nd (reviewer) → approve verdict.
    let backend = NoopBackend::with_responses(vec![
        "Implemented the feature.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]);

    let graph = TaskGraph {
        slug: "merge-loop-test".into(),
        tasks: vec![task("land-it", "the thing lands on develop", &[])],
    };

    // develop's state BEFORE the run (must be on develop in the main checkout).
    assert_eq!(current_branch(&repo_root), "develop");
    let count_before = commit_count(&repo_root);

    // ── Run the loop ──────────────────────────────────────────────────────────
    let (report, graph_ref) = common::run_graph_in_repo(
        repo_root.clone(),
        graph,
        Arc::new(backend) as Arc<dyn AgentBackend>,
        Config::resolve(GlobalConfig::default(), ProjectConfig::default()),
    )
    .await;

    // The task reached Done.
    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("land-it"), TaskState::Done)],
        "land-it should reach Done after approve + merge"
    );
    let snapshot = common::graph_snapshot(&graph_ref).await;
    assert_eq!(
        snapshot.get(&TaskId::new("land-it")).unwrap().state,
        TaskState::Done
    );

    // ── A squashed commit landed on develop ───────────────────────────────────
    assert_eq!(
        commit_count(&repo_root),
        count_before + 1,
        "exactly one squashed commit must land on develop"
    );
    let head_subject = git_stdout(&repo_root, &["log", "-1", "--pretty=%s"]);
    assert!(
        head_subject.contains("land-it"),
        "the squash commit subject must reference the task; got: {head_subject:?}"
    );

    // ── The worktree + branch were torn down ──────────────────────────────────
    let worktree_path = common::scheduler_worktree_path(&repo_root, "land-it");
    let branch = common::scheduler_task_branch("land-it");
    assert!(
        !worktree_path.exists(),
        "worktree dir must be gone after the run"
    );
    assert!(
        !branch_exists(&repo_root, &branch),
        "{branch} branch must be gone after the run"
    );

    // develop's working tree is clean (excluding .makina/ which the supervisor
    // persistence and worktree machinery write as an intentionally-untracked
    // artifact directory).
    let dirty: String = status_porcelain(&repo_root)
        .lines()
        .filter(|line| !line.contains(".makina/"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        dirty.is_empty(),
        "develop must be clean after the run; status:\n{}",
        status_porcelain(&repo_root)
    );
}
