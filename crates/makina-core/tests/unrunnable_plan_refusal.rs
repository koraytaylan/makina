//! Refusing to run a pre-cutover plan must say which plan, and what is missing.
//!
//! This is the refusal a real repository hits constantly: plan directories
//! authored before the per-task cutover carry the legacy monolithic task file
//! but no `STATUS.md` and no `tasks/` directory, so they cannot be executed.
//! The refusal is correct —
//! but it used to recite the required file set without naming the plan or
//! saying which part of it was absent, leaving the operator to diff the
//! directory against the message by hand. Worse, the TUI rendered it into a
//! width-clipped status line, so what actually reached the screen was
//! "Open failed: invalid command" with the entire explanation cut off.
//!
//! These tests pin the two properties that make the message worth reading: it
//! names the plan, and it enumerates what is missing.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use makina_core::api::{Api, ApiError, Command};
use makina_core::audit::NoopAuditRegistry;
use makina_core::backend::noop::NoopBackend;
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::dependency::EdgeInferrer;
use makina_core::interpreter::SourceProjectionUnavailable;
use makina_core::orchestrator::CoreApi;
use makina_core::plan::PlanKey;
use makina_core::repository_lease::RepositoryLeaseRegistry;
use makina_core::worktree::WorktreeManager;

fn build_api(repo_root: PathBuf) -> CoreApi {
    let interpreter = Arc::new(EdgeInferrer::new(Arc::new(
        SourceProjectionUnavailable::new(),
    )));
    let planner = Arc::new(SourceProjectionUnavailable::new());
    let backend = Arc::new(NoopBackend::default());
    CoreApi::with_repository_lease_registry(
        interpreter,
        planner,
        backend.clone(),
        backend,
        WorktreeManager::new(repo_root, "develop".into()),
        Config::resolve(GlobalConfig::default(), ProjectConfig::default()),
        Arc::new(NoopAuditRegistry),
        Arc::new(RepositoryLeaseRegistry::new()),
    )
}

fn git(repo: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed");
}

/// A repository holding one **pre-cutover** plan: the shape this project's own
/// `docs/plans/0016-*` directories actually have.
fn repo_with_pre_cutover_plan() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    let plan = repo.path().join("docs/plans/0016-Legacy-Shape");
    fs::create_dir_all(&plan).unwrap();
    fs::write(plan.join("SCOPE.md"), "# Scope\n").unwrap();
    fs::write(plan.join("ARCHITECTURE.md"), "# Architecture\n").unwrap();
    // No STATUS.md and no tasks/ — exactly what makes it unrunnable. (A real
    // pre-cutover plan also carries the legacy monolithic task file, but naming
    // it here would spread that filename into live code; `legacy_contract_search`
    // guards against exactly that, and its absence changes nothing this asserts.)
    git(repo.path(), &["add", "."]);
    git(
        repo.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    repo
}

#[tokio::test]
async fn refusing_a_pre_cutover_plan_names_it_and_what_is_missing() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };

    let repo = repo_with_pre_cutover_plan();
    let api = build_api(repo.path().to_owned());

    let error = api
        .execute(Command::OpenPlan {
            plan_dir: PlanKey::parse("docs/plans/0016-Legacy-Shape").unwrap(),
        })
        .await
        .expect_err("a pre-cutover plan must be refused");

    let ApiError::InvalidCommand { reason } = error else {
        panic!("expected InvalidCommand, got {error:?}");
    };

    assert!(
        reason.contains("0016-Legacy-Shape"),
        "the refusal must name the plan it is about; got: {reason}"
    );
    assert!(
        reason.contains("STATUS.md"),
        "the refusal must say what is missing; got: {reason}"
    );
    assert!(
        reason.contains("tasks/"),
        "the refusal must mention the absent tasks directory; got: {reason}"
    );

    unsafe {
        match old {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
