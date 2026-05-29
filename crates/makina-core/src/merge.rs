//! Squash-merge of an approved task branch into the base branch (task 23).
//!
//! # What this module does
//!
//! When the Reviewer approves a task, the Supervisor squash-merges the task's
//! branch `task/{id}` into the base branch (`develop`) as **one** commit, then
//! tears down the worktree.  [`SquashMerger`] owns the git mechanics of that
//! merge, operating in the **main repository** (`repo_root`, which has
//! `base_branch` checked out — the worktrees are separate checkouts of the
//! `task/{id}` branches).
//!
//! # The git commands
//!
//! A clean merge is exactly two git invocations in `repo_root`:
//!
//! ```text
//! git -C {repo_root} merge --squash {task_branch}
//! git -C {repo_root} commit --allow-empty -m {commit_message}
//! ```
//!
//! `merge --squash` stages the *net diff* of `{task_branch}` onto the current
//! branch **without** creating a merge commit or recording the merged branch as a
//! parent — so the follow-up `commit` records the whole change as a single new
//! commit on `develop` (the task's individual commits do NOT appear on
//! `develop`).  This is precisely the "lands as ONE squashed commit" requirement.
//!
//! ## Empty-diff (no-op task) handling
//!
//! The `commit` uses `--allow-empty`.  This is deliberate: with the
//! `NoopBackend` the Developer makes no file changes, so `{task_branch}` carries
//! an empty (`--allow-empty`) commit and the squash stages nothing.  Rather than
//! treat "nothing to commit" as an error or a silent skip, we record an **empty
//! commit** so that *every* approved task lands a commit on `develop` — the
//! audit trail ("this task was merged") is preserved uniformly whether or not the
//! agent touched files.  Real agent edits are captured the same way (a non-empty
//! squashed commit).
//!
//! # The hard invariant: `develop` is NEVER left broken
//!
//! The Planner's dependency detection serializes overlapping tasks, so most
//! conflicts are prevented at the source.  A straggler conflict that still slips
//! through must be **reconciled, not allowed to corrupt `develop`**.  On ANY
//! failure path ([`MergeOutcome::Conflict`] or a hard [`MergeError`]) this module
//! guarantees the working tree and index are restored to a clean `base_branch`:
//!
//! 1. `git merge --abort` — unwinds an in-progress merge if `MERGE_HEAD` exists
//!    (best-effort; a `merge --squash` does NOT set `MERGE_HEAD`, so this is
//!    belt-and-suspenders for any git that leaves merge state behind).
//! 2. `git reset --hard HEAD` — discards any staged/working-tree changes the
//!    squash left behind, returning every tracked file to its committed state.
//! 3. `git clean -fd` — removes any untracked files/dirs the squash introduced.
//!
//! After this sequence `git status` is clean and `HEAD` is unchanged from before
//! the merge attempt: `develop` is pristine and the task can be safely retried or
//! failed.  This restore runs whether the conflict was detected from `merge
//! --squash`'s non-zero exit OR from a later step failing — it is the single
//! choke point for the invariant.
//!
//! # Agent-driven reconciliation seam (architecture)
//!
//! The architecture's intended conflict resolution is **agent-driven**: on a
//! conflict, spawn an agent in the task's worktree, give it the conflict, let it
//! resolve + re-commit on `task/{id}`, then retry the squash-merge once.  This
//! module returns a structured [`MergeOutcome::Conflict`] (with details) so the
//! Supervisor can drive that loop; the MVP Supervisor instead fails the task
//! safely (never corrupting `develop`) and leaves the agent-reconciliation as a
//! clearly-commented seam (see [`crate::actors::supervisor`]).  Because this
//! module already restores `develop` on conflict, a retry simply re-invokes
//! [`SquashMerger::squash_merge`] after the agent re-commits.
//!
//! # Design notes
//!
//! - Uses the **git CLI** via [`tokio::process::Command`], mirroring
//!   [`crate::worktree::WorktreeManager`] (the canonical, well-tested interface;
//!   libgit2 merge support is more painful).
//! - Every git command's stderr is captured and surfaced in [`MergeError`] so
//!   operators can diagnose a true infrastructure failure without re-running git.
//! - [`SquashMerger`] is `Clone` (cheap — two owned strings/paths) so it can live
//!   alongside the `WorktreeManager` in the Supervisor's `Args`.
//!
//! # Concurrency caveat (task 24)
//!
//! `squash_merge` mutates the **shared** `base_branch` checkout in `repo_root`
//! (it runs `merge`/`commit`/`reset` there).  Two concurrent merges into the same
//! `develop` would race on that single working tree.  The MVP runs tasks strictly
//! sequentially (one at a time), so there is no contention today; task 24
//! (concurrency) must **serialize merges into `develop`** (e.g. a mutex around
//! the merge, or a dedicated merge queue) even when tasks otherwise run in
//! parallel.

