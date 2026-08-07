//! Worktree + branch lifecycle manager for Makina.
//!
//! The [`WorktreeManager`] creates and tears down git worktrees and their
//! associated branches on behalf of the Supervisor.  Each task gets an
//! isolated checkout at `repo_root/.makina/worktrees/{short_worktree_name}/`
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
//! Worktrees live under `repo_root/.makina/worktrees/` (in-repo, gitignored)
//! so they are never committed accidentally. The `.makina/.gitignore` file
//! lists `/worktrees/`, `/runs/`, and `/checkpoints/` as transient.
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

use std::path::{Path, PathBuf};
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
    #[error("recovery evidence retained at `{path}`: {reason}")]
    RecoveryEvidence { path: PathBuf, reason: String },
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

/// Run-qualified coordinator checkout. It is private runtime state and is
/// never the operator's repository checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationWorkspace {
    pub path: PathBuf,
    pub plan_branch: String,
}

/// What happened to the operator's base branch when a plan was registered.
///
/// Registration always publishes the plan onto `refs/heads/plan/{slug}`; this
/// records whether the base branch was additionally fast-forwarded onto it, and
/// when it was not, why. A `Skipped` is never a failure — the plan is durable on
/// its own ref either way — but it is the difference between the operator seeing
/// their new plan in `git log` and wondering where it went, so the reason
/// travels with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseAdvance {
    /// The base branch, and the checkout on it, now contain the plan commit.
    Advanced,
    /// The base branch was left untouched, for this reason.
    Skipped { reason: String },
}

impl BaseAdvance {
    /// Whether the plan commit reached the base branch.
    pub fn advanced(&self) -> bool {
        matches!(self, BaseAdvance::Advanced)
    }

    /// A one-line description suitable for a log line or an operator-facing note.
    pub fn describe(&self, base_branch: &str) -> String {
        match self {
            BaseAdvance::Advanced => format!("landed on {base_branch}"),
            BaseAdvance::Skipped { reason } => {
                format!("not landed on {base_branch}: {reason}")
            }
        }
    }
}

/// Summary returned by [`WorktreeManager::purge_makina_worktrees`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreePurgeReport {
    /// Number of registered git worktrees removed.
    pub worktrees_removed: usize,
    /// Number of orphan directories removed from Makina's transient worktree root.
    pub orphan_dirs_removed: usize,
    /// Candidates retained because deleting them could destroy recovery evidence.
    pub preserved: Vec<PreservedWorktree>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreservedWorktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub reason: String,
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
#[derive(Clone)]
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
    repository_child_token: Option<Arc<crate::repository_lease::RepositoryChildToken>>,

    /// Optional callback fired before each git command, carrying the command
    /// string and working dir. Set by the supervisor so the TUI can display a
    /// live execution log. Cloned (Arc) so it survives `with_fork_branch` etc.
    command_sink: Option<CommandSink>,

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

type CommandSink = Arc<dyn Fn(&str, &std::path::Path) + Send + Sync>;

impl WorktreeManager {
    /// Create immutable recovery refs for every active plan/task ref. Existing
    /// exact refs are reused; divergent archive names fail closed.
    pub async fn archive_run_refs(
        &self,
        plan_slug: &str,
        run_uid: &str,
        task_ids: &[String],
    ) -> Result<Vec<String>, WorktreeError> {
        let plan_ref = format!("refs/heads/plan/{plan_slug}");
        let mut requested = vec![plan_ref];
        requested.extend(task_ids.iter().map(|task| {
            format!(
                "refs/heads/task/{}",
                paths::short_worktree_name(plan_slug, task)
            )
        }));
        let mut args = vec!["for-each-ref", "--format=%(refname) %(objectname)"];
        args.extend(requested.iter().map(String::as_str));
        let output = self
            .run_git(&args, "enumerate refs for reset archive")
            .await?;
        let mut archived = Vec::new();
        for line in output.lines().filter(|line| !line.is_empty()) {
            let (source_ref, oid) =
                line.split_once(' ')
                    .ok_or_else(|| WorktreeError::GitCommandFailed {
                        command: "parse reset archive refs".into(),
                        stderr: format!("malformed ref record: {line}"),
                    })?;
            let suffix = source_ref
                .strip_prefix("refs/heads/")
                .expect("enumerated head ref")
                .replace('/', "--");
            let archive = format!("refs/makina/recovery/{plan_slug}/{run_uid}/{suffix}");
            if self
                .run_git(
                    &["update-ref", &archive, oid, ""],
                    "create reset recovery ref",
                )
                .await
                .is_err()
            {
                let actual = self
                    .run_git(
                        &["rev-parse", "--verify", &archive],
                        "verify reset recovery ref",
                    )
                    .await?;
                if actual.trim() != oid {
                    return Err(WorktreeError::RecoveryEvidence {
                        path: self.repo_root.clone(),
                        reason: format!("archive {archive} is divergent"),
                    });
                }
            }
            archived.push(archive);
        }
        Ok(archived)
    }

