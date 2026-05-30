//! Atomic persistence for [`TaskGraph`] artifacts.
//!
//! This module provides the two operations the Supervisor uses to read and
//! write `.tasks/{slug}.json` files:
//!
//! - [`persist_graph`] — serializes a [`TaskGraph`] to pretty JSON and writes
//!   it atomically: the JSON lands in a temp file first, then a same-filesystem
//!   rename makes the update visible.  A crash mid-write can leave a temp file
//!   behind but can never produce a partial `{slug}.json`.
//!
//! - [`load_graph`] — reads and deserializes `{slug}.json`, returning
//!   `Ok(None)` for a missing file instead of an error.
//!
//! # Atomicity guarantee
//!
//! The temp file is written inside `.tasks/` (same directory, same filesystem
//! mount) so that `tokio::fs::rename` is always a same-filesystem rename — an
//! O_RENAME / rename(2) syscall that the kernel completes atomically from the
//! perspective of any concurrent reader.
//!
//! The temp file is named `.{slug}.json.tmp.<pid>.<seq>`.  Using the process
//! ID plus a per-call monotonic counter avoids collisions between concurrent
//! Supervisor processes and between concurrent calls within the same process.
//!
//! # Commit policy
//!
//! This increment writes `.tasks/{slug}.json` on every FSM transition (and
//! seeds it when the Supervisor handles `OpenRun`), but does **not**
//! automatically `git add` or `git commit` the file.  Committing is left to
//! the user, CI, or a future increment.
//!
//! Tradeoff: the live state is always inspectable via `git status` / `git diff`
//! — the project's "reviewable via diff" property holds for human inspection —
//! but the per-transition snapshots are **not** auto-committed into history.
//! Skipping auto-commits avoids producing dozens of noisy micro-commits per
//! task and eliminates merge-lock contention on a shared branch (e.g.
//! `develop`) when multiple tasks run in parallel.
//!
//! The repo lays out its Makina state under a single `.makina/` directory with
//! a commit/ignore split: `.makina/config.toml` and the task artifacts under
//! `.makina/tasks/` are **committed** (never listed in any `.gitignore`), while
//! the transient runtime state — `.makina/runs/` and `.makina/worktrees/` — is
//! gitignored via `.makina/.gitignore` (which lists `/runs/` and `/worktrees/`).
//! So the artifact is committable whenever the user/CI wants a checkpoint, and
//! the run/worktree checkouts stay out of history because they are transient.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use crate::task::{TaskGraph, TaskState};

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors that can occur while persisting or loading a [`TaskGraph`].
#[derive(Debug, Error)]
pub enum PersistError {
    /// Serialization of the [`TaskGraph`] to JSON failed.
    #[error("failed to serialize task graph `{slug}`: {source}")]
    Serialize {
        /// The graph slug involved.
        slug: String,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },

    /// Deserialization of JSON into a [`TaskGraph`] failed.
    #[error("failed to deserialize task graph from `{path}`: {source}")]
    Deserialize {
        /// Path of the file that could not be parsed.
        path: String,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },

    /// An I/O operation (create dir, write temp file, rename, read) failed.
    #[error("I/O error for `{path}`: {source}")]
    Io {
        /// Path involved in the failed operation.
        path: String,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },
}

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Returns the canonical path for a task-graph artifact:
/// `{repo_root}/.tasks/{slug}.json`.
///
/// # Example
///
/// ```
/// # use std::path::Path;
/// # use makina_core::persist::tasks_path;
/// let p = tasks_path(Path::new("/repo"), "my-feature");
/// assert_eq!(p, std::path::PathBuf::from("/repo/.makina/tasks/my-feature.json"));
/// ```
pub fn tasks_path(repo_root: &Path, slug: &str) -> PathBuf {
    crate::paths::task_graph(repo_root, slug)
}

