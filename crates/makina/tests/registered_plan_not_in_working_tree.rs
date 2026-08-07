//! A registered plan that is not checked out into the working tree is still
//! listed, and is still runnable.
//!
//! Discovery resolves plans from three sources: the working tree, the
//! configured base revision, and retained `refs/heads/plan/*` registration
//! refs. So a plan the planner authored and registered — whose branch was never
//! merged into the checked-out branch — legitimately appears in the sidebar even
//! though `docs/plans/<id>/` does not exist on disk. That is the model: the plan
//! branch is authoritative and the run executes in its own worktree.
//!
//! The TUI's project router did not agree. It `canonicalize`d the plan directory
//! against the project root and failed when it was absent, making it strictly
//! stricter than the orchestrator it delegates to. The result was a plan that
//! could be seen but never started, reporting:
//!
//! ```text
//! Could not start 0001-fsm-cli-workspace: invalid command: cannot resolve plan
//! directory <root>/docs/plans/0001-fsm-cli-workspace: No such file or directory
//! ```
//!
//! This test reproduces exactly that shape: register a plan, remove it from the
//! working tree, and require that it both still discovers and still opens.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use makina_core::orchestrator::AuthoringCoordinator;
use makina_core::plan::{
    FilesystemPlanFileSource, PlanCandidate, PlanKey, PlanReservations, load_plan,
};
use makina_core::repository_lease::RepositoryLeaseRegistry;

fn git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A repository whose plan bundle has been registered onto `refs/heads/plan/*`
/// and then removed from the working tree — the state a planner leaves behind
/// when its branch is not merged into the checked-out branch.
fn repo_with_plan_only_on_a_ref() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "develop"]);
    git(repo.path(), &["config", "user.email", "t@example.invalid"]);
    git(repo.path(), &["config", "user.name", "Test"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../makina-core/tests/fixtures/plan-bundles/valid/0049-Sample");
    copy_tree(&fixture, &repo.path().join("docs/plans/0049-Sample"));
    fs::write(
        repo.path().join("docs/plans/STATUS.md"),
        "# Plans\n\n| Plan | Title | Status | Progress | Outcome | Link |\n|---|---|---|---|---|---|\n",
    )
    .unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "plan bundle"]);
    repo
}

#[tokio::test]
async fn a_registered_plan_absent_from_the_working_tree_discovers_and_opens() {
    let _home_guard = makina_core::HOME_ENV_LOCK.lock().await;
    let state_home = tempfile::tempdir().unwrap();
    let previous_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", state_home.path()) };

    let repo = repo_with_plan_only_on_a_ref();
    let key = PlanKey::parse("docs/plans/0049-Sample").unwrap();
    let base = {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };

    // Register the plan onto refs/heads/plan/0049-Sample.
    let source = FilesystemPlanFileSource::new(repo.path(), Some(base.clone())).unwrap();
    let PlanCandidate::Plan(plan) =
        load_plan(&source, key.clone(), &PlanReservations::default()).unwrap()
    else {
        panic!("fixture must be a plan candidate");
    };
    let coordinator = AuthoringCoordinator::new(
        repo.path().to_path_buf(),
        "develop".into(),
        Arc::new(RepositoryLeaseRegistry::new()),
    );
    coordinator
        .publish_candidate(
            key.clone(),
            base.clone(),
            plan.source_digest.to_string(),
            true,
        )
        .await
        .expect("registration must succeed");

    // Now remove it from the working tree and commit that removal: the plan
    // lives ONLY on its registration ref, exactly as in the reported repository.
    git(repo.path(), &["rm", "-r", "-q", "docs/plans/0049-Sample"]);
    git(
        repo.path(),
        &["commit", "-qm", "drop plan from working tree"],
    );
    assert!(
        !repo.path().join("docs/plans/0049-Sample").exists(),
        "precondition: the plan is gone from the working tree"
    );

    // 1. It is still discovered — this is why the sidebar lists it.
    let discovered = makina_core::orchestrator::discover_plans(repo.path());
    assert!(
        discovered.iter().any(|entry| entry.key == key),
        "a registered plan must still be discovered from its plan ref; got {:?}",
        discovered.iter().map(|e| &e.key).collect::<Vec<_>>()
    );

    // 2. And it must still be loadable, so opening it is not a dead end.
    makina_core::orchestrator::load_authoritative_plan(repo.path(), &key)
        .expect("a registered plan must load from its ref even when absent on disk");

    unsafe {
        match previous_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
