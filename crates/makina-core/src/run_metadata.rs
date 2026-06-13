//! Run-level metadata persisted to `.makina/runs/{run_uid}/run.json`.
//!
//! [`RunMetadata`] is a small, self-describing record of a Run's identity and
//! lifecycle window: its persistent [`run_uid`](RunMetadata::run_uid), the
//! human-facing `run_slug`, the terminal [`RunStatus`], and the
//! `started_at`/`ended_at` timestamps.  It is written **best-effort** at run
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

use std::collections::HashSet;
use std::path::Path;

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
}

/// A durable snapshot of a Run's identity and lifecycle window.
///
/// Written to `.makina/runs/{run_uid}/run.json` at finalization.  This carries
/// only fields with an in-graph source today: there is no task→worktree map
/// here because `Task` has no worktree field, the orchestrator stores no such
/// map, and `WorktreeManager::worktree_path` is private and still returns the
/// pre-relocation path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMetadata {
    /// Persistent, sortable 26-char run identity (the ULID minted at open).
    run_uid: String,
    /// Human-facing run slug (e.g. the plan slug the run was opened from).
    run_slug: String,
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
        status: RunStatus,
        started_at: DateTime<Utc>,
        ended_at: DateTime<Utc>,
    ) -> Self {
        Self {
            run_uid,
            run_slug,
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
        status: RunStatus,
        started_at: DateTime<Utc>,
        ended_at: DateTime<Utc>,
        tasks: Vec<TaskSnapshot>,
    ) -> Self {
        Self {
            run_uid,
            run_slug,
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
/// Serializes `meta` with [`serde_json::to_string_pretty`] and writes it to
/// `paths::run_dir(repo_root, &meta.run_uid).join("run.json")`, creating the run
/// directory with [`tokio::fs::create_dir_all`] first.
///
/// Mirrors [`persist::persist_graph`](crate::persist::persist_graph)'s
/// create-dir-then-write shape (minus the temp-file/atomic-rename dance, which
/// is unnecessary for this single write-once-at-finalization record).
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

    let dest = dir.join("run.json");
    tokio::fs::write(&dest, json.as_bytes()).await?;

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
        })
        .collect();

    // Reconstruct the task-list path from the run_slug stored in the metadata.
    // For disk snapshots the original absolute path is not persisted; we use a
    // `.tasks/{slug}.json` relative path as a best-effort label.
    let task_list_path = std::path::PathBuf::from(format!(".tasks/{}.json", meta.run_slug()));

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
    let runs_dir = repo_root.join(".makina").join("runs");
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
                // run.json absent — directory may be a partial/corrupt run.
                tracing::warn!(run_uid = %run_uid, "run directory has no run.json; skipping");
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

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::TimeZone;

    fn fixed_ts(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 10, 0, 0)
            .single()
            .expect("valid date")
    }

    /// A [`RunMetadata`] written by [`write_run_metadata`] and read back via
    /// `serde_json::from_str` must preserve `run_uid`, `run_slug`, `status`, and
    /// both timestamps, and the file must land at
    /// `.makina/runs/{run_uid}/run.json`.
    #[tokio::test]
    async fn round_trip_through_write_run_metadata() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();

        let started = fixed_ts(2026, 5, 1);
        let ended = fixed_ts(2026, 5, 2);
        let meta = RunMetadata::new(
            "01ABCDEF0123456789ABCDEFGH".to_string(),
            "demo-plan".to_string(),
            RunStatus::Completed,
            started,
            ended,
        );

        write_run_metadata(&meta, root)
            .await
            .expect("write_run_metadata must succeed");

        let expected_path = root
            .join(".makina")
            .join("runs")
            .join(&meta.run_uid)
            .join("run.json");
        assert!(
            expected_path.exists(),
            "run.json must land at .makina/runs/{{run_uid}}/run.json"
        );

        let contents = std::fs::read_to_string(&expected_path).expect("read run.json");
        let loaded: RunMetadata = serde_json::from_str(&contents).expect("parse run.json");

        assert_eq!(loaded.run_uid, meta.run_uid, "run_uid must survive");
        assert_eq!(loaded.run_slug, meta.run_slug, "run_slug must survive");
        assert_eq!(loaded.status, meta.status, "status must survive");
        assert_eq!(loaded.started_at, started, "started_at must survive");
        assert_eq!(loaded.ended_at, ended, "ended_at must survive");
    }

    /// A fixture with only an old `run.json` (no `tasks` field) must be surfaced
    /// by `load_disk_run_views` as a [`RunView`].  This verifies the back-compat
    /// path: old run records (pre-`TaskSnapshot`) still appear in the run list
    /// when the process restarts.
    #[test]
    fn old_run_json_without_snapshot_still_loads() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();
        let run_uid = "01ABCDEF0123456789ABCDEFGH";

        // Create a minimal run.json without the `tasks` field (simulating an old file).
        let old_json = r#"{
  "run_uid": "01ABCDEF0123456789ABCDEFGH",
  "run_slug": "demo-plan",
  "status": "completed",
  "started_at": "2026-05-01T10:00:00Z",
  "ended_at": "2026-05-02T10:00:00Z"
}
"#;

        let run_dir = root.join(".makina").join("runs").join(run_uid);
        std::fs::create_dir_all(&run_dir).expect("create run dir");
        std::fs::write(run_dir.join("run.json"), old_json).expect("write old run.json");

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
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();

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
            },
        ];

        let meta = RunMetadata::with_tasks(
            "01ABCDEF0123456789ABCDEFGH".to_string(),
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

        let t2 = &view.tasks[1];
        assert_eq!(t2.id, TaskId::new("task-two"));
        assert_eq!(t2.title, "Second task");
        assert_eq!(t2.state, TaskState::Done);
        assert_eq!(t2.gate_iterations, 1);
        assert_eq!(t2.review_iterations, 1);
        assert_eq!(t2.depends_on, vec![TaskId::new("task-one")]);
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
    }
}
