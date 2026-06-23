//! Worktree + branch lifecycle manager for Makina.
//!
//! The [`WorktreeManager`] creates and tears down git worktrees and their
//! associated branches on behalf of the Supervisor.  Each task gets an
//! isolated checkout at `~/.makina/projects/{project_ns}/worktrees/{short_worktree_name}/`
//! on branch `task/{short_worktree_name}`, branched off the configured base
//! branch (typically `develop`).  The `short_worktree_name` is a bounded,
//! deterministic `{plan#}-{task-trunc}-{hash4}` form (see
//! [`crate::paths::short_worktree_name`]).
//!
//! # Design
//!
//! The manager uses the **git CLI** via [`tokio::process::Command`].  We
//! deliberately avoid libgit2/git2 because their worktree support has
//! historically been incomplete and the CLI is the canonical, well-tested
//! interface.
//!
//! # Isolation note
//!
//! [`WorktreeManager`] takes `repo_root` and `base_branch` as explicit
//! constructor arguments instead of calling `Config::load_defaults()`.  This
//! is intentional: when a task runs inside a spawned worktree its current
//! working directory differs from the project root, so a CWD-relative config
//! load would fail or pick up wrong config.  Callers (i.e. the Supervisor in
//! task 21) must supply the repo root and base branch they already know.
//!
//! # Transient storage
//!
//! Worktrees live under `~/.makina/projects/{project_ns}/worktrees/` (off-repo)
//! so they are never committed and never need a gitignore rule in the project.
//! `.makina/tasks/` (the task artifact directory) IS committed and is NOT
//! ignored.
//!
//! # Concurrency
//!
//! The manager is safe to call concurrently for **distinct** task IDs: each
//! call operates on a different worktree path and branch name.  Concurrent
//! calls for the **same** task ID will likely race; callers are responsible for
//! ensuring a given task ID is only dispatched once at a time (enforced by the
//! concurrency controller in task 24).
//!
//! # Error behaviour
//!
//! Every git command's stderr is captured and surfaced in the returned error so
//! operators can diagnose failures without running git manually.
//!
//! ## `create` robustness
//!
//! Before creating, the manager runs `git worktree prune` to clear stale
//! registrations left by crashed previous runs.  If the worktree path or
//! branch already exists when `create` is called, an error is returned rather
//! than silently clobbering existing work — the caller should `remove` first or
//! investigate.
//!
//! ## `remove` best-effort teardown
//!
//! `remove` is designed to be called even after a crash.  "Not found" outcomes
//! for both `worktree remove` and `branch -D` are treated as success (the goal
//! — the worktree and branch no longer exist — is already achieved).  A
//! `git worktree prune` is also run so the git index stays tidy.  Filesystem
//! errors on `tokio::fs::remove_dir_all` after a successful `worktree remove`
//! are similarly treated as best-effort and ignored (the git-level cleanup
//! already happened).

use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::Mutex;

use crate::paths;

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors produced by [`WorktreeManager`].
#[derive(Debug, Error)]
pub enum WorktreeError {
    /// The supplied `task_id` is empty, contains path-traversal sequences
    /// (`..`), forward slashes, or other characters that are not safe for use
    /// as a kebab-case identifier.
    #[error(
        "invalid task_id {task_id:?}: must be non-empty, contain only \
         [a-z0-9-], and must not contain '..' or '/'"
    )]
    InvalidTaskId { task_id: String },

    /// A git command returned a non-zero exit code.
    ///
    /// `command` is the human-readable form of the full invocation (e.g.
    /// `"git worktree add ..."`) and `stderr` is the raw captured output.
    #[error("git command failed: {command}\nstderr: {stderr}")]
    GitCommandFailed { command: String, stderr: String },

    /// An I/O error occurred outside of a git subprocess (e.g. checking
    /// whether a path exists, or removing a leftover directory).
    #[error("I/O error in worktree manager: {0}")]
    Io(#[from] std::io::Error),
}

// ── WorktreeHandle ────────────────────────────────────────────────────────────

