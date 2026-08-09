use makina_core::actors::supervisor::{
    FootprintChange, enforce_authored_footprint, enforce_task_branch_footprint, parse_name_status_z,
};
use makina_core::plan::AuthoredTaskStatus;
use makina_core::plan::{RepoChange, RepoPattern, TaskKind, parse_generated_repo_pattern};
use makina_core::task::{
    AuthoredSeedEvidence, AuthoredSeedOutcome, TaskState, authored_seed_is_dispatchable,
    authored_seed_satisfies_dependency, seed_authored_state,
};

#[test]
fn authored_statuses_are_seeded_exhaustively_and_fail_closed() {
    let none = AuthoredSeedEvidence::default();
    assert_eq!(
        seed_authored_state(AuthoredTaskStatus::Planned, false, none),
        AuthoredSeedOutcome::Seeded(TaskState::New)
    );
    assert_eq!(
        seed_authored_state(AuthoredTaskStatus::Planned, true, none),
        AuthoredSeedOutcome::Gated
    );
    assert_eq!(
        AuthoredSeedOutcome::Gated.runtime_state(),
        Some(TaskState::Gated)
    );
    assert_eq!(
        seed_authored_state(AuthoredTaskStatus::InProgress, false, none),
        AuthoredSeedOutcome::NeedsInProgressReconciliation
    );
    assert_eq!(
        seed_authored_state(AuthoredTaskStatus::Done, false, none),
        AuthoredSeedOutcome::NeedsLandingVerification
    );
    assert_eq!(
        seed_authored_state(AuthoredTaskStatus::Blocked, false, none),
        AuthoredSeedOutcome::Blocked
    );
    assert_eq!(
        AuthoredSeedOutcome::Blocked.runtime_state(),
        Some(TaskState::Blocked)
    );
    assert_eq!(
        seed_authored_state(AuthoredTaskStatus::Dropped, false, none),
        AuthoredSeedOutcome::Dropped
    );
    assert_eq!(
        AuthoredSeedOutcome::Dropped.runtime_state(),
        Some(TaskState::Dropped)
    );
}

#[test]
fn only_external_live_and_landing_evidence_seed_active_or_done() {
    assert_eq!(
        seed_authored_state(
            AuthoredTaskStatus::InProgress,
            false,
            AuthoredSeedEvidence {
                matching_live_driver: true,
                verified_landing: false
            },
        ),
        AuthoredSeedOutcome::Seeded(TaskState::InProgress)
    );
    let done = seed_authored_state(
        AuthoredTaskStatus::Done,
        false,
        AuthoredSeedEvidence {
            matching_live_driver: false,
            verified_landing: true,
        },
    );
    assert_eq!(done, AuthoredSeedOutcome::Seeded(TaskState::Done));
    assert!(authored_seed_satisfies_dependency(done));
}

#[test]
fn gates_and_drops_never_dispatch_and_drops_do_not_unlock_dependents() {
    for outcome in [AuthoredSeedOutcome::Gated, AuthoredSeedOutcome::Dropped] {
        assert!(!authored_seed_is_dispatchable(outcome));
        assert!(!authored_seed_satisfies_dependency(outcome));
    }
    assert!(authored_seed_is_dispatchable(AuthoredSeedOutcome::Seeded(
        TaskState::New
    )));
}

#[test]
fn review_and_pre_phase_a_footprint_policy_is_fail_closed() {
    let ordinary = [RepoPattern::Glob("src/**".into())];
    assert!(
        enforce_authored_footprint(
            &ordinary,
            &[FootprintChange {
                path: "src/lib.rs".into(),
                change: RepoChange::Added,
                ordinary_file_result: true,
            }]
        )
        .is_ok()
    );
    assert!(
        enforce_authored_footprint(
            &ordinary,
            &[FootprintChange {
                path: "tests/hidden.rs".into(),
                change: RepoChange::Added,
                ordinary_file_result: true,
            }]
        )
        .is_err()
    );
    assert!(
        enforce_authored_footprint(
            &[RepoPattern::Glob("docs/**".into())],
            &[FootprintChange {
                path: "docs/plans/STATUS.md".into(),
                change: RepoChange::ModifiedOrdinaryFile,
                ordinary_file_result: true,
            }]
        )
        .is_err()
    );
}

