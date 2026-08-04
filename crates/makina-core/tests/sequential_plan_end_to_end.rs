//! Functional end-to-end coverage for a **multi-task authored plan** driven by
//! the production `CoreApi` (`OpenPlan` → `StartRun`) — the exact path the
//! `makina run` binary takes.
//!
//! # Why this test exists
//!
//! Every other supervisor test drives a bare [`TaskGraph`] through `run_graph`
//! with **no checkpoint identity**, which makes `DriverContext::commit_claim`
//! and `DriverContext::commit_phase_b` early-return `Ok(())`. The durable
//! claim/Phase-B *status writes* — the ones that render `STATUS.md` and that the
//! **next** task's claim must re-parse — were therefore never exercised by the
//! suite at all.
//!
//! A real three-task chain caught it immediately: Phase B wrote an
//! `awaiting-integration` + `mode` combination the plan loader rejects as
//! incoherent, so the *second* task's durable claim failed with a hard error and
//! the third was skipped. The single-task fixtures could never see it, because
//! nothing ever re-read what the last task wrote.
//!
//! # Coverage
//!
//! 1. **The chain completes** — three sequentially dependent tasks all reach
//!    `Done`, against a mixed-case plan directory, so the run has to resolve the
//!    very `refs/heads/plan/{id}` that registration published (Git refs are
//!    case-sensitive; a case-folded identity looks unregistered).
//! 2. **Finalization actually runs** — the tip is durable Phase P:
//!    `finalization-pending` carrying the STATUS spelling of the merge mode and
//!    a blank final-integration field. Phase P failures are log-only, so
//!    asserting the published state is the only proof it ran.
//! 3. **Every published status re-loads** — walking the whole plan-ref history,
//!    each commit loads through `load_plan` without a diagnostic, because each
//!    one is what some later claim reads. Intermediate commits must report
//!    `assembling` with no merge mode, not `awaiting-integration`.
//!
//! # Test-strategy compliance
//!
//! - The agent stand-in is [`NoopBackend`] — no real CLI, no model call.
//! - A fresh temporary git repo and a temporary `HOME` per test.
//! - Determinism via awaited command completion — no sleeps.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use makina_core::api::{Api, Command, CommandOutcome, RunStatus, TaskState};
use makina_core::backend::noop::NoopBackend;
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::dependency::EdgeInferrer;
use makina_core::interpreter::SourceProjectionUnavailable;
use makina_core::orchestrator::CoreApi;
use makina_core::plan::{
    AuthoredTaskStatus, GitTreePlanFileSource, PlanCandidate, PlanIntegrationState, PlanKey,
    PlanReservations, load_plan,
};
use makina_core::worktree::WorktreeManager;

const PLAN_DIR: &str = "docs/plans/0001-Sequential-Chain";
const CHAIN: [&str; 3] = ["first-link", "second-link", "third-link"];

