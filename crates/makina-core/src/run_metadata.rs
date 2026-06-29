//! Run-level metadata persisted to `.makina/runs/{run_uid}/run.json`.
//!
//! [`RunMetadata`] is a small, self-describing record of a Run's identity and
//! lifecycle window: its persistent [`run_uid`](RunMetadata::run_uid), the
//! human-facing `run_slug`, the derived `plan_slug`, the terminal [`RunStatus`],
//! and the `started_at`/`ended_at` timestamps.  It is written **best-effort** at run
//! finalization by [`write_run_metadata`] so that tooling (and humans) can
//! reconstruct what happened in a run directory without replaying the audit log.
//!
//! The writer mirrors [`persist::persist_graph`](crate::persist::persist_graph):
//! it serializes with [`serde_json::to_string_pretty`], `create_dir_all`s the
//! destination directory, and writes the file — all on Tokio's async filesystem
//! API.
//!
//! ## Disk RunSnapshot
//!
//! [`load_disk_run_views`] scans `.makina/runs/` and returns a [`RunView`] for
//! every finished run whose `run_uid` is **not** already present in the live
//! registry.  This is the seam used by [`crate::orchestrator::CoreApi::runs`] to
//! surface historical runs that have been evicted from memory.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::{IngestionReport, RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
use crate::paths;

/// A snapshot of a single task's state at run finalization.
///
/// Captured alongside [`RunMetadata`] to enable reconstruction of task
/// list and states for a finished run without the live registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSnapshot {
    /// Unique kebab-case identifier within the Run.
    pub id: String,
    /// Human-readable task title (taken verbatim from the task-list file).
    pub title: String,
    /// Final lifecycle state of the task at run completion.
    pub state: TaskState,
    /// Number of times this task cycled through the Developer → gate → failed-gate loop.
    #[serde(default)]
    pub gate_iterations: u32,
    /// Number of times this task cycled through the Developer → Reviewer → changes-requested loop.
    #[serde(default)]
    pub review_iterations: u32,
    /// IDs of tasks that must reach [`TaskState::Done`] before this task becomes ready.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// When the task first entered `InProgress`. Additive: old `run.json`
    /// files without it still load with `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// When the task reached its terminal state. Additive, as above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// Why the task reached `Failed`, or `None` for any non-`Failed` task.
    /// Additive field: old `run.json` files without it still load with `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<crate::api::FailureReason>,
    /// Raw Markdown entry for this task, used to reconstruct task-detail Scope
    /// content for disk-loaded runs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub entry_text: String,
}

/// A durable snapshot of a Run's identity and lifecycle window.
///
/// Written to `.makina/runs/{run_uid}/run.json` at finalization.  This carries
/// only fields with an in-graph source today: there is no task→worktree map
/// here because `Task` has no worktree field and the orchestrator stores no such
/// map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMetadata {
    /// Persistent, sortable 26-char run identity (the ULID minted at open).
    run_uid: String,
    /// Human-facing run slug (e.g. the plan slug the run was opened from).
    run_slug: String,
    /// Derived plan slug (parent dir of the originating task list). Stored so
    /// disk-loaded [`RunView`]s can reconstruct a `task_list_path` for which
    /// `plan_slug()` and run labels compute correctly, letting historical runs
    /// for plans participate in sidebar deduplication and context resolution.
    /// `#[serde(default)]` for back-compat with pre- plan_slug run.json files.
    #[serde(default)]
    plan_slug: String,
    /// Terminal status of the run.
    status: RunStatus,
    /// When the run transitioned to `Running`.
    started_at: DateTime<Utc>,
    /// When the run reached its terminal status.
    ended_at: DateTime<Utc>,
    /// Per-task snapshots capturing final state and iteration counts.
    /// Additive field: old `run.json` files without it still load with `#[serde(default)]`.
    #[serde(default)]
    tasks: Vec<TaskSnapshot>,
}

impl RunMetadata {
    /// Construct a new [`RunMetadata`] record.
    pub fn new(
        run_uid: String,
        run_slug: String,
        plan_slug: String,
        status: RunStatus,
        started_at: DateTime<Utc>,
        ended_at: DateTime<Utc>,
    ) -> Self {
        Self {
            run_uid,
            run_slug,
            plan_slug,
            status,
            started_at,
            ended_at,
            tasks: Vec::new(),
        }
    }

