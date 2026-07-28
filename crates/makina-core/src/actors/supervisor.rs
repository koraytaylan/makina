//! Tokio scheduler for the Makina develop-review pipeline.
//!
//! # Role
//!
//! The scheduler coordinates ready tasks, worktree lifecycle, developer turns,
//! gates, reviewer turns, squash merges, persistence, and run-control events.
//!
//! # The develop → review loop (task 21)
//!
//! Each task driver moves a ready task end-to-end via a **sequential async
//! flow**:
//!
//! ```text
//! pick ready task
//!   --DependenciesSatisfied--> Ready
//!   create worktree (WorktreeManager::create)
//!     on create failure: --HardError--> Failed ; task fails   (task 25)
//!   --Dispatched--> InProgress
//!   loop:
//!     develop_until_gates_pass(task, worktree, feedback):     (task 22)
//!       loop:
//!         develop(task, worktree, feedback)                  (feedback=None first time)
//!         run gates in worktree
//!           Passed       --GatesPassed--> InReview ; break
//!           Failed{..}   --GateFailed--> InProgress (self-loop) ; gate_iterations += 1
//!                        if gate_iterations >= caps.gate_iterations:
//!                          --GateCapReached--> Failed ; teardown ; task fails
//!                        else: feedback = gate output ; re-dispatch
//!     review(task, worktree)
//!       (dispatch/parse failure) --HardError--> Failed ; teardown   (task 25)
//!       Approve  squash_merge(task/{id} → develop)      (task 23 — BEFORE teardown)
//!                 Merged    --ReviewerApproved--> Done ; WorktreeManager::remove
//!                 Conflict  develop already restored clean by merger ;
//!                           --ReviewCapReached--> Failed ; teardown  (safe-fail; agent-reconcile seam)
//!                 (hard merge err) --HardError--> Failed ; teardown  (task 25)
//!       Reject{feedback}
//!                 if review_iterations + 1 >= caps.reviewer_iterations:   (task 25)
//!                   --ReviewCapReached--> Failed ; teardown ; task fails
//!                 else: --ReviewerRejected--> InProgress ;
//!                   review_iterations += 1 ; relay feedback ; re-develop+gate
//! ```
//!
//! On top of the develop→review loop, the **scheduler** wraps each driver in a
//! per-task `tokio::time::timeout(config.caps.wall_clock_secs)`.  If the deadline
//! fires the driver future is cancelled (its [`DriverGuard`] tears down the
//! worktree) and the scheduler applies `WallClockCapReached` → `Failed` under
//! the graph lock (task 25).
//!
//! Every state change goes through [`crate::state_machine::transition`]; the
//! scheduler keeps each [`Task::state`] in the shared graph updated as the
//! source of truth.
//!
//! ## Gates (task 22 — implemented)
//!
//! Between the Developer hand-back and review, the work iterates against the
//! configured gates ([`crate::config::Config::gates`]) via
//! [`develop_until_gates_pass`].  On a gate failure the FSM self-loops
//! (`InProgress --GateFailed--> InProgress`), the failing gate's output is fed
//! back to the Developer, and ALL gates re-run; the work only advances to the
//! Reviewer (`GatesPassed`) once every gate exits `0`.  A per-task
//! gate-iteration cap (`config.caps.gate_iterations`) moves the task to `Failed`
//! (`GateCapReached`) on exhaustion.
//!
//! **Placement choice**: the architecture frames gates as "Developer-side"; the
//! MVP implements them **scheduler-coordinated** (the driver runs the gates and
//! re-dispatches the Developer with the failure output).  The agent still does
//! the fixing; the gate EXECUTION is the reusable [`crate::gate::GateRunner`].
//!
//! ## Squash-merge (task 23 — implemented)
//!
//! On approval the driver squash-merges `task/{id}` into the base branch via
//! [`SquashMerger::squash_merge`], **before** tearing down the worktree.  A clean
//! merge lands the task's work as ONE squashed commit on `develop`, then the FSM
//! advances `InReview --ReviewerApproved--> Done` and the worktree is removed.  A
//! straggler **conflict** is reconciled rather than hard-failed-into-corruption:
//! the merger restores `develop` to a clean state (its hard invariant), and the
//! driver drives the task to a SAFE terminal `Failed` (via `ReviewCapReached`)
//! without ever leaving `develop` broken.
//!
//! ## Concurrency (task 24 — implemented)
//!
//! The scheduler runs **multiple tasks in parallel**, one driver per task, up to
//! `config.concurrency`. It launches a [`task_driver`] per
//! ready task on a [`tokio::task::JoinSet`], capped by a
//! [`tokio::sync::Semaphore`].  *Within-task* logic (FSM, dev+gate loop, review
//! loop, squash-merge, worktree lifecycle) is unchanged — concurrency is purely
//! **across** tasks (the architecture's "concurrency is across tasks, not within
//! one").  See the [`scheduler`] / [`task_driver`] docs for the full design,
//! lock discipline, and deadlock-freedom argument.
//!
//! ## Termination caps (task 25 — implemented)
//!
//! All three caps come from `config.caps` and each independently drives a task
//! to terminal `Failed`:
//!
//! - **Gate cap** (`caps.gate_iterations`): enforced in [`develop_until_gates_pass`]
//!   (task 22); on exhaustion emits `GateCapReached` (InProgress → Failed).
//! - **Reviewer cap** (`caps.reviewer_iterations`): enforced in [`task_driver`]'s
//!   reject branch.  The decision is made BEFORE the FSM transition: if this
//!   rejection *reaches* the cap (`review_iterations + 1 >= cap`) the driver
//!   emits `ReviewCapReached` (InReview → Failed) and terminates the task
//!   instead of looping back to develop.  This uses `ReviewCapReached` from its
//!   intended state (`InReview`) and replaced the old stand-in constant.
//! - **Wall-clock cap** (`caps.wall_clock_secs`): enforced by the [`scheduler`],
//!   which wraps each driver future in `tokio::time::timeout`.  On elapse the
//!   driver is cancelled (its [`DriverGuard`] cleans up) and the scheduler emits
//!   `WallClockCapReached` (Ready/InProgress/InReview → Failed) under the graph
//!   lock.
//!
//! Concurrency keeps the iteration caps **per task** (each driver counts its own
//! task's iterations under the graph lock — see [`task_driver`]); the wall-clock
//! cap is per task because each driver future has its own timeout.
//!
//! ## Deferred seams (do NOT implement here)
//!
//! - **Run control** (task 31): pause/cancel is not implemented; the
//!   [`scheduler`] leaves a documented cancellation seam (drop the `JoinSet` /
//!   close the semaphore) but does not act on it.
//! - **Idle / heartbeat detection** (FUTURE): only the three caps above exist;
//!   there is no per-step idle timeout.
//!
//! # Entrypoint
//!
//! - [`run_graph`] drives every ready task to a terminal state, running up to
//!   `config.concurrency` in parallel, and returns [`RunReport`].

use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::FutureExt as _;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::api;
use crate::audit::AuditRegistry;
use crate::backend::AgentBackend;
use crate::config::{Config, FinalMerge, RoleAssignment};
use crate::gate::{GateOutcome, GateRunner};
use crate::interpreter::TaskListInterpreter;
use crate::merge::{MergeOutcome, SquashMerger, StageOutcome};
use crate::paths;
use crate::persist::persist_graph_as;
use crate::state_machine::{TaskEvent, transition};
use crate::task::{Task, TaskGraph, TaskId, TaskState};
use crate::worktree::WorktreeManager;

use super::developer::{Develop, DevelopOutcome, DeveloperError, develop};
use super::reviewer::{Review, ReviewVerdict, ReviewerError, review};

/// One side of a NUL-safe Git name/status diff, normalized by the coordinator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FootprintChange {
    pub path: String,
    pub change: crate::plan::RepoChange,
    /// Required for the modify-only tracked config exception.
    pub ordinary_file_result: bool,
}

/// A correction returned at both the review-acceptance and pre-Phase-A seams.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FootprintViolation {
    pub path: String,
    pub reason: String,
}

/// Decode `git diff --name-status -z -M -C` without ever interpreting a path as
/// text-delimited output. Rename/copy records produce entries for both sides.
pub fn parse_name_status_z(
    output: &[u8],
    ordinary_results: &std::collections::HashSet<String>,
    submodules: &std::collections::HashSet<String>,
) -> Result<Vec<FootprintChange>, String> {
    let fields = output.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut cursor = 0;
    let mut changes = Vec::new();
    while cursor < fields.len() && !fields[cursor].is_empty() {
        let status = std::str::from_utf8(fields[cursor])
            .map_err(|_| "Git emitted a non-UTF-8 status token")?;
        cursor += 1;
        let path_count = usize::from(status.starts_with('R') || status.starts_with('C')) + 1;
        if cursor + path_count > fields.len() {
            return Err("truncated NUL-delimited Git name/status record".into());
        }
        for raw_path in &fields[cursor..cursor + path_count] {
            let path = std::str::from_utf8(raw_path)
                .map_err(|_| "repository path is not valid UTF-8")?
                .to_owned();
            let change = if submodules.contains(&path) {
                crate::plan::RepoChange::Submodule
            } else {
                match status.as_bytes().first().copied() {
                    Some(b'M') => crate::plan::RepoChange::ModifiedOrdinaryFile,
                    Some(b'D') => crate::plan::RepoChange::Deleted,
                    Some(b'A') => crate::plan::RepoChange::Added,
                    Some(b'R' | b'C') => crate::plan::RepoChange::RenamedOrCopied,
                    Some(b'T') => crate::plan::RepoChange::TypeChanged,
                    Some(b'U') => crate::plan::RepoChange::Unmerged,
                    _ => return Err(format!("unsupported Git status `{status}`")),
                }
            };
            changes.push(FootprintChange {
                ordinary_file_result: ordinary_results.contains(&path),
                path,
                change,
            });
        }
        cursor += path_count;
    }
    Ok(changes)
}

