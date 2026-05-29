//! Worktree + branch lifecycle manager for Makina.
//!
//! The [`WorktreeManager`] creates and tears down git worktrees and their
//! associated branches on behalf of the Supervisor.  Each task gets an
//! isolated checkout at `.worktrees/{task_id}/` on branch `task/{task_id}`,
//! branched off the configured base branch (typically `develop`).
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
//! `.worktrees/` is **not** committed — it is listed in the repo `.gitignore`.
//! `.tasks/` (the task artifact directory) IS committed and is NOT ignored.
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

use thiserror::Error;

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
    /// `task/{task_id}`.
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
/// let handle = mgr.create("my-task").await?;
/// // … dispatch work into handle.path …
/// mgr.remove("my-task").await?;
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
        }
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Create a worktree and branch for `task_id`.
    ///
    /// # What this does
    ///
    /// 1. Validates `task_id` (kebab-case, non-empty, no `..` or `/`).
    /// 2. Runs `git worktree prune` to clear stale registrations from crashed
    ///    previous runs.
    /// 3. Checks that neither the worktree path nor the branch already exist;
    ///    if either does, returns [`WorktreeError::GitCommandFailed`] with a
    ///    clear message rather than silently clobbering existing work.
    /// 4. Runs `git -C {repo_root} worktree add {worktree_path} -b
    ///    task/{task_id} {base_branch}` to create the branch off `base_branch`
    ///    and check it out in the new worktree.
    ///
    /// # Returns
    ///
    /// A [`WorktreeHandle`] with the task ID, branch name, and absolute path.
    ///
    /// # Errors
    ///
    /// - [`WorktreeError::InvalidTaskId`] — bad task ID.
    /// - [`WorktreeError::GitCommandFailed`] — any git command failed, or the
    ///   worktree/branch already exists.
    /// - [`WorktreeError::Io`] — filesystem check error.
    pub async fn create(&self, task_id: &str) -> Result<WorktreeHandle, WorktreeError> {
        validate_task_id(task_id)?;

        let worktree_path = self.worktree_path(task_id);
        let branch = format!("task/{task_id}");

        // Prune stale registrations first so git doesn't complain about
        // already-registered-but-gone paths from previous crashed runs.
        self.git_worktree_prune().await?;

        // Guard: reject if worktree path already exists on disk.
        if worktree_path.exists() {
            return Err(WorktreeError::GitCommandFailed {
                command: format!("guard: worktree path {worktree_path:?} already exists"),
                stderr: "Worktree path already exists; call remove() first or investigate."
                    .to_string(),
            });
        }

        // Guard: reject if the branch already exists.
        if self.branch_exists(&branch).await? {
            return Err(WorktreeError::GitCommandFailed {
                command: format!("guard: branch {branch:?} already exists"),
                stderr: "Branch already exists; call remove() first or investigate.".to_string(),
            });
        }

        // Create the worktree + branch in one atomic git command.
        let wt_path_str = worktree_path.to_string_lossy();
        self.run_git(
            &[
                "worktree",
                "add",
                &wt_path_str,
                "-b",
                &branch,
                &self.base_branch,
            ],
            &format!(
                "git -C {} worktree add {wt_path_str} -b {branch} {}",
                self.repo_root.display(),
                self.base_branch
            ),
        )
        .await?;

        Ok(WorktreeHandle {
            task_id: task_id.to_string(),
            branch,
            path: worktree_path,
        })
    }

    /// Remove the worktree and branch for `task_id`.
    ///
    /// # What this does
    ///
    /// 1. Validates `task_id`.
    /// 2. Runs `git -C {repo_root} worktree remove --force {worktree_path}`.
    ///    "Not found" / "not a worktree" outcomes are treated as success.
    /// 3. Runs `git -C {repo_root} branch -D task/{task_id}`.
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
    pub async fn remove(&self, task_id: &str) -> Result<(), WorktreeError> {
        validate_task_id(task_id)?;

        let worktree_path = self.worktree_path(task_id);
        let branch = format!("task/{task_id}");

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

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Compute the worktree path for a given task ID.
    fn worktree_path(&self, task_id: &str) -> PathBuf {
        self.repo_root.join(".worktrees").join(task_id)
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

    #[test]
    fn worktree_path_is_under_repo_root() {
        let mgr = WorktreeManager::new(PathBuf::from("/repo"), "develop".into());
        let path = mgr.worktree_path("my-task");
        assert_eq!(path, PathBuf::from("/repo/.worktrees/my-task"));
    }
}
