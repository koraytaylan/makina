use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use makina_core::api::{Api, Command, CommandOutcome, RunStatus};
use makina_core::audit::NoopAuditRegistry;
use makina_core::backend::noop::NoopBackend;
use makina_core::checkpoint::{
    CheckpointIdentity, checkpoint_path, persist_checkpoint_with_evidence,
};
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::dependency::EdgeInferrer;
use makina_core::interpreter::SourceProjectionUnavailable;
use makina_core::orchestrator::CoreApi;
use makina_core::plan::{FilesystemPlanFileSource, PlanCandidate, PlanKey, PlanReservations};
use makina_core::plan_runtime::ProjectedTaskGraph;
use makina_core::repository_lease::{
    RepositoryLeaseOperation, RepositoryLeaseOwner, RepositoryLeaseRegistry,
};
use makina_core::task::TaskState;
use makina_core::worktree::WorktreeManager;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn shared_core_apis_wait_before_running_and_busy_purge_does_not_mutate() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, _plan_dir) = fixture_repo();
    commit_fixture(repo.path());
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let leases = Arc::new(RepositoryLeaseRegistry::new());
    let first = build_api_with_leases(repo.path().to_owned(), Arc::clone(&leases));
    let second = Arc::new(build_api_with_leases(
        repo.path().to_owned(),
        Arc::clone(&leases),
    ));
    let key = PlanKey::parse("docs/plans/0049-Sample").unwrap();
    let CommandOutcome::RunOpened { run } = second
        .execute(Command::OpenPlan { plan_dir: key })
        .await
        .unwrap()
    else {
        panic!()
    };
    let blocker = leases
        .acquire(
            repo.path(),
            RepositoryLeaseOwner {
                plan_dir: "docs/plans/blocker".into(),
                run_uid: "first-api".into(),
                operation: RepositoryLeaseOperation::Run,
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let orphan = makina_core::paths::worktrees_dir(repo.path())
        .unwrap()
        .join("active-recovery");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("evidence"), "unchanged").unwrap();
    let busy = first
        .execute(Command::PurgeWorktrees {
            project_root: repo.path().to_owned(),
        })
        .await
        .unwrap();
    assert!(matches!(busy, CommandOutcome::RepositoryBusy { .. }));
    assert_eq!(fs::read(orphan.join("evidence")).unwrap(), b"unchanged");

    let runner = Arc::clone(&second);
    let start = tokio::spawn(async move { runner.execute(Command::StartRun { run }).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let view = second.run(run).await.unwrap();
            if matches!(view.status, RunStatus::WaitingForRepository { .. }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let checkpoint = checkpoint_path(
        repo.path(),
        &PlanKey::parse("docs/plans/0049-Sample").unwrap(),
    )
    .unwrap();
    fs::create_dir_all(checkpoint.parent().unwrap()).unwrap();
    fs::write(&checkpoint, "{queued-drift").unwrap();
    drop(blocker);
    let error = start.await.unwrap().unwrap_err().to_string();
    assert!(error.contains("malformed or unreadable"), "{error}");
    assert_eq!(second.run(run).await.unwrap().status, RunStatus::Pending);
    restore_home(old);
}

#[tokio::test]
async fn open_validates_plan_source_without_creating_a_checkpoint() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, _plan_dir) = fixture_repo();
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let api = build_api(repo.path().to_owned());
    let outcome = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap();
    let CommandOutcome::RunOpened { run } = outcome else {
        panic!()
    };
    assert_eq!(api.run(run).await.unwrap().tasks.len(), 1);
    assert!(
        !home.path().join(".makina").exists(),
        "read-only open created runtime state"
    );
    restore_home(old);
}

#[tokio::test]
async fn start_fails_before_running_when_external_state_disappears() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, _plan_dir) = fixture_repo();
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let api = build_api(repo.path().to_owned());
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    unsafe { std::env::remove_var("HOME") };
    let error = api
        .execute(Command::StartRun { run })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("external runtime state"), "{error}");
    restore_home(old);
}

#[tokio::test]
async fn malformed_checkpoint_blocks_start_even_when_open_source_was_valid() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, _plan_dir) = fixture_repo();
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let api = build_api(repo.path().to_owned());
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let key = makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap();
    let path = checkpoint_path(repo.path(), &key).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"{not-json").unwrap();
    let error = api
        .execute(Command::StartRun { run })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("malformed or unreadable"), "{error}");
    restore_home(old);
}