    /// Prove a stale claimed task has no unlanded recovery state. Absence is
    /// clean; an existing branch must still equal the claim commit and any
    /// registered worktree must have no tracked or untracked changes.
    pub async fn prove_stale_claim_clean(
        &self,
        plan_slug: &str,
        task_id: &str,
        claim_oid: &str,
    ) -> Result<(), WorktreeError> {
        let branch = format!("task/{}", paths::short_worktree_name(plan_slug, task_id));
        let path = self.worktree_path(plan_slug, task_id)?;
        if !self.branch_exists(&branch).await? {
            if path.exists() {
                return Err(WorktreeError::RecoveryEvidence {
                    path,
                    reason: "unregistered stale task directory may contain recovery data".into(),
                });
            }
            return Ok(());
        }
        let tip = self
            .run_git(&["rev-parse", &branch], "inspect stale task branch")
            .await?;
        if tip.trim() != claim_oid {
            return Err(WorktreeError::RecoveryEvidence {
                path,
                reason: format!("task branch diverged from claim {claim_oid}"),
            });
        }
        if path.exists() {
            let head = self
                .run_git_at(
                    &path,
                    &["symbolic-ref", "--short", "HEAD"],
                    "inspect stale worktree branch",
                )
                .await?;
            if head.trim() != branch {
                return Err(WorktreeError::RecoveryEvidence {
                    path,
                    reason: format!("worktree is attached to {head}, expected {branch}"),
                });
            }
            let dirty = self
                .run_git_at(
                    &path,
                    &["status", "--porcelain", "--untracked-files=all"],
                    "inspect stale worktree dirtiness",
                )
                .await?;
            if !dirty.is_empty() {
                return Err(WorktreeError::RecoveryEvidence {
                    path,
                    reason: "worktree contains tracked or untracked recovery changes".into(),
                });
            }
        }
        Ok(())
    }
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
            repository_child_token: None,
            command_sink: None,
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

    pub fn with_repository_child_token(
        mut self,
        token: Arc<crate::repository_lease::RepositoryChildToken>,
    ) -> Self {
        self.repository_child_token = Some(token);
        self
    }

    /// Set a callback that fires before each git command, carrying the command
    /// string and working dir. Used by the supervisor to emit `RunCommand`
    /// events so the TUI can display a live execution log.
    pub fn with_command_sink(mut self, sink: CommandSink) -> Self {
        self.command_sink = Some(sink);
        self
    }

