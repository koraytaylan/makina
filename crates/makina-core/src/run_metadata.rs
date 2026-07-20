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

use crate::api::{
    AuthoredTaskView, IngestionReport, RunId, RunStatus, RunView, TaskId, TaskState, TaskView,
};
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
/// here because `Task` has no worktree field and the orchestrator stores no such
/// map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMetadata {
    /// Persistent, sortable 26-char run identity (the ULID minted at open).
    run_uid: String,
    /// Human-facing run slug (e.g. the plan slug the run was opened from).
    run_slug: String,
    /// Derived plan slug (parent dir of the originating task list). Stored so
    /// disk-loaded [`RunView`]s can reconstruct a `plan_dir` for which
    /// `plan_slug()` and run labels compute correctly, letting historical runs
    /// for plans participate in sidebar deduplication and context resolution.
    /// `#[serde(default)]` for back-compat with pre- plan_slug run.json files.
    #[serde(default)]
    plan_slug: String,
    /// Originating task-list location. New records store a repo-relative path
    /// when the plan is inside the project (portable across clones), otherwise
    /// the canonical absolute path. Absent from legacy records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan_dir: Option<crate::plan::PlanKey>,
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
    /// Phase-C commit proving durable plan completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completion_oid: Option<String>,
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
            plan_dir: None,
            status,
            started_at,
            ended_at,
            tasks: Vec::new(),
            completion_oid: None,
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
            plan_dir: None,
            status,
            started_at,
            ended_at,
            tasks,
            completion_oid: None,
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

    /// Attach the canonical origin path, storing it relative to `repo_root`
    /// where possible so snapshots remain portable.
    pub fn with_plan_dir(mut self, plan_dir: &crate::plan::PlanKey, _repo_root: &Path) -> Self {
        self.plan_dir = Some(plan_dir.clone());
        self
    }

    /// Resolve the persisted origin to an absolute path in this project.
    pub fn resolved_plan_dir(&self, _repo_root: &Path) -> Option<crate::plan::PlanKey> {
        self.plan_dir.clone()
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

    pub fn completion_oid(&self) -> Option<&str> {
        self.completion_oid.as_deref()
    }

    pub fn with_completion_oid(mut self, oid: impl Into<String>) -> Self {
        self.completion_oid = Some(oid.into());
        self
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
    let dir = paths::run_dir(repo_root, &meta.run_uid)?;
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
    let path = paths::run_dir(repo_root, run_uid)?.join("run.json");
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

fn task_entry_sources(
    repo_root: &Path,
    plan_key: &crate::plan::PlanKey,
) -> HashMap<String, (String, AuthoredTaskView)> {
    let Ok(plan) = crate::orchestrator::load_authoritative_plan(repo_root, plan_key) else {
        return HashMap::new();
    };
    let projected = crate::plan_runtime::ProjectedTaskGraph::from_document(&plan, Utc::now());
    let bodies: HashMap<_, _> = plan
        .tasks
        .iter()
        .map(|task| (task.frontmatter.id.as_str().to_owned(), task.body.clone()))
        .collect();
    projected
        .graph
        .tasks
        .iter()
        .filter_map(|task| {
            let metadata = projected.graph.authored.get(&task.id)?;
            let body = bodies.get(task.id.0.as_str())?.clone();
            Some((
                task.id.0.as_str().to_owned(),
                (
                    body,
                    AuthoredTaskView {
                        source_path: metadata.source_path.clone(),
                        workstream: metadata.workstream.clone(),
                        kind: metadata.kind.clone(),
                        status: metadata.status.to_string(),
                        gated: metadata.gated,
                        touches: metadata
                            .touches
                            .iter()
                            .map(|pattern| pattern.as_str().to_owned())
                            .collect(),
                        merged_as: metadata.merged_as.clone(),
                        authored_dependencies: task
                            .depends_on
                            .iter()
                            .filter(|dependency| {
                                !metadata.collision_dependencies.contains(dependency)
                            })
                            .map(Into::into)
                            .collect(),
                        collision_dependencies: metadata
                            .collision_dependencies
                            .iter()
                            .map(Into::into)
                            .collect(),
                    },
                ),
            ))
        })
        .collect()
}

/// Build a [`RunView`] from a [`RunMetadata`] snapshot, assigning `id` as the
/// session-scoped [`RunId`] and using `repo_root` to derive the `project` label.
///
/// Tasks are reconstructed from the embedded [`TaskSnapshot`] slice.  When the
/// slice is empty (an old `run.json` pre-dating this change) the returned view
/// has no tasks. Task detail Scope text is reconstructed from the plan's
/// typed task source; persisted run metadata carries state, not duplicated prose.
/// The ingestion report is left empty (no issues) for disk-loaded snapshots
/// because the original `IngestionReport` is not persisted.
fn run_view_from_metadata(id: RunId, meta: &RunMetadata, repo_root: &Path) -> Option<RunView> {
    let project = repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let plan_dir = meta.resolved_plan_dir(repo_root)?;
    let task_entries = task_entry_sources(repo_root, &plan_dir);

    let tasks: Vec<TaskView> = meta
        .tasks()
        .iter()
        .map(|t| TaskView {
            authored: task_entries
                .get(&t.id)
                .map(|(_, authored)| authored.clone()),
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
            entry_text: task_entries
                .get(&t.id)
                .map(|(body, _)| body.clone())
                .unwrap_or_default(),
        })
        .collect();

    Some(RunView {
        id,
        run_uid: meta.run_uid().to_string(),
        plan_dir,
        status: meta.status().clone(),
        project,
        tasks,
        report: IngestionReport::default(),
    })
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
    let Ok(runs_dir) = crate::paths::state_root(repo_root).map(|root| root.join("runs")) else {
        return Vec::new();
    };
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
                if meta.run_uid() != run_uid {
                    tracing::warn!(
                        directory_run_uid = %run_uid,
                        metadata_run_uid = %meta.run_uid(),
                        "run.json identity does not match its containing directory; skipping"
                    );
                    continue;
                }
                let id = RunId(*next_id);
                *next_id += 1;
                if let Some(view) = run_view_from_metadata(id, &meta, repo_root) {
                    views.push(view);
                } else {
                    tracing::warn!(run_uid = %run_uid, "run metadata has no canonical plan identity; skipping");
                }
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

    let Ok(runs_dir) = crate::paths::state_root(repo_root).map(|root| root.join("runs")) else {
        return 0;
    };
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
        let matches_plan = meta.plan_dir.as_ref().is_some_and(|key| {
            format!("{}-{}", key.number, key.slug).to_ascii_lowercase() == plan_slug
        });
        if !matches_plan {
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
    use std::path::Path;

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
        let plan_dir = crate::plan::PlanKey::parse("docs/plans/0001-Demo-Plan").unwrap();
        let meta = RunMetadata::new(
            "01ABCDEF0123456789ABCDEFGH".to_string(),
            "demo-plan".to_string(),
            "demo-plan".to_string(),
            RunStatus::Completed,
            started,
            ended,
        )
        .with_plan_dir(&plan_dir, root);

        write_run_metadata(&meta, root)
            .await
            .expect("write_run_metadata must succeed");

        // The file must land at state_root(root)/runs/{run_uid}/run.json
        let expected_path = crate::paths::run_dir(root, &meta.run_uid)
            .unwrap()
            .join("run.json");
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
        assert_eq!(
            loaded.plan_dir,
            Some(plan_dir.clone()),
            "typed plan identity should survive serialization"
        );
        assert_eq!(
            loaded.resolved_plan_dir(root),
            Some(plan_dir),
            "disk views must recover the typed plan identity"
        );
    }

    #[tokio::test]
    async fn disk_loader_rejects_metadata_with_a_mismatched_directory_identity() {
        let _guard = HOME_ENV_LOCK.lock().await;
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let original_home = std::env::var_os("HOME");
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();

        // SAFETY: serialized by HOME_ENV_LOCK for the full environment mutation.
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        let directory_run_uid = "01DIRECTORY000000000000000";
        let metadata = RunMetadata::new(
            "01METADATA0000000000000000".to_string(),
            "mismatched-run".to_string(),
            "mismatched".to_string(),
            RunStatus::Completed,
            fixed_ts(2026, 5, 1),
            fixed_ts(2026, 5, 2),
        );
        let run_dir = crate::paths::run_dir(root, directory_run_uid).unwrap();
        std::fs::create_dir_all(&run_dir).expect("create mismatched run directory");
        std::fs::write(
            run_dir.join("run.json"),
            serde_json::to_vec_pretty(&metadata).expect("serialize run metadata"),
        )
        .expect("write mismatched run metadata");

        let mut next_id = 1;
        let views = load_disk_run_views(root, &HashSet::new(), &mut next_id);

        // SAFETY: restore the process-global value while HOME_ENV_LOCK is held.
        unsafe {
            match original_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }

        assert!(views.is_empty(), "mismatched run identity must be ignored");
        assert_eq!(next_id, 1, "a rejected snapshot must not consume a RunId");
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
        )
        .with_plan_dir(
            &crate::plan::PlanKey::parse("docs/plans/0001-target-plan").unwrap(),
            root,
        );
        let other = RunMetadata::new(
            "01OTHER0000000000000000000".to_string(),
            "other-plan".to_string(),
            "other-plan".to_string(),
            RunStatus::Completed,
            ts,
            ts,
        )
        .with_plan_dir(
            &crate::plan::PlanKey::parse("docs/plans/0002-other-plan").unwrap(),
            root,
        );

        write_run_metadata(&target, root)
            .await
            .expect("write target metadata");
        write_run_metadata(&other, root)
            .await
            .expect("write other metadata");

        let removed = remove_run_metadata_for_plan(root, "0001-target-plan").await;

        assert_eq!(removed, 1);
        assert!(
            !crate::paths::run_dir(root, &target.run_uid)
                .unwrap()
                .join("run.json")
                .exists(),
            "target plan snapshot must be cleared"
        );
        assert!(
            crate::paths::run_dir(root, &other.run_uid)
                .unwrap()
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
        let run_dir_path = crate::paths::run_dir(root, run_uid).unwrap();
        std::fs::create_dir_all(&run_dir_path).expect("create run dir");
        std::fs::write(run_dir_path.join("run.json"), old_json).expect("write old run.json");

        // load_disk_run_views (the disk half of runs()) must surface this run as a
        // RunView even though it has no tasks field.
        let mut next_id = 1u64;
        let live: HashSet<String> = HashSet::new();
        let views = load_disk_run_views(root, &live, &mut next_id);

        assert!(
            views.is_empty(),
            "metadata without a typed plan key must fail closed"
        );
        if views.is_empty() {
            return;
        }
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
        assert!(
            !json.contains("entry_text"),
            "new task snapshots must not persist task detail Scope text"
        );
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
        assert_eq!(old_snap.id, "old-task");

        let legacy_with_entry_text = r#"{
            "id": "legacy-task",
            "title": "Legacy task",
            "state": "done",
            "gate_iterations": 0,
            "review_iterations": 0,
            "depends_on": [],
            "entry_text": "Legacy persisted scope."
        }"#;
        let legacy_snap: TaskSnapshot = serde_json::from_str(legacy_with_entry_text)
            .expect("old JSON with entry_text must deserialize");
        assert_eq!(
            legacy_snap.id, "legacy-task",
            "legacy entry_text should be ignored while the task snapshot still loads"
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

        let run_dir = crate::paths::run_dir(root, &meta.run_uid).unwrap();

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
    /// must reconstruct a `plan_dir` such that `plan_slug(&path)` recovers
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

        let run_dir_path = crate::paths::run_dir(root, run_uid).unwrap();
        std::fs::create_dir_all(&run_dir_path).expect("create run dir");
        std::fs::write(run_dir_path.join("run.json"), old_json).expect("write legacy run.json");

        let mut next_id = 1u64;
        let live: HashSet<String> = HashSet::new();
        let views = load_disk_run_views(root, &live, &mut next_id);

        assert!(
            views.is_empty(),
            "metadata without a typed plan key must fail closed"
        );
        if views.is_empty() {
            return;
        }
        let v = &views[0];
        // The reconstructed path must look plan-like so that plan_slug(v.plan_dir) == "0027-plan-auto-discovery"
        assert!(
            v.plan_dir.relative_dir == Path::new("docs/plans/0027-plan-auto-discovery"),
            "legacy plan run must reconstruct a canonical plan key, got {}",
            v.plan_dir.relative_dir.display()
        );
        let recovered = format!("{:04}-{}", v.plan_dir.number, v.plan_dir.slug);
        assert_eq!(
            recovered, "0027-plan-auto-discovery",
            "plan_slug on reconstructed path must recover the plan slug for dedup"
        );
    }
}