/// The whole chain must land, and every status the run wrote must still parse.
#[tokio::test]
async fn sequential_chain_lands_every_task_and_leaves_a_loadable_plan() {
    let _home_guard = makina_core::HOME_ENV_LOCK.lock().await;
    let repo = fixture_repo();
    let home = tempfile::tempdir().unwrap();
    let restore = set_home(home.path());

    let api = build_api(repo.path().to_owned());
    register(&api, repo.path()).await;
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: PlanKey::parse(PLAN_DIR).unwrap(),
        })
        .await
        .expect("opening a valid bundle must succeed")
    else {
        panic!("OpenPlan must return RunOpened")
    };

    api.execute(Command::StartRun { run })
        .await
        .expect("the run must drive the chain without a hard error");
    let view = await_run(&api, run).await;
    // Report the whole chain plus every failure reason: a stranded task's reason
    // is the only thing that names which write path corrupted the plan.
    let outcome = view
        .tasks
        .iter()
        .map(|task| {
            format!(
                "{}={:?}{}",
                task.id.0,
                task.state,
                task.failure_reason
                    .as_ref()
                    .map(|reason| format!(" ({:?}: {})", reason.kind, reason.message))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    for id in CHAIN {
        let task = view
            .tasks
            .iter()
            .find(|task| task.id.0 == id)
            .unwrap_or_else(|| panic!("task {id} is missing from the run view; got {outcome}"));
        assert_eq!(
            task.state,
            TaskState::Done,
            "task {id} must reach Done; the whole chain was {outcome}",
        );
    }

    // The plan ref must still load cleanly — a status write that the loader
    // rejects strands every task that has not claimed yet.
    let plan = load_plan_at(repo.path(), "refs/heads/plan/0001-Sequential-Chain");
    for task in &plan.tasks {
        assert_eq!(
            task.frontmatter.status,
            AuthoredTaskStatus::Done,
            "task {} must be authored done on the plan ref",
            task.frontmatter.id.as_str(),
        );
        assert!(
            task.frontmatter.merged_as.is_some(),
            "task {} must record landing evidence",
            task.frontmatter.id.as_str(),
        );
    }
    assert_eq!(plan.status.done, CHAIN.len());
    // A typed run stops at durable Phase P: the tip is `finalization-pending`
    // carrying the selected mode, and F/C wait for an explicit `FinalizePlan`.
    // Phase P failures are only logged, so asserting the published state here is
    // the only thing that proves finalization actually ran.
    assert_eq!(
        plan.status.integration_state,
        PlanIntegrationState::FinalizationPending,
        "the run must publish durable Phase P once the chain lands",
    );
    assert_eq!(
        plan.status.mode.as_deref(),
        Some("Squash"),
        "Phase P must record the STATUS spelling of the final merge mode, \
         not the commit-trailer spelling",
    );
    assert!(
        plan.status.final_oid.is_none(),
        "Phase P must leave the final-integration field blank",
    );

    restore();
}

/// Every status the run publishes — not just the tip — must load back.
///
/// The tip alone is a weak assertion: the run failed originally because an
/// *intermediate* commit was unreadable, and by the time anything inspected the
/// plan the run had already died. Walking the whole plan-ref history checks each
/// commit the way the next claim would read it.
#[tokio::test]
async fn every_published_status_commit_loads_back() {
    let _home_guard = makina_core::HOME_ENV_LOCK.lock().await;
    let repo = fixture_repo();
    let home = tempfile::tempdir().unwrap();
    let restore = set_home(home.path());

    let api = build_api(repo.path().to_owned());
    register(&api, repo.path()).await;
    let CommandOutcome::RunOpened { run } = api
        .execute(Command::OpenPlan {
            plan_dir: PlanKey::parse(PLAN_DIR).unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!("OpenPlan must return RunOpened")
    };
    api.execute(Command::StartRun { run }).await.unwrap();
    await_run(&api, run).await;

    // Walk the plan ref's history and reload the bundle at every status commit
    // the run published. Each one is what the *next* claim reads, so every one
    // of them has to be loader-valid — not just the tip.
    let history = git_output(
        repo.path(),
        &[
            "rev-list",
            "--reverse",
            "refs/heads/plan/0001-Sequential-Chain",
        ],
    );
    let mut assembling = 0usize;
    let mut awaiting = 0usize;
    let mut finalizing = 0usize;
    for oid in history.lines().filter(|line| !line.is_empty()) {
        let source = GitTreePlanFileSource::new(repo.path(), oid).unwrap();
        let key = PlanKey::parse(PLAN_DIR).unwrap();
        let candidate = load_plan(&source, key, &PlanReservations::default())
            .unwrap_or_else(|report| panic!("commit {oid} left an unloadable plan: {report:?}"));
        let PlanCandidate::Plan(plan) = candidate else {
            continue;
        };
        match plan.status.integration_state {
            PlanIntegrationState::Assembling => {
                assembling += 1;
                assert!(
                    plan.status.mode.is_none(),
                    "commit {oid} recorded a final merge mode before finalization",
                );
            }
            PlanIntegrationState::AwaitingIntegration => {
                awaiting += 1;
                assert_eq!(
                    plan.status.done,
                    CHAIN.len(),
                    "commit {oid} claimed awaiting-integration with work outstanding",
                );
            }
            PlanIntegrationState::FinalizationPending => {
                finalizing += 1;
                assert_eq!(
                    plan.status.mode.as_deref(),
                    Some("Squash"),
                    "commit {oid} recorded a mode spelling the loader rejects",
                );
            }
            _ => {}
        }
    }
    assert!(
        assembling > 0,
        "the run must publish at least one assembling status",
    );
    assert!(
        awaiting > 0,
        "the run must publish an awaiting-integration status once the chain lands",
    );
    assert!(
        finalizing > 0,
        "the run must publish durable Phase P before it stops",
    );

    restore();
}

// ── Fixture ──────────────────────────────────────────────────────────────────

fn fixture_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    git(root, &["init", "-q", "-b", "develop"]);
    git(root, &["config", "user.email", "test@example.invalid"]);
    git(root, &["config", "user.name", "Sequential Test"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    fs::create_dir_all(root.join("docs/plans")).unwrap();
    fs::write(root.join(".gitignore"), ".makina/\n").unwrap();
    // The roll-up row is authored, not synthesized: Phase P overlays this plan's
    // row onto the *base* board, and `update_root_row` requires exactly one
    // existing row there — a board without it makes finalization fail.
    fs::write(
        root.join("docs/plans/STATUS.md"),
        "# Plans — roll-up board\n\n\
         | Plan | Title | Status | Tasks | Outcome | Status doc |\n\
         |---|---|---|---|---|---|\n\
         | 0001 | Sequential Chain | 📋 Planned | 0/3 | three sequential tasks land in order and the plan stays loadable throughout. | [status](0001-Sequential-Chain/STATUS.md) |\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/plan-bundles/sequential/0001-Sequential-Chain"),
        &root.join(PLAN_DIR),
    );
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "scaffold"]);
    repo
}

/// Publish the committed bundle as an evidenced plan ref — what an operator does
/// before the first run of a plan.
async fn register(api: &CoreApi, repo: &Path) {
    let base = git_output(repo, &["rev-parse", "develop"])
        .trim()
        .to_owned();
    let source = makina_core::plan::FilesystemPlanFileSource::new(repo, None).unwrap();
    let PlanCandidate::Plan(plan) = load_plan(
        &source,
        PlanKey::parse(PLAN_DIR).unwrap(),
        &PlanReservations::default(),
    )
    .unwrap() else {
        panic!("the fixture bundle must be a plan")
    };
    let outcome = api
        .execute(Command::RegisterPlan {
            plan_dir: PlanKey::parse(PLAN_DIR).unwrap(),
            expected_base_oid: base,
            expected_source_digest: plan.source_digest.to_string(),
        })
        .await
        .expect("registering the committed fixture bundle must succeed");
    assert!(
        matches!(outcome, CommandOutcome::PlanRegistered { .. }),
        "the committed bundle must register, got {outcome:?}",
    );
}

/// `StartRun` spawns the scheduler in the background, so poll the run view until
/// it reaches a terminal status. Bounded by a timeout — never a bare sleep.
async fn await_run(api: &CoreApi, run: makina_core::api::RunId) -> makina_core::api::RunView {
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            let view = api.run(run).await.expect("the run must stay queryable");
            if matches!(view.status, RunStatus::Completed | RunStatus::Failed) {
                return view;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the run must terminate within the timeout")
}

fn build_api(repo_root: PathBuf) -> CoreApi {
    let interpreter = Arc::new(EdgeInferrer::new(Arc::new(
        SourceProjectionUnavailable::new(),
    )));
    // One canned developer response plus an approving reviewer verdict; the
    // NoopBackend cycles them, so the same pair serves every task in the chain.
    let backend = Arc::new(NoopBackend::with_responses(vec![
        "Implemented the link.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]));
    CoreApi::new(
        interpreter,
        backend,
        WorktreeManager::new(repo_root, "develop".into()),
        Config::resolve(GlobalConfig::default(), ProjectConfig::default()),
    )
}

fn load_plan_at(repo: &Path, reference: &str) -> Box<makina_core::plan::PlanDocument> {
    let oid = git_output(repo, &["rev-parse", reference]);
    let source = GitTreePlanFileSource::new(repo, oid.trim()).unwrap();
    let key = PlanKey::parse(PLAN_DIR).unwrap();
    match load_plan(&source, key, &PlanReservations::default()) {
        Ok(PlanCandidate::Plan(plan)) => plan,
        Ok(PlanCandidate::NotCandidate) => panic!("{reference} lost the plan bundle"),
        Err(report) => panic!("{reference} carries an unloadable plan: {report:?}"),
    }
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

fn git(repo: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
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

fn git_output(repo: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap()
}

fn set_home(path: &Path) -> impl FnOnce() {
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", path) };
    move || match old {
        Some(value) => unsafe { std::env::set_var("HOME", value) },
        None => unsafe { std::env::remove_var("HOME") },
    }
}