    pub(crate) fn repository_child_token(
        &self,
    ) -> Option<Arc<crate::repository_lease::RepositoryChildToken>> {
        self.repository_child_token.clone()
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

        let worktree_path = self.worktree_path(plan_slug, task_id)?;
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
            return Err(WorktreeError::RecoveryEvidence {
                path: worktree_path,
                reason: format!("task branch `{branch}` or worktree already exists"),
            });
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

    /// Create or verify the run's private integration workspace. Creation is
    /// deliberately create-only: an existing path is inspected and retained on
    /// any ambiguity instead of being reset, cleaned, or force-removed.
    /// However, if the path exists but git no longer recognizes it as a
    /// worktree (e.g. the worktree was pruned from git's registry but the
    /// directory survived on disk — a stale artifact from a previous
    /// interrupted run or scaffold), the orphaned directory is removed and a
    /// fresh worktree is created. There is nothing to recover from an
    /// unregistered directory: git has no record of it.
    pub async fn create_integration_workspace(
        &self,
        plan_slug: &str,
        run_uid: &str,
    ) -> Result<IntegrationWorkspace, WorktreeError> {
        let _op_guard = self.op_lock.lock().await;
        let path = paths::run_dir(&self.repo_root, run_uid)?.join("integration");
        let branch = format!("plan/{plan_slug}");
        if path.exists() {
            let inside = self
                .git_command(&path)
                .args(["rev-parse", "--is-inside-work-tree"])
                .output()
                .await?;
            if !inside.status.success() {
                // The path exists on disk but git does not recognize it as a
                // worktree. This is a stale artifact (e.g. from a previous
                // interrupted run/scaffold whose worktree was pruned from
                // git's registry). The directory is orphaned — git has no
                // record of it, so there is nothing to recover. Remove it
                // and fall through to create a fresh worktree.
                tokio::fs::remove_dir_all(&path).await.map_err(|e| {
                    WorktreeError::GitCommandFailed {
                        command: format!("remove stale integration workspace {}", path.display()),
                        stderr: e.to_string(),
                    }
                })?;
            } else {
                let head = self
                    .git_command(&path)
                    .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
                    .output()
                    .await?;
                if head.status.success() && String::from_utf8_lossy(&head.stdout).trim() != branch {
                    return Err(WorktreeError::GitCommandFailed {
                        command: format!("verify retained integration workspace {}", path.display()),
                        stderr: "workspace is detached or attached to unexpected lineage; retained for recovery"
                            .into(),
                    });
                }
                if !head.status.success() && self.branch_exists(&branch).await? {
                    self.run_git_at_checked(
                        &path,
                        &["checkout", &branch],
                        "attach retained plan ref",
                    )
                    .await?;
                }
                return Ok(IntegrationWorkspace {
                    path,
                    plan_branch: branch,
                });
            }
        }
        tokio::fs::create_dir_all(path.parent().expect("integration path has parent")).await?;
        let base_oid = self
            .run_git(
                &["rev-parse", &self.base_branch],
                "resolve integration base",
            )
            .await?
            .trim()
            .to_string();
        // Canonicalize the worktree path to an absolute form before passing it
        // to `git worktree add`. When repo_root is a relative path (e.g. the
        // scaffold passes `target` as a relative CWD-relative dir), git resolves
        // the worktree path relative to the `-C repo_root` dir, not CWD — which
        // double-prefixes the path. An absolute path avoids this ambiguity.
        let absolute_path = path.canonicalize().unwrap_or_else(|_| {
            // canonicalize fails if the path doesn't exist yet (the parent was
            // just created_dir_all'd but the leaf doesn't exist). Fall back to
            // joining CWD.
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            cwd.join(&path)
        });
        self.run_git(
            &[
                "worktree",
                "add",
                "--detach",
                absolute_path.to_string_lossy().as_ref(),
                &base_oid,
            ],
            &format!("create detached integration workspace {}", path.display()),
        )
        .await?;
        if self.branch_exists(&branch).await? {
            // A branch can be checked out in only one worktree. A previous
            // generation or an interrupted run can leave one still holding this
            // plan ref, which would make the plan permanently unstartable — so
            // reclaim that holder when it is safe to (see
            // [`Self::reclaim_branch_holder`]) before attaching.
            self.reclaim_branch_holder(&branch, &path).await?;
            let attached = self
                .git_command(&path)
                .args(["checkout", &branch])
                .output()
                .await?;
            if !attached.status.success() {
                return Err(WorktreeError::GitCommandFailed {
                    command: format!("attach integration workspace to retained {branch}"),
                    stderr: String::from_utf8_lossy(&attached.stderr).trim().into(),
                });
            }
            return Ok(IntegrationWorkspace {
                path,
                plan_branch: branch,
            });
        }
        Ok(IntegrationWorkspace {
            path,
            plan_branch: branch,
        })
    }

    /// Atomically publish a fully constructed detached registration candidate.
    /// Response-loss retries are idempotent when the ref already equals the
    /// exact candidate. No branch is exposed before both the candidate parent
    /// and current base match `expected_base`.
    /// Release a workspace whose purpose is complete, freeing the plan branch.
    ///
    /// A git branch can be checked out in **one** worktree at a time, so a
    /// workspace left attached to `plan/{slug}` blocks every later attempt to
    /// attach it — which is what made a generated plan impossible to start:
    /// registration published the ref and then kept its scratch worktree
    /// checked out on it forever.
    ///
    /// Call this only once the work the workspace existed for is durably
    /// recorded elsewhere (for registration, that is the published plan ref —
    /// the worktree holds nothing the ref does not). Failure paths must NOT
    /// call it: an unpublished workspace is the only copy of its evidence.
    ///
    /// Best-effort, and never fails the caller: the caller's work already
    /// succeeded, and failing it because a scratch directory resisted cleanup
    /// would turn a tidiness problem into a lost result. Removal is attempted
    /// first; if the workspace holds anything git will not discard, it is
    /// detached instead — which frees the branch without touching a single
    /// file.
    pub async fn release_integration_workspace(&self, workspace: &IntegrationWorkspace) {
        let _op_guard = self.op_lock.lock().await;
        if self.remove_worktree_path(&workspace.path).await.is_ok() {
            let _ = self.git_worktree_prune().await;
            return;
        }
        // Removal refuses on a workspace carrying modified or untracked files.
        // Freeing the branch is the part that matters; the directory can stay.
        match self.detach_worktree(&workspace.path).await {
            Ok(()) => tracing::info!(
                path = %workspace.path.display(),
                branch = %workspace.plan_branch,
                "integration workspace retained its files; detached to free the plan branch"
            ),
            Err(error) => tracing::warn!(
                path = %workspace.path.display(),
                branch = %workspace.plan_branch,
                %error,
                "could not release integration workspace; it will be reclaimed on next use"
            ),
        }
        let _ = self.git_worktree_prune().await;
    }

    /// Fast-forward the base branch onto `commit`, when doing so cannot cost
    /// the operator anything.
    ///
    /// # Why this is guarded rather than simply done
    ///
    /// Registration deliberately does not touch the operator's checkout: it
    /// publishes `refs/heads/plan/{slug}` in one compare-and-swap transaction
    /// precisely so a dirty working tree, a mid-edit index, or a moved base can
    /// never be disturbed by authoring a plan. That safety is the reason the
    /// plan bundle does not appear on the base branch — which is also why it
    /// looks, to the person who just created a plan, as though nothing was
    /// written at all.
    ///
    /// This closes that gap without giving up the property that motivated it.
    /// The advance happens only when every one of these holds, and otherwise
    /// the checkout is left exactly as it was and the reason is reported:
    ///
    /// * the base branch still points at `expected_base`, so `commit` is a
    ///   direct fast-forward and no merge decision is being made on the
    ///   operator's behalf;
    /// * the checkout is actually on the base branch, so nothing is switched;
    /// * the checkout has no uncommitted tracked changes, so nothing in flight
    ///   can be overwritten.
    ///
    /// The final step is `git merge --ff-only`, which independently refuses
    /// anything that is not a clean fast-forward — including one that would
    /// clobber an untracked file. The gates above are what make the *outcome*
    /// explainable; `--ff-only` is what makes it safe.
    ///
    /// Never fails the caller: the plan is already registered and durable on
    /// its ref, so a declined advance is information, not an error.
    pub async fn fast_forward_base_to(&self, commit: &str, expected_base: &str) -> BaseAdvance {
        let skipped = |reason: String| BaseAdvance::Skipped { reason };

        let current_base = match self
            .run_git(
                &["rev-parse", &self.base_branch],
                "resolve base branch for plan landing",
            )
            .await
        {
            Ok(oid) => oid.trim().to_owned(),
            Err(error) => {
                return skipped(format!("could not resolve {}: {error}", self.base_branch));
            }
        };
        if current_base != expected_base {
            return skipped(format!(
                "{} moved since the plan was validated; the plan stays on its own branch",
                self.base_branch
            ));
        }

        let head = match self
            .run_git(
                &["rev-parse", "--abbrev-ref", "HEAD"],
                "resolve checkout branch for plan landing",
            )
            .await
        {
            Ok(head) => head.trim().to_owned(),
            Err(error) => {
                return skipped(format!("could not resolve the current checkout: {error}"));
            }
        };
        if head != self.base_branch {
            return skipped(format!(
                "the checkout is on {head}, not {}; nothing was switched",
                self.base_branch
            ));
        }

        match self
            .run_git(
                &["status", "--porcelain", "--untracked-files=no"],
                "inspect checkout before landing the plan",
            )
            .await
        {
            Ok(status) if !status.trim().is_empty() => {
                return skipped(
                    "the checkout has uncommitted changes; the plan stays on its own branch"
                        .to_owned(),
                );
            }
            Err(error) => {
                return skipped(format!("could not inspect the checkout: {error}"));
            }
            Ok(_) => {}
        }

        match self
            .run_git(
                &["merge", "--ff-only", commit],
                "land the plan on the base branch",
            )
            .await
        {
            Ok(_) => BaseAdvance::Advanced,
            Err(error) => skipped(format!("git declined the fast-forward: {error}")),
        }
    }

    /// Free `branch` from any other worktree currently holding it.
    ///
    /// Git refuses to check a branch out twice, so a holder left behind by a
    /// finished generation or an interrupted run is fatal to the run that needs
    /// it next — the plan becomes permanently unstartable.
    ///
    /// Reclaiming **detaches** the holder rather than removing it: `git checkout
    /// --detach` moves its HEAD from `plan/{slug}` to the very commit that ref
    /// already names, so the working tree, the index, and every untracked file
    /// are left exactly as they were. Nothing has to be judged disposable, and
    /// nothing is lost — which matters, because these workspaces routinely
    /// carry untracked leftovers from abandoned authoring attempts.
    ///
    /// The one thing that is refused is a holder **outside** this repository's
    /// `.makina/runs/`. A plan branch the operator checked out in their own
    /// worktree is theirs; silently detaching their HEAD would be a surprising
    /// thing to do to someone else's checkout, so that is reported instead.
    async fn reclaim_branch_holder(
        &self,
        branch: &str,
        requesting_path: &Path,
    ) -> Result<(), WorktreeError> {
        // A holder whose directory is already gone only lingers in git's
        // registry; pruning is enough to free the branch.
        let _ = self.git_worktree_prune().await;

        let runs_root = paths::runs_dir(&self.repo_root).map_err(WorktreeError::Io)?;
        let requesting = requesting_path.canonicalize();
        let holders: Vec<RegisteredWorktree> = self
            .registered_worktrees()
            .await?
            .into_iter()
            .filter(|registered| registered.branch.as_deref() == Some(branch))
            .filter(
                |registered| match (&requesting, registered.path.canonicalize()) {
                    // The workspace we are about to attach is not its own blocker.
                    (Ok(requesting), Ok(holder)) => *requesting != holder,
                    _ => true,
                },
            )
            .collect();

        for holder in holders {
            let owned = holder
                .path
                .canonicalize()
                .ok()
                .zip(runs_root.canonicalize().ok())
                .is_some_and(|(holder, runs)| holder.starts_with(runs));
            if !owned {
                return Err(WorktreeError::RecoveryEvidence {
                    path: holder.path.clone(),
                    reason: format!(
                        "{branch} is checked out in a worktree outside this project's Makina run \
                         state; detach or remove it before starting this plan"
                    ),
                });
            }

            tracing::info!(
                path = %holder.path.display(),
                %branch,
                "detaching a Makina workspace still holding the plan branch"
            );
            self.detach_worktree(&holder.path).await?;
        }

        let _ = self.git_worktree_prune().await;
        Ok(())
    }

    /// Point a worktree's HEAD at its current commit instead of a branch.
    ///
    /// Content-preserving by construction: the commit is the one the branch
    /// already names, so the checkout does not change a single tracked file, and
    /// untracked files are never touched by it.
    async fn detach_worktree(&self, path: &Path) -> Result<(), WorktreeError> {
        self.run_git_at_checked(
            path,
            &["checkout", "--detach"],
            "detach worktree from its branch",
        )
        .await
        .map(|_| ())
    }

    pub async fn publish_registration(
        &self,
        workspace: &IntegrationWorkspace,
        candidate: &str,
        expected_base: &str,
    ) -> Result<(), WorktreeError> {
        let parent = self
            .run_git_at(
                &workspace.path,
                &["rev-parse", &format!("{candidate}^")],
                "verify R parent",
            )
            .await?;
        if parent.trim() != expected_base {
            return Err(WorktreeError::RecoveryEvidence {
                path: workspace.path.clone(),
                reason: "registration candidate is not a child of the expected base".into(),
            });
        }
        if self.branch_exists(&workspace.plan_branch).await? {
            let actual = self
                .run_git(
                    &["rev-parse", &workspace.plan_branch],
                    "verify published registration",
                )
                .await?;
            if actual.trim() != candidate {
                return Err(WorktreeError::RecoveryEvidence {
                    path: workspace.path.clone(),
                    reason: "plan ref already names different recovery evidence".into(),
                });
            }
        } else {
            let transaction = format!(
                "start\nverify refs/heads/{} {}\ncreate refs/heads/{} {}\nprepare\ncommit\n",
                self.base_branch, expected_base, workspace.plan_branch, candidate
            );
            let mut child = self.git_command(&self.repo_root);
            child
                .args(["update-ref", "--stdin"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let mut child = child.spawn()?;
            use tokio::io::AsyncWriteExt;
            child
                .stdin
                .as_mut()
                .expect("piped stdin")
                .write_all(transaction.as_bytes())
                .await?;
            let output = child.wait_with_output().await?;
            if !output.status.success() {
                return Err(WorktreeError::GitCommandFailed {
                    command: "atomic registration ref transaction".into(),
                    stderr: format!(
                        "{}; detached candidate retained at {}",
                        String::from_utf8_lossy(&output.stderr).trim(),
                        workspace.path.display()
                    ),
                });
            }
        }
        self.run_git_at_checked(
            &workspace.path,
            &["checkout", &workspace.plan_branch],
            "attach published registration",
        )
        .await
    }

    /// Atomically archive and replace an unconsumed registration.
    pub async fn publish_registration_refresh(
        &self,
        workspace: &IntegrationWorkspace,
        candidate: &str,
        expected_base: &str,
        expected_old: &str,
    ) -> Result<(), WorktreeError> {
        let parent = self
            .run_git_at(
                &workspace.path,
                &["rev-parse", &format!("{candidate}^")],
                "verify R2 parent",
            )
            .await?;
        if parent.trim() != expected_base {
            return Err(WorktreeError::RecoveryEvidence {
                path: workspace.path.clone(),
                reason: "refreshed registration is not a child of expected base".into(),
            });
        }
        let archive = format!(
            "refs/makina/recovery/{}/{}",
            workspace.plan_branch, expected_old
        );
        let archive_exists = self
            .run_git(
                &["rev-parse", "--verify", &archive],
                "inspect registration archive",
            )
            .await
            .ok();
        if archive_exists
            .as_deref()
            .is_some_and(|oid| oid.trim() != expected_old)
        {
            return Err(WorktreeError::RecoveryEvidence {
                path: workspace.path.clone(),
                reason: "registration archive ref is divergent".into(),
            });
        }
        let archive_command = if archive_exists.is_some() {
            format!("verify {archive} {expected_old}\n")
        } else {
            format!("create {archive} {expected_old}\n")
        };
        let transaction = format!(
            "start\nverify refs/heads/{} {}\n{}update refs/heads/{} {} {}\nprepare\ncommit\n",
            self.base_branch,
            expected_base,
            archive_command,
            workspace.plan_branch,
            candidate,
            expected_old
        );
        let mut child = self.git_command(&self.repo_root);
        child
            .args(["update-ref", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = child.spawn()?;
        use tokio::io::AsyncWriteExt;
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(transaction.as_bytes())
            .await?;
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            // Response loss: exact new plan ref + archive is success.
            let plan = self
                .run_git(
                    &["rev-parse", &workspace.plan_branch],
                    "verify refreshed registration",
                )
                .await
                .ok();
            let archived = self
                .run_git(&["rev-parse", &archive], "verify registration archive")
                .await
                .ok();
            if plan.as_deref().is_none_or(|oid| oid.trim() != candidate)
                || archived
                    .as_deref()
                    .is_none_or(|oid| oid.trim() != expected_old)
            {
                return Err(WorktreeError::GitCommandFailed {
                    command: "atomic registration refresh transaction".into(),
                    stderr: String::from_utf8_lossy(&output.stderr).trim().into(),
                });
            }
        }
        self.run_git_at_checked(
            &workspace.path,
            &["checkout", &workspace.plan_branch],
            "attach refreshed registration",
        )
        .await
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

    /// Preserve the pre-transactional cleanup contract for graph-only runs.
    pub async fn remove_legacy(&self, plan_slug: &str, task_id: &str) -> Result<(), WorktreeError> {
        validate_task_id(task_id)?;
        let _op_guard = self.op_lock.lock().await;
        let worktree_path = self.worktree_path(plan_slug, task_id)?;
        let branch = format!("task/{}", paths::short_worktree_name(plan_slug, task_id));
        let path = worktree_path.to_string_lossy();
        let removal = self
            .run_git(
                &["worktree", "remove", "--force", &path],
                &format!(
                    "git -C {} worktree remove --force {path}",
                    self.repo_root.display()
                ),
            )
            .await;
        if let Err(WorktreeError::GitCommandFailed { ref stderr, .. }) = removal
            && !is_not_found_stderr(stderr)
        {
            return removal.map(|_| ());
        }
        if worktree_path.exists() {
            let _ = tokio::fs::remove_dir_all(&worktree_path).await;
        }
        let deletion = self
            .run_git(
                &["branch", "-D", &branch],
                &format!("git -C {} branch -D {branch}", self.repo_root.display()),
            )
            .await;
        if let Err(WorktreeError::GitCommandFailed { ref stderr, .. }) = deletion
            && !is_not_found_stderr(stderr)
        {
            return deletion.map(|_| ());
        }
        let _ = self.git_worktree_prune().await;
        Ok(())
    }

    /// Purge every git worktree registered under Makina's transient worktree
    /// directory for this repository.
    ///
    /// Only worktrees whose path starts with [`paths::worktrees_dir`] are touched;
    /// user-created worktrees elsewhere are ignored. Branch cleanup is likewise
    /// scoped to the branch checked out by those Makina worktrees.
    pub async fn purge_makina_worktrees(&self) -> Result<WorktreePurgeReport, WorktreeError> {
        let _op_guard = self.op_lock.lock().await;
        self.git_worktree_prune().await?;

        let worktrees_root = paths::worktrees_dir(&self.repo_root)?;
        let registered = self.registered_worktrees().await?;
        let mut worktrees_removed = 0usize;

        let mut preserved = Vec::new();
        for registered in registered
            .into_iter()
            .filter(|wt| wt.path.starts_with(&worktrees_root))
        {
            let Some(branch) = registered.branch.as_deref() else {
                preserved.push(PreservedWorktree {
                    path: registered.path,
                    branch: None,
                    reason: "detached or unknown branch; recovery ownership is ambiguous".into(),
                });
                continue;
            };
            if !branch.starts_with("task/") {
                preserved.push(PreservedWorktree {
                    path: registered.path,
                    branch: Some(branch.to_string()),
                    reason: "worktree path and branch ownership do not agree".into(),
                });
                continue;
            }
            let status = self
                .git_command(&registered.path)
                .args(["status", "--porcelain", "--untracked-files=normal"])
                .output()
                .await
                .map_err(WorktreeError::Io)?;
            if !status.status.success() || !status.stdout.is_empty() {
                preserved.push(PreservedWorktree {
                    path: registered.path,
                    branch: Some(branch.to_string()),
                    reason: if status.status.success() {
                        "worktree contains tracked or untracked recovery changes".into()
                    } else {
                        "worktree cleanliness could not be verified".into()
                    },
                });
                continue;
            }
            let landed = self
                .git_command(&self.repo_root)
                .args(["merge-base", "--is-ancestor", branch, &self.base_branch])
                .status()
                .await
                .map_err(WorktreeError::Io)?;
            if !landed.success() {
                preserved.push(PreservedWorktree {
                    path: registered.path,
                    branch: Some(branch.to_string()),
                    reason: "branch contains commits not reachable from the retained base".into(),
                });
                continue;
            }
            self.remove_worktree_path(&registered.path).await?;
            self.delete_branch_if_exists(branch).await?;
            worktrees_removed += 1;
        }

        let orphan_dirs_removed = 0usize;
        if worktrees_root.exists() {
            let mut entries = tokio::fs::read_dir(&worktrees_root).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.is_dir() && !preserved.iter().any(|item| item.path == path) {
                    preserved.push(PreservedWorktree {
                        path,
                        branch: None,
                        reason: "unregistered directory may contain recovery evidence".into(),
                    });
                }
            }
        }

        let _ = self.git_worktree_prune().await;

        Ok(WorktreePurgeReport {
            worktrees_removed,
            orphan_dirs_removed,
            preserved,
        })
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

        let worktree_path = self.worktree_path(plan_slug, task_id)?;
        let branch = format!("task/{}", paths::short_worktree_name(plan_slug, task_id));

        // Remove the worktree (--force handles dirty checkouts; ignore
        // "not a worktree" / "not found" so the call is idempotent).
        self.remove_worktree_path(&worktree_path).await?;

        // Retain the task ref until durable landing evidence proves its commits
        // are recoverable elsewhere. A squash result is not ancestry proof.
        let _ = branch;

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
        if !self.branch_exists(&branch).await? {
            self.run_git(
                &["branch", &branch, &self.base_branch],
                &format!(
                    "git -C {} branch {branch} {}",
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

    /// Delete `plan/{plan_slug}` if it exists, after restoring the base branch.
    pub async fn delete_plan_branch(&self, plan_slug: &str) -> Result<(), WorktreeError> {
        let branch = format!("plan/{plan_slug}");
        if self.branch_exists(&branch).await? {
            tracing::warn!(branch, "retaining plan ref as recovery evidence");
        }
        Ok(())
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Compute the worktree path for a given plan slug + task ID.
    fn worktree_path(&self, plan_slug: &str, task_id: &str) -> Result<PathBuf, WorktreeError> {
        paths::worktree(&self.repo_root, plan_slug, task_id).map_err(WorktreeError::Io)
    }

    async fn remove_worktree_path(&self, worktree_path: &Path) -> Result<(), WorktreeError> {
        let wt_path_str = worktree_path.to_string_lossy();
        let remove_result = self
            .run_git(
                &["worktree", "remove", &wt_path_str],
                &format!(
                    "git -C {} worktree remove {wt_path_str}",
                    self.repo_root.display()
                ),
            )
            .await;

        if let Err(WorktreeError::GitCommandFailed { ref stderr, .. }) = remove_result
            && !is_not_found_stderr(stderr)
        {
            return remove_result.map(|_| ());
        }

        Ok(())
    }

    async fn delete_branch_if_exists(&self, branch: &str) -> Result<(), WorktreeError> {
        let delete_result = self
            .run_git(
                &["branch", "-d", branch],
                &format!("git -C {} branch -d {branch}", self.repo_root.display()),
            )
            .await;

        if let Err(WorktreeError::GitCommandFailed { ref stderr, .. }) = delete_result
            && !is_not_found_stderr(stderr)
        {
            return delete_result.map(|_| ());
        }

        Ok(())
    }

    async fn registered_worktrees(&self) -> Result<Vec<RegisteredWorktree>, WorktreeError> {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .await
            .map_err(WorktreeError::Io)?;

        if !output.status.success() {
            return Err(WorktreeError::GitCommandFailed {
                command: format!(
                    "git -C {} worktree list --porcelain",
                    self.repo_root.display()
                ),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        Ok(parse_worktree_list_porcelain(&String::from_utf8_lossy(
            &output.stdout,
        )))
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
        self.run_git_at(&self.repo_root, args, human_command).await
    }

    async fn run_git_at(
        &self,
        path: &Path,
        args: &[&str],
        human_command: &str,
    ) -> Result<String, WorktreeError> {
        // Fire the command sink so the TUI can display what's being executed.
        if let Some(sink) = &self.command_sink {
            let cmd_str = format!("git -C {} {}", path.display(), args.join(" "));
            sink(&cmd_str, path);
        }
        let output = self.git_command(path).args(args).output().await?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(WorktreeError::GitCommandFailed {
                command: human_command.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }

    async fn run_git_at_checked(
        &self,
        path: &Path,
        args: &[&str],
        human_command: &str,
    ) -> Result<(), WorktreeError> {
        self.run_git_at(path, args, human_command).await.map(|_| ())
    }

    fn git_command(&self, working_dir: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("git");
        command.arg("-C").arg(working_dir).kill_on_drop(true);
        #[cfg(unix)]
        if let Some(token) = &self.repository_child_token {
            token.inherit_into(&mut command);
        }
        command
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegisteredWorktree {
    path: PathBuf,
    branch: Option<String>,
}

fn parse_worktree_list_porcelain(output: &str) -> Vec<RegisteredWorktree> {
    let mut entries = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    let flush = |entries: &mut Vec<RegisteredWorktree>,
                 current_path: &mut Option<PathBuf>,
                 current_branch: &mut Option<String>| {
        if let Some(path) = current_path.take() {
            entries.push(RegisteredWorktree {
                path,
                branch: current_branch.take(),
            });
        } else {
            current_branch.take();
        }
    };

    for line in output.lines() {
        if line.is_empty() {
            flush(&mut entries, &mut current_path, &mut current_branch);
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            flush(&mut entries, &mut current_path, &mut current_branch);
            current_path = Some(PathBuf::from(path));
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            current_branch = Some(branch.to_string());
        }
    }
    flush(&mut entries, &mut current_path, &mut current_branch);

    entries
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

    /// The worktree path leaf must use the short name format
    /// `{plan#}-{task-trunc}-{hash4}` and resolve under `repo/.makina/worktrees/`.
    #[test]
    fn worktree_path_uses_short_name() {
        let tmp_repo = tempfile::tempdir().expect("create temp repo");
        let repo_root = tmp_repo.path().to_path_buf();

        let mgr = WorktreeManager::new(repo_root.clone(), "develop".into());
        let path = mgr
            .worktree_path("0003-runtime-and-tui-hardening", "sample-task")
            .unwrap();

        // Must be under repo/.makina/worktrees/ (the in-repo state root).
        let state_root = crate::paths::state_root(&repo_root).unwrap();
        assert!(
            path.starts_with(&state_root),
            "worktree_path must be under state_root ({}), got {}",
            state_root.display(),
            path.display()
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
    /// in-repo `repo_root/.makina/worktrees/{short_worktree_name}` layout,
    /// never the old `~/.makina/projects/` external layout or the
    /// `.makina/worktrees/{plan_slug}--{task_id}/` old in-repo format.
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

        // In-repo layout must be present with the new short-name form.
        assert!(
            module_doc.contains("repo_root/.makina/worktrees/{short_worktree_name}"),
            "module doc must reference the in-repo `repo_root/.makina/worktrees/` layout"
        );
        assert!(
            module_doc.contains("short_worktree_name"),
            "module doc must reference short_worktree_name"
        );

        // Old external layout must NOT appear in the module doc.
        assert!(
            !module_doc.contains("~/.makina/projects/"),
            "module doc must not reference the old external ~/.makina/projects/ layout"
        );
        // Old in-repo plan--task format must NOT appear in the module doc.
        assert!(
            !module_doc.contains(".makina/worktrees/{plan_slug}--{task_id}"),
            "module doc must not reference the old plan-slug--task_id format"
        );
    }
}
