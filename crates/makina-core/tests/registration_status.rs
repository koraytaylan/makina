use std::{fs, path::Path, sync::Arc};

use makina_core::{
    api::{Api, Command, CommandOutcome},
    backend::noop::NoopBackend,
    config::{Config, GlobalConfig, ProjectConfig},
    dependency::EdgeInferrer,
    interpreter::SourceProjectionUnavailable,
    orchestrator::{AuthoringCoordinator, CoreApi, GeneratedPlanBundle},
    plan::{
        FilesystemPlanFileSource, GitTreePlanFileSource, PlanCandidate, PlanKey, PlanReservations,
        load_plan,
    },
    worktree::WorktreeManager,
};
use std::collections::BTreeMap;

#[tokio::test]
async fn generate_command_publishes_direct_r_without_operator_files_or_run_side_effects() {
    use makina_core::api::{
        GeneratedInitialStatusBlueprint, GeneratedPlanBlueprint, GeneratedTaskBlueprint,
        GeneratedWorkstreamBlueprint,
    };
    let _home_guard = makina_core::HOME_ENV_LOCK.lock().await;
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "develop"]);
    git(
        repo.path(),
        &["config", "user.email", "generated@example.invalid"],
    );
    git(repo.path(), &["config", "user.name", "Generated Test"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    fs::create_dir_all(repo.path().join("docs/plans")).unwrap();
    fs::write(repo.path().join(".gitignore"), ".makina/\n").unwrap();
    fs::write(repo.path().join("docs/plans/STATUS.md"), "# Plans\n\n| Plan | Title | Status | Progress | Outcome | Link |\n|---|---|---|---|---|---|\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "base"]);
    let state_home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("HOME", state_home.path()) };
    let api = build_api(repo.path());
    let blueprint = GeneratedPlanBlueprint {
            slug: "generated-sample".into(),
            title: "Generated Sample".into(),
            scope: "## In scope\n\n- **0001 — Core.** Generate the bundle.".into(),
            architecture: "## 0001 — Core\n\nRender the canonical bundle.".into(),
            initial_status: GeneratedInitialStatusBlueprint {
                goal: "publish one complete generated plan.".into(),
                root_cause: "free-form output is not safely executable.".into(),
                approach: "render and validate typed documents.".into(),
                outcome: "the registered bundle is ready.".into(),
                last_updated: "2026-07-20".into(),
            },
            workstreams: vec![GeneratedWorkstreamBlueprint { id: "0001".into(), title: "Core".into() }],
            tasks: vec![GeneratedTaskBlueprint {
                sequence: "01".into(), id: "render-bundle".into(), title: "Render Bundle".into(),
                workstream: "0001".into(), kind: "task".into(), depends_on: vec![],
                touches: vec!["src/**".into()], gated: false,
                body: "# Render Bundle\n\nRender the bundle.\n\n**Steps:**\n\n1. Render it.\n\n- **Done when:** the generated plan is loader-valid.".into(),
            }],
    };
    let outcome = api
        .execute(Command::GeneratePlanBundle {
            blueprint: blueprint.clone(),
        })
        .await
        .unwrap();
    let CommandOutcome::PlanGenerated {
        plan_dir,
        registration_oid,
        report,
    } = outcome
    else {
        panic!("expected generated plan")
    };
    assert_eq!(
        plan_dir,
        PlanKey::parse("docs/plans/0001-generated-sample").unwrap()
    );
    assert!(report.diagnostics.is_empty());
    assert_eq!(
        output(repo.path(), &["rev-parse", &plan_dir.ref_name()]),
        registration_oid
    );
    let retry = api
        .execute(Command::GeneratePlanBundle { blueprint })
        .await
        .unwrap();
    let CommandOutcome::PlanGenerated {
        registration_oid: retried,
        ..
    } = retry
    else {
        panic!()
    };
    assert_eq!(
        retried, registration_oid,
        "response-loss retry must reuse R"
    );
    assert!(!repo.path().join(&plan_dir.relative_dir).exists());
    let status = output(repo.path(), &["status", "--porcelain"]);
    assert!(status.is_empty(), "working tree must stay clean: {status}");
    assert!(
        api.runs().await.is_empty(),
        "generation must not open or start a run"
    );
}

/// One rejection names every fault, across every task.
///
/// The author of a blueprint is an agent that gets one message back per
/// attempt. Bailing on the first bad field meant the same mistake repeated in
/// three tasks cost three regeneration rounds — and the `touches` grammar,
/// which is the easiest rule to get wrong, was checked one task at a time.
#[tokio::test]
async fn a_rejected_blueprint_reports_every_fault_in_one_message() {
    use makina_core::api::{
        GeneratedInitialStatusBlueprint, GeneratedPlanBlueprint, GeneratedTaskBlueprint,
        GeneratedWorkstreamBlueprint,
    };
    let _home_guard = makina_core::HOME_ENV_LOCK.lock().await;
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "develop"]);
    git(
        repo.path(),
        &["config", "user.email", "generated@example.invalid"],
    );
    git(repo.path(), &["config", "user.name", "Generated Test"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    fs::create_dir_all(repo.path().join("docs/plans")).unwrap();
    fs::write(repo.path().join(".gitignore"), ".makina/\n").unwrap();
    fs::write(repo.path().join("docs/plans/STATUS.md"), "# Plans\n\n| Plan | Title | Status | Progress | Outcome | Link |\n|---|---|---|---|---|---|\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "base"]);
    let state_home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("HOME", state_home.path()) };
    let api = build_api(repo.path());

    let task =
        |sequence: &str, id: &str, kind: &str, touches: Vec<String>| GeneratedTaskBlueprint {
            sequence: sequence.into(),
            id: id.into(),
            title: id.into(),
            workstream: "0001".into(),
            kind: kind.into(),
            depends_on: vec![],
            touches,
            gated: false,
            body: format!("# {id}\n\nWork.\n\n**Steps:**\n\n1. Do it.\n\n- **Done when:** done."),
        };
    let blueprint = GeneratedPlanBlueprint {
        slug: "multi-fault".into(),
        title: "Multi Fault".into(),
        scope: "## In scope\n\n- **0001 — Core.** Generate the bundle.".into(),
        architecture: "## 0001 — Core\n\nRender the canonical bundle.".into(),
        initial_status: GeneratedInitialStatusBlueprint {
            goal: "publish one plan.".into(),
            root_cause: "free-form output is not executable.".into(),
            approach: "render typed documents.".into(),
            outcome: "the bundle is ready.".into(),
            last_updated: "2026-07-20".into(),
        },
        workstreams: vec![GeneratedWorkstreamBlueprint {
            id: "0001".into(),
            title: "Core".into(),
        }],
        tasks: vec![
            // The reported failure: a glob with a `*` inside a segment.
            task("01", "first", "task", vec!["src/**/*.rs".into()]),
            // The same mistake again, plus an unsupported kind. Neither was
            // reachable before the first task's `touches` was fixed.
            task(
                "02",
                "second",
                "epic",
                vec!["crates/*/src/**/mod.rs".into()],
            ),
        ],
    };

    let error = api
        .execute(Command::GeneratePlanBundle { blueprint })
        .await
        .expect_err("the blueprint is invalid");
    let reported = error.to_string();

    for expected in [
        "tasks[0]",
        "src/**/*.rs",
        "tasks[1]",
        "crates/*/src/**/mod.rs",
        "kind `epic`",
    ] {
        assert!(
            reported.contains(expected),
            "{expected:?} must appear in the single rejection: {reported}",
        );
    }
    assert!(
        !repo.path().join("docs/plans/0001-multi-fault").exists(),
        "a rejected blueprint must leave nothing behind",
    );
}

#[tokio::test]
async fn generated_closed_bundle_registers_without_operator_materialization_and_reuses_r() {
    let _home_guard = makina_core::HOME_ENV_LOCK.lock().await;
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "develop"]);
    git(
        repo.path(),
        &["config", "user.email", "generated@example.invalid"],
    );
    git(repo.path(), &["config", "user.name", "Generated Test"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    fs::create_dir_all(repo.path().join("docs/plans")).unwrap();
    fs::write(repo.path().join("docs/plans/STATUS.md"), "# Plans\n\n| Plan | Title | Status | Progress | Outcome | Link |\n|---|---|---|---|---|---|\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "base"]);
    let base = output(repo.path(), &["rev-parse", "HEAD"]);
    let state_home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("HOME", state_home.path()) };
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plan-bundles/valid/0049-Sample");
    let mut files = BTreeMap::new();
    collect_bundle(&fixture, &fixture, &mut files);
    let digest_root = tempfile::tempdir().unwrap();
    git(digest_root.path(), &["init", "-q"]);
    for (path, bytes) in &files {
        let target = digest_root.path().join("docs/plans/0049-Sample").join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, bytes).unwrap();
    }
    let key = PlanKey::parse("docs/plans/0049-Sample").unwrap();
    let digest_source = FilesystemPlanFileSource::new(digest_root.path(), None).unwrap();
    let PlanCandidate::Plan(generated) =
        load_plan(&digest_source, key.clone(), &PlanReservations::default()).unwrap()
    else {
        panic!()
    };
    let bundle = GeneratedPlanBundle {
        key,
        expected_source_digest: generated.source_digest.to_string(),
        files,
    };
    let authoring = AuthoringCoordinator::new(
        repo.path().to_path_buf(),
        "develop".into(),
        Arc::new(makina_core::repository_lease::RepositoryLeaseRegistry::new()),
    );
    assert_eq!(
        authoring
            .start_authoring_session()
            .await
            .unwrap()
            .expected_base_oid,
        base
    );
    let first = authoring
        .publish_generated(bundle.clone(), base.clone())
        .await
        .unwrap();
    let second = authoring
        .publish_generated(bundle, base.clone())
        .await
        .unwrap();
    let (
        CommandOutcome::PlanRegistered {
            registration_oid: first,
        },
        CommandOutcome::PlanRegistered {
            registration_oid: second,
        },
    ) = (first, second)
    else {
        panic!()
    };
    assert_eq!(first, second);
    assert!(
        !repo.path().join("docs/plans/0049-Sample").exists(),
        "operator checkout was mutated"
    );
    assert_eq!(
        output(repo.path(), &["rev-parse", &format!("{first}^")]),
        base
    );
    let message = output(repo.path(), &["show", "-s", "--format=%B", &first]);
    assert!(message.contains("Makina-Source-Origin: generated"));
    assert!(
        output(
            repo.path(),
            &["show", &format!("{first}:docs/plans/STATUS.md")]
        )
        .contains("| 0049 |")
    );
}

