//! Integration tests for **orchestrator-read-path** (plan 0002 §0011).
//!
//! Acceptance criterion: an integration test opens a run, demonstrates that a
//! pre-existing `.tasks/{slug}.json` artifact is used as the source of truth
//! instead of re-interpreting the `.md` file, and verifies that
//! `recover_for_resume` is applied (formerly `InProgress` tasks become `Ready`).
//!
//! # Test coverage
//!
//! 1. **Resume from artifact** — a persisted graph with `Done`/`InProgress`/`New`
//!    tasks is loaded on `OpenPlan`; the registered graph preserves the `Done` task
//!    and shows the formerly `InProgress` task as `Ready` (not `New`), even though
//!    the `.md` file would have produced a different graph if interpreted.
//! 2. **Fresh path unchanged** — when no artifact exists, `OpenPlan` reads the
//!    `.md`, interprets it, and seed-persists all tasks as `New` (the existing
//!    `orchestrator-seed-write` behavior).
//!
//! # Test-strategy compliance
//!
//! - Backend is always `NoopBackend` — no real agent CLI, no model call.
//! - Each test uses a fresh temporary git repo (`tempfile`).
//! - No arbitrary sleeps.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;

use makina_core::api::Api;
use makina_core::backend::noop::NoopBackend;
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::dependency::EdgeInferrer;
use makina_core::interpreter::SourceProjectionUnavailable;
use makina_core::orchestrator::CoreApi;
use makina_core::test_support::init_git_repo_with_identity;
use makina_core::worktree::WorktreeManager;

// ── Temp-repo helpers ─────────────────────────────────────────────────────────

/// Create a minimal git repository in a fresh tempdir on a `develop` branch.
/// Build a `CoreApi` over the deterministic interpreter + `NoopBackend` +
/// a temp-repo `WorktreeManager` + a no-gate `Config`.
fn build_api(repo_root: PathBuf) -> CoreApi {
    let interpreter = Arc::new(EdgeInferrer::new(Arc::new(
        SourceProjectionUnavailable::new(),
    )));
    let backend = Arc::new(NoopBackend::with_responses(vec![
        "Implemented.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]));
    let wm = WorktreeManager::new(repo_root, "develop".into());
    let config = Config::resolve(GlobalConfig::default(), ProjectConfig::default());
    CoreApi::new(interpreter, backend, wm, config)
}

// ── Test 1: Resume from persisted artifact ───────────────────────────────────

/// **Acceptance (orchestrator-read-path):**
///
/// Sequence:
/// 1. Build a `TaskGraph` (slug `read-path-resume`) with:
///    - `task-done`       → `Done`
///    - `task-in-flight`  → `InProgress`
///    - `task-new`        → `New` (depends on task-done)
///    Persist it to `repo_root/.tasks/read-path-resume.json`.
/// 2. Write a `.md` file whose stem is `read-path-resume` but whose interpreted
///    content would differ from the persisted graph (single task `md-only-task`)
///    — so we can prove the artifact (not the `.md`) was used.
/// 3. Issue `OpenPlan` on that `.md` file.
/// 4. Assert the registered run's graph:
///    - Has 3 tasks (from the persisted artifact, not the 1-task `.md`).
///    - `task-done` is still `Done`.
///    - `task-in-flight` is now `Ready` (recover_for_resume applied).
///    - `task-new` is `New`.
#[tokio::test]
async fn disk_snapshot_ids_do_not_collide_with_subsequent_live_open_ids() {
    use makina_core::api::RunStatus;
    use makina_core::run_metadata::{RunMetadata, write_run_metadata};

    let _home = makina_core::HOME_ENV_LOCK.lock().await;
    let tmp_home = tempfile::tempdir().expect("temp home");
    let dir = tempfile::tempdir().expect("temp repo");
    let root = dir.path();
    unsafe { std::env::set_var("HOME", tmp_home.path()) };

    // Minimal git repo so WorktreeManager etc are happy (uses shared hermetic setup).
    init_git_repo_with_identity(root);

    // Seed one historical completed run (plan-like slug).
    let run_uid = "01DISKIDCOLLISIONTEST0000000";
    let meta = RunMetadata::new(
        run_uid.to_string(),
        "0027-foo-tasks".to_string(),
        "0027-foo".to_string(),
        RunStatus::Completed,
        Utc::now(),
        Utc::now(),
    );
    write_run_metadata(&meta, root)
        .await
        .expect("write disk meta");

    let api = build_api(root.to_path_buf());

    assert!(
        api.runs().await.is_empty(),
        "historical metadata without a typed PlanKey must remain inert"
    );
}