use std::path::PathBuf;

use thiserror::Error;

// ── MergeError ────────────────────────────────────────────────────────────────

/// An error that prevented the squash-merge from being **carried out** at all.
///
/// This is distinct from a merge *conflict* (a normal, expected outcome reported
/// via [`MergeOutcome::Conflict`]).  `MergeError` means a git command failed for
/// an infrastructure reason (git missing, repo broken, the restore itself
/// failing) — something the caller surfaces as a hard error.  Even when a
/// `MergeError` is returned, [`SquashMerger`] has attempted to restore
/// `base_branch` to a clean state first.
#[derive(Debug, Error)]
pub enum MergeError {
    /// A git command returned a non-zero exit code in a context where that was
    /// not an expected conflict.
    ///
    /// `command` is the human-readable invocation (e.g.
    /// `"git -C /repo commit --allow-empty -m ..."`) and `stderr` is the raw
    /// captured output.
    #[error("git command failed: {command}\nstderr: {stderr}")]
    GitCommandFailed {
        /// Human-readable form of the full git invocation that failed.
        command: String,
        /// Raw captured stderr from the failed git command.
        stderr: String,
    },

    /// A git subprocess could not be spawned/awaited (an OS-level I/O error,
    /// e.g. `git` not on `PATH`).
    #[error("I/O error invoking git in squash merger: {0}")]
    Io(#[from] std::io::Error),
}

// ── MergeOutcome ────────────────────────────────────────────────────────────────

/// The result of a [`SquashMerger::squash_merge`] attempt.
///
/// Distinguishes a clean squash-merge (one new commit landed on the base branch)
/// from a conflict (nothing landed; the base branch was safely restored).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The task branch was squash-merged successfully: exactly ONE new commit now
    /// sits on top of `base_branch` carrying the task's net change (possibly an
    /// empty commit for a no-op task — see the module docs).
    Merged,

    /// The squash-merge hit a conflict and was **not** applied.  `base_branch` has
    /// been restored to a clean state (no conflict markers, clean `git status`,
    /// `HEAD` unchanged).  `details` carries git's conflict output for diagnosis
    /// and for handing to an agent-driven reconciliation step.
    Conflict {
        /// Git's reported conflict detail (combined stdout+stderr of the failed
        /// `merge --squash`), for diagnosis / agent reconciliation.
        details: String,
    },
}

// ── SquashMerger ────────────────────────────────────────────────────────────────

/// Squash-merges approved task branches into the base branch.
///
/// Operates in the **main repository** working tree (`repo_root`), which has
/// `base_branch` checked out.  Stateless beyond its two configuration fields, so
/// a single instance is reused for every task's merge.
///
/// # Example
///
/// ```rust,no_run
/// # use std::path::PathBuf;
/// # use makina_core::merge::{SquashMerger, MergeOutcome};
/// # async fn example() -> Result<(), makina_core::merge::MergeError> {
/// let merger = SquashMerger::new(PathBuf::from("/path/to/repo"), "develop".into());
/// match merger.squash_merge("task/my-task", "task(my-task): My task").await? {
///     MergeOutcome::Merged => { /* tear down worktree, mark Done */ }
///     MergeOutcome::Conflict { details } => {
///         // develop is already clean; reconcile (agent) or fail safely.
///         eprintln!("merge conflict:\n{details}");
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct SquashMerger {
    /// Absolute path to the repository root.  All `git -C {repo_root}` calls use
    /// this as the working directory, so the merger is correct regardless of the
    /// process's CWD.  This checkout has `base_branch` checked out.
    pub repo_root: PathBuf,

    /// The branch task branches are merged **into** (e.g. `"develop"`).  Used in
    /// messages and (conceptually) the branch the restore returns to; the actual
    /// restore is `reset --hard HEAD`, which targets whatever `repo_root` has
    /// checked out (expected to be `base_branch`).
    pub base_branch: String,
}