#[test]
fn tracked_makina_exceptions_enforce_exact_status_and_result_type() {
    let config = [RepoPattern::TrackedMakinaConfig {
        validation_base: "base".into(),
    }];
    let change = |change, ordinary_file_result| FootprintChange {
        path: ".makina/config.toml".into(),
        change,
        ordinary_file_result,
    };
    assert!(
        enforce_authored_footprint(&config, &[change(RepoChange::ModifiedOrdinaryFile, true)])
            .is_ok()
    );
    for rejected in [
        RepoChange::Added,
        RepoChange::Deleted,
        RepoChange::RenamedOrCopied,
        RepoChange::TypeChanged,
        RepoChange::Unmerged,
        RepoChange::Submodule,
    ] {
        assert!(enforce_authored_footprint(&config, &[change(rejected, true)]).is_err());
    }
    assert!(
        enforce_authored_footprint(&config, &[change(RepoChange::ModifiedOrdinaryFile, false)])
            .is_err()
    );

    let deletion = [RepoPattern::TrackedMakinaDeletion {
        path: ".makina/tasks/0005-tui-ingestion-responsiveness-tasks.json".into(),
        validation_base: "base".into(),
    }];
    assert!(
        enforce_authored_footprint(
            &deletion,
            &[FootprintChange {
                path: ".makina/tasks/0005-tui-ingestion-responsiveness-tasks.json".into(),
                change: RepoChange::Deleted,
                ordinary_file_result: false,
            }]
        )
        .is_ok()
    );
    for rejected in [
        RepoChange::Added,
        RepoChange::ModifiedOrdinaryFile,
        RepoChange::RenamedOrCopied,
        RepoChange::TypeChanged,
        RepoChange::Unmerged,
        RepoChange::Submodule,
    ] {
        assert!(
            enforce_authored_footprint(
                &deletion,
                &[FootprintChange {
                    path: ".makina/tasks/0005-tui-ingestion-responsiveness-tasks.json".into(),
                    change: rejected,
                    ordinary_file_result: true,
                }]
            )
            .is_err()
        );
    }
    assert!(
        parse_generated_repo_pattern(
            ".makina/**",
            TaskKind::Chore,
            std::path::Path::new("docs/plans/0048-example/tasks/0504-clean.md")
        )
        .is_err(),
        "the exact tracked deletion exception must never become a writable glob"
    );
    for path in [
        ".makina/.gitignore",
        ".makina/tasks/0005-tui-ingestion-responsiveness-tasks.json",
        ".makina/tasks/0008-gate-sandboxing-tasks.json",
    ] {
        assert!(matches!(
            parse_generated_repo_pattern(
                path,
                TaskKind::Chore,
                std::path::Path::new("docs/plans/0048-example/tasks/0504-clean.md")
            ),
            Ok(RepoPattern::TrackedMakinaDeletionCandidate(candidate)) if candidate == path
        ));
    }
}

#[test]
fn nul_name_status_parser_retains_both_rename_and_copy_paths() {
    let ordinary = ["new name.rs".to_string()].into_iter().collect();
    let submodules = ["vendor/sub".to_string()].into_iter().collect();
    let changes = parse_name_status_z(
        b"R100\0old name.rs\0new name.rs\0C75\0source.rs\0copy.rs\0M\0vendor/sub\0",
        &ordinary,
        &submodules,
    )
    .unwrap();
    assert_eq!(changes.len(), 5);
    assert_eq!(changes[0].path, "old name.rs");
    assert_eq!(changes[1].path, "new name.rs");
    assert_eq!(changes[4].change, RepoChange::Submodule);
}

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn commit_all(repo: &std::path::Path, message: &str) -> String {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-m", message]);
    git(repo, &["rev-parse", "HEAD"])
}

fn repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "user.name", "Test"]);
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/a.rs"), "base\n").unwrap();
    std::fs::write(repo.path().join(":(glob)magic.rs"), "base\n").unwrap();
    std::fs::create_dir_all(repo.path().join(".makina")).unwrap();
    std::fs::write(repo.path().join(".makina/config.toml"), "x=1\n").unwrap();
    std::fs::write(repo.path().join(".makina/legacy.toml"), "old\n").unwrap();
    commit_all(repo.path(), "base");
    repo
}

#[tokio::test]
async fn git_footprint_uses_recorded_base_and_rechecks_pre_phase_a() {
    let repo = repo();
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    git(repo.path(), &["switch", "-c", "task"]);
    std::fs::write(repo.path().join("src/a.rs"), "task\n").unwrap();
    commit_all(repo.path(), "declared");
    let touches = [makina_core::task::AuthoredRepoPattern::Glob(
        "src/**".into(),
    )];
    let id = makina_core::task::TaskId::new("task");
    enforce_task_branch_footprint(repo.path(), &id, "task", &touches, &base)
        .await
        .unwrap();

    std::fs::write(repo.path().join("undeclared.txt"), "late\n").unwrap();
    commit_all(repo.path(), "mutation between review and phase A");
    let error = enforce_task_branch_footprint(repo.path(), &id, "task", &touches, &base)
        .await
        .unwrap_err();
    assert!(error.contains("undeclared.txt"));
}