    /// Construct a [`RunMetadata`] record with per-task snapshots.
    pub fn with_tasks(
        run_uid: String,
        run_slug: String,
        plan_slug: String,
        status: RunStatus,
        started_at: DateTime<Utc>,
        ended_at: DateTime<Utc>,
        tasks: Vec<TaskSnapshot>,
    ) -> Self {
        Self {
            run_uid,
            run_slug,
            plan_slug,
            status,
            started_at,
            ended_at,
            tasks,
        }
    }

    /// The persistent, sortable 26-char run identity (the ULID minted at open).
    pub fn run_uid(&self) -> &str {
        &self.run_uid
    }

    /// The human-facing run slug (e.g. the plan slug the run was opened from).
    pub fn run_slug(&self) -> &str {
        &self.run_slug
    }

    /// The derived plan slug for the run's originating task list (used for
    /// sidebar dedup etc). Empty for legacy records that predate the field.
    pub fn plan_slug(&self) -> &str {
        &self.plan_slug
    }

    /// The terminal status recorded for the run.
    pub fn status(&self) -> &RunStatus {
        &self.status
    }

    /// When the run transitioned to `Running`.
    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    /// When the run reached its terminal status.
    pub fn ended_at(&self) -> DateTime<Utc> {
        self.ended_at
    }

    /// Per-task snapshots captured at run finalization.
    pub fn tasks(&self) -> &[TaskSnapshot] {
        &self.tasks
    }
}

/// Best-effort writer for [`RunMetadata`].
///
/// Serializes `meta` with [`serde_json::to_string_pretty`] and writes it
/// atomically to `paths::run_dir(repo_root, &meta.run_uid).join("run.json")`:
///
/// 1. Create the run directory with [`tokio::fs::create_dir_all`].
/// 2. Serialize to JSON.
/// 3. Write to a temp file `run.json.tmp.{pid}.{seq}`.
/// 4. Atomically rename the temp file to `run.json`.
///
/// A crash mid-write can leave a temp file behind, but never a truncated
/// `run.json`. This mirrors [`persist::persist_graph`](crate::persist::persist_graph)'s
/// atomic write pattern.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] on serialization or I/O failure.
/// Callers persist run metadata best-effort and must not abort a run on error.
pub async fn write_run_metadata(meta: &RunMetadata, repo_root: &Path) -> std::io::Result<()> {
    let dir = paths::run_dir(repo_root, &meta.run_uid);
    tokio::fs::create_dir_all(&dir).await?;

    let mut json = serde_json::to_string_pretty(meta)?;
    // Trailing newline: POSIX convention and tidy `git diff` output.
    json.push('\n');

    // Generate a temp file path using process ID and a monotonic sequence counter.
    // Temp files left behind by crashed writes are cleaned up by the next run or explicit prune.
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("run.json.tmp.{}.{}", pid, seq));

    // Write to temp file, then atomically rename to destination.
    tokio::fs::write(&tmp, json.as_bytes()).await?;

    let dest = dir.join("run.json");
    if let Err(e) = tokio::fs::rename(&tmp, &dest).await {
        // Best-effort cleanup of temp file on rename failure.
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }

    Ok(())
}

/// Attempt to read [`RunMetadata`] from `.makina/runs/{run_uid}/run.json`.
///
/// Returns `Ok(None)` if the file does not exist; propagates parse errors.
pub fn read_run_metadata(repo_root: &Path, run_uid: &str) -> std::io::Result<Option<RunMetadata>> {
    let path = paths::run_dir(repo_root, run_uid).join("run.json");
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let meta = serde_json::from_str(&contents)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            Ok(Some(meta))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn effective_plan_slug(meta: &RunMetadata) -> String {
    if !meta.plan_slug().is_empty() {
        meta.plan_slug().to_string()
    } else if let Some(p) = meta.run_slug().strip_suffix("-tasks") {
        if !p.is_empty() {
            p.to_string()
        } else {
            String::new()
        }
    } else {
        String::new()
    }
}

fn synthetic_task_list_path(effective_plan: &str, run_slug: &str) -> std::path::PathBuf {
    if !effective_plan.is_empty() {
        std::path::PathBuf::from_iter(["docs", "plans", effective_plan, "TASKS.md"])
    } else {
        std::path::PathBuf::from(format!(".tasks/{run_slug}.json"))
    }
}

fn find_plan_tasks_path(repo_root: &Path, effective_plan: &str) -> Option<std::path::PathBuf> {
    if effective_plan.is_empty() {
        return None;
    }

    let plans_dir = repo_root.join("docs").join("plans");
    if let Ok(entries) = std::fs::read_dir(&plans_dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("TASKS.md");
            if candidate.is_file() && crate::orchestrator::plan_slug(&candidate) == effective_plan {
                return Some(candidate);
            }
        }
    }

    let candidate = plans_dir.join(effective_plan).join("TASKS.md");
    candidate.is_file().then_some(candidate)
}

