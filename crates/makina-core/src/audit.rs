//! JSONL audit ledger for governance decisions.
//!
//! This module provides the [`JsonlAuditSink`] — the Supervisor-owned file
//! appender that enriches transport-emitted [`AuditEntry`] records with the
//! real `run_id` / `task_id` and routes them to
//! `.tasks/{slug}/audit.jsonl`.
//!
//! # Design
//!
//! The ACP transport ([`crate::governance::AuditSink`]) emits one entry per
//! permission decision, but it cannot know which run/task the session belongs
//! to.  Instead it fills in placeholder values (`run_id: "acp-transport"`,
//! `task_id: None`) and sets the real `working_dir`.
//!
//! The Supervisor (which creates and owns each task's worktree) calls
//! [`JsonlAuditSink::register`] right before dispatching a task driver,
//! recording the mapping `working_dir → (run_id, slug, task_id)`.  When the
//! transport fires `record(&entry)` the sink looks up `entry.working_dir`,
//! enriches the entry, and appends it as a compact single-line JSON object
//! to `repo_root/.tasks/{slug}/audit.jsonl` (opened with `create+append`).
//!
//! Both the registration and the file-write path are **non-panicking** and
//! produce only best-effort `tracing::warn!` logs on failure, preserving the
//! [`AuditSink::record`] contract that it must not abort the caller.
//!
//! # Supervisor-only writer invariant
//!
//! The sink is Supervisor-owned: `JsonlAuditSink` is constructed in
//! `main.rs`, given to `AcpBackend::with_audit_sink`, and an `Arc<dyn
//! AuditRegistry>` clone is passed into the orchestrator so the Supervisor
//! can call [`AuditRegistry::register`].  Nothing else writes to
//! `.tasks/{slug}/audit.jsonl`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::governance::{AuditEntry, AuditSink};

// ── AuditContext ────────────────────────────────────────────────────────────────

/// The run/task context associated with a worktree.
///
/// Stored in [`JsonlAuditSink`]'s registry, keyed by `working_dir`.
#[derive(Clone, Debug)]
struct AuditContext {
    /// The unique run identifier used to locate the run's directory
    /// (e.g. `.makina/runs/{run_uid}`).
    ///
    /// Read by `record()` to route the enriched entry to
    /// `.makina/runs/{run_uid}/audit.jsonl` via [`crate::paths::audit_log`].
    run_uid: String,
    /// The stable run identifier (e.g. `"run:1"` from `RunId::to_string()`).
    run_id: String,
    /// The task-graph slug (the file stem of the task-list file, e.g. `"my-feature"`).
    slug: String,
    /// The task id (the task's kebab-case slug, e.g. `"task-a"`).
    task_id: String,
}

// ── AuditRegistry trait ─────────────────────────────────────────────────────────

/// Object-safe seam for the Supervisor to register a task's worktree context.
///
/// Call [`register`](AuditRegistry::register) **before** dispatching a
/// task driver so the audit sink knows how to enrich entries arriving from
/// that worktree.
///
/// The trait is intentionally minimal: one method, `Send + Sync`, `Arc`-safe.
pub trait AuditRegistry: Send + Sync {
    /// Associate `working_dir` with `(run_uid, run_id, slug, task_id)`.
    ///
    /// Calling this with the same `working_dir` a second time (e.g. for a
    /// re-dispatched task) replaces the previous entry, which is correct —
    /// the latest registration wins.
    fn register(
        &self,
        working_dir: PathBuf,
        run_uid: String,
        run_id: String,
        slug: String,
        task_id: String,
    );
}

// ── NoopAuditRegistry ──────────────────────────────────────────────────────────

/// No-op implementation of [`AuditRegistry`].
///
/// Used as the default in tests and in builds that do not need the full
/// ledger.  All registrations are silently discarded.
#[derive(Clone, Debug, Default)]
pub struct NoopAuditRegistry;

impl AuditRegistry for NoopAuditRegistry {
    fn register(
        &self,
        _working_dir: PathBuf,
        _run_uid: String,
        _run_id: String,
        _slug: String,
        _task_id: String,
    ) {
    }
}

// ── JsonlAuditSink ──────────────────────────────────────────────────────────────

/// A [`AuditSink`] + [`AuditRegistry`] that appends enriched JSONL records to
/// `.tasks/{slug}/audit.jsonl` under `repo_root`.
///
/// Construct with [`JsonlAuditSink::new`] and share the same `Arc` as both the
/// sink (inject into `AcpBackend::with_audit_sink`) and the registry (pass to
/// the orchestrator / Supervisor).
///
/// # Thread safety
///
/// The internal registry is a `Mutex<HashMap<…>>`.  The mutex is only held for
/// the brief in-memory look-up; the file I/O runs without holding the lock.
///
/// # Registry growth
///
/// Registry entries are never evicted; they accumulate for the process
/// lifetime.  This is acceptable for the MVP (entries are tiny and bounded by
/// the total number of dispatched tasks), but should be revisited if `makina`
/// becomes a long-running service.
pub struct JsonlAuditSink {
    /// Root of the project repository; the JSONL file lives at
    /// `repo_root/.tasks/{slug}/audit.jsonl`.
    repo_root: PathBuf,
    /// Maps `working_dir → AuditContext`.  Populated by `register`, read by
    /// `record`.
    registry: Mutex<HashMap<PathBuf, AuditContext>>,
}