fn collect_bundle(root: &Path, current: &Path, files: &mut BTreeMap<std::path::PathBuf, Vec<u8>>) {
    for entry in fs::read_dir(current).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            collect_bundle(root, &entry.path(), files);
        } else {
            files.insert(
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                fs::read(entry.path()).unwrap(),
            );
        }
    }
}

#[tokio::test]
async fn committed_bundle_registers_once_and_reuses_exact_r() {
    let _home_guard = makina_core::HOME_ENV_LOCK.lock().await;
    let repo = fixture_repo();
    let state_home = tempfile::tempdir().unwrap();
    // SAFETY: HOME_ENV_LOCK serializes in-process HOME mutation.
    unsafe { std::env::set_var("HOME", state_home.path()) };
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "source"]);
    let base = output(repo.path(), &["rev-parse", "HEAD"]);
    let key = PlanKey::parse("docs/plans/0049-Sample").unwrap();
    let source = GitTreePlanFileSource::new(repo.path(), &base).unwrap();
    let PlanCandidate::Plan(plan) =
        load_plan(&source, key.clone(), &PlanReservations::default()).unwrap()
    else {
        panic!("fixture is a plan")
    };
    let authoring = AuthoringCoordinator::new(
        repo.path().to_path_buf(),
        "develop".into(),
        Arc::new(makina_core::repository_lease::RepositoryLeaseRegistry::new()),
    );
    let first = authoring
        .publish_committed(key.clone(), base.clone(), plan.source_digest.to_string())
        .await
        .unwrap();
    let second = authoring
        .publish_committed(key, base.clone(), plan.source_digest.to_string())
        .await
        .unwrap();
    let (
        CommandOutcome::PlanRegistered {
            registration_oid: a,
        },
        CommandOutcome::PlanRegistered {
            registration_oid: b,
        },
    ) = (first, second)
    else {
        panic!("registration outcome")
    };
    assert_eq!(a, b);
    assert_eq!(output(repo.path(), &["rev-parse", "plan/0049-Sample"]), a);
    assert_eq!(output(repo.path(), &["rev-parse", &format!("{a}^")]), base);
    let message = output(repo.path(), &["show", "-s", "--format=%B", &a]);
    assert!(message.contains("Makina-Phase: plan-registration"));
    assert!(message.contains("Makina-Source-Origin: base"));
}