fn task_entry_fallbacks(repo_root: &Path, effective_plan: &str) -> HashMap<String, String> {
    let Some(tasks_path) = find_plan_tasks_path(repo_root, effective_plan) else {
        return HashMap::new();
    };
    let Ok(contents) = std::fs::read_to_string(tasks_path) else {
        return HashMap::new();
    };

    crate::orchestrator::parse_plan_tasks(&contents)
        .into_iter()
        .filter(|task| !task.body.trim().is_empty())
        .map(|task| (task.id, task.body))
        .collect()
}

/// Build a [`RunView`] from a [`RunMetadata`] snapshot, assigning `id` as the
/// session-scoped [`RunId`] and using `repo_root` to derive the `project` label.
///
/// Tasks are reconstructed from the embedded [`TaskSnapshot`] slice.  When the
/// slice is empty (an old `run.json` pre-dating this change) the returned view
/// has no tasks — callers may optionally fall back to the task-list artifact.
/// The ingestion report is left empty (no issues) for disk-loaded snapshots
/// because the original `IngestionReport` is not persisted.
fn run_view_from_metadata(id: RunId, meta: &RunMetadata, repo_root: &Path) -> RunView {
    let project = repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let effective_plan = effective_plan_slug(meta);
    let fallback_entries = task_entry_fallbacks(repo_root, &effective_plan);

    let tasks: Vec<TaskView> = meta
        .tasks()
        .iter()
        .map(|t| TaskView {
            id: TaskId::new(t.id.clone()),
            title: t.title.clone(),
            state: t.state.clone(),
            gate_iterations: t.gate_iterations,
            review_iterations: t.review_iterations,
            depends_on: t
                .depends_on
                .iter()
                .map(|d| TaskId::new(d.clone()))
                .collect(),
            started_at: t.started_at,
            finished_at: t.finished_at,
            failure_reason: t.failure_reason.clone(),
            entry_text: if t.entry_text.trim().is_empty() {
                fallback_entries.get(&t.id).cloned().unwrap_or_default()
            } else {
                t.entry_text.clone()
            },
        })
        .collect();

    // Reconstruct a task-list path for the RunView.
    //
    // - Prefer a synthetic `docs/plans/{plan}/TASKS.md` when we have a plan_slug
    //   (or can derive one for legacy run_slugs ending in "-tasks"). This makes
    //   `orchestrator::plan_slug(&path)` and `ui::run_label(&view)` return the
    //   same values they do for live plan runs, so historical plan runs
    //   participate in sidebar plan/run dedup and context resolution.
    // - Fall back to the old `.tasks/{run_slug}.json` for non-plan runs and
    //   pre-plan_slug records whose run_slug does not look plan-like.
    let task_list_path = synthetic_task_list_path(&effective_plan, meta.run_slug());

    RunView {
        id,
        run_uid: meta.run_uid().to_string(),
        task_list_path,
        status: meta.status().clone(),
        project,
        tasks,
        report: IngestionReport::default(),
    }
}