impl JsonlAuditSink {
    /// Create a new sink rooted at `repo_root`.
    ///
    /// All audit files are created under `repo_root/.tasks/`.  `repo_root`
    /// should be the repository root (the same value used by `WorktreeManager`
    /// and `SquashMerger`).
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            registry: Mutex::new(HashMap::new()),
        }
    }
}

impl AuditRegistry for JsonlAuditSink {
    /// Record that `working_dir` belongs to the given run/slug/task.
    ///
    /// Non-panicking: if the mutex is poisoned we log a warning and skip.
    ///
    /// # Path equality
    ///
    /// The registry is keyed by **exact** [`PathBuf`] equality on `working_dir`
    /// — no canonicalization is applied.  The path registered here and the
    /// [`AuditEntry::working_dir`] set by the transport must be the same byte
    /// form (both absolute, or both the same relative form).  If they differ
    /// (e.g. one is canonicalized and the other is not) the lookup in `record`
    /// will miss and the entry will be dropped with a `tracing::warn!`.
    fn register(
        &self,
        working_dir: PathBuf,
        run_uid: String,
        run_id: String,
        slug: String,
        task_id: String,
    ) {
        match self.registry.lock() {
            Ok(mut map) => {
                map.insert(
                    working_dir,
                    AuditContext {
                        run_uid,
                        run_id,
                        slug,
                        task_id,
                    },
                );
            }
            Err(_) => {
                tracing::warn!(
                    "audit registry mutex poisoned; skipping registration for {}",
                    working_dir.display()
                );
            }
        }
    }
}