impl SquashMerger {
    /// Create a new merger for the given repository and base branch.
    ///
    /// # Arguments
    ///
    /// * `repo_root` — absolute path to the repository root (has `base_branch`
    ///   checked out).
    /// * `base_branch` — the branch task branches are merged into.
    pub fn new(repo_root: PathBuf, base_branch: String) -> Self {
        Self {
            repo_root,
            base_branch,
        }
    }

    /// Squash-merge `task_branch` into `base_branch` as one commit.
    ///
    /// On success the base branch gains exactly ONE new commit (message ==
    /// `commit_message`) carrying the task branch's net change; the task branch's
    /// individual commits are NOT replayed onto the base branch.  An empty diff
    /// still records a commit (`--allow-empty`) — see the module docs.
    ///
    /// On a merge conflict the base branch is **restored to a clean state** and
    /// [`MergeOutcome::Conflict`] is returned (nothing is committed).
    ///
    /// # Returns
    ///
    /// - [`MergeOutcome::Merged`] — the squashed commit landed on `base_branch`.
    /// - [`MergeOutcome::Conflict`] — a conflict occurred; `base_branch` is clean
    ///   and unchanged (`details` carries git's conflict output).
    ///
    /// # Errors
    ///
    /// [`MergeError`] on a true infrastructure failure (git missing, the `commit`
    /// failing for a non-conflict reason, or the safety-restore itself failing).
    /// On the conflict path the restore runs before returning, so `base_branch`
    /// is left clean even when an error is propagated.
    pub async fn squash_merge(
        &self,
        task_branch: &str,
        commit_message: &str,
    ) -> Result<MergeOutcome, MergeError> {
        // ── Step 1: stage the net diff of the task branch (squash) ─────────────
        //
        // `merge --squash` does NOT create a commit or set MERGE_HEAD; it stages
        // the combined diff. A non-zero exit means a conflict (the common case)
        // — we treat that as MergeOutcome::Conflict after restoring `develop`.
        let squash = self
            .run_git_raw(&["merge", "--squash", task_branch])
            .await?;

        if !squash.status.success() {
            // Conflict (or another merge-level refusal). Capture git's detail,
            // then RESTORE the base branch to pristine before returning so
            // `develop` is never left with conflict markers / a half-staged index.
            let details = combine_output(&squash.stdout, &squash.stderr);
            self.restore_base_branch().await?;
            return Ok(MergeOutcome::Conflict { details });
        }

        // ── Step 2: record the staged change as ONE commit on the base branch ──
        //
        // `--allow-empty` so a no-op task (NoopBackend → empty task commit →
        // empty squash) still lands a commit, preserving a uniform audit trail.
        let commit = self
            .run_git_raw(&["commit", "--allow-empty", "-m", commit_message])
            .await?;

        if !commit.status.success() {
            // The squash staged cleanly but the commit failed for some other
            // reason (e.g. a misconfigured repo). This is NOT a normal conflict;
            // restore the base branch so we don't leave a staged-but-uncommitted
            // index on `develop`, then surface a hard error.
            let stderr = String::from_utf8_lossy(&commit.stderr).trim().to_string();
            // Best-effort restore; if the restore itself errors, prefer to report
            // the restore error (the invariant is more important to surface).
            self.restore_base_branch().await?;
            return Err(MergeError::GitCommandFailed {
                command: format!(
                    "git -C {} commit --allow-empty -m {commit_message:?}",
                    self.repo_root.display()
                ),
                stderr,
            });
        }

        Ok(MergeOutcome::Merged)
    }

    // ── Private helpers ─────────────────────────────────────────────────────────

