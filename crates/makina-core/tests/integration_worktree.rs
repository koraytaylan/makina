use makina_core::merge::{MergeOutcome, SquashMerger};
use makina_core::test_support::{run_git, setup_temp_repo};
use makina_core::worktree::WorktreeManager;

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let output = run_git(repo, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn snapshot(repo: &std::path::Path) -> (String, String, String, Vec<u8>) {
    let head = git(repo, &["symbolic-ref", "HEAD"]);
    let unstaged = git(repo, &["diff", "--binary"]);
    let staged = git(repo, &["diff", "--cached", "--binary"]);
    let untracked = std::fs::read(repo.join("operator-untracked.txt")).unwrap();
    (head, unstaged, staged, untracked)
}

#[tokio::test]
async fn private_integration_workspace_preserves_poisoned_operator_checkout() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let repo = setup_temp_repo();
    std::fs::write(repo.path().join("staged.txt"), "base\n").unwrap();
    git(repo.path(), &["add", "staged.txt"]);
    git(repo.path(), &["commit", "-m", "add poison fixture"]);
    std::fs::write(repo.path().join("README.md"), "operator unstaged\n").unwrap();
    std::fs::write(repo.path().join("staged.txt"), "operator staged\n").unwrap();
    git(repo.path(), &["add", "staged.txt"]);
    std::fs::write(
        repo.path().join("operator-untracked.txt"),
        b"operator bytes",
    )
    .unwrap();
    let before = snapshot(repo.path());

    let manager = WorktreeManager::new(repo.path().to_owned(), "develop".into());
    let workspace = manager
        .create_integration_workspace("0048-isolation", "01TEST0402")
        .await
        .unwrap();
    let expected_base = git(repo.path(), &["rev-parse", "develop"]);
    git(
        &workspace.path,
        &["commit", "--allow-empty", "-m", "Phase R candidate"],
    );
    let candidate = git(&workspace.path, &["rev-parse", "HEAD"]);
    manager
        .publish_registration(&workspace, candidate.trim(), expected_base.trim())
        .await
        .unwrap();
    git(&workspace.path, &["checkout", "-b", "task/0048-private"]);
    std::fs::write(workspace.path.join("integrated.txt"), "private\n").unwrap();
    git(&workspace.path, &["add", "integrated.txt"]);
    git(&workspace.path, &["commit", "-m", "task candidate"]);
    git(&workspace.path, &["checkout", &workspace.plan_branch]);
    assert!(matches!(
        SquashMerger::new(workspace.path.clone(), workspace.plan_branch.clone())
            .squash_merge("task/0048-private", "land task privately")
            .await
            .unwrap(),
        MergeOutcome::Merged { .. }
    ));

    assert_eq!(snapshot(repo.path()), before);
    assert_eq!(
        git(repo.path(), &["rev-parse", &workspace.plan_branch]),
        git(&workspace.path, &["rev-parse", "HEAD"])
    );
    if let Some(value) = old_home {
        unsafe { std::env::set_var("HOME", value) };
    } else {
        unsafe { std::env::remove_var("HOME") };
    }
}

#[tokio::test]
async fn integration_workspace_fails_before_git_mutation_without_external_state() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::remove_var("HOME") };
    let repo = setup_temp_repo();
    let before = git(repo.path(), &["show-ref"]);
    let manager = WorktreeManager::new(repo.path().to_owned(), "develop".into());
    assert!(
        manager
            .create_integration_workspace("0048-isolation", "01NOHOME")
            .await
            .is_err()
    );
    assert_eq!(git(repo.path(), &["show-ref"]), before);
    assert!(!repo.path().join(".makina/runs").exists());
    if let Some(value) = old_home {
        unsafe { std::env::set_var("HOME", value) };
    }
}

#[tokio::test]
async fn ambiguous_existing_integration_path_is_preserved() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let repo = setup_temp_repo();
    let path = makina_core::paths::run_dir(repo.path(), "01STALE")
        .unwrap()
        .join("integration");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("recovery.txt"), "retain exactly").unwrap();
    let manager = WorktreeManager::new(repo.path().to_owned(), "develop".into());
    assert!(
        manager
            .create_integration_workspace("0048-isolation", "01STALE")
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(path.join("recovery.txt")).unwrap(),
        "retain exactly"
    );
    assert!(
        git(repo.path(), &["branch", "--list", "plan/0048-isolation"])
            .trim()
            .is_empty()
    );
    if let Some(value) = old_home {
        unsafe { std::env::set_var("HOME", value) };
    }
}