#[tokio::test]
async fn literal_pathspec_and_tracked_exceptions_are_enforced_from_git() {
    let repo = repo();
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    git(repo.path(), &["switch", "-c", "task"]);
    std::fs::write(repo.path().join(":(glob)magic.rs"), "changed\n").unwrap();
    std::fs::write(repo.path().join(".makina/config.toml"), "x=2\n").unwrap();
    std::fs::remove_file(repo.path().join(".makina/legacy.toml")).unwrap();
    commit_all(repo.path(), "typed changes");
    let touches = [
        makina_core::task::AuthoredRepoPattern::Path(":(glob)magic.rs".into()),
        makina_core::task::AuthoredRepoPattern::TrackedMakinaConfig {
            validation_base: "immutable-validation-provenance".into(),
        },
        makina_core::task::AuthoredRepoPattern::TrackedMakinaDeletion {
            path: ".makina/legacy.toml".into(),
            validation_base: "different-and-still-valid".into(),
        },
    ];
    enforce_task_branch_footprint(
        repo.path(),
        &makina_core::task::TaskId::new("task"),
        "task",
        &touches,
        &base,
    )
    .await
    .unwrap();
}

/// A rejection names a bounded number of paths, and says what a directory of
/// them usually is.
///
/// The message is handed back to the developer as the correction to make, so it
/// becomes prompt. A task that committed a build directory produced one
/// violation per artifact: a 236KB wall listing 1676 paths, which is not a
/// correction anyone can act on — and which the developer could not have fixed
/// anyway, because rebuilding is what its own task asked for.
#[tokio::test]
async fn a_rejection_bounds_the_paths_it_names_and_explains_a_flood_of_them() {
    let repo = repo();
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    git(repo.path(), &["switch", "-c", "task"]);
    std::fs::create_dir_all(repo.path().join("target/debug")).unwrap();
    for index in 0..200 {
        std::fs::write(
            repo.path().join(format!("target/debug/artifact-{index}")),
            "build output\n",
        )
        .unwrap();
    }
    commit_all(repo.path(), "a build directory nobody declared");

    let touches = [makina_core::task::AuthoredRepoPattern::Glob(
        "src/**".into(),
    )];
    let error = enforce_task_branch_footprint(
        repo.path(),
        &makina_core::task::TaskId::new("task"),
        "task",
        &touches,
        &base,
    )
    .await
    .unwrap_err();

    assert!(
        error.contains("target/debug/artifact-0"),
        "the rejection must still name offending paths: {error}"
    );
    assert!(
        error.contains("and 188 more"),
        "and must say how many it left out: {error}"
    );
    assert!(
        error.contains(".gitignore"),
        "and what a whole directory of them usually means: {error}"
    );
    assert!(
        error.len() < 3_000,
        "a correction must be readable, not a wall: {} bytes",
        error.len()
    );
}

/// The correction says where the violation lives and how to undo it.
///
/// A returned task keeps committing on the same branch, so a path it touched
/// on its first attempt stays in the branch diff however clean the working
/// tree looks afterwards. A developer told only "this path is outside your
/// footprint" checks its tree, finds nothing, and says so — five times, until
/// the reviewer cap fails a task whose work had been approved on every round.
#[tokio::test]
async fn a_correction_says_the_footprint_spans_the_branch_and_how_to_undo_it() {
    let repo = repo();
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    git(repo.path(), &["switch", "-c", "task"]);

    // First attempt strays outside the footprint...
    std::fs::write(repo.path().join("src/lib.rs"), "strayed\n").unwrap();
    commit_all(repo.path(), "attempt 1");
    // ...and the next attempt leaves the tree clean without putting it back.
    std::fs::create_dir_all(repo.path().join("declared")).unwrap();
    std::fs::write(repo.path().join("declared/work.rs"), "declared\n").unwrap();
    commit_all(repo.path(), "attempt 2");

    let touches = [makina_core::task::AuthoredRepoPattern::Glob(
        "declared/**".into(),
    )];
    let error = enforce_task_branch_footprint(
        repo.path(),
        &makina_core::task::TaskId::new("task"),
        "task",
        &touches,
        &base,
    )
    .await
    .unwrap_err();

    assert!(
        error.contains("src/lib.rs"),
        "the stray path must be named: {error}"
    );
    assert!(
        error.contains("every commit this branch has made since"),
        "the correction must say the check spans the branch, not the tree: {error}"
    );
    assert!(
        error.contains(&base) && error.contains("git checkout"),
        "and must give the exact way to put it back: {error}"
    );
    assert!(
        error.starts_with("The work was approved"),
        "a correction follows an approval, and must not read as a rejection: {error}"
    );
}
