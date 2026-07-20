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
//! The blocking file I/O is offloaded to a background `writer_loop` task: when
//! the sink is constructed inside a Tokio runtime, `record` enriches the entry
//! and hands the `(path, line)` pair to the writer via a bounded channel using
//! non-blocking `try_send`, so the caller's (ACP reader) thread never blocks on
//! `std::fs`.  Outside a runtime (unit tests) it falls back to a synchronous
//! append so behavior is unchanged.
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

use tokio::sync::mpsc::{self, Sender, error::TrySendError};

use crate::governance::{AuditEntry, AuditSink};

/// Bounded capacity of the background audit-writer queue.
///
/// `record` enqueues `(PathBuf, String)` pairs with `try_send` (non-blocking).
/// When the queue is full the entry is dropped with a `tracing::warn!` rather
/// than blocking the caller (the ACP transport reader loop), preserving the
/// sync, must-return-quickly [`AuditSink::record`] contract.
const AUDIT_QUEUE_CAP: usize = 1024;

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

    /// Evict every registry entry belonging to `run_id` (the `"run:{n}"` form).
    ///
    /// Called when a run reaches a terminal status so that per-task worktree
    /// contexts do not accumulate for the process lifetime.  The default body is
    /// a no-op so implementors that keep no state (e.g. [`NoopAuditRegistry`])
    /// and test spies need not implement it.
    fn evict_run(&self, run_id: &str) {
        let _ = run_id;
    }
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
/// # Async writer
///
/// `record` does **no** blocking `std::fs` work on the caller's thread.  When
/// constructed inside a Tokio runtime it spawns a background [`writer_loop`]
/// and hands enriched `(path, line)` pairs to it via a bounded
/// [`mpsc::channel`] (capacity [`AUDIT_QUEUE_CAP`]) using non-blocking
/// `try_send`.  When constructed outside a runtime (unit tests) the `sender`
/// is `None` and `record` falls back to the synchronous write path so behavior
/// is unchanged.
///
/// # Registry growth
///
/// Registry entries are evicted on terminal: when a run reaches a terminal
/// status the orchestrator calls [`AuditRegistry::evict_run`], which drops
/// every entry whose [`AuditContext::run_id`] matches the run.  This keeps the
/// registry bounded by the number of *in-flight* runs rather than every run the
/// process has ever started, so `makina` can run as a long-lived service
/// without the map growing without bound.
pub struct JsonlAuditSink {
    /// Root of the project repository; the JSONL file lives at
    /// `repo_root/.tasks/{slug}/audit.jsonl`.
    repo_root: PathBuf,
    /// Maps `working_dir → AuditContext`.  Populated by `register`, read by
    /// `record`.
    registry: Mutex<HashMap<PathBuf, AuditContext>>,
    /// Channel to the background writer task.  `Some` when constructed inside a
    /// Tokio runtime, `None` otherwise (the synchronous fallback is used).
    sender: Option<Sender<(PathBuf, String)>>,
    /// Join handle of the spawned [`writer_loop`].  Test-only seam: tests call
    /// [`JsonlAuditSink::flush`] to close the sender and await the writer so all
    /// enqueued lines are durably on disk before asserting file contents.
    #[cfg(test)]
    writer_handle: Option<tokio::task::JoinHandle<()>>,
}

impl JsonlAuditSink {
    /// Create a new sink rooted at `repo_root`.
    ///
    /// All audit files are created under `repo_root/.makina/runs/`.  `repo_root`
    /// should be the repository root (the same value used by `WorktreeManager`
    /// and `SquashMerger`).
    ///
    /// When called inside a Tokio runtime this spawns a background
    /// [`writer_loop`] (mirroring the `Handle::try_current` precedent in
    /// `actors::supervisor`) so `record`'s file I/O never blocks the caller.
    /// Outside a runtime (unit tests) the writer is not spawned and `record`
    /// uses the synchronous fallback path.
    pub fn new(repo_root: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel::<(PathBuf, String)>(AUDIT_QUEUE_CAP);
        // Spawn the background writer only when a runtime is current; the ACP
        // transport (the real caller) always runs inside one.  In unit tests
        // there is no current runtime, so `sender` stays `None` and `record`
        // falls back to the synchronous write path (behavior unchanged).
        let (sender, _writer_handle) = match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let h = handle.spawn(writer_loop(rx));
                (Some(tx), Some(h))
            }
            Err(_) => (None, None),
        };
        Self {
            repo_root,
            registry: Mutex::new(HashMap::new()),
            sender,
            #[cfg(test)]
            writer_handle: _writer_handle,
        }
    }

    /// Test-only seam: close the sender and await the background writer so every
    /// enqueued `(path, line)` pair is durably written before file assertions.
    ///
    /// Dropping `self.sender` closes the channel, causing `writer_loop`'s
    /// `rx.recv().await` to return `None` and the task to finish; awaiting the
    /// stored `JoinHandle` then guarantees the loop drained its queue.  When the
    /// sink was built outside a runtime (no writer) this is a no-op.
    #[cfg(test)]
    async fn flush(mut self) {
        // Drop the sender first so the writer's `recv()` observes the close.
        self.sender = None;
        if let Some(handle) = self.writer_handle.take() {
            let _ = handle.await;
        }
    }
}