/// A handle to a created worktree.
///
/// Returned by [`WorktreeManager::create`].  Contains the paths and branch
/// name needed by the Supervisor to dispatch work into the worktree and later
/// call [`WorktreeManager::remove`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeHandle {
    /// The task ID this worktree was created for.
    pub task_id: String,

    /// The git branch checked out in this worktree, of the form
    /// `task/{short_worktree_name}`.
    pub branch: String,

    /// Absolute path to the worktree directory on the filesystem.
    pub path: PathBuf,
}

// ── WorktreeManager ───────────────────────────────────────────────────────────

/// Creates and tears down git worktrees and branches for Makina tasks.
///
/// # Example
///
/// ```rust,no_run
/// # use std::path::PathBuf;
/// # use makina_core::worktree::WorktreeManager;
/// # async fn example() -> Result<(), makina_core::worktree::WorktreeError> {
/// let mgr = WorktreeManager::new(PathBuf::from("/path/to/repo"), "develop".into());
/// let handle = mgr.create("my-plan", "my-task").await?;
/// // … dispatch work into handle.path …
/// mgr.remove("my-plan", "my-task").await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct WorktreeManager {
    /// Absolute path to the repository root.  All `git -C {repo_root}` calls
    /// use this as the working directory so the manager is correct regardless
    /// of the process's CWD.
    pub repo_root: PathBuf,

    /// The branch that new task branches are created off (e.g. `"develop"`).
    pub base_branch: String,

    /// Branch new task branches fork from. `None` ⇒ fork from `base_branch`
    /// (legacy ask-path). The run sets this to `plan/{plan_slug}`.
    pub fork_branch: Option<String>,

    /// Serializes worktree-lifecycle git operations ([`create`](Self::create) /
    /// [`remove`](Self::remove)) across concurrent drivers running against the
    /// **same repository**.
    ///
    /// `git worktree prune` — run by both `create` and `remove` — deletes any
    /// `.git/worktrees/<name>/` admin directory that looks incomplete.  Without
    /// this lock it races a *concurrent* `git worktree add` that has already
    /// created the admin dir but not yet written its `gitdir` file: prune deletes
    /// the half-built dir, then `add` fails with
    /// `could not open '.git/worktrees/<name>/gitdir' for writing: No such file
    /// or directory`, driving the task to a spurious `Failed`.  This guard makes
    /// `prune`/`add`/`remove` mutually exclusive on the shared repo.
    ///
    /// Shared across clones via the `Arc` — the manager is cloned per task (and
    /// the per-task clones flow through [`with_fork_branch`](Self::with_fork_branch)),
    /// so a single `new()` yields one process-wide lock for that repo.  The guard
    /// is held only around the (fast) git metadata ops, never across the
    /// dev/review work, so it does not serialize the tasks themselves.
    op_lock: Arc<Mutex<()>>,
}