#[tokio::test]
async fn compatible_checkpoint_is_overlaid_only_when_start_applies_fresh_source() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, _plan_dir) = fixture_repo();
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let plan = load_fixture_plan(repo.path());
    let mut projected = ProjectedTaskGraph::from_document(&plan, chrono::Utc::now());
    projected.graph.tasks[0].state = TaskState::Gated;
    projected.graph.tasks[0].gate_iterations = 7;
    persist_checkpoint_with_evidence(
        repo.path(),
        CheckpointIdentity::from_plan(&plan),
        &projected.graph,
        vec![],
        vec![],
    )
    .await
    .unwrap();
    let api = build_api(repo.path().to_owned());
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    api.execute(Command::StartRun { run }).await.unwrap();
    let view = api.run(run).await.unwrap();
    assert_eq!(view.tasks[0].state, makina_core::api::TaskState::Gated);
    assert_eq!(view.tasks[0].gate_iterations, 7);
    restore_home(old);
}

#[tokio::test]
async fn mismatched_checkpoint_with_active_evidence_blocks_start() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, _plan_dir) = fixture_repo();
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let plan = load_fixture_plan(repo.path());
    let projected = ProjectedTaskGraph::from_document(&plan, chrono::Utc::now());
    let mut identity = CheckpointIdentity::from_plan(&plan);
    identity.executable_digest = "0".repeat(64);
    persist_checkpoint_with_evidence(
        repo.path(),
        identity,
        &projected.graph,
        vec!["refs/heads/task/recovery".into()],
        vec![],
    )
    .await
    .unwrap();
    let api = build_api(repo.path().to_owned());
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let error = api
        .execute(Command::StartRun { run })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("requires recovery"), "{error}");
    restore_home(old);
}

#[tokio::test]
async fn clean_mismatched_checkpoint_is_archived_before_start() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, _plan_dir) = fixture_repo();
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let plan = load_fixture_plan(repo.path());
    let projected = ProjectedTaskGraph::from_document(&plan, chrono::Utc::now());
    let mut identity = CheckpointIdentity::from_plan(&plan);
    identity.executable_digest = "0".repeat(64);
    persist_checkpoint_with_evidence(repo.path(), identity, &projected.graph, vec![], vec![])
        .await
        .unwrap();
    let checkpoint = checkpoint_path(repo.path(), &plan.key).unwrap();
    let api = build_api(repo.path().to_owned());
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    api.execute(Command::StartRun { run }).await.unwrap();
    assert!(!checkpoint.exists());
    assert!(
        fs::read_dir(checkpoint.parent().unwrap())
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains("archive"))
    );
    restore_home(old);
}

#[tokio::test]
async fn executable_edit_after_open_is_reprojected_at_start() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, plan_dir) = fixture_repo();
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let api = build_api(repo.path().to_owned());
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let task = plan_dir.join("tasks/0101-first-task.md");
    let source = fs::read_to_string(&task).unwrap();
    fs::write(
        &task,
        source.replace(
            "Implement the sample task.",
            "Implement the freshly reread sample task.",
        ),
    )
    .unwrap();
    api.execute(Command::StartRun { run }).await.unwrap();
    assert!(
        api.run(run).await.unwrap().tasks[0]
            .entry_text
            .contains("freshly reread")
    );
    restore_home(old);
}

#[tokio::test]
async fn bookkeeping_only_edit_keeps_checkpoint_compatible_but_reloads_source() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, plan_dir) = fixture_repo();
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let plan = load_fixture_plan(repo.path());
    let mut projected = ProjectedTaskGraph::from_document(&plan, chrono::Utc::now());
    projected.graph.tasks[0].state = TaskState::Gated;
    projected.graph.tasks[0].review_iterations = 9;
    persist_checkpoint_with_evidence(
        repo.path(),
        CheckpointIdentity::from_plan(&plan),
        &projected.graph,
        vec![],
        vec![],
    )
    .await
    .unwrap();
    let api = build_api(repo.path().to_owned());
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let status = plan_dir.join("STATUS.md");
    let text = fs::read_to_string(&status).unwrap();
    fs::write(
        status,
        text.replace("validate a sample bundle", "validate the sample bundle"),
    )
    .unwrap();
    api.execute(Command::StartRun { run }).await.unwrap();
    assert_eq!(api.run(run).await.unwrap().tasks[0].review_iterations, 9);
    restore_home(old);
}