/// Returns the path of the temp file used during an atomic write:
/// `{repo_root}/.tasks/.{slug}.json.tmp.{pid}.{seq}`.
///
/// The temp file lives **inside** `.tasks/` so that the subsequent rename is
/// guaranteed to stay on the same filesystem mount.
///
/// The PID component avoids collisions between concurrent processes; the
/// monotonic sequence counter avoids collisions between concurrent calls
/// within the same process.
fn temp_path(repo_root: &Path, slug: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    repo_root
        .join(".makina")
        .join("tasks")
        .join(format!(".{slug}.json.tmp.{pid}.{seq}"))
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Atomically persist `graph` to `.tasks/{slug}.json` under `repo_root`.
///
/// Steps:
/// 1. Create `.tasks/` if it does not exist.
/// 2. Serialize `graph` to pretty JSON (matching the normative on-disk schema).
/// 3. Write the JSON to `.tasks/.{slug}.json.tmp.{pid}.{seq}`.
/// 4. `tokio::fs::rename` the temp file over `.tasks/{slug}.json`.
///
/// A trailing newline is appended after the JSON object so that the file ends
/// cleanly when committed to version control.
///
/// A crash between steps 3 and 4 can leave the temp file on disk; a crash
/// during step 3 can leave a partial temp file.  In either case `{slug}.json`
/// is either absent (first write) or still holds the previous complete
/// contents.
///
/// # Concurrency
///
/// Concurrent writes for the same slug are safe: each call writes to a unique
/// temp file (distinct PID + sequence) and atomically renames it into place, so
/// a reader always sees a complete, consistent snapshot.  Ordering is
/// last-writer-wins, which is acceptable for this durability aid — no
/// caller-side mutex is required.
///
/// # Errors
///
/// Returns [`PersistError`] on serialization or I/O failure.
pub async fn persist_graph(graph: &TaskGraph, repo_root: &Path) -> Result<(), PersistError> {
    let tasks_dir = repo_root.join(".makina").join("tasks");

    // 1. Ensure .tasks/ exists.
    tokio::fs::create_dir_all(&tasks_dir)
        .await
        .map_err(|e| PersistError::Io {
            path: tasks_dir.display().to_string(),
            source: e,
        })?;

    // 2. Serialize.
    let mut json = serde_json::to_string_pretty(graph).map_err(|e| PersistError::Serialize {
        slug: graph.slug.clone(),
        source: e,
    })?;
    // Append a trailing newline so the file ends cleanly (POSIX convention and
    // makes `git diff` output tidy when committed to VCS).
    json.push('\n');

    // 3. Write to temp file inside .tasks/.
    let tmp = temp_path(repo_root, &graph.slug);
    tokio::fs::write(&tmp, json.as_bytes())
        .await
        .map_err(|e| PersistError::Io {
            path: tmp.display().to_string(),
            source: e,
        })?;

    // 4. Atomic rename onto the canonical path.
    //    tokio::fs::rename keeps this off the blocking thread pool; on failure
    //    we do a best-effort cleanup of the temp file to avoid leaking it.
    let dest = tasks_path(repo_root, &graph.slug);
    if let Err(e) = tokio::fs::rename(&tmp, &dest).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(PersistError::Io {
            path: dest.display().to_string(),
            source: e,
        });
    }

    Ok(())
}

/// Load and deserialize `.tasks/{slug}.json` under `repo_root`.
///
/// Returns `Ok(None)` if the file does not exist (the graph has not yet been
/// persisted), rather than treating a missing file as an error.
///
/// # Errors
///
/// Returns [`PersistError`] on I/O failure (other than "not found") or if the
/// JSON cannot be deserialized into a [`TaskGraph`].
pub async fn load_graph(repo_root: &Path, slug: &str) -> Result<Option<TaskGraph>, PersistError> {
    let path = tasks_path(repo_root, slug);

    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(PersistError::Io {
                path: path.display().to_string(),
                source: e,
            });
        }
    };

    let graph: TaskGraph =
        serde_json::from_slice(&bytes).map_err(|e| PersistError::Deserialize {
            path: path.display().to_string(),
            source: e,
        })?;

    Ok(Some(graph))
}

// ── Resume recovery ───────────────────────────────────────────────────────────