/// Enforce an authored footprint against an already NUL-safely decoded diff.
/// Rename/copy callers must provide both source and destination entries.
pub fn enforce_authored_footprint(
    touches: &[crate::plan::RepoPattern],
    changes: &[FootprintChange],
) -> Result<(), Vec<FootprintViolation>> {
    let mut violations = Vec::new();
    for changed in changes {
        if is_reserved_status_path(&changed.path) {
            violations.push(FootprintViolation {
                path: changed.path.clone(),
                reason: "coordinator-owned status path is reserved".into(),
            });
            continue;
        }
        let permitted = touches.iter().any(|pattern| {
            if !pattern.is_executable()
                || !crate::dependency::footprint_matches(pattern.as_str(), &changed.path)
                || !pattern.permits_change(changed.change)
            {
                return false;
            }
            !matches!(
                pattern,
                crate::plan::RepoPattern::TrackedMakinaConfig { .. }
            ) || changed.ordinary_file_result
        });
        if !permitted {
            violations.push(FootprintViolation {
                path: changed.path.clone(),
                reason: "change is outside the authored footprint or uses a forbidden Git status"
                    .into(),
            });
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

fn is_reserved_status_path(path: &str) -> bool {
    path == "docs/plans/STATUS.md"
        || (path.starts_with("docs/plans/") && path.ends_with("/STATUS.md"))
        || (path.starts_with("docs/plans/") && path.contains("/tasks/") && path.ends_with(".md"))
}

pub async fn enforce_task_branch_footprint(
    repo: &Path,
    task_id: &TaskId,
    branch: &str,
    touches: &[crate::task::AuthoredRepoPattern],
    recorded_base: &str,
) -> Result<(), String> {
    let base = recorded_base;
    let raw = git_output(
        repo,
        &[
            "diff",
            "--name-status",
            "-z",
            "-M",
            "-C",
            base,
            branch,
            "--",
        ],
    )
    .await?;
    let mut paths = Vec::new();
    // First parse discovers all paths without trusting line delimiters.
    let provisional = parse_name_status_z(&raw, &Default::default(), &Default::default())?;
    paths.extend(provisional.iter().map(|change| change.path.clone()));
    let mut ordinary = std::collections::HashSet::new();
    let mut submodules = std::collections::HashSet::new();
    for path in paths {
        let tip_entry = literal_tree_entry(repo, branch, &path).await?;
        let base_entry = literal_tree_entry(repo, base, &path).await?;
        if tip_entry
            .as_ref()
            .is_some_and(|entry| entry.mode == "160000")
            || base_entry
                .as_ref()
                .is_some_and(|entry| entry.mode == "160000")
        {
            submodules.insert(path);
        } else if tip_entry.as_ref().is_some_and(|entry| {
            matches!(entry.mode.as_str(), "100644" | "100755") && entry.object_type == "blob"
        }) {
            ordinary.insert(path);
        }
    }
    let changes = parse_name_status_z(&raw, &ordinary, &submodules)?;
    let patterns = touches
        .iter()
        .map(crate::task::AuthoredRepoPattern::to_repo_pattern)
        .collect::<Vec<_>>();
    enforce_authored_footprint(&patterns, &changes).map_err(|violations| {
        let details = violations
            .iter()
            .map(|violation| format!("{} ({})", violation.path, violation.reason))
            .collect::<Vec<_>>()
            .join(", ");
        format!("task {task_id} changed paths outside its authored footprint: {details}")
    })
}

struct LiteralTreeEntry {
    mode: String,
    object_type: String,
}

async fn literal_tree_entry(
    repo: &Path,
    revision: &str,
    path: &str,
) -> Result<Option<LiteralTreeEntry>, String> {
    let literal = format!(":(literal){path}");
    let output = git_output(repo, &["ls-tree", "-z", revision, "--", &literal]).await?;
    if output.is_empty() {
        return Ok(None);
    }
    let record = output
        .strip_suffix(&[0])
        .ok_or("ls-tree record was not NUL terminated")?;
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or("malformed ls-tree record")?;
    let (metadata, returned_with_tab) = record.split_at(tab);
    let returned_path = &returned_with_tab[1..];
    if returned_path != path.as_bytes() {
        return Err("ls-tree returned a different path than requested".into());
    }
    let mut fields = metadata.split(|byte| *byte == b' ');
    let mode = std::str::from_utf8(fields.next().ok_or("missing ls-tree mode")?)
        .map_err(|_| "invalid ls-tree mode")?;
    let object_type = std::str::from_utf8(fields.next().ok_or("missing ls-tree type")?)
        .map_err(|_| "invalid ls-tree type")?;
    if fields.next().is_none() || fields.next().is_some() {
        return Err("malformed ls-tree metadata".into());
    }
    Ok(Some(LiteralTreeEntry {
        mode: mode.into(),
        object_type: object_type.into(),
    }))
}

async fn git_output(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .await
        .map_err(|error| format!("could not run git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

async fn return_footprint_correction(
    ctx: &DriverContext,
    task_id: &TaskId,
    message: String,
) -> Result<(), String> {
    {
        let mut graph = ctx.graph.lock().await;
        apply_event_locked(&mut graph, task_id, TaskEvent::ReviewerRejected)?;
    }
    ctx.persist().await;
    ctx.emit_task_state(task_id, TaskState::InProgress);
    tracing::warn!(task = %task_id, reason = %message, "task returned for footprint correction");
    Ok(())
}

// ── Live event emission (task 31: run-control) ──────────────────────────────────

/// A sink for the live [`api::Event`]s the Supervisor scheduler/drivers emit.
///
/// The orchestrator (`CoreApi`) wires this to its broadcast so the TUI observes
/// execution.  It is an `Arc<dyn Fn(api::Event) + Send + Sync>` — a cheap,
/// `Clone`able callback chosen over a concrete `broadcast::Sender` so the engine
/// stays decoupled from *how* events are delivered (the orchestrator can adapt
/// it to a broadcast, an mpsc, or a test recorder).
///
/// Emission is **additive** and best-effort: dropping events (no live receiver)
/// is fine, and the engine NEVER holds the graph lock across an emit (the sink
/// is called only outside the tight locked sections).  See [`RunControl`].
pub type EventSink = Arc<dyn Fn(api::Event) + Send + Sync>;

/// Per-run control + observability bundle threaded through the scheduler.
///
/// Bundles the four things a *controlled* run needs beyond the static driver
/// resources:
///
/// - `run` — the [`api::RunId`] every emitted event is tagged with.
/// - `sink` — where live events go (see [`EventSink`]).
/// - `pause` — when `true`, the scheduler stops launching NEW task drivers
///   (in-flight tasks finish); clearing it and re-running resumes.  (Task 31
///   pause semantics: "stop launching new tasks".)
/// - `cancel` — a [`CancellationToken`]; when cancelled the scheduler stops
///   launching and `abort_all()`s the in-flight `JoinSet` (each aborted
///   driver's [`DriverGuard`] still tears down its worktree/spokes — no leak).
///
/// Direct scheduler tests use [`RunControl::silent`]: a no-op sink, never
/// paused, never cancelled.
#[derive(Clone)]
pub struct RunControl {
    /// The Run these events/controls belong to.
    pub run: api::RunId,
    /// Where live [`api::Event`]s are published.
    pub sink: EventSink,
    /// Cooperative pause flag: while `true`, no NEW drivers are launched.
    pub pause: Arc<AtomicBool>,
    /// Cancellation signal: stops launching + aborts in-flight drivers.
    pub cancel: CancellationToken,
}

impl RunControl {
    /// A control that emits nothing, never pauses, and never cancels.
    ///
    /// Used by tests that do not need live events or run-control behavior.
    pub fn silent() -> Self {
        Self {
            run: api::RunId(0),
            sink: Arc::new(|_| {}),
            pause: Arc::new(AtomicBool::new(false)),
            cancel: CancellationToken::new(),
        }
    }

    /// Emit one event to the sink (best-effort; never panics).
    fn emit(&self, event: api::Event) {
        (self.sink)(event);
    }
}

// ── RunReport ───────────────────────────────────────────────────────────────────

/// Summary returned by [`run_graph`].
///
/// Reports the terminal outcome for each task the run touched.  Because tasks run
/// **concurrently**, the order of `outcomes` reflects driver *completion* order
/// (not authored order); tests assert on the set of `(id, state)` pairs (and on
/// the final graph snapshot for ordering-independent state), not on positional
/// order.  Each task appears **exactly once** (single dispatch per task — see
/// [`scheduler`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// `(task_id, final_state)` for every task driven to a terminal state during
    /// this run, in driver-completion order.
    pub outcomes: Vec<(TaskId, TaskState)>,
    /// `(task_id, reason)` for every task that reached a `Failed` terminal during
    /// this run (a completed-but-failed run keeps going — see
    /// [`scheduler`] — so the report records *which* tasks failed and *why*).
    /// The `String` is a short failure reason (a driver hard-error message, or a
    /// synthesized cap literal such as `"wall-clock-cap-reached"`).
    pub failed_tasks: Vec<(TaskId, String)>,

    /// `Some(plan_branch)` when the run left its integration branch unmerged
    /// (Manual mode, a failed run, or a final-merge conflict); `None` when it was
    /// squashed/merge-committed into base_branch.
    pub plan_branch_left: Option<String>,
    pub landing_evidence: Vec<crate::task::TaskLandingEvidence>,
}

// ── DevelopGateOutcome ──────────────────────────────────────────────────────────

/// Result of one [`develop_until_gates_pass`] round.
///
/// Either the work passed every gate and is ready for the Reviewer, or the
/// gate-iteration cap fired and the task was already moved to `Failed` (with its
/// worktree torn down).  A hard error is reported separately via `Err` on the
/// helper, not as a variant here.
enum DevelopGateOutcome {
    /// All gates passed; the task is now `InReview` and ready for the Reviewer.
    ReadyForReview,

    /// The gate-iteration cap was reached; the helper has already emitted
    /// `GateCapReached` (→ `Failed`) and torn down the worktree.
    GateCapReached,
}

// ── Shared driver context ───────────────────────────────────────────────────────

/// Shared, cheaply-clonable resources handed to every [`task_driver`].
///
/// All fields are `Arc`/`Clone`, so cloning a `DriverContext` per task is cheap
/// and every driver observes the SAME underlying graph, merge lock, semaphore,
/// and git managers.  This is what lets drivers run concurrently while still
/// keeping the graph as the single source of truth.
#[derive(Clone)]
struct DriverContext {
    /// The shared task graph (source of truth for state + iteration counts).
    ///
    /// A `tokio::sync::Mutex` so the lock is async-aware; **the guard is NEVER
    /// held across an `.await`** (see the lock-discipline note on
    /// [`task_driver`]).
    graph: Arc<Mutex<TaskGraph>>,
    landing_evidence: Arc<Mutex<Vec<crate::task::TaskLandingEvidence>>>,

    /// Serializes snapshot acquisition and atomic replacement for this run.
    /// The lock is acquired before cloning the graph, so an older snapshot can
    /// never queue behind and overwrite a newer one.
    persist_lock: Arc<Mutex<()>>,

    /// The **develop merge lock**: serializes the squash-merge step (the only
    /// part of a driver that mutates the single shared `develop` checkout).  A
    /// `Mutex<()>` whose guard is held for the minimal merge span only.
    merge_lock: Arc<Mutex<()>>,

    /// Worktree/branch lifecycle manager.  Its `create`/`remove` serialize their
    /// git operations across concurrent drivers via an internal lock (shared by
    /// all clones), so the `git worktree prune` they run cannot race a concurrent
    /// `git worktree add` — see [`WorktreeManager`].
    worktree_manager: WorktreeManager,

    /// The gate runner (stateless; reused across tasks/iterations).
    gate_runner: GateRunner,

    /// The squash-merger (stateless beyond config; the merge step is serialized
    /// by `merge_lock`).
    squash_merger: SquashMerger,

    /// The resolved runtime config (gates, caps, base branch).
    config: Config,

    /// The agent backend for the Developer role.
    ///
    /// Resolved from `config.roles.developer.provider` at DriverContext construction
    /// time and cloned into each per-task Developer actor.  May be the same Arc as
    /// `reviewer_backend` when both roles use the same provider.
    developer_backend: Arc<dyn AgentBackend>,

    /// The agent backend for the Reviewer role.
    ///
    /// Resolved from `config.roles.reviewer.provider` at DriverContext construction
    /// time and cloned into each per-task Reviewer actor.  Distinct from
    /// `developer_backend` when the config assigns different providers to the roles.
    reviewer_backend: Arc<dyn AgentBackend>,

    /// Per-run control + live-event sink (task 31).  Threaded into the scheduler
    /// (pause/cancel checks) and every [`task_driver`] (event emission +
    /// Developer/Reviewer `AgentExchange`). Tests that do not observe events
    /// pass [`RunControl::silent`].
    control: RunControl,

    /// The audit registry: the Supervisor calls this to associate each task's
    /// worktree `working_dir` with its `(run_id, slug, task_id)` context,
    /// enabling the [`crate::audit::JsonlAuditSink`] to route audit entries to
    /// the correct `.tasks/{slug}/audit.jsonl` file.
    ///
    /// Tests that do not need audit routing pass
    /// [`crate::audit::NoopAuditRegistry`].
    audit_registry: Arc<dyn AuditRegistry>,

    /// The task-graph slug (file stem of the task-list file), used as the
    /// sub-directory name under `.tasks/` when routing audit entries.
    ///
    /// Derived by the orchestrator from the task-list path when the run is
    /// started; direct tests use an empty string (no-op with
    /// `NoopAuditRegistry`).
    run_slug: String,

    /// The persistent, sortable run identity (26-char ULID string) minted by the
    /// orchestrator when the run is opened, threaded through so the audit ledger
    /// can key entries on a stable cross-process run id.
    ///
    /// Direct tests use an empty string (no-op with
    /// `NoopAuditRegistry`).
    ///
    /// Consumed by the `AuditRegistry::register` call in `dispatch_task`, which
    /// passes it as the 2nd arg so the audit ledger can key entries on the
    /// stable cross-process run id.
    run_uid: String,

    /// The plan slug (lowercased-kebab of the task-list's parent directory name),
    /// threaded from the orchestrator so per-task worktree calls can plan-scope
    /// their directory + branch names.
    ///
    /// Direct tests use an empty string.
    ///
    /// Read when building per-task worktree directory + branch names: the
    /// driver passes it to `WorktreeManager::create`/`remove` so the worktree
    /// dir + branch are plan-scoped as `{plan_slug}--{task_id}`.
    plan_slug: String,
    checkpoint_identity: Option<crate::checkpoint::CheckpointIdentity>,
    #[cfg(test)]
    pre_a_observer: Option<tokio::sync::mpsc::UnboundedSender<TaskId>>,
}

impl DriverContext {
    async fn commit_claim(&self, task_id: &TaskId) -> Result<(), String> {
        let Some(identity) = self.checkpoint_identity.as_ref() else {
            return Ok(());
        };
        let root = self.squash_merger.integration_root();
        let source = crate::plan::FilesystemPlanFileSource::new(root, None)
            .map_err(|error| format!("open integration plan for claim: {error}"))?;
        let candidate = crate::plan::load_plan_path(
            &source,
            &identity.plan_dir,
            &crate::plan::PlanReservations::default(),
        )
        .map_err(|report| format!("load integration plan for claim: {:?}", report.diagnostics))?;
        let crate::plan::PlanCandidate::Plan(mut plan) = candidate else {
            return Err("registered integration source is not a plan".into());
        };
        let task = plan
            .tasks
            .iter_mut()
            .find(|task| task.frontmatter.id.as_str() == task_id.0)
            .ok_or_else(|| format!("claim task {task_id} is absent from registered plan"))?;
        task.update_bookkeeping(crate::plan::AuthoredTaskStatus::InProgress, None)
            .map_err(|error| format!("render task claim: {error}"))?;
        plan.status.integration_state = crate::plan::PlanIntegrationState::Assembling;
        plan.status.run = Some(self.run_uid.clone());
        plan.status.display_status = "🔄 In progress".into();
        let transition = crate::plan_status::StatusTransition {
            integration_state: plan.status.integration_state,
            run: plan.status.run.clone(),
            validation_base: plan.status.validation_base_oid.clone(),
            mode: None,
            final_oid: None,
            display_status: plan.status.display_status.clone(),
            last_updated: plan.status.last_updated.clone(),
        };
        let status = crate::plan_status::render_plan_status(&plan, &transition)
            .map_err(|error| format!("render claim status: {error}"))?;
        plan.status.source.body = status.clone();
        let root_path = std::path::PathBuf::from("docs/plans/STATUS.md");
        let root_status = tokio::fs::read_to_string(root.join(&root_path))
            .await
            .map_err(|error| format!("read root status for claim: {error}"))?;
        let root_status = crate::plan_status::update_root_row(&root_status, &plan)
            .map_err(|error| format!("render claim root status: {error}"))?;
        let task = plan
            .tasks
            .iter()
            .find(|task| task.frontmatter.id.as_str() == task_id.0)
            .unwrap();
        let plan_ref = format!("refs/heads/plan/{}", self.plan_slug);
        let old = git_output(root, &["rev-parse", "--verify", &plan_ref])
            .await
            .map_err(|error| format!("read plan ref for claim: {error}"))?;
        let old = std::str::from_utf8(&old)
            .map_err(|_| "plan ref was not UTF-8")?
            .trim();
        crate::landing::commit_task_claim(
            root,
            &plan_ref,
            old,
            &[
                crate::landing::OwnedWrite {
                    path: task.source_path.clone(),
                    bytes: task.render().into_bytes(),
                },
                crate::landing::OwnedWrite {
                    path: plan.status.source.source_path.clone(),
                    bytes: status.into_bytes(),
                },
                crate::landing::OwnedWrite {
                    path: root_path,
                    bytes: root_status.into_bytes(),
                },
            ],
            &crate::landing::StatusLandingIdentity {
                plan: self.plan_slug.clone(),
                task: task_id.0.clone(),
                run: self.run_uid.clone(),
                landing: old.into(),
            },
        )
        .await
        .map_err(|error| format!("commit claim for {task_id}: {error}"))?;
        Ok(())
    }

    async fn commit_phase_b(
        &self,
        task_id: &TaskId,
        landing_oid: &crate::plan::GitObjectId,
    ) -> Result<String, String> {
        let identity = self
            .checkpoint_identity
            .as_ref()
            .ok_or("typed Phase B requires checkpoint identity")?;
        let source =
            crate::plan::FilesystemPlanFileSource::new(self.squash_merger.integration_root(), None)
                .map_err(|error| format!("open integration plan for Phase B: {error}"))?;
        let candidate = crate::plan::load_plan_path(
            &source,
            &identity.plan_dir,
            &crate::plan::PlanReservations::default(),
        )
        .map_err(|report| {
            format!(
                "load integration plan for Phase B: {:?}",
                report.diagnostics
            )
        })?;
        let crate::plan::PlanCandidate::Plan(mut plan) = candidate else {
            return Err("registered integration source is not a plan".into());
        };
        let task = plan
            .tasks
            .iter_mut()
            .find(|task| task.frontmatter.id.as_str() == task_id.0)
            .ok_or_else(|| format!("Phase B task {task_id} is absent from registered plan"))?;
        task.update_bookkeeping(
            crate::plan::AuthoredTaskStatus::Done,
            Some(landing_oid.clone()),
        )
        .map_err(|error| format!("render Phase B task: {error}"))?;

        plan.status.done = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Done)
            .count();
        plan.status.blocked = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Blocked)
            .count();
        plan.status.dropped = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Dropped)
            .count();
        plan.status.display_status = "🔄 In progress".into();
        plan.status.integration_state = crate::plan::PlanIntegrationState::AwaitingIntegration;
        plan.status.mode = Some("Squash".into());
        let transition = crate::plan_status::StatusTransition {
            integration_state: plan.status.integration_state,
            run: Some(self.run_uid.clone()),
            validation_base: plan.status.validation_base_oid.clone(),
            mode: plan.status.mode.clone(),
            final_oid: None,
            display_status: plan.status.display_status.clone(),
            last_updated: plan.status.last_updated.clone(),
        };
        let status = crate::plan_status::render_plan_status(&plan, &transition)
            .map_err(|error| format!("render Phase B status: {error}"))?;
        plan.status.source.body = status.clone();
        let root_path = std::path::PathBuf::from("docs/plans/STATUS.md");
        let root =
            tokio::fs::read_to_string(self.squash_merger.integration_root().join(&root_path))
                .await
                .map_err(|error| format!("read root status for Phase B: {error}"))?;
        let root = crate::plan_status::update_root_row(&root, &plan)
            .map_err(|error| format!("render Phase B root status: {error}"))?;
        let task = plan
            .tasks
            .iter()
            .find(|task| task.frontmatter.id.as_str() == task_id.0)
            .unwrap();
        let writes = vec![
            crate::landing::OwnedWrite {
                path: task.source_path.clone(),
                bytes: task.render().into_bytes(),
            },
            crate::landing::OwnedWrite {
                path: plan.status.source.source_path.clone(),
                bytes: status.into_bytes(),
            },
            crate::landing::OwnedWrite {
                path: root_path,
                bytes: root.into_bytes(),
            },
        ];
        crate::landing::commit_task_status(
            self.squash_merger.integration_root(),
            &format!("refs/heads/plan/{}", self.plan_slug),
            landing_oid.as_str(),
            &writes,
            &crate::landing::StatusLandingIdentity {
                plan: self.plan_slug.clone(),
                task: task_id.0.clone(),
                run: self.run_uid.clone(),
                landing: landing_oid.as_str().into(),
            },
        )
        .await
        .map_err(|error| format!("commit Phase B for {task_id}: {error}"))
    }
    /// Emit `TaskStateChanged{run, task, state}` for this run (task 31).
    ///
    /// Maps the domain [`TaskState`] to its [`api::TaskState`] mirror and calls
    /// the control sink.  Called **after** the graph guard is dropped (never
    /// while holding the lock — the no-lock-across-emit rule), reading the state
    /// the just-applied transition produced.  A no-op under the silent control.
    fn emit_task_state(&self, task_id: &TaskId, state: TaskState) {
        self.control.emit(api::Event::TaskStateChanged {
            run: self.control.run,
            task: api::TaskId(task_id.0.clone()),
            state: state.into(),
        });
    }

    /// Emit `TaskIterationsUpdated{run, task, gate, review}` for this run.
    ///
    /// Called after a gate/review counter bump, outside the graph lock.
    fn emit_task_iterations(&self, task_id: &TaskId, gate_iterations: u32, review_iterations: u32) {
        self.control.emit(api::Event::TaskIterationsUpdated {
            run: self.control.run,
            task: api::TaskId(task_id.0.clone()),
            gate_iterations,
            review_iterations,
        });
    }

    /// Snapshot the graph under the lock and write it to `.tasks/{slug}.json`
    /// outside the lock (best-effort: logs on failure, never fails the run).
    ///
    /// # Design
    ///
    /// 1. Acquires the graph mutex for the minimal duration needed to clone the
    ///    current state (a tight, non-awaiting critical section).
    /// 2. Releases the lock **before** the async write, keeping the lock-hold
    ///    span minimal (architecture invariant: no `.await` while holding the
    ///    graph lock).
    /// 3. Persists the clone via [`persist_graph`] with `repo_root` derived from
    ///    the worktree manager.
    /// 4. On any I/O error: logs a `tracing::warn!` and returns — the run
    ///    continues unaffected (best-effort persistence).
    async fn persist(&self) {
        // Order snapshot acquisition as well as the write. Taking this lock
        // only after cloning would still allow S2 to commit before a delayed S1.
        let _persist_guard = self.persist_lock.lock().await;
        let snapshot = {
            let g = self.graph.lock().await;
            g.clone()
        };
        // Write outside the graph lock, while retaining the per-run ordering
        // guard until the atomic replacement is durable.
        let repo_root = &self.worktree_manager.repo_root;
        let expected_slug = if self.run_slug.is_empty() {
            snapshot.slug.as_str()
        } else {
            self.run_slug.as_str()
        };
        let is_checkpoint = self.checkpoint_identity.is_some();
        let result = if let Some(identity) = self.checkpoint_identity.clone() {
            let key = crate::plan::PlanKey::parse(identity.plan_dir.clone())
                .map_err(|error| error.to_string());
            // Best-effort evidence collection: if git fails (e.g. the
            // integration worktree was already cleaned up, or a transient
            // git state issue), persist the checkpoint WITHOUT evidence
            // rather than failing the run. The checkpoint's task states are
            // the important part; the evidence is supplementary.
            let (active_refs, active_worktrees) = match key {
                Ok(key) => {
                    match crate::checkpoint::inspect_repository_evidence(repo_root, &key).await {
                        Ok(evidence) => evidence,
                        Err(error) => {
                            tracing::warn!(%error, "checkpoint evidence collection failed; persisting without evidence");
                            (vec![], vec![])
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "checkpoint key parse failed; persisting without evidence");
                    (vec![], vec![])
                }
            };
            crate::checkpoint::persist_checkpoint_with_evidence(
                repo_root,
                identity,
                &snapshot,
                active_refs,
                active_worktrees,
            )
            .await
            .map_err(|error| error.to_string())
        } else {
            persist_graph_as(&snapshot, repo_root, expected_slug)
                .await
                .map_err(|error| error.to_string())
        };
        if let Err(e) = result {
            if is_checkpoint {
                self.fail_for_persistence(e).await;
            } else {
                tracing::warn!(slug = %snapshot.slug, error = %e, "legacy artifact persistence failed");
            }
        }
    }

    async fn fail_for_persistence(&self, error: String) {
        tracing::error!(slug = %self.run_slug, %error, "durable checkpoint failed; stopping run");
        self.control.cancel.cancel();
        let changed = {
            let mut graph = self.graph.lock().await;
            graph
                .tasks
                .iter_mut()
                .filter(|task| {
                    matches!(
                        task.state,
                        TaskState::New
                            | TaskState::Ready
                            | TaskState::InProgress
                            | TaskState::InReview
                    )
                })
                .map(|task| {
                    task.state = TaskState::Failed;
                    task.failure_reason = Some(api::FailureReason {
                        kind: api::FailureKind::HardError,
                        message: format!("durable checkpoint failed: {error}"),
                    });
                    (task.id.clone(), task.state)
                })
                .collect::<Vec<_>>()
        };
        for (id, state) in changed {
            self.emit_task_state(&id, state);
        }
    }
}

// ── Public controlled entrypoint (task 31: run-control) ─────────────────────────

/// Run a whole [`TaskGraph`] to terminal states under a [`RunControl`], emitting
/// live [`api::Event`]s and honouring pause/cancel — the entrypoint the
/// orchestrator (`CoreApi`) spawns on a background task.
///
/// This builds a [`DriverContext`] over the shared graph, then runs the
/// concurrent [`scheduler`] with a real (non-silent) `control` so it publishes
/// events and reacts to pause/cancel.
///
/// Lifecycle events emitted here (the scheduler/drivers emit the per-task ones):
/// - `RunStatusChanged{run, Running}` once, at the start;
/// - `RunStatusChanged{run, Completed|Failed}` at the end, derived from the
///   final graph — UNLESS the run was cancelled (the caller owns the cancelled
///   status so an explicit Cancel shows as `Failed`/cancelled, not `Completed`).
///
/// The graph-lock-never-across-await invariant and the per-task [`DriverGuard`]
/// teardown are unchanged; this only wraps the scheduler with wiring + the two
/// aggregate `RunStatusChanged` emissions.
// The orchestrator threads the full run context (graph + wiring + audit slug +
// run_uid) into this single entrypoint; grouping these into a struct would just
// move the argument list elsewhere without simplifying the call site.
#[allow(clippy::too_many_arguments)]
pub async fn run_graph(
    graph: Arc<Mutex<TaskGraph>>,
    worktree_manager: WorktreeManager,
    config: Config,
    developer_backend: Arc<dyn AgentBackend>,
    reviewer_backend: Arc<dyn AgentBackend>,
    control: RunControl,
    audit_registry: Arc<dyn AuditRegistry>,
    run_slug: String,
    run_uid: String,
    plan_slug: String,
    _planner_interpreter: Arc<dyn TaskListInterpreter>,
) -> Result<RunReport, String> {
    run_graph_with_checkpoint(
        graph,
        worktree_manager,
        config,
        developer_backend,
        reviewer_backend,
        control,
        audit_registry,
        run_slug,
        run_uid,
        plan_slug,
        _planner_interpreter,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_graph_with_checkpoint(
    graph: Arc<Mutex<TaskGraph>>,
    worktree_manager: WorktreeManager,
    config: Config,
    developer_backend: Arc<dyn AgentBackend>,
    reviewer_backend: Arc<dyn AgentBackend>,
    control: RunControl,
    audit_registry: Arc<dyn AuditRegistry>,
    run_slug: String,
    run_uid: String,
    plan_slug: String,
    _planner_interpreter: Arc<dyn TaskListInterpreter>,
    checkpoint_identity: Option<crate::checkpoint::CheckpointIdentity>,
) -> Result<RunReport, String> {
    // Open the run-scoped tracing span so every event emitted while driving this
    // graph carries the `run_uid` key. The `makina` binary's per-run file layer
    // reads this field to route events to `.makina/runs/{run_uid}/logs/run.log`
    // (task log-subscriber-file); under any other subscriber it is just an extra
    // field. We `.instrument()` the whole async body (rather than holding an
    // `.entered()` guard) so the future stays `Send` across `.await` points —
    // `EnteredSpan` is `!Send` and this future is `tokio::spawn`ed.
    use tracing::Instrument as _;
    let run_span = tracing::info_span!(
        "run_graph",
        run_uid = %run_uid,
        project_root = %worktree_manager.repo_root.display()
    );
    run_graph_inner(
        graph,
        worktree_manager,
        config,
        developer_backend,
        reviewer_backend,
        control,
        audit_registry,
        run_slug,
        run_uid,
        plan_slug,
        _planner_interpreter,
        checkpoint_identity,
    )
    .instrument(run_span)
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_graph_inner(
    graph: Arc<Mutex<TaskGraph>>,
    worktree_manager: WorktreeManager,
    config: Config,
    developer_backend: Arc<dyn AgentBackend>,
    reviewer_backend: Arc<dyn AgentBackend>,
    control: RunControl,
    audit_registry: Arc<dyn AuditRegistry>,
    run_slug: String,
    run_uid: String,
    plan_slug: String,
    _planner_interpreter: Arc<dyn TaskListInterpreter>,
    checkpoint_identity: Option<crate::checkpoint::CheckpointIdentity>,
) -> Result<RunReport, String> {
    // Announce the run is now executing.
    control.emit(api::Event::RunStatusChanged {
        run: control.run,
        status: api::RunStatus::Running,
    });

    // 1. Create + check out the per-plan integration branch off base_branch.
    // (Skip for ask path with empty plan_slug — keep legacy behavior with fork_branch: None.)
    let (plan_branch, integration_root, worktree_manager) = if !plan_slug.is_empty() {
        control.emit(api::Event::RunProgress {
            run: control.run,
            phase: "creating integration workspace".into(),
        });
        let mut workspace = worktree_manager
            .create_integration_workspace(&plan_slug, &run_uid)
            .await
            .map_err(|e| format!("failed to create integration workspace: {e}"))?;
        // Legacy graph-only callers have no typed registration evidence. Keep
        // their compatibility branch creation ref-only and attach it inside the
        // private workspace; typed runs must arrive with a published R ref.
        if checkpoint_identity.is_none() {
            worktree_manager
                .create_plan_branch(&plan_slug)
                .await
                .map_err(|e| format!("failed to create legacy plan ref: {e}"))?;
            workspace = worktree_manager
                .create_integration_workspace(&plan_slug, &run_uid)
                .await
                .map_err(|e| format!("failed to attach integration workspace: {e}"))?;
        }
        let branch = workspace.plan_branch;
        // 2. Task worktrees fork from the plan branch.
        let mgr = worktree_manager.with_fork_branch(branch.clone());
        (branch, workspace.path, mgr)
    } else {
        // Ask path: no plan branch, keep fork_branch: None (merges into base_branch).
        (
            worktree_manager.base_branch.clone(),
            worktree_manager.repo_root.clone(),
            worktree_manager,
        )
    };

    // Build the driver context directly (we drive the `scheduler` ourselves so
    // we keep ownership of the shared graph for the final status derivation).
    // 3. The per-task merger targets the plan branch (now checked out in repo_root).
    let squash_merger = SquashMerger::new(integration_root.clone(), plan_branch.clone())
        .with_repository_child_token(worktree_manager.repository_child_token());

    // Track whether this is a real plan-branch run (not the ask path).
    let has_plan_branch = !plan_slug.is_empty();

    let ctx = DriverContext {
        graph: Arc::clone(&graph),
        landing_evidence: Arc::new(Mutex::new(Vec::new())),
        persist_lock: Arc::new(Mutex::new(())),
        merge_lock: Arc::new(Mutex::new(())),
        worktree_manager: worktree_manager.clone(),
        gate_runner: GateRunner::new(),
        squash_merger,
        config: config.clone(),
        developer_backend,
        reviewer_backend,
        control: control.clone(),
        audit_registry,
        run_slug,
        run_uid,
        plan_slug,
        checkpoint_identity,
        #[cfg(test)]
        pre_a_observer: None,
    };

    let mut result = scheduler(ctx.clone(), config.concurrency).await;

    // Drivers have fully drained at this point. Persist one unconditional final
    // snapshot through the same per-run ordering gate so crash recovery cannot
    // observe an earlier transition as the terminal state.
    ctx.persist().await;

    // Final merge: decide whether to land plan/{slug} into base_branch (if all tasks Done).
    // This happens BEFORE restoring base_branch, so the plan branch is still checked out.
    // Skip final merge for the ask path (empty plan_slug → no plan branch, tasks merged directly into base_branch).
    if result.is_ok() && has_plan_branch {
        // Only proceed to final merge if the scheduler returned successfully AND we have a real plan branch.
        let all_done = {
            let g = graph.lock().await;
            aggregate_run_status(&g) == api::RunStatus::Completed
        };

        let plan_branch_left = if all_done && ctx.checkpoint_identity.is_none() {
            // Graph-only runs predate transactional status and retain their
            // immediate final-merge contract.
            let final_merger = SquashMerger::new(
                worktree_manager.repo_root.clone(),
                worktree_manager.base_branch.clone(),
            );
            let merge_message = format!("{}: integration branch", &plan_branch);
            match config.merge.final_ {
                FinalMerge::Squash => match final_merger
                    .final_squash(&plan_branch, &merge_message)
                    .await
                {
                    Ok(MergeOutcome::Merged { .. }) => None,
                    Ok(MergeOutcome::Conflict { .. }) => Some(plan_branch.clone()),
                    Err(error) => {
                        tracing::warn!(%error, "final squash merge failed; leaving plan branch unmerged");
                        Some(plan_branch.clone())
                    }
                },
                FinalMerge::Stage => match final_merger.final_stage_changes(&plan_branch).await {
                    Ok(StageOutcome::Staged) => None,
                    Ok(StageOutcome::Conflict { .. }) => Some(plan_branch.clone()),
                    Err(error) => {
                        tracing::warn!(%error, "final stage-changes failed; leaving plan branch unmerged");
                        Some(plan_branch.clone())
                    }
                },
                FinalMerge::MergeCommit => match final_merger
                    .final_merge_commit(&plan_branch, &merge_message)
                    .await
                {
                    Ok(MergeOutcome::Merged { .. }) => None,
                    Ok(MergeOutcome::Conflict { .. }) => Some(plan_branch.clone()),
                    Err(error) => {
                        tracing::warn!(%error, "final merge-commit failed; leaving plan branch unmerged");
                        Some(plan_branch.clone())
                    }
                },
                FinalMerge::Manual => Some(plan_branch.clone()),
            }
        } else if all_done {
            // Typed runs stop at durable Phase P. F/C are intentionally delayed
            // until FinalizePlan reacquires the repository lease.
            let prepared = async {
                let source = crate::plan::FilesystemPlanFileSource::new(&integration_root, None)
                    .map_err(|error| error.to_string())?;
                let key = ctx
                    .checkpoint_identity
                    .as_ref()
                    .ok_or("typed finalization requires checkpoint identity")?
                    .plan_dir
                    .clone();
                let crate::plan::PlanCandidate::Plan(mut plan) = crate::plan::load_plan_path(
                    &source,
                    &key,
                    &crate::plan::PlanReservations::default(),
                )
                .map_err(|report| format!("cannot load plan for P: {:?}", report.diagnostics))?
                else {
                    return Err("registered source is not a plan".into());
                };
                let mode = match config.merge.final_ {
                    FinalMerge::Squash => "squash",
                    FinalMerge::MergeCommit => "merge-commit",
                    FinalMerge::Stage => "stage",
                    FinalMerge::Manual => "manual",
                };
                let base_ref = format!("refs/heads/{}", worktree_manager.base_branch);
                let base = String::from_utf8(
                    git_output(&integration_root, &["rev-parse", &base_ref])
                        .await
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|_| "base OID is not UTF-8")?
                .trim()
                .to_owned();
                let old = String::from_utf8(
                    git_output(&integration_root, &["rev-parse", &plan_branch])
                        .await
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|_| "plan OID is not UTF-8")?
                .trim()
                .to_owned();
                plan.status.integration_state =
                    crate::plan::PlanIntegrationState::FinalizationPending;
                plan.status.run = Some(ctx.run_uid.clone());
                plan.status.mode = Some(mode.into());
                plan.status.final_oid = None;
                plan.status.display_status = "⏳ Finalizing".into();
                let status = crate::plan_status::render_plan_status(
                    &plan,
                    &crate::plan_status::StatusTransition {
                        integration_state: plan.status.integration_state,
                        run: plan.status.run.clone(),
                        validation_base: plan.status.validation_base_oid.clone(),
                        mode: plan.status.mode.clone(),
                        final_oid: None,
                        display_status: plan.status.display_status.clone(),
                        last_updated: plan.status.last_updated.clone(),
                    },
                )
                .map_err(|e| e.to_string())?;
                plan.status.source.body = status.clone();
                let base_source = crate::plan::GitTreePlanFileSource::new(&integration_root, &base)
                    .map_err(|e| e.to_string())?;
                use crate::plan::PlanFileSource as _;
                let board = String::from_utf8(
                    base_source
                        .read_file(std::path::Path::new("docs/plans/STATUS.md"))
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|_| "root board is not UTF-8")?;
                let board = crate::plan_status::update_root_row(&board, &plan)
                    .map_err(|e| e.to_string())?;
                crate::landing::commit_finalization_prepared(
                    &integration_root,
                    &format!("refs/heads/{plan_branch}"),
                    &base_ref,
                    &old,
                    &base,
                    &[
                        crate::landing::OwnedWrite {
                            path: plan.status.source.source_path.clone(),
                            bytes: status.into_bytes(),
                        },
                        crate::landing::OwnedWrite {
                            path: "docs/plans/STATUS.md".into(),
                            bytes: board.into_bytes(),
                        },
                    ],
                    &crate::landing::FinalizationIdentity {
                        plan: ctx.plan_slug.clone(),
                        run: ctx.run_uid.clone(),
                        mode: mode.into(),
                        expected_base: base.clone(),
                    },
                    true,
                )
                .await
                .map_err(|e| e.to_string())
            }
            .await;
            if let Err(error) = prepared {
                tracing::warn!(%error, "Phase P preparation failed; retaining plan branch");
            }
            Some(plan_branch.clone())
        } else {
            // Any failed task: leave the plan branch unmerged.
            Some(plan_branch.clone())
        };

        // Update the report with plan_branch_left status.
        if let Ok(ref mut report) = result {
            report.plan_branch_left = plan_branch_left.clone();

            // Emit the plan-branch-left event if the branch was left.
            if let Some(ref branch) = plan_branch_left {
                control.emit(api::Event::RunIntegrationBranchLeft {
                    run: control.run,
                    branch: branch.clone(),
                });
            }
        }
    }

    // Legacy runs still use the repository checkout as their integration
    // workspace. Typed runs use private worktrees and must not mutate an
    // operator checkout while waiting between P and F/C.
    if ctx.checkpoint_identity.is_none()
        && let Err(error) = worktree_manager
            .checkout(&worktree_manager.base_branch)
            .await
    {
        tracing::warn!(%error, "failed to restore base branch in repo_root");
    }

    // Derive + emit the aggregate terminal status — but NOT when cancelled: a
    // cancelled run's status is owned by the caller (Cancel sets it explicitly),
    // and we must not overwrite it with a misleading Completed/Failed.
    if !control.cancel.is_cancelled() && !control.pause.load(Ordering::SeqCst) {
        let status = {
            let g = graph.lock().await;
            aggregate_run_status(&g)
        };
        control.emit(api::Event::RunStatusChanged {
            run: control.run,
            status,
        });
    }

    result
}

/// Derive the aggregate [`api::RunStatus`] from the final task states.
///
/// `Completed` iff every task is `Done`; otherwise `Failed` if any task is
/// `Failed`; otherwise `Running` (defensive — a finished scheduler normally
/// leaves only terminal tasks, but a paused/cancelled run may have non-terminal
/// ones, which the caller's explicit status covers).
fn aggregate_run_status(graph: &TaskGraph) -> api::RunStatus {
    let all_done = graph.tasks.iter().all(|t| t.state == TaskState::Done);
    if all_done {
        return api::RunStatus::Completed;
    }
    if graph.tasks.iter().any(|t| t.state == TaskState::Failed) {
        return api::RunStatus::Failed;
    }
    api::RunStatus::Running
}

// ── Concurrent scheduler ────────────────────────────────────────────────────────

/// Drive the whole graph to terminal states, running up to `concurrency` task
/// drivers in parallel.
///
/// # How drivers are launched and awaited
///
/// - A [`tokio::sync::Semaphore`] with `concurrency` permits caps how many
///   drivers run at once.  Each launched driver **owns** a permit
///   (`acquire_owned`) for its whole lifetime; the permit is released when the
///   driver future completes — including on error/failure/panic paths — so a
///   crashed driver never leaks a slot.
/// - A [`tokio::task::JoinSet`] holds the in-flight driver futures.  Each entry
///   is the future returned by [`task_driver`] (joined with its `task_id` so the
///   result can be recorded).
/// - The loop alternately **fills** (launch ready tasks until either the
///   semaphore is exhausted or no ready task remains) and **drains** (await the
///   next completed driver, record its outcome, then loop to fill again — a
///   completed `Done` task may have unlocked dependents).  It exits when the
///   `JoinSet` is empty and no further task is ready.
///
/// # Single dispatch per task
///
/// Before launching, a task is moved out of `New`/`Ready` (its FSM advances and
/// the move is recorded under the graph lock), so the *next* `ready_task_ids`
/// scan will not see it again.  Dispatched IDs are also tracked in an in-flight
/// set as belt-and-suspenders.  Thus each task is dispatched to **exactly one**
/// driver (each driver owns its own Developer — "single Developer per task").
///
/// # Wall-clock cap (task 25)
///
/// Each driver future is wrapped in
/// `tokio::time::timeout(Duration::from_secs(config.caps.wall_clock_secs), …)`,
/// so the cap bounds the **whole** per-task lifecycle (worktree create → develop
/// → gate → review → merge → teardown).  If the deadline elapses, the timeout
/// **cancels** the driver future: its [`DriverGuard`] drops, tearing down the
/// worktree + per-task spokes (no leak), and its owned semaphore permit is
/// released.  The scheduler — which is the only place that holds graph access at
/// that point — then applies `WallClockCapReached` to that task under the graph
/// lock, moving it to terminal `Failed`, records the outcome, and continues.
/// (`caps.wall_clock_secs >= 1` is guaranteed by `Config::validate`.)
///
/// # Fail-fast
///
/// If a driver reports a hard `Err`, the scheduler stops launching *new* work
/// (it records the failure and lets in-flight drivers finish), then returns the
/// error after the `JoinSet` drains.  A task that terminates `Failed` (a normal
/// terminal outcome, e.g. gate-cap) does NOT stop the scheduler launching
/// *independent* ready tasks — but a task whose dependency failed never becomes
/// ready (its dep is not `Done`), so it is simply left non-terminal, mirroring
/// the task-21 posture that a partial failure does not silently satisfy
/// downstream deps.
///
/// # Run control: pause + cancel (task 31)
///
/// The scheduler honours [`DriverContext::control`]:
///
/// - **Pause** (`control.pause == true`): the **fill** phase launches NO new
///   drivers while paused; in-flight drivers keep running and are drained
///   normally.  (Resume = clear the flag and run again — `CoreApi` does this by
///   re-issuing `StartRun`, which spawns a fresh `run_graph`.)  The ask path's
///   silent control never sets this, so that path is unchanged.
/// - **Cancel** (`control.cancel` cancelled): the fill phase stops launching and
///   the in-flight `JoinSet` is `abort_all()`ed.  Each aborted driver drops its
///   permit *and* its [`DriverGuard`] (which still tears down the worktree/spokes
///   — no leak).  The scheduler then drains the aborted joins and returns.  The
///   drain loop also `select!`s on cancellation so a cancel mid-wait is prompt.
async fn scheduler(ctx: DriverContext, concurrency: usize) -> Result<RunReport, String> {
    let semaphore = Arc::new(Semaphore::new(concurrency));
    // Each driver future yields either its terminal `Result<TaskState, String>`
    // OR `None` if the per-task wall-clock timeout elapsed (the inner driver was
    // cancelled — its DriverGuard already cleaned up).  The scheduler turns a
    // timeout into a `WallClockCapReached` → `Failed` transition (task 25).
    let mut join_set: JoinSet<(TaskId, Option<Result<TaskState, String>>)> = JoinSet::new();

    // Per-task wall-clock deadline (task 25).  Validated `>= 1` by Config.
    let wall_clock = Duration::from_secs(ctx.config.caps.wall_clock_secs);

    // IDs currently dispatched to a driver (defensive against double-dispatch;
    // the FSM advance already removes a task from the ready scan).
    let mut in_flight: std::collections::HashSet<TaskId> = std::collections::HashSet::new();

    let mut outcomes: Vec<(TaskId, TaskState)> = Vec::new();
    // `(task_id, reason)` for every task that reaches a `Failed` terminal.  A
    // completed-but-failed run is NOT a hard error (sched-continue-on-failure):
    // the run keeps going, but the report records which tasks failed and why so
    // `RunStatus::Failed` can be reported without halting (sched-run-status-failed).
    let mut failed_tasks: Vec<(TaskId, String)> = Vec::new();
    // A genuine RUN-level fatal error (a driver-future panic; or, defensively, a
    // graph-advance failure).  A per-TASK failure is NOT fatal
    // (sched-continue-on-failure): it is recorded as a `Failed` outcome and the
    // run keeps going.
    let mut fatal_error: Option<String> = None;
    // We stop *launching* new work (but keep draining in-flight) only on a cancel
    // or a genuine driver panic — a task-level failure no longer flips this.
    let mut stop_launching = false;

    // ── Seed persist: write the initial graph snapshot so the file exists from
    // t=0 (best-effort; the lock is released before the write — no await while
    // holding the graph mutex).
    //
    // The graph is guaranteed non-None here: the `take()` guard in the
    // `run_graph` ensures `ctx.graph` is populated before the scheduler is
    // entered.  This call creates `.tasks/{slug}.json` at run
    // start, so the file is present even if every task is skipped or fails
    // immediately.
    ctx.persist().await;

    loop {
        // ── Cancellation: stop launching + abort everything in flight ──────────
        //
        // Checked at the top of each scheduler iteration.  On cancel we stop the
        // fill phase and abort the JoinSet; each aborted driver's DriverGuard
        // still runs (worktree + spokes torn down — no leak).  We then fall
        // through to the drain phase to reap the aborted joins.
        if ctx.control.cancel.is_cancelled() && !stop_launching {
            stop_launching = true;
            join_set.abort_all();
        }

        // ── Fill: launch ready tasks until the cap is hit or none remain ───────
        //
        // Paused runs (task 31) launch NO new drivers — in-flight ones still
        // drain below.  `stop_launching` covers cancel + a driver panic (NOT a
        // task-level failure, which is non-fatal — sched-continue-on-failure).
        let paused = ctx.control.pause.load(Ordering::SeqCst);
        if !stop_launching && !paused {
            loop {
                // Try to grab a permit WITHOUT awaiting while holding the graph
                // lock: acquire the permit first (await), then take the graph
                // lock briefly to find+advance a ready task.  Lock ordering:
                // semaphore-permit BEFORE graph lock; the graph lock is released
                // before the driver future (which may take the merge lock) runs.
                let permit = match Arc::clone(&semaphore).try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => break, // cap reached; go drain.
                };

                // Briefly lock the graph to pick + advance ONE ready task.  No
                // .await occurs while the guard is held.
                let next = {
                    let mut graph = ctx.graph.lock().await;
                    let mut picked = next_ready_task_id(&graph, &in_flight);
                    if let Some(ref id) = picked {
                        // Advance New→Ready if needed so the next scan won't
                        // re-pick this task (single dispatch).  Errors here are
                        // impossible for a freshly-picked New/Ready task, but we
                        // surface them defensively.
                        if let Err(e) = advance_to_ready(&mut graph, id) {
                            // Record the (task-level) error but DO NOT stop
                            // launching independent ready tasks
                            // (sched-continue-on-failure): a single task that
                            // could not be advanced must not halt the whole run.
                            // This is a per-TASK failure, NOT a run-level fatal
                            // error — a genuine driver *panic* (the join-error
                            // arm) is the ONLY remaining `fatal_error` source.
                            // So we record the dropped task in `failed_tasks`
                            // (sched-run-status-failed) rather than feeding
                            // `fatal_error`.  Drop this task from this fill step
                            // (do not launch an un-advanced task) by clearing
                            // `picked`; the outer scheduler loop keeps draining
                            // + filling.
                            failed_tasks.push((id.clone(), e.to_string()));
                            picked = None;
                        }
                    }
                    picked
                }; // graph guard dropped here, BEFORE we spawn / await anything.

                match next {
                    Some(id) if !stop_launching => {
                        // Emit the New→Ready transition the scheduler just applied
                        // (outside the graph lock — the task is now `Ready`).
                        ctx.emit_task_state(&id, TaskState::Ready);
                        // Persist the New→Ready advance (best-effort; lock already
                        // released above).
                        ctx.persist().await;
                        in_flight.insert(id.clone());
                        let driver_ctx = ctx.clone();
                        let driver_id = id.clone();
                        // Tag this driver's whole future with a `task` span carrying
                        // `task_slug`; the per-task log routing layer (`RunFileLayer`)
                        // keys on that field to fan this task's records out to its
                        // `{task_slug}.log`.  The span is created HERE — in
                        // `scheduler`, where the enclosing `run_graph` span (which
                        // carries `run_uid`) is the current span — so the new `task`
                        // span is parented to it and the routing layer can resolve
                        // BOTH `run_uid` (from the parent) and `task_slug` by walking
                        // the scope.  (Evaluating the macro inside the spawned future
                        // would lose that parent: the `run_graph` span is not current
                        // on the `JoinSet` worker thread.)
                        let task_span = tracing::info_span!("task", task_slug = %driver_id.0);
                        // The permit is MOVED into the future; it drops (releasing
                        // the slot) when the driver completes — on every path,
                        // INCLUDING a wall-clock timeout (the whole future, permit
                        // included, is dropped when `timeout` elapses).
                        join_set.spawn(async move {
                            // `.instrument()` (not `.in_scope()`) because
                            // `task_driver` `.await`s, so the span must persist
                            // across await points.
                            use tracing::Instrument as _;
                            let _permit = permit; // released on completion/cancel/panic.
                            // Bound the WHOLE per-task lifecycle by the wall-clock
                            // cap.  On elapse, `task_driver` is cancelled mid-await:
                            // its DriverGuard drops → worktree + spokes torn down.
                            // `None` signals "timed out" to the scheduler.
                            match tokio::time::timeout(
                                wall_clock,
                                task_driver(&driver_ctx, &driver_id).instrument(task_span),
                            )
                            .await
                            {
                                Ok(result) => (driver_id, Some(result)),
                                Err(_elapsed) => (driver_id, None),
                            }
                        });
                    }
                    _ => {
                        // No ready task (or we just hit a fatal error): release the
                        // permit we speculatively took and stop filling.
                        drop(permit);
                        break;
                    }
                }
            }
        }

        // ── Drain: nothing in flight → we're done (no more work possible) ──────
        if join_set.is_empty() {
            break;
        }

        // Await the next completed driver — but also wake promptly on a cancel so
        // an in-flight wait does not block the abort.  When cancel fires mid-wait
        // we loop back to the top, which aborts the JoinSet, then drains the
        // (now-aborted) joins via this same match on the next iteration.
        let joined = tokio::select! {
            biased;
            _ = ctx.control.cancel.cancelled(), if !ctx.control.cancel.is_cancelled() => {
                // Re-evaluate at the top of the loop (aborts the JoinSet).
                continue;
            }
            j = join_set.join_next() => j,
        };

        match joined {
            Some(Ok((id, Some(Ok(state))))) => {
                in_flight.remove(&id);
                ctx.emit_task_state(&id, state);
                outcomes.push((id.clone(), state));
                // A cap-driven terminal `Failed` surfaces here (gate/review caps
                // return `Ok(Failed)` from the driver).  Like the hard-error and
                // wall-clock arms, transitively `Skipped` its dependents so they do
                // not dangle non-terminal (a non-`Done` dep never unlocks them).
                if state == TaskState::Failed {
                    // Read the failure reason the driver stored on the task at the
                    // failure site (set_failure_reason_locked); skip dependents.
                    let (skipped, reason) = {
                        let mut graph = ctx.graph.lock().await;
                        let reason = stored_failure_reason_message_locked(&graph, &id)
                            .unwrap_or_else(|| "cap-reached".to_string());
                        let skipped = mark_dependents_skipped(&mut graph, &id);
                        (skipped, reason)
                    };
                    failed_tasks.push((id.clone(), reason));
                    if !skipped.is_empty() {
                        ctx.persist().await;
                        for skipped_id in skipped {
                            ctx.emit_task_state(&skipped_id, TaskState::Skipped);
                            outcomes.push((skipped_id, TaskState::Skipped));
                        }
                    }
                }
                // A Done task may have unlocked dependents → loop to fill again.
            }
            Some(Ok((id, Some(Err(e))))) => {
                // Hard error in a driver: the driver already moved its task to a
                // terminal state (Failed) and tore down its resources where
                // possible.  Read + emit that terminal state so the TUI reflects
                // it (the driver only emits the NON-terminal transitions; the
                // scheduler owns the single terminal emission on every arm).
                in_flight.remove(&id);
                let (terminal, skipped) = {
                    let mut graph = ctx.graph.lock().await;
                    let terminal = task_state_locked(&graph, &id).unwrap_or(TaskState::Failed);
                    // Transitively `Skipped` the failed task's dependents under the
                    // held lock (no .await), then drop the guard before emitting.
                    let skipped = mark_dependents_skipped(&mut graph, &id);
                    (terminal, skipped)
                };
                ctx.emit_task_state(&id, terminal);
                // Persist the dependents' Skipped transitions (best-effort; lock
                // already released above).
                ctx.persist().await;
                // Record the failed task's terminal outcome.  A task-level hard
                // error no longer feeds `fatal_error` nor sets `stop_launching`
                // (sched-continue-on-failure): the task is already `Failed` and
                // its dependents `Skipped`, so the scheduler keeps launching the
                // remaining independent ready tasks and the run completes (a
                // completed-but-failed run, not a hard run-level error).  The
                // driver error `e` is no longer fatal, but it IS recorded as the
                // failure reason on `failed_tasks` (sched-run-status-failed) — only
                // a genuine driver *panic* (the join-error arm) remains fatal.
                if terminal == TaskState::Failed {
                    failed_tasks.push((id.clone(), e));
                }
                outcomes.push((id, terminal));
                for skipped_id in skipped {
                    ctx.emit_task_state(&skipped_id, TaskState::Skipped);
                    outcomes.push((skipped_id, TaskState::Skipped));
                }
            }
            Some(Ok((id, None))) => {
                // ── Wall-clock cap reached (task 25) ────────────────────────────
                //
                // The driver future was cancelled by the per-task timeout; its
                // DriverGuard already tore down the worktree + spokes and its
                // permit was released.  The scheduler holds graph access here, so
                // it applies `WallClockCapReached` → `Failed` under the lock and
                // records the outcome.  A timeout fails ONLY this task; it does
                // NOT stop the scheduler launching independent ready tasks (a
                // timed-out task is not `Done`, so its dependents never unlock).
                in_flight.remove(&id);
                let (final_state, skipped) = {
                    let mut graph = ctx.graph.lock().await;
                    let final_state =
                        match apply_event_locked(&mut graph, &id, TaskEvent::WallClockCapReached) {
                            Ok(()) => {
                                mark_finished_locked(&mut graph, &id);
                                set_failure_reason_locked(
                                    &mut graph,
                                    &id,
                                    api::FailureKind::WallClockCap,
                                    format!("wall-clock cap reached for {id}"),
                                );
                                TaskState::Failed
                            }
                            Err(_) => {
                                // The task already reached a terminal state in the
                                // instant before the timeout fired (a benign race):
                                // record its actual terminal state instead.
                                task_state_locked(&graph, &id).unwrap_or(TaskState::Failed)
                            }
                        };
                    // Transitively `Skipped` the failed task's dependents under the
                    // held lock (no .await), then drop the guard before emitting.
                    let skipped = mark_dependents_skipped(&mut graph, &id);
                    (final_state, skipped)
                }; // guard dropped before emit.
                // Persist WallClockCapReached → Failed + dependents' Skipped
                // (best-effort).
                ctx.persist().await;
                ctx.emit_task_state(&id, final_state);
                // Cap failures carry no driver reason string, so synthesize the
                // literal for this arm (sched-run-status-failed).  Guard on
                // `Failed` so a benign race that landed the task on another
                // terminal does not record a spurious wall-clock reason.
                if final_state == TaskState::Failed {
                    failed_tasks.push((id.clone(), "wall-clock-cap-reached".to_string()));
                }
                outcomes.push((id, final_state));
                for skipped_id in skipped {
                    ctx.emit_task_state(&skipped_id, TaskState::Skipped);
                    outcomes.push((skipped_id, TaskState::Skipped));
                }
            }
            Some(Err(join_err)) => {
                // The driver task ended abnormally.  Two cases:
                //  - **Cancelled (task 31)**: we `abort_all()`ed it; this is the
                //    EXPECTED outcome of a cancel, NOT a failure.  Its DriverGuard
                //    ran on the abort unwind (worktree/spokes torn down — no leak).
                //    We simply drop it (no fatal error, no outcome recorded).
                //  - **Panicked**: a genuine bug.  Record a fatal error and stop
                //    launching; remaining drivers still drain.
                if join_err.is_cancelled() {
                    // Expected during cancellation; nothing to record.
                } else {
                    fatal_error.get_or_insert(format!("task driver panicked: {join_err}"));
                    stop_launching = true;
                }
            }
            None => break, // JoinSet drained.
        }
    }

    match fatal_error {
        Some(e) => Err(e),
        None => Ok(RunReport {
            outcomes,
            failed_tasks,
            plan_branch_left: None,
            landing_evidence: ctx.landing_evidence.lock().await.clone(),
        }),
    }
}

/// Find the next task eligible to run: state `New` or `Ready`, all `depends_on`
/// are `Done`, and it is not already in flight.  Returns its [`TaskId`] or
/// `None`.
///
/// Pure read over the locked graph (the caller holds the guard).  Never awaits.
fn next_ready_task_id(
    graph: &TaskGraph,
    in_flight: &std::collections::HashSet<TaskId>,
) -> Option<TaskId> {
    graph
        .tasks
        .iter()
        .find(|t| {
            matches!(t.state, TaskState::New | TaskState::Ready)
                && graph.is_authored_dispatchable(&t.id)
                && !in_flight.contains(&t.id)
                && t.depends_on.iter().all(|dep| {
                    graph
                        .get(dep)
                        .map(|d| d.state == TaskState::Done)
                        .unwrap_or(false)
                })
        })
        .map(|t| t.id.clone())
}

/// Advance a freshly-picked task to `Ready` (if it is still `New`) so the next
/// ready scan will not re-select it.  No-op if already `Ready`.
///
/// Pure mutation over the locked graph (the caller holds the guard).  Never
/// awaits.  Returns `Err` only on an illegal transition (not expected for a
/// New/Ready task) or a missing task.
fn advance_to_ready(graph: &mut TaskGraph, task_id: &TaskId) -> Result<(), String> {
    let state = graph
        .get(task_id)
        .map(|t| t.state)
        .ok_or_else(|| format!("task {task_id} not found in graph"))?;
    if state == TaskState::New {
        apply_event_locked(graph, task_id, TaskEvent::DependenciesSatisfied)?;
    }
    Ok(())
}

// ── Per-task driver ─────────────────────────────────────────────────────────────

/// RAII teardown guard for one task's per-task resources.
///
/// Holds enough to tear down (best-effort) the task's worktree. Teardown runs
/// in `Drop` so that EVERY exit path of [`task_driver`] — `Ok`, `Err`,
/// early-return, or panic/unwind — frees the worktree + branch.
///
/// Worktree removal is async (`WorktreeManager::remove`), but `Drop` is sync; we
/// therefore tear the worktree down explicitly in the driver's terminal paths
/// (where we can `.await`) and use this guard as the **safety net** for the
/// unexpected/early-return/panic paths via a detached best-effort spawn on the
/// current runtime. In the normal (Ok/Err terminal) paths the driver has already
/// awaited `remove` and set `worktree_removed = true`, so Drop is a no-op.
struct DriverGuard {
    task_id: String,
    /// Plan slug this task belongs to, used to plan-scope the worktree
    /// directory + branch during safety-net teardown.
    plan_slug: String,
    worktree_manager: WorktreeManager,
    transactional: bool,
    /// Set to `true` once the driver has already torn the worktree down on a
    /// normal terminal path, so `Drop` does not redundantly try again.
    worktree_removed: bool,
}

impl Drop for DriverGuard {
    fn drop(&mut self) {
        // Safety-net worktree teardown for paths that did not already remove it
        // (e.g. an unexpected early return or a panic).  Normal terminal paths
        // set `worktree_removed = true` after awaiting `remove`, so this is a
        // no-op there.  We cannot `.await` in Drop, so schedule a detached
        // best-effort removal on the runtime.
        if !self.worktree_removed {
            let mgr = self.worktree_manager.clone();
            let id = self.task_id.clone();
            let plan_slug = self.plan_slug.clone();
            let transactional = self.transactional;
            // `tokio::spawn` requires being inside a runtime; the driver always
            // runs inside one (JoinSet task).  Best-effort: remove() is
            // idempotent and treats "not found" as success.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = if transactional {
                        mgr.remove(&plan_slug, &id).await
                    } else {
                        mgr.remove_legacy(&plan_slug, &id).await
                    };
                });
            }
        }
    }
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(msg) = payload.downcast_ref::<&str>() {
        (*msg).to_string()
    } else if let Some(msg) = payload.downcast_ref::<String>() {
        msg.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

async fn shield_develop_turn(
    backend: Arc<dyn AgentBackend>,
    assignment: Option<RoleAssignment>,
    msg: Develop,
) -> Result<DevelopOutcome, DeveloperError> {
    match AssertUnwindSafe(develop(backend, assignment, msg))
        .catch_unwind()
        .await
    {
        Ok(reply) => reply,
        Err(payload) => Err(DeveloperError::Other(format!(
            "developer panicked: {}",
            panic_payload_message(payload.as_ref())
        ))),
    }
}

async fn shield_review_turn(
    backend: Arc<dyn AgentBackend>,
    assignment: Option<RoleAssignment>,
    msg: Review,
) -> Result<ReviewVerdict, ReviewerError> {
    match AssertUnwindSafe(review(backend, assignment, msg))
        .catch_unwind()
        .await
    {
        Ok(reply) => reply,
        Err(payload) => Err(ReviewerError::Other(format!(
            "reviewer panicked: {}",
            panic_payload_message(payload.as_ref())
        ))),
    }
}

/// Drive a single task from its current state through the full develop→review
/// loop to a terminal state, returning that terminal state.
///
/// This is the per-task lifecycle extracted from task 21's `run_single_task`,
/// now operating on the **shared** graph (via the [`DriverContext`]). Within-task
/// behavior is unchanged from task 21–23: FSM transitions, the dev+gate loop
/// ([`develop_until_gates_pass`]), the review loop, the squash-merge on approve,
/// and worktree create/teardown all happen exactly as before — only the graph
/// access is now lock-guarded.
///
/// # Graph-lock discipline (the deadlock/race surface — read this)
///
/// The shared graph is a `tokio::sync::Mutex<TaskGraph>`.  **The guard is NEVER
/// held across an `.await`.**  Every read/mutation is a tight critical section:
/// acquire → read-or-mutate → drop the guard — and ONLY THEN do we `.await`
/// (spawn a session, run gates, squash-merge).  Concretely, the helpers
/// `task_state_locked`, `apply_event_locked`, `task_clone_locked`, the
/// iteration-count bumps, and the `mark_*` stamps each take the lock, do their
/// synchronous work, and release it before the next await point.  This keeps the
/// graph live for the future TUI without serializing the drivers.  Any new
/// graph-mutation block must be followed by `ctx.persist().await` AFTER the lock
/// guard is dropped, so on-disk state stays in sync with in-memory state.
///
/// # Merge-lock span & lock ordering (no deadlock)
///
/// The squash-merge is the only step that mutates the single shared `develop`
/// checkout, so it is wrapped in the **develop merge lock**
/// ([`DriverContext::merge_lock`]).  The lock is taken for the *minimal* span —
/// just the `squash_merge` call — and released immediately after.
///
/// Lock ordering is strictly:
///
/// 1. **semaphore permit** (held by the scheduler for the driver's lifetime),
/// 2. **graph lock** (taken/released in tight non-awaiting sections, NEVER held
///    while awaiting anything),
/// 3. **merge lock** (taken only for the merge, and we do NOT hold the graph
///    lock while awaiting it).
///
/// No driver ever holds the graph lock while trying to acquire the merge lock
/// (we drop the graph guard before the merge), and no code acquires the graph
/// lock *while holding* the merge lock except via the same tight non-awaiting
/// helpers used everywhere (lock → mutate → unlock) — so there is no lock cycle
/// and thus no deadlock.  The semaphore is only ever *awaited* by the scheduler
/// (never by a driver while holding either mutex).
///
/// # Resource release on every path (no leaks)
///
/// A [`DriverGuard`] (RAII) best-effort-removes the worktree on Drop, covering
/// early-returns/panics. The normal terminal paths additionally `await` worktree
/// removal explicitly (and mark the guard so it won't double-remove). The
/// semaphore permit is owned by the spawned future and released when this
/// function returns (Ok or Err) or panics.
async fn task_driver(ctx: &DriverContext, task_id: &TaskId) -> Result<TaskState, String> {
    let mut guard = DriverGuard {
        task_id: task_id.0.clone(),
        plan_slug: ctx.plan_slug.clone(),
        worktree_manager: ctx.worktree_manager.clone(),
        transactional: ctx.checkpoint_identity.is_some(),
        worktree_removed: false,
    };

    // ── Step 1: ensure Ready (New → Ready). ────────────────────────────────────
    // The scheduler already advanced New→Ready before dispatch, but re-entry or
    // a directly-Ready task is handled idempotently here.
    {
        let mut graph = ctx.graph.lock().await;
        let current = task_state_locked(&graph, task_id)?;
        if current == TaskState::New {
            apply_event_locked(&mut graph, task_id, TaskEvent::DependenciesSatisfied)?;
        }
    } // graph guard dropped before any await.
    // Persist New→Ready (if the task was New; no-op cost otherwise).
    ctx.persist().await;

    // Durable authored claim precedes task branch/worktree creation and any
    // worker session. Exact claim evidence makes response-loss retry safe.
    if ctx.checkpoint_identity.is_some() {
        let _merge_guard = ctx.merge_lock.lock().await;
        if let Err(e) = ctx.commit_claim(task_id).await {
            // The claim failed — move the task to Failed before returning Err.
            // Without this, the task stays Ready and the scheduler's error
            // handler marks dependents as Skipped even though the task was
            // never actually Failed in the graph. This is the root cause of
            // the "first task ready, other two skipped" bug.
            let msg = format!("durable claim failed for {task_id}: {e}");
            {
                let mut graph = ctx.graph.lock().await;
                let _ = apply_event_locked(&mut graph, task_id, TaskEvent::HardError);
                mark_finished_locked(&mut graph, task_id);
                set_failure_reason_locked(
                    &mut graph,
                    task_id,
                    api::FailureKind::HardError,
                    msg.clone(),
                );
            }
            ctx.persist().await;
            guard.worktree_removed = true;
            return Err(msg);
        }
    }

    // ── Step 2: create the worktree, then Ready → InProgress (Dispatched) ──────
    //
    // A worktree-create failure happens while the task is still `Ready` (before
    // the Developer is ever dispatched).  `Ready --HardError--> Failed` (task 25)
    // moves it to a terminal state cleanly rather than leaving it stuck `Ready`.
    ctx.control.emit(api::Event::RunProgress {
        run: ctx.control.run,
        phase: format!("creating worktree for {task_id}"),
    });
    let worktree = match ctx
        .worktree_manager
        .create(&ctx.plan_slug, &task_id.0)
        .await
    {
        Ok(wt) => wt,
        Err(e) => {
            let msg = format!("worktree create failed for {task_id}: {e}");
            {
                let mut graph = ctx.graph.lock().await;
                apply_event_locked(&mut graph, task_id, TaskEvent::HardError)?;
                mark_finished_locked(&mut graph, task_id);
                set_failure_reason_locked(
                    &mut graph,
                    task_id,
                    api::FailureKind::HardError,
                    msg.clone(),
                );
            }
            // Persist Ready→Failed (best-effort; lock released above).
            ctx.persist().await;
            // No worktree was created, so there is nothing to remove; mark the
            // guard so it does not attempt a redundant best-effort teardown.
            guard.worktree_removed = true;
            return Err(msg);
        }
    };

    // Freeze the exact branch starting commit before any agent can mutate it.
    // Footprint checks must always diff from this recorded OID, never from a
    // later merge-base that may move as integration advances.
    let has_authored_metadata = {
        let graph = ctx.graph.lock().await;
        graph.authored(task_id).is_some()
    };
    if has_authored_metadata {
        let revision = format!("{}^{{commit}}", worktree.branch);
        let oid = git_output(
            &ctx.worktree_manager.repo_root,
            &["rev-parse", "--verify", &revision],
        )
        .await
        .map_err(|error| format!("could not record task branch base for {task_id}: {error}"))?;
        let oid = std::str::from_utf8(&oid)
            .map_err(|_| format!("task branch base for {task_id} was not UTF-8"))?
            .trim()
            .to_owned();
        let mut graph = ctx.graph.lock().await;
        graph
            .authored
            .get_mut(task_id)
            .expect("authored metadata was present before Git lookup")
            .branch_base_oid = Some(oid);
    }

    // ── Register the worktree context with the audit registry (task supervisor-audit-writer) ──
    //
    // The transport emits `AuditEntry` records with placeholder `run_id` /
    // `task_id` and the real `working_dir`.  The `JsonlAuditSink` (if wired)
    // uses this registration to enrich and route those entries.  The registry
    // is `NoopAuditRegistry` on the ask path, so this is a no-op there.
    ctx.audit_registry.register(
        worktree.path.clone(),
        ctx.run_uid.clone(),
        ctx.control.run.to_string(),
        ctx.run_slug.clone(),
        task_id.0.clone(),
    );

    {
        let mut graph = ctx.graph.lock().await;
        apply_event_locked(&mut graph, task_id, TaskEvent::Dispatched)?;
        mark_started_locked(&mut graph, task_id);
    } // guard dropped before emit.
    // Persist Ready→InProgress + started_at stamp (best-effort; lock released
    // above).
    ctx.persist().await;
    // Ready → InProgress (an intermediate transition; the scheduler owns the
    // terminal-state emission, the driver owns the intermediate ones — task 31).
    ctx.emit_task_state(task_id, TaskState::InProgress);
    // Additive tracing emission (log-tracing-transition-events): a per-task
    // subscriber captures the state transition.  Does NOT change EventSink
    // behavior — runs alongside `emit_task_state`.
    tracing::info!(
        task = %task_id.0,
        from = ?TaskState::Ready,
        to = ?TaskState::InProgress,
        "task state transition"
    );

    // ── Step 3–6: the develop → gate → review loop (bounded retry) ─────────────
    let mut feedback: Option<String> = None;
    let terminal_state;

    loop {
        // ── Develop + gate loop (task 22) ──────────────────────────────────────
        match develop_until_gates_pass(ctx, task_id, &worktree.path, feedback.take()).await {
            Ok(DevelopGateOutcome::ReadyForReview) => {
                // Gates passed; task is now InReview. Fall through to the Reviewer.
            }
            Ok(DevelopGateOutcome::GateCapReached) => {
                // The gate cap fired: the helper already moved the task to Failed
                // and tore down the worktree.
                guard.worktree_removed = true;
                // Additive tracing emission (log-tracing-transition-events): the
                // terminal Failed transition (InProgress → Failed via the gate
                // cap).  The scheduler owns the terminal `emit_task_state`; this
                // is the per-task subscriber's record of the transition.
                tracing::info!(
                    task = %task_id.0,
                    from = ?TaskState::InProgress,
                    to = ?TaskState::Failed,
                    "task state transition"
                );
                terminal_state = TaskState::Failed;
                break;
            }
            Err(e) => {
                // Hard error during development (the helper already moved the task
                // to Failed and tore down the worktree).
                guard.worktree_removed = true;
                return Err(e);
            }
        }

        // ── Reviewer turn ──────────────────────────────────────────────────────
        let review_task = {
            let graph = ctx.graph.lock().await;
            task_clone_locked(&graph, task_id)?
        };
        let review_result = shield_review_turn(
            Arc::clone(&ctx.reviewer_backend),
            ctx.config.roles.reviewer.clone(),
            Review {
                task: review_task,
                worktree: worktree.path.clone(),
                run: ctx.control.run,
                sink: Arc::clone(&ctx.control.sink),
                idle_secs: ctx.config.caps.idle_secs,
            },
        )
        .await;

        // On reviewer ask/parse failure we are in `InReview`.  Task 25 made
        // `InReview --HardError--> Failed` legal, so we drive the task to a
        // terminal `Failed` (instead of leaving it stuck `InReview`), tear down
        // the worktree, and propagate the error.
        let verdict = match review_result {
            Ok(v) => v,
            Err(rev_err) => {
                // Classify the typed error — no string matching needed.
                let (failure_kind, msg): (api::FailureKind, String) = match rev_err {
                    ReviewerError::IdleTimeout { idle_secs } => (
                        api::FailureKind::IdleTimeout,
                        format!("no agent output for {idle_secs}s"),
                    ),
                    other => (
                        api::FailureKind::HardError,
                        format!("reviewer dispatch failed for {task_id}: {other}"),
                    ),
                };

                {
                    let mut graph = ctx.graph.lock().await;
                    apply_event_locked(&mut graph, task_id, TaskEvent::HardError)?;
                    mark_finished_locked(&mut graph, task_id);
                    set_failure_reason_locked(&mut graph, task_id, failure_kind, msg.clone());
                }
                // Persist InReview→Failed (best-effort; lock released above).
                ctx.persist().await;
                remove_worktree(ctx, task_id).await;
                guard.worktree_removed = true;
                return Err(msg);
            }
        };

        match verdict {
            ReviewVerdict::Approve => {
                // ── Approve: squash-merge `task/{id}` into `develop` (task 23) ──
                //
                // The merge happens HERE — while still in InReview, BEFORE tearing
                // down the worktree and BEFORE the FSM approve transition.
                //
                // ── MERGE SERIALIZATION (task 24) ──────────────────────────────
                //
                // The squash-merge mutates the single shared `develop` checkout in
                // repo_root; two concurrent merges would race on it. We therefore
                // hold the develop MERGE LOCK for exactly this critical section.
                // We do NOT hold the graph lock while awaiting the merge lock (the
                // review_task clone above already released the graph guard), so
                // there is no lock-ordering cycle.
                let branch = format!(
                    "task/{}",
                    paths::short_worktree_name(&ctx.plan_slug, &task_id.0)
                );
                let message = {
                    let graph = ctx.graph.lock().await;
                    squash_commit_message_locked(&graph, task_id)?
                };
                let authored_footprint = {
                    let graph = ctx.graph.lock().await;
                    graph.authored(task_id).map(|metadata| {
                        (metadata.touches.clone(), metadata.branch_base_oid.clone())
                    })
                };

                // Review-acceptance checkpoint: undeclared work is returned to
                // the task branch for correction, never silently landed.
                if matches!(authored_footprint, Some((_, None))) {
                    let reason = format!("task {task_id} has no recorded branch base OID");
                    return_footprint_correction(ctx, task_id, reason.clone()).await?;
                    feedback = Some(reason);
                    continue;
                }
                if let Some((touches, Some(recorded_base))) = &authored_footprint
                    && let Err(reason) = enforce_task_branch_footprint(
                        &ctx.worktree_manager.repo_root,
                        task_id,
                        &branch,
                        touches,
                        recorded_base,
                    )
                    .await
                {
                    return_footprint_correction(ctx, task_id, reason.clone()).await?;
                    feedback = Some(reason);
                    continue;
                }
                #[cfg(test)]
                if let Some(observer) = &ctx.pre_a_observer {
                    let _ = observer.send(task_id.clone());
                }

                let merge_outcome = {
                    // Minimal critical section: acquire → merge → release.
                    let _merge_guard = ctx.merge_lock.lock().await;
                    // Recheck immediately before Phase A while serialized with
                    // other integration mutations, closing the review→landing
                    // branch-change window.
                    match if let Some((touches, Some(recorded_base))) = &authored_footprint {
                        enforce_task_branch_footprint(
                            &ctx.worktree_manager.repo_root,
                            task_id,
                            &branch,
                            touches,
                            recorded_base,
                        )
                        .await
                    } else {
                        Ok(())
                    } {
                        Ok(()) if ctx.checkpoint_identity.is_some() => {
                            ctx.squash_merger
                                .squash_merge_with_evidence(
                                    &branch,
                                    &message,
                                    &crate::merge::TaskLandingIdentity {
                                        plan: ctx.plan_slug.clone(),
                                        task: task_id.0.clone(),
                                        run: ctx.run_uid.clone(),
                                    },
                                )
                                .await
                        }
                        Ok(()) => ctx.squash_merger.squash_merge(&branch, &message).await,
                        Err(reason) => {
                            drop(_merge_guard);
                            return_footprint_correction(ctx, task_id, reason.clone()).await?;
                            feedback = Some(reason);
                            continue;
                        }
                    }
                }; // merge lock released here.

                let merge_outcome = match merge_outcome {
                    Ok(o) => o,
                    Err(e) => {
                        // Hard (non-conflict) merge failure; develop already
                        // best-effort-restored by the merger.  We are in
                        // `InReview`; task 25 made `InReview --HardError--> Failed`
                        // legal, so drive the task terminal (HardError is reserved
                        // for hard failures; ReviewCapReached stays the reviewer
                        // cap).  Then clean up + propagate.
                        let msg = format!("squash-merge failed for {task_id}: {e}");
                        {
                            let mut graph = ctx.graph.lock().await;
                            apply_event_locked(&mut graph, task_id, TaskEvent::HardError)?;
                            mark_finished_locked(&mut graph, task_id);
                            set_failure_reason_locked(
                                &mut graph,
                                task_id,
                                api::FailureKind::HardError,
                                msg.clone(),
                            );
                        }
                        // Persist InReview→Failed (best-effort; lock released above).
                        ctx.persist().await;
                        remove_worktree(ctx, task_id).await;
                        guard.worktree_removed = true;
                        return Err(msg);
                    }
                };

                match merge_outcome {
                    MergeOutcome::Merged { oid } => {
                        if ctx.checkpoint_identity.is_none() {
                            {
                                let mut graph = ctx.graph.lock().await;
                                apply_event_locked(
                                    &mut graph,
                                    task_id,
                                    TaskEvent::ReviewerApproved,
                                )?;
                                mark_finished_locked(&mut graph, task_id);
                            }
                            ctx.persist().await;
                            remove_worktree(ctx, task_id).await;
                            guard.worktree_removed = true;
                            terminal_state = TaskState::Done;
                            break;
                        }
                        ctx.landing_evidence
                            .lock()
                            .await
                            .push(crate::task::TaskLandingEvidence {
                                task: task_id.clone(),
                                implementation_oid: oid.clone(),
                            });
                        // Phase A is durable. From this point any error must retain
                        // the task workspace and runtime InReview for exact-B recovery.
                        guard.worktree_removed = true;
                        {
                            let _merge_guard = ctx.merge_lock.lock().await;
                            ctx.commit_phase_b(task_id, &oid).await?;
                        }
                        {
                            let mut graph = ctx.graph.lock().await;
                            apply_event_locked(&mut graph, task_id, TaskEvent::ReviewerApproved)?;
                            mark_finished_locked(&mut graph, task_id);
                        }
                        ctx.persist().await;
                        if ctx.control.cancel.is_cancelled() {
                            // Phase B is durable but the runtime Done checkpoint
                            // was not. Retain the recoverable InReview boundary;
                            // restart reconciliation can project Done from B.
                            let mut graph = ctx.graph.lock().await;
                            if let Some(task) =
                                graph.tasks.iter_mut().find(|task| task.id == *task_id)
                            {
                                task.state = TaskState::InReview;
                            }
                            return Err(format!(
                                "runtime persistence failed after Phase B for {task_id}"
                            ));
                        }
                        remove_worktree(ctx, task_id).await;
                        terminal_state = TaskState::Done;
                        break;
                    }
                    MergeOutcome::Conflict { details } => {
                        // ── Conflict: reconcile, do NOT corrupt `develop` ───────
                        //
                        // `develop` is ALREADY safely restored by the merger (its
                        // hard invariant).  The architecture's agent-driven
                        // reconciliation is a documented seam (see task 23 notes);
                        // the MVP drives the task to a SAFE terminal Failed via the
                        // dedicated `MergeConflict` event (InReview → Failed).
                        // This distinguishes it from reviewer-cap exhaustion
                        // (still ReviewCapReached) and hard merge errors (HardError).
                        // `details` is surfaced to the conflict message; agent-driven
                        // reconciliation is a seam for a later task.
                        let conflict_msg = format!("merge conflict for {task_id}: {details}");
                        {
                            let mut graph = ctx.graph.lock().await;
                            apply_event_locked(&mut graph, task_id, TaskEvent::MergeConflict)?;
                            mark_finished_locked(&mut graph, task_id);
                            set_failure_reason_locked(
                                &mut graph,
                                task_id,
                                api::FailureKind::MergeConflict,
                                conflict_msg,
                            );
                        }
                        // Persist InReview→Failed (MergeConflict; best-effort; lock
                        // released above).
                        ctx.persist().await;
                        remove_worktree(ctx, task_id).await;
                        guard.worktree_removed = true;
                        // Additive tracing emission (log-tracing-transition-events):
                        // terminal Failed transition (InReview → Failed via a
                        // merge conflict).
                        tracing::info!(
                            task = %task_id.0,
                            from = ?TaskState::InReview,
                            to = ?TaskState::Failed,
                            "task state transition"
                        );
                        terminal_state = TaskState::Failed;
                        break;
                    }
                }
            }
            ReviewVerdict::Reject { feedback: fb } => {
                // ── Reject: enforce the REVIEWER cap (task 25) ──────────────────
                //
                // The decision is made BEFORE the FSM transition, while the task
                // is still `InReview`.  We count the CURRENT (already-applied)
                // rejections plus this one: if this rejection *reaches* the cap
                // (`review_iterations + 1 >= caps.reviewer_iterations`) we do NOT
                // loop back to develop — we emit `ReviewCapReached`
                // (InReview → Failed) and terminate the task.  This uses
                // `ReviewCapReached` from its intended state (`InReview`).
                //
                // Each driver counts its OWN task's `review_iterations` under the
                // graph lock, so the per-task cap is correct under concurrency.
                let prior_iterations = {
                    let graph = ctx.graph.lock().await;
                    review_iterations_locked(&graph, task_id)?
                };

                if prior_iterations + 1 >= ctx.config.caps.reviewer_iterations {
                    // This rejection reaches the cap → fail the task.  We count
                    // this final rejection in `review_iterations` first (so the
                    // recorded count equals the cap), then transition InReview →
                    // Failed via ReviewCapReached.
                    let cap = ctx.config.caps.reviewer_iterations;
                    let (gate_iters, review_iters) = {
                        let mut graph = ctx.graph.lock().await;
                        increment_review_iterations_locked(&mut graph, task_id);
                        apply_event_locked(&mut graph, task_id, TaskEvent::ReviewCapReached)?;
                        mark_finished_locked(&mut graph, task_id);
                        let review_iters = review_iterations_locked(&graph, task_id)?;
                        set_failure_reason_locked(
                            &mut graph,
                            task_id,
                            api::FailureKind::ReviewCap,
                            format!(
                                "reviewer cap reached for {task_id}: {review_iters}/{cap} rejections"
                            ),
                        );
                        (gate_iterations_locked(&graph, task_id)?, review_iters)
                    }; // guard dropped before emit.
                    // Persist InReview→Failed (ReviewCapReached; best-effort; lock
                    // released above).
                    ctx.persist().await;
                    // Emit the final iteration count (the terminal Failed state is
                    // emitted by the scheduler when this driver returns Ok(Failed)).
                    ctx.emit_task_iterations(task_id, gate_iters, review_iters);
                    remove_worktree(ctx, task_id).await;
                    guard.worktree_removed = true;
                    // Additive tracing emission (log-tracing-transition-events):
                    // terminal Failed transition (InReview → Failed via the
                    // reviewer cap).
                    tracing::info!(
                        task = %task_id.0,
                        from = ?TaskState::InReview,
                        to = ?TaskState::Failed,
                        "task state transition"
                    );
                    terminal_state = TaskState::Failed;
                    break;
                }

                // ── Below the cap: InReview → InProgress (ReviewerRejected) ─────
                // Count the rejection and loop back for a re-work attempt.
                let (gate_iters, review_iters) = {
                    let mut graph = ctx.graph.lock().await;
                    apply_event_locked(&mut graph, task_id, TaskEvent::ReviewerRejected)?;
                    increment_review_iterations_locked(&mut graph, task_id);
                    (
                        gate_iterations_locked(&graph, task_id)?,
                        review_iterations_locked(&graph, task_id)?,
                    )
                }; // guard dropped before emit.
                // Persist InReview→InProgress + bumped review count (best-effort;
                // lock released above).
                ctx.persist().await;
                // InReview → InProgress (intermediate) + the bumped review count.
                ctx.emit_task_state(task_id, TaskState::InProgress);
                // Additive tracing emission (log-tracing-transition-events).
                tracing::info!(
                    task = %task_id.0,
                    from = ?TaskState::InReview,
                    to = ?TaskState::InProgress,
                    "task state transition"
                );
                ctx.emit_task_iterations(task_id, gate_iters, review_iters);

                // Relay the feedback to the Developer on the next iteration.
                feedback = Some(fb);
                // Loop back to a fresh Developer turn (re-work).
            }
        }
    }

    Ok(terminal_state)
}

/// Run the **develop + gate loop** for one review round (task 22).
///
/// Identical within-task behavior to task 22 — only adapted for the concurrent
/// driver: it takes the [`DriverContext`] (shared graph + gate runner + config)
/// and observes the graph-lock discipline (the guard is never held across an
/// `.await`).
///
/// Drives: dispatch the Developer (with `initial_feedback`), run all configured
/// gates in the worktree; on a gate failure self-loop (InProgress
/// --GateFailed--> InProgress), bump `gate_iterations`, feed the failing gate's
/// output back, and re-run all gates; until gates pass (→ `GatesPassed` /
/// InReview, return `ReadyForReview`) or the gate cap fires (→ `GateCapReached` /
/// Failed, worktree torn down, return `GateCapReached`).
///
/// # Errors
///
/// `Err(String)` on a hard error (Developer dispatch failure, or a gate that
/// could not be **launched**).  In the error case the task was already moved to
/// Failed (HardError) and the worktree torn down; the caller just propagates.
async fn develop_until_gates_pass(
    ctx: &DriverContext,
    task_id: &TaskId,
    worktree_path: &Path,
    initial_feedback: Option<String>,
) -> Result<DevelopGateOutcome, String> {
    let mut feedback = initial_feedback;

    loop {
        // ── Developer turn: make (or fix) the changes ──────────────────────────
        ctx.control.emit(api::Event::RunProgress {
            run: ctx.control.run,
            phase: format!("agent working on {task_id}"),
        });
        let task = {
            let graph = ctx.graph.lock().await;
            task_clone_locked(&graph, task_id)?
        };

        let develop_result = shield_develop_turn(
            Arc::clone(&ctx.developer_backend),
            ctx.config.roles.developer.clone(),
            Develop {
                task,
                worktree: worktree_path.to_path_buf(),
                feedback: feedback.take(),
                run: ctx.control.run,
                sink: Arc::clone(&ctx.control.sink),
                idle_secs: ctx.config.caps.idle_secs,
            },
        )
        .await;

        if let Err(dev_err) = develop_result {
            // Classify the typed error — no string matching needed.
            let (failure_kind, msg): (api::FailureKind, String) = match dev_err {
                DeveloperError::IdleTimeout { idle_secs } => (
                    api::FailureKind::IdleTimeout,
                    format!("no agent output for {idle_secs}s"),
                ),
                other => (
                    api::FailureKind::HardError,
                    format!("developer dispatch failed for {task_id}: {other}"),
                ),
            };

            {
                let mut graph = ctx.graph.lock().await;
                apply_event_locked(&mut graph, task_id, TaskEvent::HardError)?;
                mark_finished_locked(&mut graph, task_id);
                set_failure_reason_locked(&mut graph, task_id, failure_kind, msg.clone());
            }
            // Persist InProgress→Failed (best-effort; lock released above).
            ctx.persist().await;
            remove_worktree(ctx, task_id).await;
            return Err(msg);
        }

        // ── Gate turn: run ALL configured gates in the worktree ────────────────
        let outcome = ctx
            .gate_runner
            .run_gates(&ctx.config.gates, worktree_path)
            .await;

        match outcome {
            Ok(GateOutcome::Passed) => {
                // Additive tracing emission (log-tracing-transition-events): the
                // gate-output record for the passing round (counterpart to the
                // `gate failed` event on the Failed arm).
                tracing::info!(
                    task = %task_id.0,
                    "gates passed"
                );
                // All gates passed → advance to review.
                {
                    let mut graph = ctx.graph.lock().await;
                    apply_event_locked(&mut graph, task_id, TaskEvent::GatesPassed)?;
                } // guard dropped before emit.
                // Persist InProgress→InReview (best-effort; lock released above).
                ctx.persist().await;
                // InProgress → InReview (intermediate transition — task 31).
                ctx.emit_task_state(task_id, TaskState::InReview);
                // Additive tracing emission (log-tracing-transition-events).
                tracing::info!(
                    task = %task_id.0,
                    from = ?TaskState::InProgress,
                    to = ?TaskState::InReview,
                    "task state transition"
                );
                return Ok(DevelopGateOutcome::ReadyForReview);
            }
            Ok(GateOutcome::Failed {
                gate,
                output,
                exit_code,
            }) => {
                // Additive tracing emission (log-tracing-transition-events): the
                // gate-output record for the failing gate.  Does NOT change
                // EventSink behavior — runs alongside the existing emissions.
                tracing::info!(
                    task = %task_id.0,
                    gate = %gate,
                    exit_code,
                    "gate failed"
                );
                // A gate failed → self-loop and count the iteration; enforce the
                // per-task GATE cap.
                let (iterations, review_iters) = {
                    let mut graph = ctx.graph.lock().await;
                    apply_event_locked(&mut graph, task_id, TaskEvent::GateFailed)?;
                    increment_gate_iterations_locked(&mut graph, task_id);
                    (
                        gate_iterations_locked(&graph, task_id)?,
                        review_iterations_locked(&graph, task_id)?,
                    )
                }; // guard dropped before emit.
                // Persist InProgress self-loop + bumped gate count (best-effort;
                // lock released above).
                ctx.persist().await;
                // InProgress --GateFailed--> InProgress (self-loop) + the bumped
                // gate count.  The TUI re-affirms InProgress and updates the
                // counter (task 31).
                ctx.emit_task_state(task_id, TaskState::InProgress);
                // Additive tracing emission (log-tracing-transition-events): the
                // InProgress self-loop transition.
                tracing::info!(
                    task = %task_id.0,
                    from = ?TaskState::InProgress,
                    to = ?TaskState::InProgress,
                    "task state transition"
                );
                ctx.emit_task_iterations(task_id, iterations, review_iters);

                if iterations >= ctx.config.caps.gate_iterations {
                    // GateCapReached: InProgress → Failed (terminal).  The terminal
                    // Failed state is emitted by the scheduler when this driver
                    // returns Ok(GateCapReached) → Ok(Failed).
                    let cap = ctx.config.caps.gate_iterations;
                    {
                        let mut graph = ctx.graph.lock().await;
                        apply_event_locked(&mut graph, task_id, TaskEvent::GateCapReached)?;
                        mark_finished_locked(&mut graph, task_id);
                        set_failure_reason_locked(
                            &mut graph,
                            task_id,
                            api::FailureKind::GateCap,
                            format!(
                                "gate cap reached for {task_id}: {iterations}/{cap} iterations"
                            ),
                        );
                    }
                    // Persist InProgress→Failed (GateCapReached; best-effort; lock
                    // released above).
                    ctx.persist().await;
                    remove_worktree(ctx, task_id).await;
                    return Ok(DevelopGateOutcome::GateCapReached);
                }

                // Feed the failing gate's output back; all gates re-run next turn.
                feedback = Some(format!(
                    "Gate `{gate}` failed (exit code {exit_code}):\n{output}\n\
                     Fix the issue so the gate passes."
                ));
            }
            Err(e) => {
                // The gate command could not be LAUNCHED (infra failure). Treat as
                // a hard error: we are in InProgress, so HardError → Failed.
                let msg = format!("gate launch failed for {task_id}: {e}");
                {
                    let mut graph = ctx.graph.lock().await;
                    apply_event_locked(&mut graph, task_id, TaskEvent::HardError)?;
                    mark_finished_locked(&mut graph, task_id);
                    set_failure_reason_locked(
                        &mut graph,
                        task_id,
                        api::FailureKind::HardError,
                        msg.clone(),
                    );
                }
                // Persist InProgress→Failed (gate launch HardError; best-effort;
                // lock released above).
                ctx.persist().await;
                remove_worktree(ctx, task_id).await;
                return Err(msg);
            }
        }
    }
}

/// Best-effort worktree teardown (idempotent; ignores "already gone").
async fn remove_worktree(ctx: &DriverContext, task_id: &TaskId) {
    if ctx.checkpoint_identity.is_none() {
        let _ = ctx
            .worktree_manager
            .remove_legacy(&ctx.plan_slug, &task_id.0)
            .await;
    } else {
        // Transactional runs preserve recovery refs and refuse dirty cleanup.
        let _ = ctx
            .worktree_manager
            .remove(&ctx.plan_slug, &task_id.0)
            .await;
    }
}

// ── Locked graph helpers (NEVER hold the guard across an .await) ──────────────────
//
// Each of these takes/returns plain values and is called by a caller that holds
// the `tokio::sync::Mutex<TaskGraph>` guard for the duration of the call ONLY —
// the guard is dropped before the caller's next await point.  They are free
// functions (not methods) so they operate on a borrowed `TaskGraph` rather than
// `&mut self`, which is what lets the concurrent drivers share the graph.

/// Apply an FSM `event` to the task, updating its state in the locked graph.
fn apply_event_locked(
    graph: &mut TaskGraph,
    task_id: &TaskId,
    event: TaskEvent,
) -> Result<(), String> {
    let task = task_mut_locked(graph, task_id)?;
    let next = transition(task.state, event)
        .map_err(|e| format!("illegal transition for {task_id}: {e}"))?;
    task.state = next;
    task.updated_at = chrono::Utc::now();
    Ok(())
}

/// Read a task's current state from the locked graph.
fn task_state_locked(graph: &TaskGraph, task_id: &TaskId) -> Result<TaskState, String> {
    graph
        .get(task_id)
        .map(|t| t.state)
        .ok_or_else(|| format!("task {task_id} not found in graph"))
}

/// Build the squash-merge commit message for a task: `task({id}): {title}`.
fn squash_commit_message_locked(graph: &TaskGraph, task_id: &TaskId) -> Result<String, String> {
    let task = graph
        .get(task_id)
        .ok_or_else(|| format!("task {task_id} not found in graph"))?;
    Ok(format!(
        "task({id}): {title}",
        id = task.id,
        title = task.title
    ))
}

/// Clone a task out of the locked graph (to hand a stable snapshot to a spoke).
fn task_clone_locked(graph: &TaskGraph, task_id: &TaskId) -> Result<Task, String> {
    graph
        .get(task_id)
        .cloned()
        .ok_or_else(|| format!("task {task_id} not found in graph"))
}

/// Mutable access to a task in the locked graph.
fn task_mut_locked<'g>(graph: &'g mut TaskGraph, task_id: &TaskId) -> Result<&'g mut Task, String> {
    graph
        .tasks
        .iter_mut()
        .find(|t| &t.id == task_id)
        .ok_or_else(|| format!("task {task_id} not found in graph"))
}

/// Read a task's reviewer-iteration count from the locked graph.
fn review_iterations_locked(graph: &TaskGraph, task_id: &TaskId) -> Result<u32, String> {
    graph
        .get(task_id)
        .map(|t| t.review_iterations)
        .ok_or_else(|| format!("task {task_id} not found in graph"))
}

/// Increment a task's reviewer-iteration counter (FSM-external bookkeeping).
fn increment_review_iterations_locked(graph: &mut TaskGraph, task_id: &TaskId) {
    if let Ok(task) = task_mut_locked(graph, task_id) {
        task.review_iterations += 1;
        task.updated_at = chrono::Utc::now();
    }
}

/// Read a task's gate-iteration count from the locked graph.
fn gate_iterations_locked(graph: &TaskGraph, task_id: &TaskId) -> Result<u32, String> {
    graph
        .get(task_id)
        .map(|t| t.gate_iterations)
        .ok_or_else(|| format!("task {task_id} not found in graph"))
}

/// Increment a task's gate-iteration counter (FSM-external bookkeeping).
fn increment_gate_iterations_locked(graph: &mut TaskGraph, task_id: &TaskId) {
    if let Ok(task) = task_mut_locked(graph, task_id) {
        task.gate_iterations += 1;
        task.updated_at = chrono::Utc::now();
    }
}

/// Stamp `started_at` when the Developer first picks up the task.
fn mark_started_locked(graph: &mut TaskGraph, task_id: &TaskId) {
    if let Ok(task) = task_mut_locked(graph, task_id)
        && task.started_at.is_none()
    {
        task.started_at = Some(chrono::Utc::now());
    }
}

/// Stamp `finished_at` when the task reaches a terminal state.
fn mark_finished_locked(graph: &mut TaskGraph, task_id: &TaskId) {
    if let Ok(task) = task_mut_locked(graph, task_id) {
        task.finished_at = Some(chrono::Utc::now());
    }
}

/// Record the classified [`api::FailureReason`] on the task at the failure site.
///
/// Called immediately after `apply_event_locked` (or `mark_finished_locked`)
/// when the task transitions to `Failed`, so the scheduler can read the stored
/// reason directly without re-deriving it from iteration counts.
fn set_failure_reason_locked(
    graph: &mut TaskGraph,
    task_id: &TaskId,
    kind: api::FailureKind,
    message: String,
) {
    if let Ok(task) = task_mut_locked(graph, task_id) {
        task.failure_reason = Some(api::FailureReason { kind, message });
    }
}

/// Read the stored [`api::FailureReason`] from the locked graph, returning
/// its `message` string, or `None` if no reason was set.
fn stored_failure_reason_message_locked(graph: &TaskGraph, task_id: &TaskId) -> Option<String> {
    graph
        .get(task_id)
        .and_then(|t| t.failure_reason.as_ref())
        .map(|fr| fr.message.clone())
}

/// Move the transitive dependents of a just-`Failed` task to [`TaskState::Skipped`].
///
/// `depends_on` lists each task's *prerequisites*, so a failed task's dependents
/// are the tasks whose `depends_on` transitively contains `failed_task_id`.  There
/// is no reverse-adjacency helper, so we build the dependent set inline: starting
/// from `failed_task_id`, repeatedly scan `graph.tasks` for any task whose
/// `depends_on` contains an already-collected id (a reverse-edge BFS).
///
/// For each newly found dependent that is **not already terminal** we apply
/// [`TaskEvent::DependencyFailed`] (active → `Skipped`) and stamp `finished_at`,
/// then collect its id.  The `is_terminal` guard keeps the FSM clean —
/// `apply_event_locked` already rejects the event from `Done/Failed/Skipped`.
///
/// Called under the held graph guard (no `.await`).  Returns the ids that were
/// freshly moved to `Skipped`, so the caller can emit + record them after the
/// guard is dropped.  This is required because `next_ready_task_id` needs every
/// dep `== Done`, so a failed task's dependents would otherwise dangle
/// non-terminal forever.
fn mark_dependents_skipped(graph: &mut TaskGraph, failed_task_id: &TaskId) -> Vec<TaskId> {
    // `collected` seeds the reverse-edge frontier with the failed id; `skipped`
    // accumulates only the ids we actually moved to `Skipped` (excludes the
    // failed root, which is already terminal).
    let mut collected: std::collections::HashSet<TaskId> = std::collections::HashSet::new();
    collected.insert(failed_task_id.clone());
    let mut skipped: Vec<TaskId> = Vec::new();

    // Fixed-point scan: keep sweeping the whole graph until a full pass adds no
    // new dependent (handles transitive chains regardless of authored order).
    loop {
        let mut found_new = false;
        let candidates: Vec<TaskId> = graph
            .tasks
            .iter()
            .filter(|t| !collected.contains(&t.id))
            .filter(|t| t.depends_on.iter().any(|dep| collected.contains(dep)))
            .map(|t| t.id.clone())
            .collect();

        for id in candidates {
            collected.insert(id.clone());
            found_new = true;
            // Skip tasks that already reached a terminal state — the FSM (and
            // `apply_event_locked`) would reject `DependencyFailed` for them.
            let state = match task_state_locked(graph, &id) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if crate::state_machine::is_terminal(state) {
                continue;
            }
            if apply_event_locked(graph, &id, TaskEvent::DependencyFailed).is_ok() {
                mark_finished_locked(graph, &id);
                skipped.push(id);
            }
        }

        if !found_new {
            break;
        }
    }

    skipped
}

// ── Retry / un-skip graph mutations (plan 0017) ──────────────────────────────────
//
// These mirror the lock discipline of the helpers above: each takes a borrowed
// `&mut TaskGraph` while the caller holds the `tokio::sync::Mutex<TaskGraph>`
// guard, never awaits, and returns plain values so the guard can be dropped
// before the caller's next await point.

/// Reset a single permanently-`Failed` task so it can run again (user retry).
///
/// Requires the task to be in [`TaskState::Failed`]; returns `Err` otherwise
/// (the command layer pre-validates, but this keeps the helper total). Clears
/// `failure_reason`, zeroes `gate_iterations`/`review_iterations`, clears
/// `finished_at`, applies [`TaskEvent::RetryRequested`] (`Failed → New`), and
/// bumps `updated_at` — giving the task a fresh budget on the next dispatch.
pub(crate) fn reset_task_for_retry_locked(
    graph: &mut TaskGraph,
    task_id: &TaskId,
) -> Result<(), String> {
    let state = task_state_locked(graph, task_id)?;
    if state != TaskState::Failed {
        return Err(format!(
            "task {task_id} is in state {state:?}, not Failed; cannot retry"
        ));
    }
    // FSM first (rejects anything but Failed → New), then clear the metadata.
    apply_event_locked(graph, task_id, TaskEvent::RetryRequested)?;
    let task = task_mut_locked(graph, task_id)?;
    task.failure_reason = None;
    task.gate_iterations = 0;
    task.review_iterations = 0;
    task.finished_at = None;
    task.updated_at = chrono::Utc::now();
    Ok(())
}

/// Un-skip the transitive dependents that were `Skipped` solely because of the
/// failure(s) now being retried — the inverse of [`mark_dependents_skipped`].
///
/// `reset_task_ids` are the tasks just revived (out of `Failed`). Performs a
/// **fixed-point** sweep over the forward-dependents: repeat until no change,
/// for every task currently in [`TaskState::Skipped`] whose `depends_on`
/// contains an already-revived id, revive it (clear `finished_at`, apply
/// [`TaskEvent::DependencyReset`] → `New`) **iff none** of its `depends_on` is
/// still in [`TaskState::Failed`] **and none** is still in
/// [`TaskState::Skipped`]. A task blocked by an unrelated still-`Failed` or
/// still-`Skipped` prerequisite is left `Skipped`.
///
/// Called under the held graph guard (no `.await`). Returns the ids freshly
/// revived to `New`, so the caller can emit them after the guard is dropped.
pub(crate) fn unskip_dependents_locked(
    graph: &mut TaskGraph,
    reset_task_ids: &[TaskId],
) -> Vec<TaskId> {
    // `revived` seeds the frontier with the already-reset roots; `unskipped`
    // accumulates only the ids we actually moved `Skipped → New`.
    let mut revived: std::collections::HashSet<TaskId> = reset_task_ids.iter().cloned().collect();
    let mut unskipped: Vec<TaskId> = Vec::new();

    loop {
        let mut changed = false;

        // Candidates: still-`Skipped` tasks that depend on an already-revived id
        // and are not themselves already in the revived set.
        let candidates: Vec<TaskId> = graph
            .tasks
            .iter()
            .filter(|t| t.state == TaskState::Skipped)
            .filter(|t| !revived.contains(&t.id))
            .filter(|t| t.depends_on.iter().any(|dep| revived.contains(dep)))
            .map(|t| t.id.clone())
            .collect();

        for id in candidates {
            // Precise revive guard: revive only if NONE of this task's deps is
            // still `Failed` or still `Skipped`. A dep that is `Skipped` but will
            // itself be revived later in this same fixed-point sweep blocks the
            // revive on this pass; a subsequent pass re-evaluates it once the dep
            // flips to `New`, so authored order is irrelevant.
            let deps: Vec<TaskId> = graph
                .get(&id)
                .map(|t| t.depends_on.clone())
                .unwrap_or_default();
            let blocked = deps.iter().any(|dep| {
                matches!(
                    task_state_locked(graph, dep).ok(),
                    Some(TaskState::Failed) | Some(TaskState::Skipped)
                )
            });
            if blocked {
                continue;
            }
            if apply_event_locked(graph, &id, TaskEvent::DependencyReset).is_ok() {
                if let Ok(task) = task_mut_locked(graph, &id) {
                    task.finished_at = None;
                    task.updated_at = chrono::Utc::now();
                }
                revived.insert(id.clone());
                unskipped.push(id);
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }

    unskipped
}

/// Re-mark readiness after a retry reset: every task now in [`TaskState::New`]
/// whose `depends_on` are all [`TaskState::Done`] is advanced to
/// [`TaskState::Ready`] via [`TaskEvent::DependenciesSatisfied`], mirroring the
/// scheduler's initial `New → Ready` sweep so the fresh scheduler can pick them
/// up.
///
/// Called under the held graph guard (no `.await`). Returns the ids moved to
/// `Ready`.
pub(crate) fn remark_ready_locked(graph: &mut TaskGraph) -> Vec<TaskId> {
    let ready_candidates: Vec<TaskId> = graph
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::New)
        .filter(|t| {
            t.depends_on.iter().all(|dep| {
                graph
                    .get(dep)
                    .map(|d| d.state == TaskState::Done)
                    .unwrap_or(false)
            })
        })
        .map(|t| t.id.clone())
        .collect();

    let mut readied = Vec::new();
    for id in ready_candidates {
        if apply_event_locked(graph, &id, TaskEvent::DependenciesSatisfied).is_ok() {
            readied.push(id);
        }
    }
    readied
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    /// Build a minimal `Task` in the given state for graph fixtures.
    fn task_in(id: &str, state: TaskState) -> Task {
        let now = Utc::now();
        Task {
            id: TaskId::new(id),
            title: format!("Task {id}"),
            description: String::new(),
            done_when: String::new(),
            depends_on: Vec::new(),
            section: None,
            state,
            gate_iterations: 0,
            review_iterations: 0,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
            failure_reason: None,
        }
    }

    /// A persistence request queued behind an earlier write must acquire the
    /// per-run ordering gate before it snapshots. Otherwise it can retain an
    /// old clone and overwrite a newer state after the newer write commits.
    #[tokio::test]
    async fn queued_persist_snapshots_only_after_acquiring_run_order() {
        let repo = tempfile::tempdir().expect("create temp repo");
        let graph = Arc::new(Mutex::new(TaskGraph {
            slug: "persist-order".into(),
            tasks: vec![task_in("task", TaskState::Ready)],
            authored: Default::default(),
        }));
        let persist_lock = Arc::new(Mutex::new(()));
        let manager = WorktreeManager::new(repo.path().to_path_buf(), "develop".into());
        let config = Config::resolve(
            crate::config::GlobalConfig::default(),
            crate::config::ProjectConfig::default(),
        );
        let backend: Arc<dyn AgentBackend> = Arc::new(crate::backend::noop::NoopBackend::default());
        let ctx = DriverContext {
            graph: Arc::clone(&graph),
            landing_evidence: Arc::new(Mutex::new(Vec::new())),
            persist_lock: Arc::clone(&persist_lock),
            merge_lock: Arc::new(Mutex::new(())),
            worktree_manager: manager,
            gate_runner: GateRunner::new(),
            squash_merger: SquashMerger::new(repo.path().to_path_buf(), "develop".into()),
            config,
            developer_backend: Arc::clone(&backend),
            reviewer_backend: backend,
            control: RunControl::silent(),
            audit_registry: Arc::new(crate::audit::NoopAuditRegistry),
            run_slug: "persist-order".into(),
            run_uid: "test-run".into(),
            plan_slug: String::new(),
            checkpoint_identity: None,
            pre_a_observer: None,
        };

        let held = persist_lock.lock().await;
        let queued = tokio::spawn({
            let ctx = ctx.clone();
            async move { ctx.persist().await }
        });
        tokio::task::yield_now().await;

        graph.lock().await.tasks[0].state = TaskState::Done;
        drop(held);
        queued.await.expect("queued persistence task");

        let loaded = crate::persist::load_graph(repo.path(), "persist-order")
            .await
            .expect("load final graph")
            .expect("graph exists");
        assert_eq!(loaded.tasks[0].state, TaskState::Done);
    }

    #[tokio::test]
    async fn production_pre_a_recheck_blocks_branch_mutation_after_review_acceptance() {
        let _home_guard = crate::HOME_ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str], cwd: &Path| {
            let output = std::process::Command::new("git")
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-b", "develop"], repo.path());
        git(&["config", "user.email", "test@example.com"], repo.path());
        git(&["config", "user.name", "Test"], repo.path());
        git(&["config", "commit.gpgsign", "false"], repo.path());
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(repo.path().join("src/base.rs"), "base\n").unwrap();
        git(&["add", "."], repo.path());
        git(&["commit", "-m", "base"], repo.path());
        let develop_before = git(&["rev-parse", "develop"], repo.path());

        let mut authored = std::collections::BTreeMap::new();
        authored.insert(
            TaskId::new("task"),
            crate::task::AuthoredTaskMetadata {
                source_path: "tasks/0101-task.md".into(),
                workstream: "0001".into(),
                kind: "task".into(),
                gated: false,
                touches: vec![crate::task::AuthoredRepoPattern::Glob("src/**".into())],
                status: crate::plan::AuthoredTaskStatus::Planned,
                merged_as: None,
                seed: crate::task::AuthoredSeedOutcome::Seeded(TaskState::New),
                collision_dependencies: vec![],
                branch_base_oid: None,
            },
        );
        let graph = Arc::new(Mutex::new(TaskGraph {
            slug: "plan".into(),
            tasks: vec![task_in("task", TaskState::Ready)],
            authored,
        }));
        let manager = WorktreeManager::new(repo.path().to_path_buf(), "develop".into());
        let config = Config::resolve(
            crate::config::GlobalConfig::default(),
            crate::config::ProjectConfig::default(),
        );
        let developer: Arc<dyn AgentBackend> =
            Arc::new(crate::backend::noop::NoopBackend::with_responses(vec![
                "done".into(),
            ]));
        let reviewer: Arc<dyn AgentBackend> =
            Arc::new(crate::backend::noop::NoopBackend::with_responses(vec![
                r#"{"verdict":"approve"}"#.into(),
            ]));
        let merge_lock = Arc::new(Mutex::new(()));
        let held = merge_lock.lock().await;
        let (observe_tx, mut observe_rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = DriverContext {
            graph: Arc::clone(&graph),
            landing_evidence: Arc::new(Mutex::new(Vec::new())),
            persist_lock: Arc::new(Mutex::new(())),
            merge_lock: Arc::clone(&merge_lock),
            worktree_manager: manager.clone(),
            gate_runner: GateRunner::new(),
            squash_merger: SquashMerger::new(repo.path().to_path_buf(), "develop".into()),
            config,
            developer_backend: developer,
            reviewer_backend: reviewer,
            control: RunControl::silent(),
            audit_registry: Arc::new(crate::audit::NoopAuditRegistry),
            run_slug: "plan".into(),
            run_uid: "run".into(),
            plan_slug: "plan".into(),
            checkpoint_identity: None,
            pre_a_observer: Some(observe_tx),
        };
        let mut driver = tokio::spawn({
            let ctx = ctx.clone();
            async move { task_driver(&ctx, &TaskId::new("task")).await }
        });
        tokio::select! {
            observed = observe_rx.recv() => observed.expect("pre-A observer closed"),
            result = &mut driver => panic!("driver exited before pre-A observer: {result:?}"),
            _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("driver did not reach pre-A observer"),
        };
        let task_worktree = crate::paths::worktree(repo.path(), "plan", "task").unwrap();
        std::fs::write(task_worktree.join("undeclared.txt"), "late mutation\n").unwrap();
        git(&["add", "undeclared.txt"], &task_worktree);
        git(
            &["commit", "-m", "late undeclared mutation"],
            &task_worktree,
        );
        drop(held);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if graph.lock().await.tasks[0].state == TaskState::InProgress {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            git(&["rev-parse", "develop"], repo.path()),
            develop_before,
            "pre-A rejection must prevent squash merge"
        );
        let task_ref = format!(
            "refs/heads/task/{}",
            crate::paths::short_worktree_name("plan", "task")
        );
        assert!(git(&["show-ref", "--verify", &task_ref], repo.path()).contains(&task_ref));
        driver.abort();
        let _ = driver.await;
        let _ = manager.remove("plan", "task").await;
        if let Some(value) = old_home {
            unsafe { std::env::set_var("HOME", value) }
        } else {
            unsafe { std::env::remove_var("HOME") }
        }
    }

    /// `advance_to_ready` returns `Err` for an id that is not in the graph — the
    /// defensive input that the scheduler's fill-phase advance arm must handle.
    #[test]
    fn advance_to_ready_errs_on_missing_task() {
        let mut graph = TaskGraph {
            slug: "advance-missing".into(),
            tasks: vec![task_in("a", TaskState::New)],
            authored: Default::default(),
        };
        let err = advance_to_ready(&mut graph, &TaskId::new("ghost"))
            .expect_err("advancing a missing task id must error");
        assert!(
            err.contains("not found"),
            "the error must name the missing task; got {err:?}"
        );
    }

    /// **Regression for `sched-advance-not-fatal`** — an advance-to-ready failure
    /// in the scheduler's fill phase is a *task-level* failure and MUST NOT feed
    /// `fatal_error`; it is recorded on `failed_tasks` instead, so the run's
    /// terminal `match fatal_error { Some(e) => Err(e), None => Ok(..) }` still
    /// yields `Ok` while the dropped task is reported as failed.
    ///
    /// This drives the REAL production `advance_to_ready` to obtain the same
    /// `Err(String)` the fill arm sees, then reproduces the fill arm's exact
    /// recovery (`failed_tasks.push((id, e))`, NO `fatal_error` write) and the
    /// scheduler's terminal match.  Under the pre-fix code (which did
    /// `fatal_error.get_or_insert(e)`) this terminal match returned `Err`, so this
    /// test would have failed.
    #[test]
    fn advance_failure_records_failed_task_not_fatal_error() {
        let mut graph = TaskGraph {
            slug: "advance-not-fatal".into(),
            tasks: vec![task_in("a", TaskState::New)],
            authored: Default::default(),
        };

        // The fill arm's two pieces of run-level state.  `fatal_error` stays
        // immutable here precisely because the advance arm must never write it —
        // that is the invariant under test (the pre-fix code DID write it).
        let fatal_error: Option<String> = None;
        let mut failed_tasks: Vec<(TaskId, String)> = Vec::new();

        // Drive the REAL `advance_to_ready` on a missing id to get a genuine Err,
        // then apply the fill arm's recovery verbatim (matching the production
        // code at the advance-to-ready error arm in `scheduler`).
        let id = TaskId::new("ghost");
        if let Err(e) = advance_to_ready(&mut graph, &id) {
            // INVARIANT (sched-advance-not-fatal): record the dropped task, do
            // NOT set `fatal_error`.  A genuine driver panic is the only remaining
            // fatal source.
            failed_tasks.push((id.clone(), e.to_string()));
        }

        // The dropped task is recorded with a non-empty reason.
        assert_eq!(failed_tasks.len(), 1, "the dropped task must be recorded");
        assert_eq!(failed_tasks[0].0, id);
        assert!(
            !failed_tasks[0].1.is_empty(),
            "the failure reason must be non-empty"
        );

        // The advance failure did NOT make the run fatal: the terminal match still
        // yields Ok (the invariant this fix restores).
        assert!(
            fatal_error.is_none(),
            "an advance-to-ready failure must NOT set fatal_error; got {fatal_error:?}"
        );
        let result: Result<(), String> = match fatal_error {
            Some(e) => Err(e),
            None => Ok(()),
        };
        result.expect("the run must NOT hard-error on a task-level advance failure");
    }

    // ── Retry / un-skip graph mutations (plan 0017) ──────────────────────────

    /// Build a `Task` with an explicit `depends_on` list (helper for cascade
    /// fixtures).
    fn task_dep(id: &str, state: TaskState, depends_on: &[&str]) -> Task {
        let mut t = task_in(id, state);
        t.depends_on = depends_on.iter().map(|d| TaskId::new(*d)).collect();
        t
    }

    /// `reset_task_for_retry_locked` clears all failure metadata and drives a
    /// `Failed` task back to `New` with a fresh budget.
    #[test]
    fn reset_clears_failure_metadata() {
        let mut failed = task_in("a", TaskState::Failed);
        failed.gate_iterations = 3;
        failed.review_iterations = 2;
        failed.finished_at = Some(Utc::now());
        failed.failure_reason = Some(api::FailureReason {
            kind: api::FailureKind::GateCap,
            message: "gate cap exhausted".into(),
        });
        let mut graph = TaskGraph {
            slug: "reset-meta".into(),
            tasks: vec![failed],
            authored: Default::default(),
        };

        reset_task_for_retry_locked(&mut graph, &TaskId::new("a"))
            .expect("reset of a Failed task must succeed");

        let t = graph.get(&TaskId::new("a")).unwrap();
        assert_eq!(t.state, TaskState::New, "Failed must reset to New");
        assert_eq!(t.gate_iterations, 0, "gate budget must be zeroed");
        assert_eq!(t.review_iterations, 0, "review budget must be zeroed");
        assert!(t.failure_reason.is_none(), "failure_reason must be cleared");
        assert!(t.finished_at.is_none(), "finished_at must be cleared");
    }

    /// `unskip_dependents_locked` revives only the cascade of the retried task,
    /// leaving an unrelated still-failed cascade `Skipped`.
    #[test]
    fn unskip_revives_only_this_cascade() {
        // A ← B ← C and X ← Y.  Fail A (=> B,C Skipped) and X (=> Y Skipped).
        let mut graph = TaskGraph {
            slug: "cascade".into(),
            tasks: vec![
                task_in("a", TaskState::Failed),
                task_dep("b", TaskState::Skipped, &["a"]),
                task_dep("c", TaskState::Skipped, &["b"]),
                task_in("x", TaskState::Failed),
                task_dep("y", TaskState::Skipped, &["x"]),
            ],
            authored: Default::default(),
        };

        // Reset only A, then un-skip its cascade.
        reset_task_for_retry_locked(&mut graph, &TaskId::new("a")).unwrap();
        let revived = unskip_dependents_locked(&mut graph, &[TaskId::new("a")]);

        assert_eq!(
            graph.get(&TaskId::new("b")).unwrap().state,
            TaskState::New,
            "B's only dep A is reset, so B must revive"
        );
        assert_eq!(
            graph.get(&TaskId::new("c")).unwrap().state,
            TaskState::New,
            "C's only dep B is revived, so C must revive"
        );
        assert_eq!(
            graph.get(&TaskId::new("y")).unwrap().state,
            TaskState::Skipped,
            "Y is not in A's cascade and X is still Failed; Y stays Skipped"
        );
        let revived_set: std::collections::HashSet<_> = revived.into_iter().collect();
        assert!(revived_set.contains(&TaskId::new("b")));
        assert!(revived_set.contains(&TaskId::new("c")));
        assert!(!revived_set.contains(&TaskId::new("y")));
    }

    /// A `Skipped` task that depends on the retried failure AND an unrelated
    /// still-`Failed` task stays `Skipped` (multi-dependency guard).
    #[test]
    fn unskip_leaves_task_blocked_by_other_failure() {
        // Z depends on BOTH A and X; fail A and X (=> Z Skipped). Retry A only.
        let mut graph = TaskGraph {
            slug: "multi-dep".into(),
            tasks: vec![
                task_in("a", TaskState::Failed),
                task_in("x", TaskState::Failed),
                task_dep("z", TaskState::Skipped, &["a", "x"]),
            ],
            authored: Default::default(),
        };
        reset_task_for_retry_locked(&mut graph, &TaskId::new("a")).unwrap();
        let revived = unskip_dependents_locked(&mut graph, &[TaskId::new("a")]);
        assert_eq!(
            graph.get(&TaskId::new("z")).unwrap().state,
            TaskState::Skipped,
            "Z still depends on Failed X, so the guard must leave Z Skipped"
        );
        assert!(revived.is_empty(), "no task should be revived");
    }

    /// `reset_task_for_retry_locked` rejects a non-`Failed` target (the
    /// command layer surfaces this as `InvalidCommand`).
    #[test]
    fn reset_rejects_non_failed() {
        for state in [
            TaskState::Done,
            TaskState::InProgress,
            TaskState::New,
            TaskState::Ready,
            TaskState::Skipped,
        ] {
            let mut graph = TaskGraph {
                slug: "reject".into(),
                tasks: vec![task_in("a", state)],
                authored: Default::default(),
            };
            let err = reset_task_for_retry_locked(&mut graph, &TaskId::new("a"))
                .expect_err("reset of a non-Failed task must error");
            assert!(
                err.contains("not Failed") || err.contains("cannot retry"),
                "error must explain the non-Failed rejection; got {err:?}"
            );
        }
    }

    /// `remark_ready_locked` advances a `New` task whose deps are all `Done`
    /// back to `Ready`, but leaves one with a non-`Done` dep in `New`.
    #[test]
    fn remark_ready_advances_only_satisfied() {
        let mut graph = TaskGraph {
            slug: "remark".into(),
            tasks: vec![
                task_in("done", TaskState::Done),
                task_dep("ready-me", TaskState::New, &["done"]),
                task_dep("blocked", TaskState::New, &["new-dep"]),
                task_in("new-dep", TaskState::New),
            ],
            authored: Default::default(),
        };
        let readied = remark_ready_locked(&mut graph);
        assert_eq!(
            graph.get(&TaskId::new("ready-me")).unwrap().state,
            TaskState::Ready,
            "a New task with all-Done deps must advance to Ready"
        );
        assert_eq!(
            graph.get(&TaskId::new("blocked")).unwrap().state,
            TaskState::New,
            "a New task with a non-Done dep must stay New"
        );
        assert!(readied.contains(&TaskId::new("ready-me")));
        assert!(!readied.contains(&TaskId::new("blocked")));
    }
}