#[tokio::test]
async fn malformed_source_blocks_start_even_with_a_valid_compatible_cache() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, plan_dir) = fixture_repo();
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let plan = load_fixture_plan(repo.path());
    let projected = ProjectedTaskGraph::from_document(&plan, chrono::Utc::now());
    persist_checkpoint_with_evidence(
        repo.path(),
        CheckpointIdentity::from_plan(&plan),
        &projected.graph,
        vec![],
        vec![],
    )
    .await
    .unwrap();
    let api = build_api(repo.path().to_owned());
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    fs::write(
        plan_dir.join("tasks/0101-first-task.md"),
        b"not frontmatter",
    )
    .unwrap();
    let error = api
        .execute(Command::StartRun { run })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("plan source became invalid"), "{error}");
    restore_home(old);
}

#[tokio::test]
async fn moved_plan_after_open_is_not_silently_attached() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, plan_dir) = fixture_repo();
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let api = build_api(repo.path().to_owned());
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    fs::rename(&plan_dir, repo.path().join("docs/plans/0050-Moved")).unwrap();
    let error = api
        .execute(Command::StartRun { run })
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("invalid") || error.contains("disappeared"),
        "{error}"
    );
    restore_home(old);
}

#[tokio::test]
async fn task_add_after_open_is_revalidated() {
    assert_task_tree_mutation_blocks(|plan| {
        fs::copy(
            plan.join("tasks/0101-first-task.md"),
            plan.join("tasks/0101-renamed.md"),
        )
        .unwrap();
    })
    .await;
}

#[tokio::test]
async fn task_remove_after_open_is_revalidated() {
    assert_task_tree_mutation_blocks(|plan| {
        fs::remove_file(plan.join("tasks/0101-first-task.md")).unwrap();
    })
    .await;
}

#[tokio::test]
async fn task_move_after_open_is_revalidated() {
    assert_task_tree_mutation_blocks(|plan| {
        fs::rename(
            plan.join("tasks/0101-first-task.md"),
            plan.join("tasks/0101-renamed.md"),
        )
        .unwrap();
    })
    .await;
}

async fn assert_task_tree_mutation_blocks(mutate: impl FnOnce(&Path)) {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, plan_dir) = fixture_repo();
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let api = build_api(repo.path().to_owned());
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    mutate(&plan_dir);
    let error = api
        .execute(Command::StartRun { run })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("plan source became invalid"), "{error}");
    restore_home(old);
}

#[cfg(unix)]
#[tokio::test]
async fn external_home_symlink_into_repository_blocks_start() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, _plan_dir) = fixture_repo();
    let links = tempfile::tempdir().unwrap();
    let home_link = links.path().join("home");
    std::os::unix::fs::symlink(repo.path(), &home_link).unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", &home_link) };
    let api = build_api(repo.path().to_owned());
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let error = api
        .execute(Command::StartRun { run })
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("runtime state root")
            || error.contains("checkpoint location is unavailable"),
        "{error}"
    );
    restore_home(old);
}

#[cfg(unix)]
#[tokio::test]
async fn unwritable_external_root_blocks_before_run_state_mutation() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, _plan_dir) = fixture_repo();
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let api = build_api(repo.path().to_owned());
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    fs::set_permissions(home.path(), fs::Permissions::from_mode(0o500)).unwrap();
    let result = api.execute(Command::StartRun { run }).await;
    fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let error = result.unwrap_err().to_string();
    assert!(error.contains("durably writable"), "{error}");
    assert_eq!(
        api.run(run).await.unwrap().status,
        makina_core::api::RunStatus::Pending
    );
    restore_home(old);
}

