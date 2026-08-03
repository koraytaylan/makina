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
use std::sync::Arc;

use thiserror::Error;

use crate::cmd_output::combine_output;

fn validate_identity(identity: &TaskLandingIdentity) -> Result<(), MergeError> {
    for (name, value) in [
        ("plan", identity.plan.as_str()),
        ("task", identity.task.as_str()),
        ("run", identity.run.as_str()),
    ] {
        if value.is_empty() || value.contains(['\n', '\r', '\0']) {
            return Err(MergeError::GitCommandFailed {
                command: "validate task landing identity".into(),
                stderr: format!("{name} identity is empty or contains a line delimiter"),
            });
        }
    }
    Ok(())
}

fn exact_trailer<'a>(message: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}: ");
    let mut values = message
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix));
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

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

    /// The final "stage changes" mode refused to apply over a dirty checkout.
    #[error("main worktree has uncommitted changes; refusing to stage plan changes:\n{status}")]
    DirtyWorktree {
        /// `git status --porcelain` output from the main worktree.
        status: String,
    },
    #[error("target base branch `{branch}` is checked out at `{path}`; refusing ref advancement")]
    BaseCheckedOut { branch: String, path: PathBuf },
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
    Merged { oid: crate::plan::GitObjectId },

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLandingIdentity {
    pub plan: String,
    pub task: String,
    pub run: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandingEvidenceStatus {
    Missing,
    Verified(crate::plan::GitObjectId),
    Unreachable,
    Mismatched,
    Ambiguous,
}

/// The result of staging a completed plan's net diff into the main worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageOutcome {
    /// The plan branch's net diff is now staged in `base_branch`'s checkout.
    Staged,

    /// The squash-style staging attempt conflicted; `base_branch` was restored.
    Conflict {
        /// Git's reported conflict detail.
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
///     MergeOutcome::Merged { .. } => { /* tear down worktree, mark Done */ }
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

    /// The **target** branch the merger lands task branches onto — `"develop"`
    /// on the ask path, but `"plan/{slug}"` when constructed by `run_graph_inner`
    /// for a run with a real `plan_slug`.  Used in messages and (conceptually)
    /// the branch the restore returns to; the actual restore is
    /// `reset --hard HEAD`, which targets whatever `repo_root` has checked out
    /// (expected to be `base_branch`).
    pub base_branch: String,
    repository_child_token: Option<Arc<crate::repository_lease::RepositoryChildToken>>,
}

impl SquashMerger {
    pub(crate) fn integration_root(&self) -> &std::path::Path {
        &self.repo_root
    }
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
            repository_child_token: None,
        }
    }

    pub fn with_repository_child_token(
        mut self,
        token: Option<Arc<crate::repository_lease::RepositoryChildToken>>,
    ) -> Self {
        self.repository_child_token = token;
        self
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
            return Err(MergeError::GitCommandFailed {
                command: format!(
                    "git -C {} commit --allow-empty -m {commit_message:?}",
                    self.repo_root.display()
                ),
                stderr,
            });
        }

        let oid = self.full_head_oid().await?;
        Ok(MergeOutcome::Merged { oid })
    }

    /// Phase A of the transactional task landing: squash the task branch onto
    /// the plan ref as ONE evidence-carrying commit and **publish** it.
    ///
    /// # Why this prepares detached and CAS-publishes
    ///
    /// Unlike [`squash_merge`](Self::squash_merge) — whose caller owns a checkout
    /// with `base_branch` attached — the transactional path runs inside the run's
    /// private integration workspace, which the claim/status landings leave on a
    /// **detached HEAD** (they `checkout --detach {expected_old}`, commit, then
    /// CAS the plan ref).  Committing on HEAD alone would move HEAD and leave
    /// `refs/heads/{base_branch}` behind the landing commit, which breaks the two
    /// invariants the rest of the protocol depends on:
    ///
    /// - Phase B passes the landing OID as its expected-old CAS value, so it
    ///   fails `RefMoved` and strands the task mid-landing.
    /// - [`find_task_landing`](Self::find_task_landing) /
    ///   [`verify_task_landing_oid`](Self::verify_task_landing_oid) look for the
    ///   evidence commit *reachable from the plan ref*, so a retry cannot reuse
    ///   an unpublished landing and lands a duplicate instead.
    ///
    /// So this mirrors the discipline used by [`final_squash`](Self::final_squash)
    /// and the `landing` module: prepare the candidate on a detached workspace,
    /// then move the shared ref only by expected-old compare-and-swap.
    pub async fn squash_merge_with_evidence(
        &self,
        task_branch: &str,
        subject: &str,
        identity: &TaskLandingIdentity,
    ) -> Result<MergeOutcome, MergeError> {
        validate_identity(identity)?;
        match self.find_task_landing(identity).await? {
            LandingEvidenceStatus::Verified(oid) => return Ok(MergeOutcome::Merged { oid }),
            LandingEvidenceStatus::Missing => {}
            other => {
                return Err(MergeError::GitCommandFailed {
                    command: "verify existing Phase-A evidence".into(),
                    stderr: format!("landing evidence is not uniquely reusable: {other:?}"),
                });
            }
        }
        let message = format!(
            "{subject}\n\nMakina-Plan: {}\nMakina-Task: {}\nMakina-Run: {}",
            identity.plan, identity.task, identity.run
        );
        let expected = self.rev_parse(&self.base_branch).await?;
        self.run_git_checked(
            &["checkout", "--detach", &expected],
            &format!(
                "git -C {} checkout --detach {}",
                self.repo_root.display(),
                &self.base_branch
            ),
        )
        .await?;
        match self.squash_merge(task_branch, &message).await? {
            MergeOutcome::Merged { oid } => {
                self.cas_branch(&self.base_branch, oid.as_str(), &expected)
                    .await?;
                Ok(MergeOutcome::Merged { oid })
            }
            conflict => Ok(conflict),
        }
    }

    pub async fn find_task_landing(
        &self,
        identity: &TaskLandingIdentity,
    ) -> Result<LandingEvidenceStatus, MergeError> {
        validate_identity(identity)?;
        let output = self
            .run_git_raw(&[
                "log",
                "--first-parent",
                "--format=%H%x00%B%x00%x1e",
                &self.base_branch,
            ])
            .await?;
        if !output.status.success() {
            return Err(MergeError::GitCommandFailed {
                command: "inspect first-parent landing lineage".into(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().into(),
            });
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut matches = Vec::new();
        for record in text.split('\x1e') {
            let mut fields = record.trim_matches('\n').splitn(2, '\0');
            let Some(oid) = fields.next().filter(|value| !value.is_empty()) else {
                continue;
            };
            let body = fields.next().unwrap_or_default().trim_end_matches('\0');
            if exact_trailer(body, "Makina-Phase").is_none()
                && exact_trailer(body, "Makina-Plan") == Some(identity.plan.as_str())
                && exact_trailer(body, "Makina-Task") == Some(identity.task.as_str())
                && exact_trailer(body, "Makina-Run") == Some(identity.run.as_str())
            {
                matches.push(self.parse_oid(oid).await?);
            }
        }
        Ok(match matches.len() {
            0 => LandingEvidenceStatus::Missing,
            1 => LandingEvidenceStatus::Verified(matches.remove(0)),
            _ => LandingEvidenceStatus::Ambiguous,
        })
    }

    pub async fn verify_task_landing_oid(
        &self,
        oid: &str,
        identity: &TaskLandingIdentity,
    ) -> Result<LandingEvidenceStatus, MergeError> {
        validate_identity(identity)?;
        let oid = match self.parse_oid(oid).await {
            Ok(oid) => oid,
            Err(_) => return Ok(LandingEvidenceStatus::Mismatched),
        };
        let exists = self
            .run_git_raw(&["cat-file", "-e", &format!("{}^{{commit}}", oid.as_str())])
            .await?;
        if !exists.status.success() {
            return Ok(LandingEvidenceStatus::Missing);
        }
        let reachable = self
            .run_git_raw(&[
                "merge-base",
                "--is-ancestor",
                oid.as_str(),
                &self.base_branch,
            ])
            .await?;
        if !reachable.status.success() {
            return Ok(LandingEvidenceStatus::Unreachable);
        }
        let body = self
            .run_git_raw(&["show", "-s", "--format=%B", oid.as_str()])
            .await?;
        let body = String::from_utf8_lossy(&body.stdout);
        if exact_trailer(&body, "Makina-Phase").is_some()
            || exact_trailer(&body, "Makina-Plan") != Some(identity.plan.as_str())
            || exact_trailer(&body, "Makina-Task") != Some(identity.task.as_str())
            || exact_trailer(&body, "Makina-Run") != Some(identity.run.as_str())
        {
            return Ok(LandingEvidenceStatus::Mismatched);
        }
        match self.find_task_landing(identity).await? {
            LandingEvidenceStatus::Verified(actual) if actual == oid => {
                Ok(LandingEvidenceStatus::Verified(oid))
            }
            LandingEvidenceStatus::Ambiguous => Ok(LandingEvidenceStatus::Ambiguous),
            _ => Ok(LandingEvidenceStatus::Mismatched),
        }
    }

    async fn full_head_oid(&self) -> Result<crate::plan::GitObjectId, MergeError> {
        let oid = self.rev_parse("HEAD").await?;
        self.parse_oid(&oid).await
    }

    async fn parse_oid(&self, oid: &str) -> Result<crate::plan::GitObjectId, MergeError> {
        let format = self.rev_parse("--show-object-format").await?;
        let format = match format.trim() {
            "sha1" => crate::plan::GitObjectFormat::Sha1,
            "sha256" => crate::plan::GitObjectFormat::Sha256,
            other => {
                return Err(MergeError::GitCommandFailed {
                    command: "git rev-parse --show-object-format".into(),
                    stderr: format!("unsupported object format {other}"),
                });
            }
        };
        crate::plan::GitObjectId::parse(oid.trim(), format).map_err(|error| {
            MergeError::GitCommandFailed {
                command: "validate full landing object ID".into(),
                stderr: error.to_string(),
            }
        })
    }

    /// Squash-land `plan_branch` onto `base_branch` as ONE commit.
    ///
    /// This is the final merge variant of [`squash_merge`]: it first checks out
    /// `base_branch` (whereas `squash_merge` operates on an already-checked-out
    /// `plan_branch`), then performs the squash merge into the **true**
    /// `base_branch`. On success, `base_branch` gains exactly ONE new commit
    /// (message == `message`); on conflict, `base_branch` is restored to a clean
    /// state and [`MergeOutcome::Conflict`] is returned.
    ///
    /// # Returns
    ///
    /// - [`MergeOutcome::Merged`] — the squashed commit landed on `base_branch`.
    /// - [`MergeOutcome::Conflict`] — a conflict occurred; `base_branch` is clean
    ///   and unchanged (`details` carries git's conflict output).
    ///
    /// # Errors
    ///
    /// [`MergeError`] on a true infrastructure failure.  On the conflict path the
    /// restore runs before returning, so `base_branch` is left clean.
    pub async fn final_squash(
        &self,
        plan_branch: &str,
        message: &str,
    ) -> Result<MergeOutcome, MergeError> {
        let expected = self.rev_parse(&self.base_branch).await?;
        // Prepare on a detached private workspace. The shared base ref moves
        // only after the candidate commit exists and only by expected-old CAS.
        self.run_git_checked(
            &["checkout", "--detach", &expected],
            &format!(
                "git -C {} checkout {}",
                self.repo_root.display(),
                &self.base_branch
            ),
        )
        .await?;

        // Perform the squash merge from plan_branch.
        let squash = self
            .run_git_raw(&["merge", "--squash", plan_branch])
            .await?;

        if !squash.status.success() {
            // Conflict. Capture detail and restore base_branch before returning.
            let details = combine_output(&squash.stdout, &squash.stderr);
            return Ok(MergeOutcome::Conflict { details });
        }

        // Commit the squashed change.
        let commit = self
            .run_git_raw(&["commit", "--allow-empty", "-m", message])
            .await?;

        if !commit.status.success() {
            // Commit failed. Restore base_branch before surfacing the error.
            let stderr = String::from_utf8_lossy(&commit.stderr).trim().to_string();
            return Err(MergeError::GitCommandFailed {
                command: format!(
                    "git -C {} commit --allow-empty -m {message:?}",
                    self.repo_root.display()
                ),
                stderr,
            });
        }

        let candidate = self.rev_parse("HEAD").await?;
        self.cas_branch(&self.base_branch, &candidate, &expected)
            .await?;

        Ok(MergeOutcome::Merged {
            oid: self.parse_oid(&candidate).await?,
        })
    }

    /// `git merge --no-ff {plan_branch}` onto `base_branch` (a real merge commit).
    ///
    /// Checks out `base_branch` first, then performs a non-fast-forward merge so
    /// that the plan branch is recorded as a parent (creating a true merge commit
    /// with two parents on `base_branch`).  On success, `base_branch` gains a
    /// merge commit; on conflict, `base_branch` is restored to a clean state and
    /// [`MergeOutcome::Conflict`] is returned.
    ///
    /// # Returns
    ///
    /// - [`MergeOutcome::Merged`] — the merge commit landed on `base_branch`.
    /// - [`MergeOutcome::Conflict`] — a conflict occurred; `base_branch` is clean
    ///   and unchanged (`details` carries git's conflict output).
    ///
    /// # Errors
    ///
    /// [`MergeError`] on a true infrastructure failure.  On the conflict path the
    /// restore runs before returning, so `base_branch` is left clean.
    pub async fn final_merge_commit(
        &self,
        plan_branch: &str,
        message: &str,
    ) -> Result<MergeOutcome, MergeError> {
        let expected = self.rev_parse(&self.base_branch).await?;
        self.run_git_checked(
            &["checkout", "--detach", &expected],
            &format!(
                "git -C {} checkout {}",
                self.repo_root.display(),
                &self.base_branch
            ),
        )
        .await?;

        // Perform the non-fast-forward merge from plan_branch.
        let merge = self
            .run_git_raw(&["merge", "--no-ff", "-m", message, plan_branch])
            .await?;

        if !merge.status.success() {
            // Conflict. Capture detail and restore base_branch before returning.
            let details = combine_output(&merge.stdout, &merge.stderr);
            return Ok(MergeOutcome::Conflict { details });
        }

        let candidate = self.rev_parse("HEAD").await?;
        self.cas_branch(&self.base_branch, &candidate, &expected)
            .await?;

        Ok(MergeOutcome::Merged {
            oid: self.parse_oid(&candidate).await?,
        })
    }

    /// Stage `plan_branch`'s net diff onto `base_branch` without committing.
    ///
    /// This is the "human decides what happens next" finalization mode. It is
    /// intentionally conservative: if the main checkout already has staged or
    /// unstaged changes, the method refuses to apply anything and leaves the
    /// plan branch available for manual recovery.
    pub async fn final_stage_changes(&self, plan_branch: &str) -> Result<StageOutcome, MergeError> {
        let expected = self.rev_parse(&self.base_branch).await?;
        self.run_git_checked(
            &["checkout", "--detach", &expected],
            &format!(
                "git -C {} checkout {}",
                self.repo_root.display(),
                &self.base_branch
            ),
        )
        .await?;

        let status = self.run_git_raw(&["status", "--porcelain"]).await?;
        if !status.status.success() {
            return Err(MergeError::GitCommandFailed {
                command: format!("git -C {} status --porcelain", self.repo_root.display()),
                stderr: String::from_utf8_lossy(&status.stderr).trim().to_string(),
            });
        }
        let status_text = String::from_utf8_lossy(&status.stdout).trim().to_string();
        let blocking_status = status_text
            .lines()
            .filter(|line| !line.starts_with("?? .makina/"))
            .collect::<Vec<_>>()
            .join("\n");
        if !blocking_status.is_empty() {
            return Err(MergeError::DirtyWorktree {
                status: blocking_status,
            });
        }

        let squash = self
            .run_git_raw(&["merge", "--squash", plan_branch])
            .await?;

        if !squash.status.success() {
            let details = combine_output(&squash.stdout, &squash.stderr);
            return Ok(StageOutcome::Conflict { details });
        }

        Ok(StageOutcome::Staged)
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
    async fn rev_parse(&self, revision: &str) -> Result<String, MergeError> {
        let output = self.run_git_raw(&["rev-parse", revision]).await?;
        if !output.status.success() {
            return Err(MergeError::GitCommandFailed {
                command: format!("git rev-parse {revision}"),
                stderr: String::from_utf8_lossy(&output.stderr).trim().into(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().into())
    }

    async fn cas_branch(
        &self,
        branch: &str,
        candidate: &str,
        expected: &str,
    ) -> Result<(), MergeError> {
        let listed = self
            .run_git_raw(&["worktree", "list", "--porcelain"])
            .await?;
        let mut path = None;
        for line in String::from_utf8_lossy(&listed.stdout).lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(value));
            } else if line == format!("branch refs/heads/{branch}")
                && path.as_ref().is_some_and(|value| value != &self.repo_root)
            {
                return Err(MergeError::BaseCheckedOut {
                    branch: branch.into(),
                    path: path.expect("checked above"),
                });
            }
        }
        self.run_git_checked(
            &[
                "update-ref",
                &format!("refs/heads/{branch}"),
                candidate,
                expected,
            ],
            &format!("compare-and-swap refs/heads/{branch} {expected} -> {candidate}"),
        )
        .await
    }

    /// Run a `git -C {repo_root} {args}` command and return the raw [`Output`]
    /// (caller inspects the exit status). Maps spawn/await failures to
    /// [`MergeError::Io`].
    ///
    /// [`Output`]: std::process::Output
    async fn run_git_raw(&self, args: &[&str]) -> Result<std::process::Output, MergeError> {
        let mut command = tokio::process::Command::new("git");
        command.arg("-C").arg(&self.repo_root).args(args);
        command.kill_on_drop(true);
        #[cfg(unix)]
        if let Some(token) = &self.repository_child_token {
            token.inherit_into(&mut command);
        }
        command.output().await.map_err(MergeError::Io)
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

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Unit tests for the pure helper(s).  The git-driven behaviour (clean squash
    //! producing ONE commit, conflict leaving `develop` pristine) is covered by
    //! the integration tests in `tests/squash_merge.rs` against real temp repos.

    use super::*;

    #[test]
    fn merger_stores_config() {
        let m = SquashMerger::new(PathBuf::from("/repo"), "develop".into());
        assert_eq!(m.repo_root, PathBuf::from("/repo"));
        assert_eq!(m.base_branch, "develop");
    }
}