#[tokio::test]
async fn checked_out_base_blocks_final_ref_advance_without_operator_mutation() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let repo = setup_temp_repo();
    std::fs::write(repo.path().join("operator-untracked.txt"), "poison").unwrap();
    let before = snapshot(repo.path());
    let base_before = git(repo.path(), &["rev-parse", "develop"]);
    let manager = WorktreeManager::new(repo.path().to_owned(), "develop".into());
    let workspace = manager
        .create_integration_workspace("0048-final", "01FINAL")
        .await
        .unwrap();
    git(
        &workspace.path,
        &["commit", "--allow-empty", "-m", "Phase R candidate"],
    );
    let candidate = git(&workspace.path, &["rev-parse", "HEAD"]);
    manager
        .publish_registration(&workspace, candidate.trim(), base_before.trim())
        .await
        .unwrap();
    std::fs::write(workspace.path.join("candidate.txt"), "candidate").unwrap();
    git(&workspace.path, &["add", "candidate.txt"]);
    git(&workspace.path, &["commit", "-m", "plan candidate"]);
    let error = SquashMerger::new(workspace.path, "develop".into())
        .final_squash(&workspace.plan_branch, "final candidate")
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        makina_core::merge::MergeError::BaseCheckedOut { .. }
    ));
    assert_eq!(git(repo.path(), &["rev-parse", "develop"]), base_before);
    assert_eq!(snapshot(repo.path()), before);
    if let Some(value) = old_home {
        unsafe { std::env::set_var("HOME", value) };
    }
}

#[tokio::test]
async fn detached_registration_candidate_is_reused_after_response_loss() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let repo = setup_temp_repo();
    let manager = WorktreeManager::new(repo.path().to_owned(), "develop".into());
    let workspace = manager
        .create_integration_workspace("0048-register", "01REGISTER")
        .await
        .unwrap();
    let base = git(repo.path(), &["rev-parse", "develop"]);
    git(
        &workspace.path,
        &["commit", "--allow-empty", "-m", "detached R"],
    );
    let candidate = git(&workspace.path, &["rev-parse", "HEAD"]);
    assert!(
        git(repo.path(), &["branch", "--list", &workspace.plan_branch])
            .trim()
            .is_empty()
    );

    manager
        .publish_registration(&workspace, candidate.trim(), base.trim())
        .await
        .unwrap();
    // Simulate loss of the first successful response.
    manager
        .publish_registration(&workspace, candidate.trim(), base.trim())
        .await
        .unwrap();
    assert_eq!(
        git(repo.path(), &["rev-parse", &workspace.plan_branch]).trim(),
        candidate.trim()
    );
    assert_eq!(
        git(&workspace.path, &["symbolic-ref", "--short", "HEAD"]).trim(),
        workspace.plan_branch
    );
    if let Some(value) = old_home {
        unsafe { std::env::set_var("HOME", value) };
    }
}

#[tokio::test]
async fn registration_base_race_retains_detached_candidate_without_plan_ref() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let repo = setup_temp_repo();
    let manager = WorktreeManager::new(repo.path().to_owned(), "develop".into());
    let workspace = manager
        .create_integration_workspace("0048-race", "01RACE")
        .await
        .unwrap();
    let expected = git(repo.path(), &["rev-parse", "develop"]);
    git(
        &workspace.path,
        &["commit", "--allow-empty", "-m", "detached R"],
    );
    let candidate = git(&workspace.path, &["rev-parse", "HEAD"]);
    std::fs::write(repo.path().join("race.txt"), "base moved").unwrap();
    git(repo.path(), &["add", "race.txt"]);
    git(repo.path(), &["commit", "-m", "move base"]);

    assert!(
        manager
            .publish_registration(&workspace, candidate.trim(), expected.trim())
            .await
            .is_err()
    );
    assert!(
        git(repo.path(), &["branch", "--list", &workspace.plan_branch])
            .trim()
            .is_empty()
    );
    assert_eq!(
        git(&workspace.path, &["rev-parse", "HEAD"]).trim(),
        candidate.trim()
    );
    if let Some(value) = old_home {
        unsafe { std::env::set_var("HOME", value) };
    }
}