#[tokio::test]
async fn real_production_named_stale_task_branch_retains_mismatched_checkpoint() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, _plan_dir) = fixture_repo();
    commit_fixture(repo.path());
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let plan = load_fixture_plan(repo.path());
    let projected = ProjectedTaskGraph::from_document(&plan, chrono::Utc::now());
    let mut identity = CheckpointIdentity::from_plan(&plan);
    identity.executable_digest = "0".repeat(64);
    persist_checkpoint_with_evidence(repo.path(), identity, &projected.graph, vec![], vec![])
        .await
        .unwrap();
    let checkpoint = checkpoint_path(repo.path(), &plan.key).unwrap();
    let short = makina_core::paths::short_worktree_name("0049-Sample", "first-task");
    git(repo.path(), &["branch", &format!("task/{short}"), "HEAD"]);
    let api = build_api(repo.path().to_owned());
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let error = api
        .execute(Command::StartRun { run })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("requires recovery"), "{error}");
    assert!(checkpoint.exists(), "recovery checkpoint was archived");
    restore_home(old);
}

#[tokio::test]
async fn real_registered_production_worktree_retains_mismatched_checkpoint() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let (repo, _plan_dir) = fixture_repo();
    commit_fixture(repo.path());
    let home = tempfile::tempdir().unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let plan = load_fixture_plan(repo.path());
    let projected = ProjectedTaskGraph::from_document(&plan, chrono::Utc::now());
    let mut identity = CheckpointIdentity::from_plan(&plan);
    identity.executable_digest = "0".repeat(64);
    persist_checkpoint_with_evidence(repo.path(), identity, &projected.graph, vec![], vec![])
        .await
        .unwrap();
    let checkpoint = checkpoint_path(repo.path(), &plan.key).unwrap();
    let short = makina_core::paths::short_worktree_name("0049-Sample", "first-task");
    let worktree = makina_core::paths::worktrees_dir(repo.path())
        .unwrap()
        .join(&short);
    fs::create_dir_all(worktree.parent().unwrap()).unwrap();
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "-b",
            &format!("task/{short}"),
            worktree.to_str().unwrap(),
            "HEAD",
        ],
    );
    let api = build_api(repo.path().to_owned());
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let error = api
        .execute(Command::StartRun { run })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("requires recovery"), "{error}");
    assert!(checkpoint.exists(), "recovery checkpoint was archived");
    assert!(
        worktree.exists(),
        "registered recovery worktree was removed"
    );
    restore_home(old);
}

fn commit_fixture(repo: &Path) {
    git(repo, &["add", "."]);
    git(
        repo,
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
}

fn git(repo: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn load_fixture_plan(repo: &Path) -> Box<makina_core::plan::PlanDocument> {
    let source = FilesystemPlanFileSource::new(repo, None).unwrap();
    let key = PlanKey::parse("docs/plans/0049-Sample").unwrap();
    let PlanCandidate::Plan(plan) =
        makina_core::plan::load_plan(&source, key, &PlanReservations::default()).unwrap()
    else {
        panic!()
    };
    plan
}

#[tokio::test]
async fn legacy_tasks_markdown_is_inert_and_cannot_open_an_executable_run() {
    let repo = tempfile::tempdir().unwrap();
    let task_list = repo.path().join("TASKS.md");
    fs::write(
        &task_list,
        "# Tasks\n\n### 0001 — Historical task\n\n- **Depends on:** none\n",
    )
    .unwrap();
    let api = build_api(repo.path().to_owned());
    let error = api
        .execute(Command::OpenPlan {
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("pre-cutover records"), "{error}");
}

fn build_api(repo_root: PathBuf) -> CoreApi {
    let interpreter = Arc::new(EdgeInferrer::new(Arc::new(
        SourceProjectionUnavailable::new(),
    )));
    let backend = Arc::new(NoopBackend::default());
    CoreApi::new(
        interpreter,
        backend,
        WorktreeManager::new(repo_root, "develop".into()),
        Config::resolve(GlobalConfig::default(), ProjectConfig::default()),
    )
}

fn build_api_with_leases(repo_root: PathBuf, leases: Arc<RepositoryLeaseRegistry>) -> CoreApi {
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
        leases,
    )
}

fn fixture_repo() -> (tempfile::TempDir, PathBuf) {
    let repo = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    let plan_dir = repo.path().join("docs/plans/0049-Sample");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/plan-bundles/valid/0049-Sample"),
        &plan_dir,
    );
    (repo, plan_dir)
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target)
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn restore_home(old: Option<std::ffi::OsString>) {
    if let Some(value) = old {
        unsafe { std::env::set_var("HOME", value) }
    } else {
        unsafe { std::env::remove_var("HOME") }
    }
}