impl AuditSink for JsonlAuditSink {
    /// Enrich `entry` with the real `run_id` / `task_id` and append to the
    /// JSONL file, routing by `entry.working_dir`.
    ///
    /// If the `working_dir` is not registered (e.g. the entry came from an
    /// unknown or unregistered session), the entry is discarded with a
    /// `tracing::warn!` — the contract says non-panicking.
    ///
    /// # File path
    ///
    /// `{repo_root}/.makina/runs/{run_uid}/audit.jsonl`
    ///
    /// The directory is created if it does not exist.
    fn record(&self, mut entry: AuditEntry) {
        // ── Look up the context for this working_dir ──────────────────────────
        // The lookup uses EXACT PathBuf equality (no canonicalization).  The
        // path in `entry.working_dir` must be the same byte form as the one
        // passed to `register`; a mismatch silently misses and warns below.
        let ctx = match self.registry.lock() {
            Ok(map) => match map.get(&entry.working_dir) {
                Some(c) => c.clone(),
                None => {
                    tracing::warn!(
                        working_dir = %entry.working_dir.display(),
                        placeholder_run_id = %entry.run_id,
                        "audit sink: working_dir not registered; discarding entry"
                    );
                    return;
                }
            },
            Err(_) => {
                tracing::warn!("audit registry mutex poisoned in record(); discarding entry");
                return;
            }
        };

        // ── Enrich with real identifiers ───────────────────────────────────────
        entry.run_id = ctx.run_id.clone();
        entry.task_id = Some(ctx.task_id.clone());

        // ── Serialize to compact single-line JSON ─────────────────────────────
        let line = match serde_json::to_string(&entry) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    slug = %ctx.slug,
                    error = %e,
                    "audit sink: failed to serialize AuditEntry; skipping"
                );
                return;
            }
        };

        // ── Append to .makina/runs/{run_uid}/audit.jsonl ──────────────────────
        // NOTE: the dir-create + open + append below are synchronous (blocking)
        // `std::fs` calls run on the caller's thread — the ACP transport reader
        // loop, an async worker. This is acceptable for the MVP because
        // `record` fires at most once per permission prompt (a very low rate);
        // if the audit rate ever grows, offload these writes to a background
        // writer task (follow-up). See the `AuditSink` trait docs.
        let path = crate::paths::audit_log(&self.repo_root, &ctx.run_uid);
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            tracing::warn!(
                run_uid = %ctx.run_uid,
                dir = %dir.display(),
                error = %e,
                "audit sink: failed to create run audit directory; skipping"
            );
            return;
        }

        // ── Append the line (create or append, never truncate) ────────────────
        // A compact single-line audit entry is well under PIPE_BUF, so the
        // O_APPEND write is atomic on Linux/macOS; concurrent appends by two
        // tasks of the same run won't interleave lines.
        use std::io::Write as _;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(e) = writeln!(file, "{line}") {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "audit sink: failed to write audit entry; skipping"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "audit sink: failed to open audit.jsonl for append; skipping"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governance::{AuditDecision, PolicyInfo, ToolRef};
    use chrono::Utc;
    use std::sync::Arc;

    fn sample_entry(working_dir: PathBuf) -> AuditEntry {
        AuditEntry {
            timestamp: Utc::now(),
            run_id: "acp-transport".to_string(), // placeholder, as the transport emits
            task_id: None,
            session_id: Some("sess-test".to_string()),
            tool: ToolRef {
                name: "write_file".to_string(),
                kind: Some("edit".to_string()),
                id: "write_file__test_1".to_string(),
                title: Some("Writing test file".to_string()),
            },
            decision: AuditDecision::Allow,
            option_id: Some("proceed_once".to_string()),
            policy: PolicyInfo {
                name: "WorktreePolicy".to_string(),
                reason: "working dir is the task worktree".to_string(),
            },
            working_dir,
        }
    }

    /// Core acceptance test: register → record → assert file content.
    ///
    /// 1. Build a `JsonlAuditSink` over a temp dir.
    /// 2. Register `working_dir` with (run-uid-1, run-1, my-slug, task-a).
    /// 3. `record` an entry with the transport's placeholder ids.
    /// 4. Assert `.makina/runs/run-uid-1/audit.jsonl` exists with one line
    ///    containing `run_id == "run-1"`, `task_id == "task-a"`, and the
    ///    decision populated.
    /// 5. `record` a second entry and assert TWO lines (append, not truncate).
    #[test]
    fn jsonl_audit_sink_enriches_and_appends() {
        let repo_dir = tempfile::tempdir().expect("create temp dir");
        let repo_root = repo_dir.path().to_path_buf();

        // The worktree lives under repo_root/.worktrees/<task-id>.
        let working_dir = repo_root.join(".worktrees").join("task-a");
        std::fs::create_dir_all(&working_dir).expect("create worktree dir");

        let sink = JsonlAuditSink::new(repo_root.clone());

        // Register the worktree context.
        sink.register(
            working_dir.clone(),
            "run-uid-1".to_string(),
            "run-1".to_string(),
            "my-slug".to_string(),
            "task-a".to_string(),
        );

        // Record the first entry (transport placeholder ids).
        sink.record(sample_entry(working_dir.clone()));

        // Assert the JSONL file exists and has exactly one line.
        let audit_path = repo_root
            .join(".makina")
            .join("runs")
            .join("run-uid-1")
            .join("audit.jsonl");
        assert!(
            audit_path.exists(),
            "audit.jsonl must be created after first record"
        );

        let contents = std::fs::read_to_string(&audit_path).expect("read audit.jsonl");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1, "first record must produce exactly one line");

        // Parse the line and verify enrichment.
        let entry: crate::governance::AuditEntry =
            serde_json::from_str(lines[0]).expect("line must be valid JSON");
        assert_eq!(
            entry.run_id, "run-1",
            "run_id must be enriched from registry"
        );
        assert_eq!(
            entry.task_id.as_deref(),
            Some("task-a"),
            "task_id must be enriched from registry"
        );
        assert_eq!(
            entry.decision,
            AuditDecision::Allow,
            "decision must be preserved"
        );
        assert_eq!(
            entry.policy.reason, "working dir is the task worktree",
            "policy reason must be preserved"
        );

        // Record a second entry and assert the file now has TWO lines (append).
        sink.record(sample_entry(working_dir.clone()));
        let contents2 = std::fs::read_to_string(&audit_path).expect("re-read audit.jsonl");
        let lines2: Vec<&str> = contents2.lines().collect();
        assert_eq!(
            lines2.len(),
            2,
            "second record must append a second line (not truncate)"
        );

        // Both lines must be valid JSON.
        for (i, line) in lines2.iter().enumerate() {
            serde_json::from_str::<crate::governance::AuditEntry>(line)
                .unwrap_or_else(|e| panic!("line {i} must be valid JSON: {e}"));
        }
    }

    /// Unregistered working_dir: sink must warn and discard (not panic).
    #[test]
    fn unregistered_working_dir_is_silently_discarded() {
        let repo_dir = tempfile::tempdir().expect("create temp dir");
        let repo_root = repo_dir.path().to_path_buf();
        let working_dir = repo_root.join(".worktrees").join("unknown-task");

        let sink = JsonlAuditSink::new(repo_root.clone());
        // Deliberately do NOT register `working_dir`.
        sink.record(sample_entry(working_dir));

        // No panic; no audit.jsonl created (nothing to route to).
        let runs_dir = repo_root.join(".makina").join("runs");
        assert!(
            !runs_dir.exists()
                || std::fs::read_dir(&runs_dir)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true),
            ".makina/runs/ must be empty or absent when no entry is routed"
        );
    }

    /// `NoopAuditRegistry` compiles and is a no-op.
    #[test]
    fn noop_registry_is_silent() {
        let reg: Arc<dyn AuditRegistry> = Arc::new(NoopAuditRegistry);
        // Must not panic.  The path is discarded by the noop, so any value works.
        reg.register(
            PathBuf::from("test-working-dir"),
            "run-uid-0".into(),
            "run-0".into(),
            "test".into(),
            "task-0".into(),
        );
    }
}