/// Scan `.makina/runs/` and return one [`RunView`] per finished run whose
/// `run_uid` is **not** in `live_run_uids`.
///
/// This is the disk-snapshot reader used by [`crate::orchestrator::CoreApi::runs`]
/// to surface historical runs that have been evicted from the live registry
/// (e.g. after process restart).
///
/// IDs for the returned views are assigned by incrementing `next_id` from its
/// current value (same counter the live registry uses, passed in by the caller
/// so all session-scoped IDs remain unique).
///
/// Non-existent runs directory and individual unreadable/unparseable `run.json`
/// files are silently skipped (best-effort; the caller logs at `warn` level if
/// desired).
pub fn load_disk_run_views(
    repo_root: &Path,
    live_run_uids: &HashSet<String>,
    next_id: &mut u64,
) -> Vec<RunView> {
    let runs_dir = crate::paths::state_root(repo_root).join("runs");
    let dir_iter = match std::fs::read_dir(&runs_dir) {
        Ok(iter) => iter,
        Err(_) => return Vec::new(), // directory absent or unreadable — nothing to load
    };

    let mut views = Vec::new();
    for entry in dir_iter.flatten() {
        let run_uid = entry.file_name().to_string_lossy().to_string();
        // Skip runs that are already tracked in the live registry.
        if live_run_uids.contains(&run_uid) {
            continue;
        }
        // Attempt to read run.json; skip on any error (best-effort).
        match read_run_metadata(repo_root, &run_uid) {
            Ok(Some(meta)) => {
                let id = RunId(*next_id);
                *next_id += 1;
                views.push(run_view_from_metadata(id, &meta, repo_root));
            }
            Ok(None) => {
                // run.json absent can be intentional: reset removes completed
                // snapshots while preserving logs/transcripts under the run dir.
                tracing::debug!(run_uid = %run_uid, "run directory has no run.json; skipping");
            }
            Err(e) => {
                tracing::warn!(run_uid = %run_uid, error = %e, "failed to read run.json; skipping");
            }
        }
    }

    // Sort by run_uid (ULID lexicographic = chronological) so the list is stable.
    views.sort_by(|a, b| a.run_uid.cmp(&b.run_uid));
    views
}