#[tokio::test]
async fn coordinator_reports_awaiting_commit_for_exact_working_candidate() {
    let _home_guard = makina_core::HOME_ENV_LOCK.lock().await;
    let repo = fixture_repo();
    let state_home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("HOME", state_home.path()) };
    git(repo.path(), &["add", "docs/plans/STATUS.md"]);
    git(repo.path(), &["commit", "-qm", "base"]);
    let base = output(repo.path(), &["rev-parse", "HEAD"]);
    let key = PlanKey::parse("docs/plans/0049-Sample").unwrap();
    let source = FilesystemPlanFileSource::new(repo.path(), Some(base.clone())).unwrap();
    let PlanCandidate::Plan(plan) =
        load_plan(&source, key.clone(), &PlanReservations::default()).unwrap()
    else {
        panic!("working candidate")
    };
    let authoring = AuthoringCoordinator::new(
        repo.path().to_path_buf(),
        "develop".into(),
        Arc::new(makina_core::repository_lease::RepositoryLeaseRegistry::new()),
    );
    let digest = plan.source_digest.to_string();
    assert!(matches!(
        authoring
            .publish_candidate(key.clone(), base.clone(), digest.clone(), false)
            .await
            .unwrap(),
        CommandOutcome::AwaitingCommit
    ));
    assert!(
        output(
            repo.path(),
            &["for-each-ref", "--format=%(refname)", "refs/heads/plan/"]
        )
        .is_empty()
    );
    fs::write(repo.path().join("unrelated-staged.txt"), "keep staged\n").unwrap();
    fs::write(
        repo.path().join("unrelated-untracked.txt"),
        "keep working\n",
    )
    .unwrap();
    git(repo.path(), &["add", "unrelated-staged.txt"]);
    let first = authoring
        .publish_candidate(key.clone(), base.clone(), digest.clone(), true)
        .await
        .unwrap();
    let second = authoring
        .publish_candidate(key, base, digest, true)
        .await
        .unwrap();
    assert!(matches!(
        (&first, &second),
        (
            CommandOutcome::PlanRegistered { registration_oid: a },
            CommandOutcome::PlanRegistered { registration_oid: b }
        ) if a == b
    ));
    let authored = output(repo.path(), &["rev-parse", "develop"]);
    let message = output(repo.path(), &["show", "-s", "--format=%B", &authored]);
    assert!(message.contains("Makina-Phase: plan-authoring"));
    assert_eq!(
        output(repo.path(), &["diff", "--cached", "--name-only"]),
        "unrelated-staged.txt"
    );
    assert!(repo.path().join("unrelated-untracked.txt").exists());
    let absent = std::process::Command::new("git")
        .args(["show", &format!("{authored}:unrelated-staged.txt")])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        !absent.status.success(),
        "authored commit must isolate unrelated staged files"
    );
}