/// Background writer task: drains `(path, line)` pairs and appends each to its
/// destination file.
///
/// Re-opens the destination per message because the path differs per run
/// (`.makina/runs/{run_uid}/audit.jsonl`).  This is the blocking
/// `create_dir_all` + `OpenOptions` + `writeln!` block moved off the caller's
/// (ACP reader) thread.  Runs until every `Sender` is dropped.
async fn writer_loop(mut rx: mpsc::Receiver<(PathBuf, String)>) {
    use std::io::Write as _;
    while let Some((path, line)) = rx.recv().await {
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "audit writer: failed to create run audit directory; dropping entry"
            );
            continue;
        }
        // Append the line (create or append, never truncate).  A compact
        // single-line audit entry is well under PIPE_BUF, so the O_APPEND
        // write is atomic on Linux/macOS; concurrent appends won't interleave.
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
                        "audit writer: failed to write audit entry; dropping entry"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "audit writer: failed to open audit.jsonl for append; dropping entry"
                );
            }
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

    /// Drop every registry entry belonging to `run_id` (the `"run:{n}"` form).
    ///
    /// The map is keyed by `working_dir` (a [`PathBuf`]), **not** by run id, so
    /// the match is on the [`AuditContext::run_id`] **value** rather than the
    /// key; we `retain` only entries for *other* runs.  Called when a run
    /// reaches a terminal status to bound registry growth.
    ///
    /// Non-panicking: on a poisoned mutex we log a warning and return (mirroring
    /// [`register`](AuditRegistry::register)'s poison handling).
    fn evict_run(&self, run_id: &str) {
        match self.registry.lock() {
            Ok(mut map) => {
                map.retain(|_, ctx| ctx.run_id != run_id);
            }
            Err(_) => {
                tracing::warn!("audit registry mutex poisoned; skipping eviction for {run_id}");
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

        // ── Resolve the per-run destination path ──────────────────────────────
        // The path is `.makina/runs/{run_uid}/audit.jsonl`, which differs per
        // run, so it is resolved here (on the caller's thread, cheap) and the
        // writer re-opens it per message.
        let path = match crate::paths::audit_log(&self.repo_root, &ctx.run_uid) {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(run_uid = %ctx.run_uid, %error, "audit state root unavailable; dropping entry");
                return;
            }
        };

        // ── Hand off to the background writer (non-blocking) ───────────────────
        // `try_send` only — never `send().await` or `blocking_send()` — to keep
        // the sync, must-return-quickly trait contract (governance.rs).  On a
        // full queue we drop the entry with a warning rather than block the ACP
        // reader thread.
        match &self.sender {
            Some(sender) => match sender.try_send((path, line)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    tracing::warn!(
                        run_uid = %ctx.run_uid,
                        "audit writer queue full; dropping entry"
                    );
                }
                Err(TrySendError::Closed(_)) => {
                    tracing::warn!(
                        run_uid = %ctx.run_uid,
                        "audit writer channel closed; dropping entry"
                    );
                }
            },
            // No background writer (constructed outside a Tokio runtime, e.g.
            // unit tests): fall back to the synchronous write path so behavior
            // is unchanged.
            None => write_line_sync(&path, &line),
        }
    }
}