    /// Restore the base-branch working tree + index to a pristine, committed
    /// state.  This is the single choke point that guarantees the invariant
    /// "`develop` is never left broken" on any failure path.
    ///
    /// Sequence (each step is independently safe to run even if there is nothing
    /// to undo):
    /// 1. `git merge --abort` — unwind an in-progress merge if `MERGE_HEAD`
    ///    exists. `merge --squash` does not set `MERGE_HEAD`, so this is normally
    ///    a no-op that exits non-zero ("no merge to abort") — which we IGNORE on
    ///    purpose. It is here as belt-and-suspenders for any git/edge case that
    ///    leaves merge state behind.
    /// 2. `git reset --hard HEAD` — discard staged + working-tree changes (this is
    ///    what actually clears a squash's staged-but-uncommitted diff / conflict
    ///    markers in tracked files). MUST succeed.
    /// 3. `git clean -fd` — remove untracked files/dirs the squash introduced.
    ///    MUST succeed.
    ///
    /// Steps 2 and 3 are load-bearing for the invariant, so a failure there is a
    /// hard [`MergeError`] (the caller then knows `develop` may be dirty and can
    /// halt rather than proceed on a corrupted base).
    async fn restore_base_branch(&self) -> Result<(), MergeError> {
        // 1) Best-effort: abort an in-progress merge if one exists. Ignore the
        //    "there is no merge to abort" non-zero exit entirely.
        let _ = self.run_git_raw(&["merge", "--abort"]).await;

        // 2) Hard reset the index + working tree to HEAD (clears the staged
        //    squash diff and any conflict markers in tracked files).
        self.run_git_checked(
            &["reset", "--hard", "HEAD"],
            &format!("git -C {} reset --hard HEAD", self.repo_root.display()),
        )
        .await?;

        // 3) Remove any untracked files/dirs the squash may have introduced.
        self.run_git_checked(
            &["clean", "-fd"],
            &format!("git -C {} clean -fd", self.repo_root.display()),
        )
        .await?;

        Ok(())
    }

    /// Run a `git -C {repo_root} {args}` command and return the raw [`Output`]
    /// (caller inspects the exit status). Maps spawn/await failures to
    /// [`MergeError::Io`].
    ///
    /// [`Output`]: std::process::Output
    async fn run_git_raw(&self, args: &[&str]) -> Result<std::process::Output, MergeError> {
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(args)
            .output()
            .await
            .map_err(MergeError::Io)
    }

    /// Run a `git -C {repo_root} {args}` command that MUST succeed; map a non-zero
    /// exit to [`MergeError::GitCommandFailed`] (with `human_command` + captured
    /// stderr).
    async fn run_git_checked(&self, args: &[&str], human_command: &str) -> Result<(), MergeError> {
        let output = self.run_git_raw(args).await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(MergeError::GitCommandFailed {
                command: human_command.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }
}

// ── Output helper ───────────────────────────────────────────────────────────────

/// Combine a git command's stdout and stderr into one human-readable string.
///
/// `merge --squash` prints conflict detail to stdout (the "CONFLICT (content):"
/// lines) and some notices to stderr, so both are surfaced for diagnosis /
/// agent reconciliation.  Decoded lossily (git output is text, not guaranteed
/// UTF-8); empty streams are omitted.
fn combine_output(stdout: &[u8], stderr: &[u8]) -> String {
    let out = String::from_utf8_lossy(stdout);
    let err = String::from_utf8_lossy(stderr);

    let out = out.trim_end();
    let err = err.trim_end();

    match (out.is_empty(), err.is_empty()) {
        (true, true) => String::new(),
        (false, true) => out.to_string(),
        (true, false) => format!("stderr:\n{err}"),
        (false, false) => format!("{out}\nstderr:\n{err}"),
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Unit tests for the pure helper(s).  The git-driven behaviour (clean squash
    //! producing ONE commit, conflict leaving `develop` pristine) is covered by
    //! the integration tests in `tests/squash_merge.rs` against real temp repos.

    use super::*;

    #[test]
    fn combine_output_merges_streams() {
        assert_eq!(combine_output(b"out", b""), "out");
        assert_eq!(combine_output(b"", b"err"), "stderr:\nerr");
        assert_eq!(combine_output(b"out", b"err"), "out\nstderr:\nerr");
        assert_eq!(combine_output(b"", b""), "");
    }

    #[test]
    fn merger_stores_config() {
        let m = SquashMerger::new(PathBuf::from("/repo"), "develop".into());
        assert_eq!(m.repo_root, PathBuf::from("/repo"));
        assert_eq!(m.base_branch, "develop");
    }
}