#[tokio::test]
async fn stale_unconsumed_registration_is_archived_and_refreshed() {
    let _home_guard = makina_core::HOME_ENV_LOCK.lock().await;
    let repo = fixture_repo();
    let state_home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("HOME", state_home.path()) };
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "source"]);
    let key = PlanKey::parse("docs/plans/0049-Sample").unwrap();
    let old_base = output(repo.path(), &["rev-parse", "HEAD"]);
    let source = GitTreePlanFileSource::new(repo.path(), &old_base).unwrap();
    let PlanCandidate::Plan(plan) =
        load_plan(&source, key.clone(), &PlanReservations::default()).unwrap()
    else {
        panic!()
    };
    let digest = plan.source_digest.to_string();
    let api = build_api(repo.path());
    let CommandOutcome::PlanRegistered {
        registration_oid: r1,
    } = api
        .execute(Command::RegisterPlan {
            plan_dir: key.clone(),
            expected_base_oid: old_base,
            expected_source_digest: digest.clone(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    fs::write(repo.path().join("unrelated.txt"), "advance\n").unwrap();
    git(repo.path(), &["add", "unrelated.txt"]);
    git(repo.path(), &["commit", "-qm", "advance base"]);
    let new_base = output(repo.path(), &["rev-parse", "develop"]);
    let CommandOutcome::PlanRegistered {
        registration_oid: r2,
    } = api
        .execute(Command::RegisterPlan {
            plan_dir: key,
            expected_base_oid: new_base.clone(),
            expected_source_digest: digest,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_ne!(r1, r2);
    assert_eq!(
        output(repo.path(), &["rev-parse", &format!("{r2}^")]),
        new_base
    );
    let message = output(repo.path(), &["show", "-s", "--format=%B", &r2]);
    assert!(message.contains(&format!("Makina-Previous-Registration: {r1}")));
    let archive = format!("refs/makina/recovery/plan/0049-Sample/{r1}");
    assert_eq!(output(repo.path(), &["rev-parse", &archive]), r1);
}

fn build_api(root: &Path) -> CoreApi {
    let interpreter = Arc::new(EdgeInferrer::new(Arc::new(
        SourceProjectionUnavailable::new(),
    )));
    let backend = Arc::new(NoopBackend::with_responses(vec![]));
    CoreApi::new(
        interpreter,
        backend,
        WorktreeManager::new(root.to_path_buf(), "develop".into()),
        Config::resolve(GlobalConfig::default(), ProjectConfig::default()),
    )
}

fn fixture_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "develop"]);
    git(
        repo.path(),
        &["config", "user.email", "register@example.invalid"],
    );
    git(repo.path(), &["config", "user.name", "Register Test"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/plan-bundles/valid/0049-Sample"),
        &repo.path().join("docs/plans/0049-Sample"),
    );
    fs::write(
        repo.path().join("docs/plans/STATUS.md"),
        "# Plans\n\n| Plan | Title | Status | Progress | Outcome | Link |\n|---|---|---|---|---|---|\n",
    )
    .unwrap();
    repo
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

fn git(repo: &Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap()
            .success()
    );
}

fn output(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
