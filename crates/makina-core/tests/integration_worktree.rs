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
async fn integration_workspace_succeeds_without_home() {
    // With the in-repo state layout, $HOME is no longer required.
    // create_integration_workspace must succeed even when HOME is unset.
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::remove_var("HOME") };
    let repo = setup_temp_repo();
    let manager = WorktreeManager::new(repo.path().to_owned(), "develop".into());
    let workspace = manager
        .create_integration_workspace("0048-isolation", "01NOHOME")
        .await
        .expect("must succeed without HOME (in-repo state)");
    assert!(
        workspace.path.starts_with(repo.path().join(".makina")),
        "integration workspace must be under repo/.makina, got {}",
        workspace.path.display()
    );
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

/// A plan branch left checked out by a finished generation must not make the
/// plan unstartable.
///
/// A git branch lives in one worktree at a time, so a `generated-*` workspace
/// still attached to `plan/{slug}` made the very plan it had just registered
/// impossible to run:
///
/// ```text
/// failed to create integration workspace: git command failed: attach
/// integration workspace to retained plan/0001-fsm-cli-workspace
/// stderr: fatal: 'plan/0001-fsm-cli-workspace' is already used by worktree at
/// '.../.makina/runs/generated-4f0faab18096/integration'
/// ```
///
/// A clean holder under this project's own run state carries nothing the branch
/// ref does not, so the run reclaims it and proceeds.
#[tokio::test]
async fn a_clean_makina_workspace_holding_the_plan_branch_is_reclaimed() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let repo = setup_temp_repo();
    let manager = WorktreeManager::new(repo.path().to_owned(), "develop".into());

    // A prior generation: registers the plan ref and stays attached to it.
    let generated = manager
        .create_integration_workspace("0001-demo", "generated-abc123")
        .await
        .unwrap();
    let expected_base = git(repo.path(), &["rev-parse", "develop"]);
    git(
        &generated.path,
        &["commit", "--allow-empty", "-m", "registration"],
    );
    let candidate = git(&generated.path, &["rev-parse", "HEAD"]);
    manager
        .publish_registration(&generated, candidate.trim(), expected_base.trim())
        .await
        .unwrap();
    assert_eq!(
        git(&generated.path, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "plan/0001-demo",
        "precondition: the generation workspace holds the plan branch"
    );

    // The run now needs that same branch for its own workspace.
    let run = manager
        .create_integration_workspace("0001-demo", "01RECLAIM")
        .await
        .expect("a clean holder must be reclaimed, not fatal");
    assert_eq!(
        git(&run.path, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "plan/0001-demo",
        "the run's workspace must end up on the plan branch"
    );
    // Reclaim frees the branch; it does not delete. Removing the scratch
    // directory is `release_integration_workspace`'s job on the success path.
    assert!(
        generated.path.exists(),
        "reclaim must not delete the holder: {}",
        generated.path.display()
    );
    assert_eq!(
        git(&generated.path, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "HEAD",
        "the holder must be detached, no longer attached to the plan branch"
    );

    unsafe {
        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}

/// A holder carrying uncommitted or untracked work is still reclaimed — and
/// keeps every one of those files.
///
/// This is the case that actually blocks people: a `generated-*` workspace is
/// reused across authoring attempts off the same base, so it routinely holds
/// untracked leftovers from an abandoned bundle. Gating reclaim on cleanliness
/// would leave exactly those repositories stuck. Detaching frees the branch
/// without judging anything disposable.
#[tokio::test]
async fn a_dirty_workspace_holding_the_plan_branch_is_detached_not_destroyed() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let repo = setup_temp_repo();
    let manager = WorktreeManager::new(repo.path().to_owned(), "develop".into());

    let generated = manager
        .create_integration_workspace("0002-dirty", "generated-def456")
        .await
        .unwrap();
    let expected_base = git(repo.path(), &["rev-parse", "develop"]);
    git(
        &generated.path,
        &["commit", "--allow-empty", "-m", "registration"],
    );
    let candidate = git(&generated.path, &["rev-parse", "HEAD"]);
    manager
        .publish_registration(&generated, candidate.trim(), expected_base.trim())
        .await
        .unwrap();
    // An abandoned authoring attempt's bundle, never added to the index.
    std::fs::create_dir_all(generated.path.join("docs/plans/0002-abandoned")).unwrap();
    std::fs::write(
        generated.path.join("docs/plans/0002-abandoned/SCOPE.md"),
        "unsaved work\n",
    )
    .unwrap();

    let run = manager
        .create_integration_workspace("0002-dirty", "01DIRTY")
        .await
        .expect("a holder with untracked work must still be reclaimed");
    assert_eq!(
        git(&run.path, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "plan/0002-dirty",
        "the run's workspace must end up on the plan branch"
    );
    assert!(
        generated
            .path
            .join("docs/plans/0002-abandoned/SCOPE.md")
            .exists(),
        "every untracked file must survive the reclaim"
    );
    assert_eq!(
        git(&generated.path, &["rev-parse", "HEAD"]).trim(),
        candidate.trim(),
        "the detached holder must still sit on the same commit"
    );

    unsafe {
        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}

/// A worktree the operator made themselves is never removed from under them,
/// even when clean — Makina only reclaims its own run state.
#[tokio::test]
async fn an_operator_worktree_holding_the_plan_branch_is_never_reclaimed() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let repo = setup_temp_repo();
    let manager = WorktreeManager::new(repo.path().to_owned(), "develop".into());

    // The operator checks the plan branch out wherever they like.
    git(repo.path(), &["branch", "plan/0003-mine", "develop"]);
    let elsewhere = tempfile::tempdir().unwrap();
    let operator_worktree = elsewhere.path().join("mine");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            operator_worktree.to_str().unwrap(),
            "plan/0003-mine",
        ],
    );

    let error = manager
        .create_integration_workspace("0003-mine", "01OPERATOR")
        .await
        .expect_err("an operator's worktree must not be reclaimed");
    let rendered = error.to_string();
    assert!(
        rendered.contains("outside this project's Makina run state"),
        "the failure must say whose worktree it declined to touch; got: {rendered}"
    );
    assert!(
        operator_worktree.exists(),
        "the operator's worktree must survive"
    );

    unsafe {
        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}

/// Releasing a workspace frees its plan branch for the next one.
///
/// This is the source-side half of the fix: registration publishes the plan ref
/// and then releases its scratch workspace, so nothing is left holding the
/// branch in the first place. (The reclaim path above is the recovery half, for
/// repositories already in that state and for interrupted runs.)
#[tokio::test]
async fn releasing_a_workspace_frees_the_plan_branch() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    let repo = setup_temp_repo();
    let manager = WorktreeManager::new(repo.path().to_owned(), "develop".into());

    let generated = manager
        .create_integration_workspace("0004-release", "generated-999999")
        .await
        .unwrap();
    let expected_base = git(repo.path(), &["rev-parse", "develop"]);
    git(
        &generated.path,
        &["commit", "--allow-empty", "-m", "registration"],
    );
    let candidate = git(&generated.path, &["rev-parse", "HEAD"]);
    manager
        .publish_registration(&generated, candidate.trim(), expected_base.trim())
        .await
        .unwrap();

    manager.release_integration_workspace(&generated).await;

    assert!(
        !generated.path.exists(),
        "the released workspace must be gone"
    );
    // The registration survives its workspace — the ref is the durable evidence.
    assert_eq!(
        git(repo.path(), &["rev-parse", "plan/0004-release"]).trim(),
        candidate.trim(),
        "releasing the workspace must not disturb the published plan ref"
    );
    // And the branch is free, so the next worktree can take it.
    let worktrees = git(repo.path(), &["worktree", "list"]);
    assert!(
        !worktrees.contains("generated-999999"),
        "git must no longer register the released worktree; got:\n{worktrees}"
    );

    unsafe {
        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
