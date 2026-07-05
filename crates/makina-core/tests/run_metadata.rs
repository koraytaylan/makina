//! Integration test for **log-run-metadata-wire** (plan 0003 §0013).
//!
//! Acceptance criterion: opening + starting a run drives it to a terminal status
//! and, at finalization, the orchestrator writes a best-effort `run.json` under
//! `.makina/runs/{run_uid}/` carrying the run's identity (`run_uid`/`run_slug`),
//! the terminal [`RunStatus`], and a `started_at <= ended_at` lifecycle window.
//!
//! # Test-strategy compliance
//!
//! - Backend is always `NoopBackend` — no real agent CLI, no model call.
//! - The test uses a fresh temporary git repo (`tempfile`).
//! - No arbitrary sleeps: the run's terminal status is awaited via `subscribe()`
//!   under a bounded [`tokio::time::timeout`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_stream::StreamExt;

use makina_core::api::{Api, Command as ApiCommand, CommandOutcome, Event, RunStatus};
use makina_core::backend::noop::NoopBackend;
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::dependency::EdgeInferrer;
use makina_core::interpreter::StructuredTextInterpreter;
use makina_core::orchestrator::{CoreApi, run_slug};
use makina_core::paths;
use makina_core::run_metadata::RunMetadata;
use makina_core::test_support::setup_temp_repo;
use makina_core::worktree::WorktreeManager;

// ── Temp-repo helpers ─────────────────────────────────────────────────────────

/// Create a minimal git repository in a fresh tempdir on a `develop` branch.
/// Build a `CoreApi` over the deterministic interpreter + `NoopBackend` +
/// a temp-repo `WorktreeManager` + a no-gate `Config`.
///
/// Mirrors `build_api` in `tests/orchestrator_read_path.rs`.
fn build_api(repo_root: PathBuf) -> CoreApi {
    let interpreter = Arc::new(EdgeInferrer::new(
        Arc::new(StructuredTextInterpreter::new()),
    ));
    let backend = Arc::new(NoopBackend::with_responses(vec![
        "Implemented.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]));
    let wm = WorktreeManager::new(repo_root, "develop".into());
    let config = Config::resolve(GlobalConfig::default(), ProjectConfig::default());
    CoreApi::new(interpreter, backend, wm, config)
}

/// A one-task list that drives a complete run to `Completed`.
const ONE_TASK_LIST: &str = r#"# Solo — Task List

A one-task list used to exercise run finalization.

---

## 0001 — Foundation

### solo-task — Implement the solo task
Do the thing in `lib.rs`.
- **Depends on:** —
- **Done when:** The solo task completes its work and all verification checks pass.
"#;

// ── Test: run.json is written at finalization ────────────────────────────────

/// **Acceptance (log-run-metadata-wire):**
///
/// `OpenRun` + `StartRun` a one-task run, await `RunStatus::Completed` over the
/// event stream, then assert `.makina/runs/{run_uid}/run.json` parses to a
/// [`RunMetadata`] whose `run_uid`/`run_slug` match the run, whose status is a
/// terminal one, and whose `started_at <= ended_at`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_metadata_terminal() {
    // Set HOME to a temp dir so state_root resolves under it (off-repo).
    // SAFETY: this is the only test in this file; no parallel HOME mutation.
    let tmp_home = tempfile::tempdir().expect("create temp home");
    unsafe { std::env::set_var("HOME", tmp_home.path()) };

    let repo = setup_temp_repo();
    let repo_root = repo.path().to_path_buf();
    let api = Arc::new(build_api(repo_root.clone()));

    // Write the task-list `.md` into the repo's plan directory so the
    // plan-scoped slug is well-formed.
    let plan_dir = repo_root.join("doc").join("plan").join("0001-foundation");
    std::fs::create_dir_all(&plan_dir).expect("create plan dir");
    let task_list_path = plan_dir.join("TASKS.md");
    std::fs::write(&task_list_path, ONE_TASK_LIST).expect("write task list");

    // Open the run.
    let run = match api
        .execute(ApiCommand::OpenRun {
            task_list_path: task_list_path.clone(),
        })
        .await
        .expect("OpenRun must succeed")
    {
        CommandOutcome::RunOpened { run } => run,
        other => panic!("unexpected OpenRun outcome: {other:?}"),
    };

    // The persistent run identity surfaced on the view.
    let view = api.run(run).await.expect("run view must exist");
    let run_uid = view.run_uid.clone();
    let expected_slug = run_slug(&task_list_path);
    assert_eq!(run_uid.len(), 26, "run_uid must be a 26-char ULID string");

    // Subscribe BEFORE starting so we capture the terminal status.
    let mut stream = api.subscribe();

    let outcome = api
        .execute(ApiCommand::StartRun { run })
        .await
        .expect("StartRun must succeed");
    assert!(
        matches!(outcome, CommandOutcome::Acknowledged),
        "StartRun returns promptly (Acknowledged)"
    );

    // Wait for RunStatusChanged{Completed} on this run, bounded.
    let waited = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(ev) = stream.next().await {
            if let Event::RunStatusChanged {
                run: r,
                status: RunStatus::Completed,
            } = ev
                && r == run
            {
                return;
            }
        }
        panic!("event stream ended before run reached Completed");
    })
    .await;
    waited.expect("run must reach Completed before the timeout");

    // finalize_run_status (which writes run.json) runs after the terminal status
    // is broadcast; poll the file into existence under a bounded deadline.
    // The file now lives under state_root(repo_root)/runs/{run_uid}/run.json.
    let run_json = paths::run_dir(&repo_root, &run_uid).join("run.json");
    let appeared = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if run_json.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    appeared.expect("run.json must be written at finalization");

    // Parse it back into a RunMetadata and assert the identity + window.
    let contents = std::fs::read_to_string(&run_json).expect("read run.json");
    let meta: RunMetadata = serde_json::from_str(&contents).expect("parse run.json");

    assert_eq!(meta.run_uid(), run_uid, "run_uid must match the run");
    assert_eq!(
        meta.run_slug(),
        expected_slug,
        "run_slug must be the plan-scoped slug"
    );
    assert!(
        matches!(meta.status(), RunStatus::Completed | RunStatus::Failed),
        "status must be terminal, got {:?}",
        meta.status()
    );
    assert!(
        meta.started_at() <= meta.ended_at(),
        "started_at ({}) must be <= ended_at ({})",
        meta.started_at(),
        meta.ended_at()
    );
}