impl WorktreeManager {
    /// Create a new manager for the given repository.
    ///
    /// # Arguments
    ///
    /// * `repo_root` — absolute path to the repository root.
    /// * `base_branch` — the branch that task branches are created off.
    pub fn new(repo_root: PathBuf, base_branch: String) -> Self {
        Self {
            repo_root,
            base_branch,
            fork_branch: None,
            op_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Builder method to set the fork point branch for task worktrees.
    ///
    /// When set, task worktrees fork from this branch instead of `base_branch`.
    /// Used by the run to fork from `plan/{plan_slug}` during the run.
    pub fn with_fork_branch(mut self, branch: String) -> Self {
        self.fork_branch = Some(branch);
        self
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Create a worktree and branch for `task_id` within `plan_slug`.
    ///
    /// The worktree directory and branch are plan-scoped using a bounded,
    /// deterministic short name `{plan#}-{task-trunc}-{hash4}` (see
    /// [`crate::paths::short_worktree_name`]) so that different plans never
    /// collide even when they share a task id.
    ///
    /// # What this does
    ///
    /// 1. Validates `task_id` (kebab-case, non-empty, no `..` or `/`).
    /// 2. Runs `git worktree prune` to clear stale registrations from crashed
    ///    previous runs.
    /// 3. **Reclaim-on-conflict:** if the worktree path or the branch already
    ///    exist — a stale slot left by a prior interrupted run — this does *not*
    ///    error. The short-name namespace is unambiguously Makina-owned transient
    ///    state, so the stale slot is reclaimed: it warns, calls
    ///    [`remove`](Self::remove) (idempotent), and falls through to recreate
    ///    the slot **fresh off the fork point** (`fork_branch` when set, else
    ///    `base_branch`; Option A — reset, not resume: the prior attempt was
    ///    never merged, so its work is throwaway).
    /// 4. Runs `git -C {repo_root} worktree add {worktree_path} -b
    ///    task/{short_worktree_name} {fork_point}` to create the branch off the
    ///    fork point and check it out in the new worktree.
    ///
    /// # Returns
    ///
    /// A [`WorktreeHandle`] with the task ID, branch name, and absolute path.
    ///
    /// # Errors
    ///
    /// - [`WorktreeError::InvalidTaskId`] — bad task ID.
    /// - [`WorktreeError::GitCommandFailed`] — any git command failed.
    /// - [`WorktreeError::Io`] — filesystem check error.
    pub async fn create(
        &self,
        plan_slug: &str,
        task_id: &str,
    ) -> Result<WorktreeHandle, WorktreeError> {
        validate_task_id(task_id)?;

        let worktree_path = self.worktree_path(plan_slug, task_id);
        let branch = format!("task/{}", paths::short_worktree_name(plan_slug, task_id));

        // Serialize this whole lifecycle against any concurrent driver's
        // `create`/`remove` on the same repo.  The `git worktree prune` below (and
        // in `remove`) deletes incomplete `.git/worktrees/<name>/` admin dirs,
        // which races a concurrent `git worktree add` mid-flight — see `op_lock`.
        // Held for the entire method; the reclaim path calls `remove_inner`, which
        // does NOT re-acquire, so the non-reentrant mutex never self-deadlocks.
        let _op_guard = self.op_lock.lock().await;

        // Prune stale registrations first so git doesn't complain about
        // already-registered-but-gone paths from previous crashed runs.
        self.git_worktree_prune().await?;

        // Reclaim-on-conflict: a leftover worktree path or branch is a stale slot
        // from a prior interrupted run, not a collision with someone else's work.
        // The short-name namespace is unambiguously Makina-owned transient state,
        // so reset the slot fresh off the fork point (Option A — the prior attempt
        // was never merged, so its work is throwaway).
        if worktree_path.exists() || self.branch_exists(&branch).await? {
            tracing::warn!(
                plan_slug,
                task_id,
                "reclaiming stale worktree/branch from a prior interrupted run"
            );
            // Already holding `op_lock`: call the non-locking inner helper.
            self.remove_inner(plan_slug, task_id).await?;
        }

        // Create the worktree + branch in one atomic git command.
        let wt_path_str = worktree_path.to_string_lossy();
        let fork_point = self.fork_branch.as_deref().unwrap_or(&self.base_branch);
        self.run_git(
            &["worktree", "add", &wt_path_str, "-b", &branch, fork_point],
            &format!(
                "git -C {} worktree add {wt_path_str} -b {branch} {fork_point}",
                self.repo_root.display(),
            ),
        )
        .await?;

        Ok(WorktreeHandle {
            task_id: task_id.to_string(),
            branch,
            path: worktree_path,
        })
    }

    /// Remove the worktree and branch for `task_id` within `plan_slug`.
    ///
    /// # What this does
    ///
    /// 1. Validates `task_id`.
    /// 2. Runs `git -C {repo_root} worktree remove --force {worktree_path}`.
    ///    "Not found" / "not a worktree" outcomes are treated as success.
    /// 3. Runs `git -C {repo_root} branch -D task/{short_worktree_name}`.
    ///    "Branch not found" outcomes are treated as success.
    /// 4. Runs `git worktree prune` to keep the git index tidy.
    ///
    /// # Idempotency
    ///
    /// Removing an already-removed worktree/branch does **not** fail: if the
    /// goal state (worktree gone, branch gone) is already achieved, the
    /// function returns `Ok(())`.  This makes teardown safe to retry after
    /// partial failures.
    ///
    /// # Errors
    ///
    /// - [`WorktreeError::InvalidTaskId`] — bad task ID.
    /// - [`WorktreeError::GitCommandFailed`] — a git command failed for a
    ///   reason other than "not found".
    ///
    /// # Concurrency
    ///
    /// Acquires `op_lock` so the teardown's `git worktree remove`/`branch -D`/
    /// `prune` cannot run concurrently with another driver's `create`/`remove`
    /// on the same repo (see `op_lock`).
    pub async fn remove(&self, plan_slug: &str, task_id: &str) -> Result<(), WorktreeError> {
        // Serialize against concurrent worktree-lifecycle git ops (see `op_lock`).
        let _op_guard = self.op_lock.lock().await;
        self.remove_inner(plan_slug, task_id).await
    }

    /// Worktree + branch teardown **without** acquiring `op_lock`.
    ///
    /// The caller MUST already hold `op_lock`: this is invoked by the public
    /// [`remove`](Self::remove) (which takes the lock) and by
    /// [`create`](Self::create)'s reclaim path (which holds the lock for its whole
    /// body).  Splitting the lock acquisition out of the body is what lets
    /// `create` reclaim a stale slot without dead-locking the non-reentrant mutex.
    async fn remove_inner(&self, plan_slug: &str, task_id: &str) -> Result<(), WorktreeError> {
        validate_task_id(task_id)?;

        let worktree_path = self.worktree_path(plan_slug, task_id);
        let branch = format!("task/{}", paths::short_worktree_name(plan_slug, task_id));

        // Remove the worktree (--force handles dirty checkouts; ignore
        // "not a worktree" / "not found" so the call is idempotent).
        let wt_path_str = worktree_path.to_string_lossy();
        let remove_result = self
            .run_git(
                &["worktree", "remove", "--force", &wt_path_str],
                &format!(
                    "git -C {} worktree remove --force {wt_path_str}",
                    self.repo_root.display()
                ),
            )
            .await;

        if let Err(WorktreeError::GitCommandFailed { ref stderr, .. }) = remove_result
            && !is_not_found_stderr(stderr)
        {
            // Treat "not a worktree", "not found", and similar "it's gone" messages
            // as success — the goal is achieved.
            return remove_result.map(|_| ());
        }

        // Attempt to clean up the directory if it still exists on disk (e.g.
        // git removed the worktree registration but left the directory).
        if worktree_path.exists() {
            // Best-effort: log but do not fail if this doesn't work.
            let _ = tokio::fs::remove_dir_all(&worktree_path).await;
        }

        // Delete the branch (treat "not found" as success).
        let delete_result = self
            .run_git(
                &["branch", "-D", &branch],
                &format!("git -C {} branch -D {branch}", self.repo_root.display()),
            )
            .await;

        if let Err(WorktreeError::GitCommandFailed { ref stderr, .. }) = delete_result
            && !is_not_found_stderr(stderr)
        {
            return delete_result.map(|_| ());
        }

        // Prune again to keep the git worktree list tidy.
        // Ignore errors here — we've already done what we can.
        let _ = self.git_worktree_prune().await;

        Ok(())
    }

    /// Create `plan/{plan_slug}` off `base_branch` and check it out in
    /// `repo_root`. Idempotent-on-restart: if the branch already exists, just
    /// check it out (a reclaimed run resumes on the same integration branch).
    pub async fn create_plan_branch(&self, plan_slug: &str) -> Result<String, WorktreeError> {
        let branch = format!("plan/{plan_slug}");
        if self.branch_exists(&branch).await? {
            self.run_git(
                &["checkout", &branch],
                &format!("git -C {} checkout {branch}", self.repo_root.display(),),
            )
            .await?;
        } else {
            self.run_git(
                &["checkout", "-b", &branch, &self.base_branch],
                &format!(
                    "git -C {} checkout -b {branch} {}",
                    self.repo_root.display(),
                    self.base_branch
                ),
            )
            .await?;
        }
        Ok(branch)
    }

    /// Check out `branch` in `repo_root` (used to restore `base_branch` at run end).
    pub async fn checkout(&self, branch: &str) -> Result<(), WorktreeError> {
        self.run_git(
            &["checkout", branch],
            &format!("git -C {} checkout {branch}", self.repo_root.display(),),
        )
        .await
        .map(|_| ())
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Compute the worktree path for a given plan slug + task ID.
    fn worktree_path(&self, plan_slug: &str, task_id: &str) -> PathBuf {
        paths::worktree(&self.repo_root, plan_slug, task_id)
    }

    /// Run `git worktree prune` in the repository.
    async fn git_worktree_prune(&self) -> Result<(), WorktreeError> {
        self.run_git(
            &["worktree", "prune"],
            &format!("git -C {} worktree prune", self.repo_root.display()),
        )
        .await
        .map(|_| ())
    }

    /// Check whether a branch with the given name exists in the repository.
    async fn branch_exists(&self, branch: &str) -> Result<bool, WorktreeError> {
        let output = tokio::process::Command::new("git")
            .args(["-C", &self.repo_root.to_string_lossy()])
            .args(["branch", "--list", branch])
            .output()
            .await
            .map_err(WorktreeError::Io)?;

        // `git branch --list <name>` exits 0 and prints the branch name if
        // it exists, or exits 0 and prints nothing if it does not.
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(!stdout.trim().is_empty())
    }

    /// Run a git command with `-C {repo_root}` and capture stdout/stderr.
    ///
    /// Returns `Ok(stdout)` on exit code 0, or [`WorktreeError::GitCommandFailed`]
    /// on non-zero exit, including the captured stderr.
    async fn run_git(&self, args: &[&str], human_command: &str) -> Result<String, WorktreeError> {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(args)
            .output()
            .await
            .map_err(WorktreeError::Io)?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(WorktreeError::GitCommandFailed {
                command: human_command.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }
}

// ── Validation ────────────────────────────────────────────────────────────────

/// Validate that `task_id` is a safe kebab-case identifier.
///
/// Rules enforced:
/// - Non-empty.
/// - Contains only ASCII lowercase letters, digits, and hyphens (`[a-z0-9-]`).
/// - Does not contain `..` (path traversal) or `/` (directory separator).
///
/// These rules are intentionally strict — task IDs come from a planner output
/// and are used in both filesystem paths and git branch names, so we accept a
/// small false-negative rate in exchange for preventing surprises.
fn validate_task_id(task_id: &str) -> Result<(), WorktreeError> {
    if task_id.is_empty() {
        return Err(WorktreeError::InvalidTaskId {
            task_id: task_id.to_string(),
        });
    }

    if task_id.contains("..") || task_id.contains('/') {
        return Err(WorktreeError::InvalidTaskId {
            task_id: task_id.to_string(),
        });
    }

    if !task_id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(WorktreeError::InvalidTaskId {
            task_id: task_id.to_string(),
        });
    }

    Ok(())
}

/// Return `true` if a git command's stderr indicates "not found" / "does not
/// exist" — meaning the object we tried to remove is already gone, which we
/// treat as success (idempotent teardown).
fn is_not_found_stderr(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    // Covers:
    //   "fatal: 'path' is not a working tree"
    //   "fatal: branch 'task/foo' not found"
    //   "error: pathspec '...' did not match any file(s) known to git"
    //   "'...worktrees/...' is not a working tree"
    lower.contains("not a working tree")
        || lower.contains("branch")
            && (lower.contains("not found") || lower.contains("does not exist"))
        || lower.contains("no such file or directory")
        || lower.contains("did not match any")
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Use the process-global HOME_ENV_LOCK from lib.rs so all test modules
    // serialize HOME mutations across crate boundaries.
    use crate::HOME_ENV_LOCK;

    // ── validate_task_id ──────────────────────────────────────────────────────

    #[test]
    fn valid_kebab_ids_are_accepted() {
        for id in &[
            "sample-task",
            "task-001",
            "a",
            "my-long-kebab-id-123",
            "abc",
        ] {
            validate_task_id(id).unwrap_or_else(|e| panic!("expected valid for {id:?}: {e}"));
        }
    }

    #[test]
    fn empty_id_is_rejected() {
        let err = validate_task_id("").expect_err("empty id must be rejected");
        assert!(matches!(err, WorktreeError::InvalidTaskId { .. }));
    }

    #[test]
    fn id_with_path_traversal_is_rejected() {
        let err = validate_task_id("../etc/passwd").expect_err(".. must be rejected");
        assert!(matches!(err, WorktreeError::InvalidTaskId { .. }));
    }

    #[test]
    fn id_with_forward_slash_is_rejected() {
        let err = validate_task_id("foo/bar").expect_err("/ must be rejected");
        assert!(matches!(err, WorktreeError::InvalidTaskId { .. }));
    }

    #[test]
    fn id_with_uppercase_is_rejected() {
        let err = validate_task_id("MyTask").expect_err("uppercase must be rejected");
        assert!(matches!(err, WorktreeError::InvalidTaskId { .. }));
    }

    #[test]
    fn id_with_space_is_rejected() {
        let err = validate_task_id("my task").expect_err("space must be rejected");
        assert!(matches!(err, WorktreeError::InvalidTaskId { .. }));
    }

    // ── is_not_found_stderr ───────────────────────────────────────────────────

    #[test]
    fn not_found_stderr_patterns_detected() {
        assert!(is_not_found_stderr("fatal: 'foo' is not a working tree"));
        assert!(is_not_found_stderr("fatal: branch 'task/foo' not found."));
        assert!(is_not_found_stderr("No such file or directory"));
    }

    #[test]
    fn real_error_stderr_not_detected_as_not_found() {
        assert!(!is_not_found_stderr(
            "fatal: could not read Username for 'https://github.com'"
        ));
        assert!(!is_not_found_stderr("fatal: repository not found"));
    }

    // ── WorktreeManager::worktree_path ────────────────────────────────────────

    /// The worktree path leaf must use the short name format
    /// `{plan#}-{task-trunc}-{hash4}` and resolve under `state_root`, not
    /// under `repo_root/.makina`.
    #[test]
    fn worktree_path_uses_short_name() {
        let _guard = HOME_ENV_LOCK.blocking_lock();
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let tmp_repo = tempfile::tempdir().expect("create temp repo");
        let repo_root = tmp_repo.path().to_path_buf();

        // SAFETY: serialised by HOME_ENV_LOCK
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        let mgr = WorktreeManager::new(repo_root.clone(), "develop".into());
        let path = mgr.worktree_path("0003-runtime-and-tui-hardening", "sample-task");

        // Must be under state_root, not repo_root/.makina.
        let state_root = crate::paths::state_root(&repo_root);
        assert!(
            path.starts_with(&state_root),
            "worktree_path must be under state_root ({}), got {}",
            state_root.display(),
            path.display()
        );
        assert!(
            !path.starts_with(repo_root.join(".makina")),
            "worktree_path must NOT be under repo_root/.makina"
        );

        // Leaf must be the short name (not the old plan--task form).
        let short =
            crate::paths::short_worktree_name("0003-runtime-and-tui-hardening", "sample-task");
        assert!(
            path.ends_with(&short),
            "worktree leaf must be short_worktree_name '{short}', got {}",
            path.display()
        );
        assert!(
            !path
                .to_string_lossy()
                .ends_with("0003-runtime-and-tui-hardening--sample-task"),
            "worktree path must not use the old plan--task format"
        );
    }

    // ── module doc-comment layout invariant ───────────────────────────────────

    /// Regression guard: the module-level doc-comments must describe the
    /// shipped `~/.makina/projects/{project_ns}/worktrees/{short_worktree_name}`
    /// layout with the new short-name scheme, never the old
    /// `.makina/worktrees/{plan_slug}--{task_id}/` in-repo paths.
    #[test]
    fn module_doc_describes_makina_plan_scoped_layout() {
        let src = include_str!("worktree.rs");
        // Only the leading `//!` module-doc block — stop at the first non-doc line.
        let module_doc: String = src
            .lines()
            .take_while(|l| {
                let t = l.trim_start();
                t.starts_with("//!") || t.is_empty()
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Relocated layout must be present with the new short-name form.
        assert!(
            module_doc.contains("~/.makina/projects/{project_ns}/worktrees/"),
            "module doc must reference the relocated `~/.makina/projects/` layout"
        );
        assert!(
            module_doc.contains("short_worktree_name"),
            "module doc must reference short_worktree_name"
        );

        // Old in-repo plan--task format must NOT appear in the module doc.
        assert!(
            !module_doc.contains(".makina/worktrees/{plan_slug}--{task_id}"),
            "module doc must not reference the old plan-slug--task_id format"
        );
    }
}