/// Remove persisted `run.json` files for a plan.
///
/// Reset uses this to prevent a previously terminal disk snapshot from
/// resurrecting as `Completed` after the process restarts. The run directories
/// are left in place so logs/transcripts are not deleted, but without `run.json`
/// they no longer appear in `load_disk_run_views`.
pub async fn remove_run_metadata_for_plan(repo_root: &Path, plan_slug: &str) -> usize {
    if plan_slug.is_empty() {
        return 0;
    }

    let runs_dir = crate::paths::state_root(repo_root).join("runs");
    let mut dir = match tokio::fs::read_dir(&runs_dir).await {
        Ok(dir) => dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read run metadata directory for reset cleanup");
            return 0;
        }
    };

    let mut removed = 0usize;
    loop {
        let entry = match dir.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "failed to scan run metadata during reset cleanup");
                break;
            }
        };
        let run_json = entry.path().join("run.json");
        let contents = match tokio::fs::read_to_string(&run_json).await {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!(
                    path = %run_json.display(),
                    error = %e,
                    "failed to read run metadata during reset cleanup"
                );
                continue;
            }
        };
        let meta = match serde_json::from_str::<RunMetadata>(&contents) {
            Ok(meta) => meta,
            Err(e) => {
                tracing::warn!(
                    path = %run_json.display(),
                    error = %e,
                    "failed to parse run metadata during reset cleanup"
                );
                continue;
            }
        };
        if effective_plan_slug(&meta) != plan_slug {
            continue;
        }
        match tokio::fs::remove_file(&run_json).await {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                path = %run_json.display(),
                error = %e,
                "failed to remove stale run metadata during reset cleanup"
            ),
        }
    }

    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::TimeZone;

    // Use the process-global HOME_ENV_LOCK from lib.rs so all test modules
    // serialize HOME mutations across crate boundaries.
    use crate::HOME_ENV_LOCK;

    fn fixed_ts(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 10, 0, 0)
            .single()
            .expect("valid date")
    }

    /// A [`RunMetadata`] written by [`write_run_metadata`] and read back via
    /// `serde_json::from_str` must preserve `run_uid`, `run_slug`, `status`, and
    /// both timestamps, and the file must land at
    /// `state_root(root)/runs/{run_uid}/run.json`.
    #[tokio::test]
    async fn round_trip_through_write_run_metadata() {
        let _guard = HOME_ENV_LOCK.lock().await;
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();

        // SAFETY: serialised by HOME_ENV_LOCK (tokio async mutex held for entire test)
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        let started = fixed_ts(2026, 5, 1);
        let ended = fixed_ts(2026, 5, 2);
        let meta = RunMetadata::new(
            "01ABCDEF0123456789ABCDEFGH".to_string(),
            "demo-plan".to_string(),
            "demo-plan".to_string(),
            RunStatus::Completed,
            started,
            ended,
        );

        write_run_metadata(&meta, root)
            .await
            .expect("write_run_metadata must succeed");

        // The file must land at state_root(root)/runs/{run_uid}/run.json
        let expected_path = crate::paths::run_dir(root, &meta.run_uid).join("run.json");
        assert!(
            expected_path.exists(),
            "run.json must land at state_root/runs/{{run_uid}}/run.json, checked {}",
            expected_path.display()
        );

        let contents = std::fs::read_to_string(&expected_path).expect("read run.json");
        let loaded: RunMetadata = serde_json::from_str(&contents).expect("parse run.json");

        assert_eq!(loaded.run_uid, meta.run_uid, "run_uid must survive");
        assert_eq!(loaded.run_slug, meta.run_slug, "run_slug must survive");
        assert_eq!(loaded.plan_slug, meta.plan_slug, "plan_slug must survive");
        assert_eq!(loaded.status, meta.status, "status must survive");
        assert_eq!(loaded.started_at, started, "started_at must survive");
        assert_eq!(loaded.ended_at, ended, "ended_at must survive");
    }

    #[tokio::test]
    async fn remove_run_metadata_for_plan_only_clears_matching_snapshots() {
        let _guard = HOME_ENV_LOCK.lock().await;
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();

        // SAFETY: serialised by HOME_ENV_LOCK (tokio async mutex held for entire test)
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        let ts = fixed_ts(2026, 5, 1);
        let target = RunMetadata::new(
            "01TARGET000000000000000000".to_string(),
            "target-plan".to_string(),
            "target-plan".to_string(),
            RunStatus::Completed,
            ts,
            ts,
        );
        let other = RunMetadata::new(
            "01OTHER0000000000000000000".to_string(),
            "other-plan".to_string(),
            "other-plan".to_string(),
            RunStatus::Completed,
            ts,
            ts,
        );

        write_run_metadata(&target, root)
            .await
            .expect("write target metadata");
        write_run_metadata(&other, root)
            .await
            .expect("write other metadata");

        let removed = remove_run_metadata_for_plan(root, "target-plan").await;

        assert_eq!(removed, 1);
        assert!(
            !crate::paths::run_dir(root, &target.run_uid)
                .join("run.json")
                .exists(),
            "target plan snapshot must be cleared"
        );
        assert!(
            crate::paths::run_dir(root, &other.run_uid)
                .join("run.json")
                .exists(),
            "other plan snapshot must be preserved"
        );
    }

    /// A fixture with only an old `run.json` (no `tasks` field) must be surfaced
    /// by `load_disk_run_views` as a [`RunView`].  This verifies the back-compat
    /// path: old run records (pre-`TaskSnapshot`) still appear in the run list
    /// when the process restarts.
    #[test]
    fn old_run_json_without_snapshot_still_loads() {
        let _guard = HOME_ENV_LOCK.blocking_lock();
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();
        let run_uid = "01ABCDEF0123456789ABCDEFGH";

        // SAFETY: serialised by HOME_ENV_LOCK (tokio blocking_lock held for entire test)
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        // Create a minimal run.json without the `tasks` field (simulating an old file).
        let old_json = r#"{
  "run_uid": "01ABCDEF0123456789ABCDEFGH",
  "run_slug": "demo-plan",
  "status": "completed",
  "started_at": "2026-05-01T10:00:00Z",
  "ended_at": "2026-05-02T10:00:00Z"
}
"#;

        // Create the run directory under state_root so load_disk_run_views finds it.
        let run_dir_path = crate::paths::run_dir(root, run_uid);
        std::fs::create_dir_all(&run_dir_path).expect("create run dir");
        std::fs::write(run_dir_path.join("run.json"), old_json).expect("write old run.json");

        // load_disk_run_views (the disk half of runs()) must surface this run as a
        // RunView even though it has no tasks field.
        let mut next_id = 1u64;
        let live: HashSet<String> = HashSet::new();
        let views = load_disk_run_views(root, &live, &mut next_id);

        assert_eq!(views.len(), 1, "runs() must yield exactly one RunView");
        let view = &views[0];
        assert_eq!(view.run_uid, run_uid, "run_uid must round-trip");
        assert_eq!(view.status, RunStatus::Completed, "status must round-trip");
        // Old run.json has no tasks — the RunView tasks list should be empty.
        assert!(
            view.tasks.is_empty(),
            "old run.json without snapshot yields empty task list"
        );
    }

    /// A fixture run directory containing a `run.json` with per-task snapshots
    /// must be surfaced by `load_disk_run_views` as a [`RunView`] with the
    /// correct task states and iteration counts.
    #[tokio::test]
    async fn open_finished_run_reconstructs_view() {
        let _guard = HOME_ENV_LOCK.lock().await;
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();

        // SAFETY: serialised by HOME_ENV_LOCK (tokio async mutex held for entire test)
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        let started = fixed_ts(2026, 5, 1);
        let ended = fixed_ts(2026, 5, 2);

        let task_snapshots = vec![
            TaskSnapshot {
                id: "task-one".to_string(),
                title: "First task".to_string(),
                state: TaskState::Done,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: "Task one scope.\n\n### Done when\n\nTask one done.".to_string(),
            },
            TaskSnapshot {
                id: "task-two".to_string(),
                title: "Second task".to_string(),
                state: TaskState::Done,
                gate_iterations: 1,
                review_iterations: 1,
                depends_on: vec!["task-one".to_string()],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: "Task two scope.".to_string(),
            },
        ];

        let meta = RunMetadata::with_tasks(
            "01ABCDEF0123456789ABCDEFGH".to_string(),
            "demo-plan".to_string(),
            "demo-plan".to_string(),
            RunStatus::Completed,
            started,
            ended,
            task_snapshots,
        );

        write_run_metadata(&meta, root)
            .await
            .expect("write_run_metadata must succeed");

        // load_disk_run_views (the disk half of runs()) must surface this run as a
        // RunView with the correct task states and iteration counts — without any
        // live registry.
        let mut next_id = 1u64;
        let live: HashSet<String> = HashSet::new();
        let views = load_disk_run_views(root, &live, &mut next_id);

        assert_eq!(views.len(), 1, "runs() must yield exactly one RunView");
        let view = &views[0];
        assert_eq!(view.run_uid, meta.run_uid(), "run_uid must round-trip");
        assert_eq!(
            view.status,
            RunStatus::Completed,
            "status must be Completed"
        );

        // Verify task states and iteration counts survived the round-trip.
        assert_eq!(view.tasks.len(), 2, "two tasks must be reconstructed");

        let t1 = &view.tasks[0];
        assert_eq!(t1.id, TaskId::new("task-one"));
        assert_eq!(t1.title, "First task");
        assert_eq!(t1.state, TaskState::Done);
        assert_eq!(t1.gate_iterations, 0);
        assert_eq!(t1.review_iterations, 0);
        assert!(t1.depends_on.is_empty());
        assert_eq!(
            t1.entry_text, "Task one scope.\n\n### Done when\n\nTask one done.",
            "task detail Scope content must survive disk reconstruction"
        );

        let t2 = &view.tasks[1];
        assert_eq!(t2.id, TaskId::new("task-two"));
        assert_eq!(t2.title, "Second task");
        assert_eq!(t2.state, TaskState::Done);
        assert_eq!(t2.gate_iterations, 1);
        assert_eq!(t2.review_iterations, 1);
        assert_eq!(t2.depends_on, vec![TaskId::new("task-one")]);
        assert_eq!(t2.entry_text, "Task two scope.");
    }

    #[test]
    fn disk_run_without_entry_text_recovers_scope_from_plan_tasks_md() {
        let _guard = HOME_ENV_LOCK.blocking_lock();
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();
        let run_uid = "01OLDRUNENTRYTEXT000000000";

        // SAFETY: serialised by HOME_ENV_LOCK
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        let plan_dir = root
            .join("docs")
            .join("plans")
            .join("0042-Detail-Pane-Rendering-And-Interaction-Fixes");
        std::fs::create_dir_all(&plan_dir).expect("create plan dir");
        std::fs::write(
            plan_dir.join("TASKS.md"),
            r#"# Tasks

## 0001 — Workstream

### scope-task — Scope Task

Recovered task scope from the original plan file.

- **Done when:** the task detail Scope section is not empty.
- **Depends on:** —
"#,
        )
        .expect("write TASKS.md");

        let old_json = r#"{
  "run_uid": "01OLDRUNENTRYTEXT000000000",
  "run_slug": "0042-detail-pane-rendering-and-interaction-fixes-tasks",
  "plan_slug": "0042-detail-pane-rendering-and-interaction-fixes",
  "status": "completed",
  "started_at": "2026-05-01T10:00:00Z",
  "ended_at": "2026-05-01T11:00:00Z",
  "tasks": [
    {
      "id": "scope-task",
      "title": "Scope Task",
      "state": "done",
      "gate_iterations": 0,
      "review_iterations": 0,
      "depends_on": []
    }
  ]
}
"#;
        let run_dir_path = crate::paths::run_dir(root, run_uid);
        std::fs::create_dir_all(&run_dir_path).expect("create run dir");
        std::fs::write(run_dir_path.join("run.json"), old_json).expect("write old run.json");

        let mut next_id = 1u64;
        let live: HashSet<String> = HashSet::new();
        let views = load_disk_run_views(root, &live, &mut next_id);

        assert_eq!(views.len(), 1);
        let task = views[0].tasks.first().expect("task reconstructed");
        assert!(
            task.entry_text
                .contains("Recovered task scope from the original plan file."),
            "old run snapshot should recover task body from matching TASKS.md, got {:?}",
            task.entry_text
        );
        assert!(
            task.entry_text
                .contains("the task detail Scope section is not empty"),
            "fallback should preserve the Done-when bullet in the task body"
        );
    }

    /// A [`TaskSnapshot`] with `started_at`/`finished_at` set survives a
    /// `serde_json` round-trip with the values intact.  Additionally, a
    /// `TaskSnapshot` deserialized from JSON that **lacks** both fields (an old
    /// `run.json`) still loads cleanly with `None/None` — verifying the
    /// `#[serde(default)]` backward-compat contract.
    #[test]
    fn snapshot_roundtrips_timestamps() {
        let ts0 = fixed_ts(2026, 3, 1);
        let ts1 = fixed_ts(2026, 3, 2);

        // --- case 1: snapshot with both timestamps set ---
        let snap = TaskSnapshot {
            id: "my-task".to_string(),
            title: "My task".to_string(),
            state: TaskState::Done,
            gate_iterations: 0,
            review_iterations: 0,
            depends_on: vec![],
            started_at: Some(ts0),
            finished_at: Some(ts1),
            failure_reason: None,
            entry_text: "Persisted task scope.".to_string(),
        };

        let json = serde_json::to_string(&snap).expect("serialize TaskSnapshot");
        let back: TaskSnapshot = serde_json::from_str(&json).expect("deserialize TaskSnapshot");

        assert_eq!(
            back.started_at,
            Some(ts0),
            "started_at must survive serde_json round-trip"
        );
        assert_eq!(
            back.finished_at,
            Some(ts1),
            "finished_at must survive serde_json round-trip"
        );
        assert_eq!(
            back.entry_text, "Persisted task scope.",
            "entry_text must survive serde_json round-trip"
        );

        // --- case 2: old JSON without started_at/finished_at still deserializes ---
        let old_json = r#"{
            "id": "old-task",
            "title": "Old task",
            "state": "done",
            "gate_iterations": 0,
            "review_iterations": 0,
            "depends_on": []
        }"#;
        let old_snap: TaskSnapshot =
            serde_json::from_str(old_json).expect("old JSON without timestamps must deserialize");

        assert_eq!(
            old_snap.started_at, None,
            "old snapshot without started_at must deserialize to None"
        );
        assert_eq!(
            old_snap.finished_at, None,
            "old snapshot without finished_at must deserialize to None"
        );
        assert!(
            old_snap.entry_text.is_empty(),
            "old snapshot without entry_text must deserialize to empty text"
        );
    }

    /// Verify the atomic write structural properties:
    ///
    /// a) The temp path has a distinct name from `run.json`.
    /// b) After a successful write the canonical `run.json` is present and parseable.
    /// c) No `*.tmp.*` residue remains after a successful write.
    /// d) A first write's output is fully replaced (not partially corrupted) by
    ///    a second write.
    #[tokio::test]
    async fn test_write_run_metadata_is_atomic() {
        let _guard = HOME_ENV_LOCK.lock().await;
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();

        // SAFETY: serialised by HOME_ENV_LOCK (tokio async mutex held for entire test)
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        let started = fixed_ts(2026, 5, 1);
        let ended = fixed_ts(2026, 5, 2);
        let meta = RunMetadata::new(
            "01ABCDEF0123456789ABCDEFGH".to_string(),
            "demo-plan".to_string(),
            "demo-plan".to_string(),
            RunStatus::Completed,
            started,
            ended,
        );

        let run_dir = crate::paths::run_dir(root, &meta.run_uid);

        // (a) Temp file should have a distinct name from run.json
        let expected_canonical = run_dir.join("run.json");

        // Write the metadata
        write_run_metadata(&meta, root)
            .await
            .expect("write_run_metadata must succeed");

        // (b) After a successful write, canonical file exists and parses
        assert!(
            expected_canonical.exists(),
            "run.json must exist after write_run_metadata"
        );

        let contents = std::fs::read_to_string(&expected_canonical).expect("read run.json");
        let _loaded: RunMetadata = serde_json::from_str(&contents).expect("parse run.json");

        // (c) No temp file residue remains
        let entries = std::fs::read_dir(&run_dir)
            .expect("read run dir")
            .flatten()
            .collect::<Vec<_>>();

        for entry in entries {
            let name = entry.file_name().into_string().expect("utf8 name");
            assert!(
                !name.contains(".tmp."),
                "unexpected temp file left behind: {name}"
            );
        }

        // (d) A second write atomically replaces the first
        write_run_metadata(&meta, root)
            .await
            .expect("second write_run_metadata must succeed");

        let contents2 =
            std::fs::read_to_string(&expected_canonical).expect("read run.json after 2nd write");
        let loaded2: RunMetadata =
            serde_json::from_str(&contents2).expect("parse run.json after 2nd write");

        assert_eq!(
            meta.run_uid, loaded2.run_uid,
            "run_uid must survive two atomic writes"
        );
        assert_eq!(
            meta.run_slug, loaded2.run_slug,
            "run_slug must survive two atomic writes"
        );
        assert_eq!(
            meta.plan_slug, loaded2.plan_slug,
            "plan_slug must survive two atomic writes"
        );
    }

    /// Legacy run.json (no `plan_slug` field) whose `run_slug` ends in `-tasks`
    /// must reconstruct a `task_list_path` such that `plan_slug(&path)` recovers
    /// the plan dir portion.  This ensures historical plan runs participate in
    /// the TUI's plan-vs-run dedup (`visible_tree_nodes`) and `active_run_id`
    /// even after restart, without id collisions or "separate run" display.
    #[test]
    fn legacy_plan_run_slug_without_plan_slug_field_reconstructs_path() {
        let _guard = HOME_ENV_LOCK.blocking_lock();
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();
        let run_uid = "01LEGACYPLAN1234567890ABCD";

        // SAFETY: serialised by HOME_ENV_LOCK
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        // Simulate old run.json for a plan run: run_slug has the full
        // "parent-tasks" form, but no plan_slug field.
        let old_json = r#"{
  "run_uid": "01LEGACYPLAN1234567890ABCD",
  "run_slug": "0027-plan-auto-discovery-tasks",
  "status": "completed",
  "started_at": "2026-05-01T10:00:00Z",
  "ended_at": "2026-05-01T11:00:00Z"
}
"#;

        let run_dir_path = crate::paths::run_dir(root, run_uid);
        std::fs::create_dir_all(&run_dir_path).expect("create run dir");
        std::fs::write(run_dir_path.join("run.json"), old_json).expect("write legacy run.json");

        let mut next_id = 1u64;
        let live: HashSet<String> = HashSet::new();
        let views = load_disk_run_views(root, &live, &mut next_id);

        assert_eq!(views.len(), 1);
        let v = &views[0];
        // The reconstructed path must look plan-like so that plan_slug(v.task_list_path) == "0027-plan-auto-discovery"
        assert!(
            v.task_list_path
                .to_string_lossy()
                .contains("0027-plan-auto-discovery/TASKS.md"),
            "legacy plan run must reconstruct plan-style TASKS.md path, got {}",
            v.task_list_path.display()
        );
        let recovered = crate::orchestrator::plan_slug(&v.task_list_path);
        assert_eq!(
            recovered, "0027-plan-auto-discovery",
            "plan_slug on reconstructed path must recover the plan slug for dedup"
        );
    }
}
