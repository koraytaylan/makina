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

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::RunStatus;
use crate::paths;

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
        }
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
}