/// Reset in-flight tasks to [`TaskState::Ready`] so a restarted Supervisor can
/// safely re-pick them up.
///
/// This is a **pure, synchronous** function — no I/O, no async.  It is intended
/// to be called immediately after [`load_graph`] returns a persisted artifact,
/// before the Supervisor begins scheduling work.
///
/// # State transitions applied
///
/// | Before               | After              |
/// |----------------------|--------------------|
/// | `InProgress`         | `Ready`            |
/// | `InReview`           | `Ready`            |
/// | `New` / `Ready` / `Done` / `Failed` | unchanged |
///
/// # What is preserved
///
/// - `gate_iterations` and `review_iterations` are **not** reset — they are
///   cumulative counters that survive crashes and must not be lost.
/// - `started_at` is left as-is (it records the first historical pickup time).
/// - `finished_at` is only set on terminal states (`Done`/`Failed`), which are
///   unchanged, so it is unaffected.
/// - `updated_at` is intentionally **not** touched: this is a structural repair
///   operation, not a domain event, so bumping it would pollute the audit trail
///   with a spurious modification time.
///
/// # Note on FSM bypass
///
/// This function directly sets `task.state` rather than routing through the
/// state machine's `transition`/`apply_event` path.  That is intentional —
/// crash-resume recovery is an infrastructure concern, not a normal lifecycle
/// event.
pub fn recover_for_resume(graph: &mut TaskGraph) {
    for task in &mut graph.tasks {
        match task.state {
            TaskState::InProgress | TaskState::InReview => {
                task.state = TaskState::Ready;
            }
            TaskState::New
            | TaskState::Ready
            | TaskState::Done
            | TaskState::Failed
            | TaskState::Skipped => {}
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Unit tests for the four "Done when" properties:
    //!
    //! 1. A [`TaskGraph`] round-trips through `persist_graph` → `load_graph`
    //!    to an equal value.
    //! 2. Omitted optional fields (`section`, `started_at`, `finished_at`)
    //!    stay omitted (no `null`) in the written file.
    //! 3. `load_graph` returns `Ok(None)` for a missing file.
    //! 4. An interrupted write never leaves a partial `{slug}.json`.
    //!
    //! # On property 4 (atomicity)
    //!
    //! We cannot simulate a mid-write OS crash in a unit test.  Instead we
    //! verify the structural invariant that makes the rename atomic:
    //!
    //! - The temp file has a distinct name from `{slug}.json` (so a partial
    //!   write never touches the canonical path).
    //! - After a successful `persist_graph`, no temp file residue remains.
    //! - A pre-existing `{slug}.json` written by a first call is still
    //!   intact and valid when the second call completes (i.e., we never
    //!   corrupt the existing file except by atomically replacing it).

    use chrono::TimeZone;

    use super::*;
    use crate::task::{Task, TaskId, TaskState};

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn fixed_ts(year: i32, month: u32, day: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .with_ymd_and_hms(year, month, day, 10, 0, 0)
            .single()
            .expect("valid date")
    }

    /// A minimal but realistic [`TaskGraph`] with:
    /// - one task with all optional fields present (`section`, `started_at`,
    ///   `finished_at`), and
    /// - one task with all optional fields absent.
    fn sample_graph() -> TaskGraph {
        let t0 = fixed_ts(2026, 5, 1);
        let t1 = fixed_ts(2026, 5, 2);

        TaskGraph {
            slug: "test-plan".to_string(),
            tasks: vec![
                Task {
                    id: TaskId::new("first-task"),
                    title: "First task".to_string(),
                    description: "Does something important.".to_string(),
                    done_when: "tests pass".to_string(),
                    depends_on: vec![],
                    section: Some("0001".to_string()),
                    state: TaskState::Done,
                    gate_iterations: 1,
                    review_iterations: 0,
                    created_at: t0,
                    updated_at: t1,
                    started_at: Some(t0),
                    finished_at: Some(t1),
                },
                Task {
                    id: TaskId::new("second-task"),
                    title: "Second task".to_string(),
                    description: "Has no optional fields set.".to_string(),
                    done_when: "builds cleanly".to_string(),
                    depends_on: vec![TaskId::new("first-task")],
                    section: None,
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    created_at: t0,
                    updated_at: t0,
                    started_at: None,
                    finished_at: None,
                },
            ],
        }
    }

    // ── Test 1: Round-trip ────────────────────────────────────────────────────

    /// `TaskGraph` written by `persist_graph` and read back by `load_graph`
    /// must equal the original value.
    #[tokio::test]
    async fn round_trip_through_persist_and_load() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();
        let graph = sample_graph();

        persist_graph(&graph, root)
            .await
            .expect("persist_graph must succeed");

        let loaded = load_graph(root, &graph.slug)
            .await
            .expect("load_graph must succeed")
            .expect("file must exist after persist_graph");

        assert_eq!(
            graph, loaded,
            "loaded TaskGraph must equal the persisted original"
        );
    }

    // ── Test 2: No null for omitted optional fields ───────────────────────────

    /// The on-disk JSON must not contain `null` values for `section`,
    /// `started_at`, or `finished_at` — those keys must be absent entirely.
    #[tokio::test]
    async fn omitted_optional_fields_not_serialized_as_null() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();
        let graph = sample_graph();

        persist_graph(&graph, root)
            .await
            .expect("persist_graph must succeed");

        let path = tasks_path(root, &graph.slug);
        let contents = std::fs::read_to_string(&path).expect("file must be readable");

        // None of the optional fields should appear as JSON `null`.
        assert!(
            !contents.contains("\"section\": null"),
            "section must be absent, not null — found in:\n{contents}"
        );
        assert!(
            !contents.contains("\"started_at\": null"),
            "started_at must be absent, not null — found in:\n{contents}"
        );
        assert!(
            !contents.contains("\"finished_at\": null"),
            "finished_at must be absent, not null — found in:\n{contents}"
        );

        // Cross-check: the task that has optional fields set should include them.
        assert!(
            contents.contains("\"section\": \"0001\""),
            "present section must appear in JSON"
        );
    }

    // ── Test 3: Missing file returns Ok(None) ─────────────────────────────────

    /// `load_graph` for a slug whose file does not yet exist must return
    /// `Ok(None)` rather than an error.
    #[tokio::test]
    async fn load_graph_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let result = load_graph(dir.path(), "nonexistent-slug")
            .await
            .expect("load_graph must not error on missing file");

        assert!(
            result.is_none(),
            "load_graph must return None for a missing file, got: {result:?}"
        );
    }

    // ── Test 4: Atomicity structural invariants ───────────────────────────────

    /// Verify the structural properties that underpin the atomicity guarantee:
    ///
    /// a) The temp path has a distinct name from `{slug}.json`.
    /// b) After a successful write the canonical `{slug}.json` is present and
    ///    parseable.
    /// c) No `*.tmp.*` residue remains after a successful write.
    /// d) A first write's output is fully replaced (not partially corrupted) by
    ///    a second write.
    #[tokio::test]
    async fn atomic_write_leaves_no_temp_residue() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();
        let graph = sample_graph();

        // (a) Temp path must differ from canonical path.
        let canonical = tasks_path(root, &graph.slug);
        let tmp = temp_path(root, &graph.slug);
        assert_ne!(
            canonical, tmp,
            "temp path must not equal the canonical .json path"
        );
        assert!(
            tmp.file_name().unwrap().to_str().unwrap().contains(".tmp."),
            "temp file name must contain '.tmp.'"
        );

        // (b) After a successful persist the canonical file exists and parses.
        persist_graph(&graph, root)
            .await
            .expect("first persist_graph must succeed");

        assert!(
            canonical.exists(),
            "{slug}.json must exist after persist",
            slug = graph.slug
        );

        let json = std::fs::read_to_string(&canonical).expect("must read");
        let _parsed: TaskGraph =
            serde_json::from_str(&json).expect("on-disk JSON must parse as TaskGraph");

        // (c) No temp file residue remains.
        assert!(
            !tmp.exists(),
            "temp file must be gone after successful persist_graph"
        );

        // Broader glob check: no .tmp. files anywhere in .makina/tasks/.
        let tasks_dir = root.join(".makina").join("tasks");
        for entry in std::fs::read_dir(&tasks_dir).expect("read .makina/tasks dir") {
            let name = entry
                .expect("valid entry")
                .file_name()
                .into_string()
                .expect("utf8 name");
            assert!(
                !name.contains(".tmp."),
                "unexpected temp file left behind: {name}"
            );
        }

        // (d) A second persist atomically replaces the first.
        persist_graph(&graph, root)
            .await
            .expect("second persist_graph must succeed");

        let json2 = std::fs::read_to_string(&canonical).expect("must read after 2nd persist");
        let parsed2: TaskGraph = serde_json::from_str(&json2).expect("2nd on-disk JSON must parse");
        assert_eq!(graph, parsed2, "2nd persist must produce the same content");
    }

    // ── Test 5: tasks_path helper ─────────────────────────────────────────────

    #[test]
    fn tasks_path_constructs_correct_path() {
        let p = tasks_path(Path::new("/some/repo"), "plan-0002");
        assert_eq!(
            p,
            PathBuf::from("/some/repo/.makina/tasks/plan-0002.json"),
            "tasks_path must produce repo/.makina/tasks/slug.json"
        );
    }

    // ── Test 6: .gitignore commit-policy invariants ───────────────────────────

    /// Assert the `.makina/` commit/ignore split: `.makina/.gitignore` ignores
    /// `runs/` and `worktrees/` (transient runtime state), while the root
    /// `.gitignore` no longer ignores `.worktrees/` and does **not** ignore
    /// `.makina/` itself (config + tasks under `.makina/` are committable).
    ///
    /// This test reads the `.gitignore` files (the root one two levels above
    /// `CARGO_MANIFEST_DIR`, plus `.makina/.gitignore`) and checks the rules by
    /// simple string matching. It is intentionally kept to string-level checks
    /// rather than spawning `git check-ignore` so that it works in any
    /// environment (including CI sandboxes without a full git context).
    #[test]
    fn gitignore_worktrees_ignored_tasks_not_ignored() {
        // CARGO_MANIFEST_DIR = .../crates/makina-core
        // Repo root           = ../..
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest
            .parent()
            .expect("crates/")
            .parent()
            .expect("repo root");

        // ── .makina/.gitignore: runs/ and worktrees/ are ignored ──────────────
        let makina_gitignore_path = repo_root.join(".makina").join(".gitignore");
        let makina_contents = std::fs::read_to_string(&makina_gitignore_path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", makina_gitignore_path.display()));

        assert!(
            makina_contents.lines().any(|l| l.trim() == "/runs/"),
            "/runs/ must be listed in .makina/.gitignore — found:\n{makina_contents}"
        );
        assert!(
            makina_contents.lines().any(|l| l.trim() == "/worktrees/"),
            "/worktrees/ must be listed in .makina/.gitignore — found:\n{makina_contents}"
        );

        // ── root .gitignore: no longer ignores .worktrees/ or .makina/ ────────
        let gitignore_path = repo_root.join(".gitignore");
        let contents = std::fs::read_to_string(&gitignore_path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", gitignore_path.display()));

        // /.worktrees/ must no longer appear as a gitignore rule.
        assert!(
            !contents.lines().any(|l| l.trim() == "/.worktrees/"),
            "/.worktrees/ must NOT be listed in the root .gitignore — found:\n{contents}"
        );

        // No rule that would ignore .makina/ or .makina should be present.
        let makina_ignored = contents.lines().any(|l| {
            let l = l.trim();
            // Reject any non-comment line that would swallow .makina paths:
            // e.g. ".makina", ".makina/", "/.makina", "/.makina/"
            !l.starts_with('#')
                && (l == ".makina" || l == ".makina/" || l == "/.makina" || l == "/.makina/")
        });
        assert!(
            !makina_ignored,
            ".makina must NOT be in the root .gitignore — found an ignoring rule in:\n{contents}"
        );
    }

    // ── Tests for recover_for_resume ──────────────────────────────────────────

    fn make_task(id: &str, state: TaskState, gate_iterations: u32, review_iterations: u32) -> Task {
        let t0 = fixed_ts(2026, 5, 1);
        Task {
            id: TaskId::new(id),
            title: id.to_string(),
            description: String::new(),
            done_when: String::new(),
            depends_on: vec![],
            section: None,
            state,
            gate_iterations,
            review_iterations,
            created_at: t0,
            updated_at: t0,
            started_at: if state == TaskState::InProgress || state == TaskState::InReview {
                Some(t0)
            } else {
                None
            },
            finished_at: if state == TaskState::Done || state == TaskState::Failed {
                Some(fixed_ts(2026, 5, 2))
            } else {
                None
            },
        }
    }

    /// Build a graph containing one task in each of the six states, two of which
    /// carry non-zero counters, then call `recover_for_resume` and verify:
    ///
    /// - `InProgress` → `Ready`
    /// - `InReview`   → `Ready`
    /// - `New`, `Ready`, `Done`, `Failed` → unchanged
    /// - `gate_iterations` / `review_iterations` are preserved on every task
    /// - `graph.validate()` returns `Ok`
    #[test]
    fn recover_for_resume_resets_in_flight_states() {
        let mut graph = TaskGraph {
            slug: "resume-test".to_string(),
            tasks: vec![
                make_task("t-new", TaskState::New, 0, 0),
                make_task("t-ready", TaskState::Ready, 0, 0),
                make_task("t-in-progress", TaskState::InProgress, 2, 1),
                make_task("t-in-review", TaskState::InReview, 1, 3),
                make_task("t-done", TaskState::Done, 1, 1),
                make_task("t-failed", TaskState::Failed, 3, 0),
            ],
        };

        super::recover_for_resume(&mut graph);

        // InProgress → Ready
        assert_eq!(
            graph.tasks[2].state,
            TaskState::Ready,
            "InProgress must become Ready"
        );
        // InReview → Ready
        assert_eq!(
            graph.tasks[3].state,
            TaskState::Ready,
            "InReview must become Ready"
        );

        // Unchanged states
        assert_eq!(graph.tasks[0].state, TaskState::New, "New must stay New");
        assert_eq!(
            graph.tasks[1].state,
            TaskState::Ready,
            "Ready must stay Ready"
        );
        assert_eq!(graph.tasks[4].state, TaskState::Done, "Done must stay Done");
        assert_eq!(
            graph.tasks[5].state,
            TaskState::Failed,
            "Failed must stay Failed"
        );

        // Counters preserved on originally-InProgress task
        assert_eq!(
            graph.tasks[2].gate_iterations, 2,
            "gate_iterations must be preserved on ex-InProgress task"
        );
        assert_eq!(
            graph.tasks[2].review_iterations, 1,
            "review_iterations must be preserved on ex-InProgress task"
        );

        // Counters preserved on originally-InReview task
        assert_eq!(
            graph.tasks[3].gate_iterations, 1,
            "gate_iterations must be preserved on ex-InReview task"
        );
        assert_eq!(
            graph.tasks[3].review_iterations, 3,
            "review_iterations must be preserved on ex-InReview task"
        );

        // Graph structural integrity must hold after the recovery pass.
        graph
            .validate()
            .expect("graph must pass validate() after recover_for_resume");
    }

    /// A graph that is already clean (no in-flight tasks) must pass through
    /// `recover_for_resume` without any state changes.
    #[test]
    fn recover_for_resume_is_idempotent_on_clean_graph() {
        let mut graph = TaskGraph {
            slug: "clean-test".to_string(),
            tasks: vec![
                make_task("t-new", TaskState::New, 0, 0),
                make_task("t-done", TaskState::Done, 1, 0),
            ],
        };
        let before = graph.clone();

        super::recover_for_resume(&mut graph);

        assert_eq!(
            graph, before,
            "a clean graph must be unchanged by recover_for_resume"
        );
    }
}