/// Synchronous append fallback used when no background writer is running
/// (the sink was constructed outside a Tokio runtime).
///
/// Mirrors [`writer_loop`]'s per-message work: create the run directory, open
/// `create+append`, and `writeln!` the line.  Non-panicking — failures are
/// logged with `tracing::warn!` and dropped.
fn write_line_sync(path: &std::path::Path, line: &str) {
    use std::io::Write as _;
    if let Some(dir) = path.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        tracing::warn!(
            dir = %dir.display(),
            error = %e,
            "audit sink: failed to create run audit directory; skipping"
        );
        return;
    }
    // A compact single-line audit entry is well under PIPE_BUF, so the
    // O_APPEND write is atomic on Linux/macOS; concurrent appends by two tasks
    // of the same run won't interleave lines.
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governance::{AuditDecision, PolicyInfo, ToolRef};
    use chrono::Utc;
    use std::sync::Arc;

    // Use the process-global HOME_ENV_LOCK from lib.rs so all test modules
    // serialize HOME mutations across crate boundaries.
    use crate::HOME_ENV_LOCK;

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

    /// Core acceptance test: register → record → flush → assert file content.
    ///
    /// 1. Build a `JsonlAuditSink` over a temp dir (inside a Tokio runtime, so
    ///    the background writer is spawned).
    /// 2. Register `working_dir` with (run-uid-1, run-1, my-slug, task-a).
    /// 3. `record` two entries with the transport's placeholder ids.
    /// 4. `flush` the writer (close the sender, await the loop) so the lines are
    ///    durably on disk.
    /// 5. Assert `.makina/runs/run-uid-1/audit.jsonl` has TWO lines (append, not
    ///    truncate), each enriched (`run_id == "run-1"`, `task_id == "task-a"`,
    ///    decision and policy reason preserved).
    #[tokio::test]
    async fn jsonl_audit_sink_enriches_and_appends() {
        let _guard = HOME_ENV_LOCK.lock().await;
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let repo_dir = tempfile::tempdir().expect("create temp dir");
        let repo_root = repo_dir.path().to_path_buf();

        // SAFETY: serialised by HOME_ENV_LOCK (tokio async mutex held for entire test)
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

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

        // Record two entries (transport placeholder ids).  The second exercises
        // the append-not-truncate path.
        sink.record(sample_entry(working_dir.clone()));
        sink.record(sample_entry(working_dir.clone()));

        // Drain: close the sender and await the background writer so all
        // enqueued lines are on disk before asserting.
        sink.flush().await;

        // Assert the JSONL file exists and has exactly two lines (append).
        // The audit log now lives under state_root(repo_root) instead of repo_root/.makina.
        let audit_path = crate::paths::audit_log(&repo_root, "run-uid-1").unwrap();
        assert!(
            audit_path.exists(),
            "audit.jsonl must be created after record + flush"
        );

        let contents = std::fs::read_to_string(&audit_path).expect("read audit.jsonl");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "two records must produce two lines (append, not truncate)"
        );

        // Parse the first line and verify enrichment.
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

        // Both lines must be valid JSON.
        for (i, line) in lines.iter().enumerate() {
            serde_json::from_str::<crate::governance::AuditEntry>(line)
                .unwrap_or_else(|e| panic!("line {i} must be valid JSON: {e}"));
        }
    }

    /// Done-when test: `evict_run` removes exactly the entries belonging to the
    /// named run (matched on the [`AuditContext::run_id`] **value**, since the
    /// map is keyed by `working_dir`), leaving other runs untouched.
    ///
    /// Register two distinct working_dirs under run id `"run:1"` and one under
    /// `"run:2"`, call `sink.evict_run("run:1")`, then inspect the
    /// in-module-visible private `sink.registry` and assert only the `"run:2"`
    /// entry survives.
    #[test]
    fn evict_run_removes_only_that_runs_entries() {
        let repo_dir = tempfile::tempdir().expect("create temp dir");
        let repo_root = repo_dir.path().to_path_buf();

        let wd_run1_a = repo_root.join(".worktrees").join("task-a");
        let wd_run1_b = repo_root.join(".worktrees").join("task-b");
        let wd_run2 = repo_root.join(".worktrees").join("task-c");

        let sink = JsonlAuditSink::new(repo_root.clone());

        // Two entries under run:1 ...
        sink.register(
            wd_run1_a.clone(),
            "run-uid-1".to_string(),
            "run:1".to_string(),
            "my-slug".to_string(),
            "task-a".to_string(),
        );
        sink.register(
            wd_run1_b.clone(),
            "run-uid-1".to_string(),
            "run:1".to_string(),
            "my-slug".to_string(),
            "task-b".to_string(),
        );
        // ... and one under run:2.
        sink.register(
            wd_run2.clone(),
            "run-uid-2".to_string(),
            "run:2".to_string(),
            "my-slug".to_string(),
            "task-c".to_string(),
        );

        // Evict run:1 only.
        sink.evict_run("run:1");

        // Inspect the private registry directly (in-module visibility).
        let map = sink.registry.lock().expect("registry mutex not poisoned");
        assert_eq!(
            map.len(),
            1,
            "only the run:2 entry must remain after evicting run:1"
        );
        assert!(
            !map.contains_key(&wd_run1_a),
            "run:1 working_dir (task-a) must be evicted"
        );
        assert!(
            !map.contains_key(&wd_run1_b),
            "run:1 working_dir (task-b) must be evicted"
        );
        let surviving = map
            .get(&wd_run2)
            .expect("run:2 working_dir (task-c) must survive");
        assert_eq!(
            surviving.run_id, "run:2",
            "the surviving entry must belong to run:2"
        );
    }

    /// Unregistered working_dir: sink must warn and discard (not panic).
    #[tokio::test]
    async fn unregistered_working_dir_is_silently_discarded() {
        let repo_dir = tempfile::tempdir().expect("create temp dir");
        let repo_root = repo_dir.path().to_path_buf();
        let working_dir = repo_root.join(".worktrees").join("unknown-task");

        let sink = JsonlAuditSink::new(repo_root.clone());
        // Deliberately do NOT register `working_dir`.
        sink.record(sample_entry(working_dir));

        // Drain the writer before asserting on the filesystem.
        sink.flush().await;

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
    #[tokio::test]
    async fn noop_registry_is_silent() {
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

    /// Done-when test: `record` enqueues without blocking on I/O, and the
    /// background writer flushes every line to disk in enqueue order.
    ///
    /// Spam N=1000 `record` calls; each must return promptly (it only enriches,
    /// serializes, and `try_send`s — no `std::fs`).  Then flush the writer and
    /// assert the audit file under `.makina/runs/{run_uid}/audit.jsonl` contains
    /// exactly N lines in enqueue order (the per-call `session_id` carries the
    /// sequence number, so order is checkable).
    #[tokio::test]
    async fn record_enqueues_without_blocking_and_writer_flushes_in_order() {
        let _guard = HOME_ENV_LOCK.lock().await;
        let tmp_home = tempfile::tempdir().expect("create temp home");
        const N: usize = 1000;

        let repo_dir = tempfile::tempdir().expect("create temp dir");
        let repo_root = repo_dir.path().to_path_buf();

        // SAFETY: serialised by HOME_ENV_LOCK (tokio async mutex held for entire test)
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        let working_dir = repo_root.join(".worktrees").join("task-a");

        let sink = JsonlAuditSink::new(repo_root.clone());
        sink.register(
            working_dir.clone(),
            "run-uid-1".to_string(),
            "run-1".to_string(),
            "my-slug".to_string(),
            "task-a".to_string(),
        );

        // Spam N record calls, each tagged with its sequence number via
        // `session_id`, timing the whole burst.  `record` must not block on
        // I/O — it only enriches, serializes, and `try_send`s — so the burst
        // completes far faster than N synchronous file appends would.
        let start = std::time::Instant::now();
        for i in 0..N {
            let mut entry = sample_entry(working_dir.clone());
            entry.session_id = Some(format!("seq-{i}"));
            sink.record(entry);
        }
        let enqueue_elapsed = start.elapsed();
        assert!(
            enqueue_elapsed < std::time::Duration::from_secs(2),
            "N={N} record calls must return promptly (not block on I/O); took {enqueue_elapsed:?}"
        );

        // Flush the writer: close the sender and await the loop so every line
        // is durably written.
        sink.flush().await;

        // Assert the file has exactly N lines, in enqueue order.
        // The audit log now lives under state_root(repo_root)/runs/{run_uid}/audit.jsonl.
        let audit_path = crate::paths::audit_log(&repo_root, "run-uid-1").unwrap();
        let contents = std::fs::read_to_string(&audit_path).expect("read audit.jsonl");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(
            lines.len(),
            N,
            "writer must flush exactly N={N} lines (none dropped)"
        );
        for (i, line) in lines.iter().enumerate() {
            let entry: crate::governance::AuditEntry =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("line {i} invalid JSON: {e}"));
            assert_eq!(
                entry.session_id.as_deref(),
                Some(format!("seq-{i}").as_str()),
                "line {i} must be in enqueue order"
            );
        }
    }
}
