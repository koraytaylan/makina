//! Headless plan execution — the `makina run` subcommand's implementation.
//!
//! The TUI is presentation only, and `main.rs` is deliberately a thin wiring
//! file with no logic, so this lives in the library alongside the other modules
//! integration tests exercise directly.
//!
//! Printing belongs here, not in `main.rs`. A `println!` inside the TUI path
//! would corrupt the rendered frame and bypass the error pane — which is why
//! `no_frame_bypass` pins the exact set of prints `main.rs` may contain. A
//! headless run has no frame: stdout *is* its output medium, and its exit code
//! is what a script or CI job branches on.

use std::path::Path;
use std::sync::Arc;

use makina_core::api::{Api, Command, CommandOutcome, Event, FinalizeInput, RunStatus, TaskState};
use makina_core::config::Config;
use makina_core::plan::{
    FilesystemPlanFileSource, PlanCandidate, PlanIntegrationState, PlanKey, PlanReservations,
    load_plan,
};

use crate::log;
use crate::project_api::canonicalize_project_root;

/// Build the orchestrator for one repository.
///
/// Injected so the agent-backend wiring stays in the binary, where the ACP
/// backend is already assembled, and so tests can drive this module with a
/// stand-in backend instead of a real agent CLI.
pub type ApiBuilder<'a> = &'a dyn Fn(&Path, Config) -> Arc<dyn Api>;

/// Drive one plan to a terminal run status with no terminal attached.
///
/// This is the same orchestration the TUI performs — register the committed
/// bundle, open it, start the supervisor, wait for a terminal status — with the
/// rendering removed. Without it the autonomous run path is reachable only by a
/// human at a keyboard, which makes it unscriptable, unusable from CI, and
/// impossible to reproduce against a real agent when diagnosing a failure.
///
/// Returns the process exit code: 0 when every task landed, 1 otherwise.
pub async fn run_plan(plan_dir: &str, finalize: bool, build_api: ApiBuilder<'_>) -> i32 {
    let launch_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let Some(repo_root) = canonicalize_project_root(&launch_dir).ok() else {
        eprintln!(
            "error: {} is not inside a Git repository",
            launch_dir.display()
        );
        return 1;
    };
    let (load_result, _paths) = Config::load_for_repo_with_paths(&repo_root);
    let config = match load_result {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: failed to load configuration: {error}");
            return 1;
        }
    };
    let base_branch = config.base_branch.clone();

    // Install the same per-run file layer the TUI does, so `.makina/runs/{uid}/
    // logs/` is written here too. Without it every `tracing` record — including
    // the ERROR a failed Phase P emits and the per-task transition log — goes
    // nowhere, and a headless run that quietly declines to finalize would leave
    // no evidence at all. Stderr carries the same records for a live operator.
    {
        use tracing_subscriber::EnvFilter;
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(log::RunFileLayer::new(repo_root.clone()))
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .try_init();
    }

    let key = match PlanKey::parse(plan_dir) {
        Ok(key) => key,
        Err(error) => {
            eprintln!("error: {plan_dir} is not a valid plan directory: {error}");
            return 1;
        }
    };

    let api = build_api(&repo_root, config);

    // Registration publishes the plan ref once. Re-issuing it against a ref that
    // already carries run evidence is correctly refused, so a plan that has been
    // run before — or resumed — must skip straight to open, exactly as the TUI
    // does. Register only when the ref does not exist yet.
    if git_rev_parse(&repo_root, &key.ref_name()).is_err() {
        let source = match FilesystemPlanFileSource::new(&repo_root, None) {
            Ok(source) => source,
            Err(error) => {
                eprintln!("error: cannot read {}: {error}", repo_root.display());
                return 1;
            }
        };
        let plan = match load_plan(&source, key.clone(), &PlanReservations::default()) {
            Ok(PlanCandidate::Plan(plan)) => plan,
            Ok(PlanCandidate::NotCandidate) => {
                eprintln!("error: {plan_dir} is not a plan bundle");
                return 1;
            }
            Err(report) => {
                eprintln!("error: {plan_dir} is not loader-valid:");
                for diagnostic in &report.diagnostics {
                    eprintln!(
                        "  {} {}: {}",
                        diagnostic.code,
                        diagnostic.path.display(),
                        diagnostic.message
                    );
                }
                return 1;
            }
        };
        let base_oid = match git_rev_parse(&repo_root, &base_branch) {
            Ok(oid) => oid,
            Err(error) => {
                eprintln!("error: cannot resolve base branch {base_branch}: {error}");
                return 1;
            }
        };
        match api
            .execute(Command::RegisterPlan {
                plan_dir: key.clone(),
                expected_base_oid: base_oid,
                expected_source_digest: plan.source_digest.to_string(),
            })
            .await
        {
            Ok(CommandOutcome::AwaitingCommit) => {
                eprintln!("error: {plan_dir} has uncommitted changes; commit the bundle first");
                return 1;
            }
            Ok(_) => println!("registered {plan_dir}"),
            Err(error) => {
                eprintln!("error: registering {plan_dir} failed: {error}");
                return 1;
            }
        }
    }

    // A plan that has landed every task is waiting on finalization, whether it
    // reached durable Phase P or stopped at `awaiting-integration` because a
    // Phase-P attempt failed. `OpenPlan` rejects both tips — their Phase-R
    // trailers no longer match the retained status — so resume through the
    // entry point those states actually have instead of reporting a spurious
    // registration error and stranding the plan for good.
    if let Some((state, run_uid, _)) = retained_finalization(&repo_root, &key) {
        match state.as_str() {
            "complete" => {
                println!("{plan_dir} is already complete");
                return 0;
            }
            "finalization-pending" | "awaiting-integration" if finalize => {
                return finalize_retained(
                    api.as_ref(),
                    &repo_root,
                    &key,
                    &run_uid,
                    plan_dir,
                    &base_branch,
                )
                .await;
            }
            "finalization-pending" | "awaiting-integration" => {
                println!(
                    "{plan_dir} has landed every task and is awaiting finalization; \
                     re-run with --finalize to merge it onto {base_branch}"
                );
                return 0;
            }
            _ => {}
        }
    }

    let run = match api
        .execute(Command::OpenPlan {
            plan_dir: key.clone(),
        })
        .await
    {
        Ok(CommandOutcome::RunOpened { run }) => run,
        Ok(other) => {
            eprintln!("error: opening {plan_dir} returned {other:?}");
            return 1;
        }
        Err(error) => {
            eprintln!("error: opening {plan_dir} failed: {error}");
            return 1;
        }
    };

    // Subscribe before starting so no transition is missed, and mirror the
    // events the TUI would render as plain lines.
    let mut events = api.subscribe();
    let progress = tokio::spawn(async move {
        use futures::StreamExt as _;
        while let Some(event) = events.next().await {
            match event {
                Event::TaskStateChanged { task, state, .. } => {
                    println!("  {} → {state:?}", task.0);
                }
                Event::TaskFailed { task, reason, .. } => {
                    println!("  {} failed: {}", task.0, reason.message);
                }
                Event::RunProgress { phase, .. } => println!("  {phase}"),
                Event::RunStatusChanged { status, .. } => {
                    println!("run status: {status:?}");
                }
                _ => {}
            }
        }
    });

    println!("running {plan_dir} on base {base_branch}");
    if let Err(error) = api.execute(Command::StartRun { run }).await {
        eprintln!("error: starting {plan_dir} failed: {error}");
        return 1;
    }

    // `StartRun` spawns the supervisor and returns; wait for a terminal status.
    let view = loop {
        let Some(view) = api.run(run).await else {
            eprintln!("error: run {run:?} disappeared");
            return 1;
        };
        if matches!(view.status, RunStatus::Completed | RunStatus::Failed) {
            break view;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };
    progress.abort();

    println!("\n{plan_dir}: {:?}", view.status);
    let mut failed = false;
    for task in &view.tasks {
        let detail = task
            .failure_reason
            .as_ref()
            .map(|reason| format!("  ({:?}: {})", reason.kind, reason.message))
            .unwrap_or_default();
        println!("  {:<28} {:?}{detail}", task.id.0, task.state);
        if task.state != TaskState::Done {
            failed = true;
        }
    }
    if failed || view.status != RunStatus::Completed {
        return 1;
    }

    if finalize {
        // The supervisor attempts Phase P at the end of a run, but that attempt
        // can fail — a genuine content conflict with a base another plan has
        // since advanced, for instance — and it only logs. Re-read the retained
        // state instead of assuming P exists: calling `FinalizePlan` on a plan
        // still at `awaiting-integration` reported "expected plan tip is not
        // exact Phase P evidence", which describes the symptom and hides the
        // conflict that actually caused it.
        return finalize_retained(
            api.as_ref(),
            &repo_root,
            &key,
            &view.run_uid,
            plan_dir,
            &base_branch,
        )
        .await;
    }
    0
}

/// Take a plan that has landed every task through Phase P (when it has none)
/// and then through F/C.
async fn finalize_retained(
    api: &dyn Api,
    repo_root: &Path,
    key: &PlanKey,
    run_uid: &str,
    plan_dir: &str,
    base_branch: &str,
) -> i32 {
    let (state, plan_oid) = match retained_finalization(repo_root, key) {
        Some((state, _, oid)) => (state, oid),
        None => {
            eprintln!(
                "error: {plan_dir} is not in a finalizable state; see the run log for the \
                 Phase P failure that left it there"
            );
            return 1;
        }
    };
    if state == "complete" {
        println!("{plan_dir} is already complete");
        return 0;
    }
    let mut plan_oid = plan_oid;
    if state == "awaiting-integration" {
        match api
            .execute(Command::ReprepareFinalization {
                plan_dir: key.clone(),
                run_uid: run_uid.to_owned(),
                expected_plan_oid: plan_oid.clone(),
            })
            .await
        {
            Ok(CommandOutcome::FinalizationAccepted { plan_oid: prepared }) => {
                println!("prepared finalization for {plan_dir}");
                plan_oid = prepared;
            }
            Ok(other) => {
                eprintln!("error: preparing finalization returned {other:?}");
                return 1;
            }
            Err(error) => {
                eprintln!("error: preparing finalization for {plan_dir} failed: {error}");
                return 1;
            }
        }
    }
    match api
        .execute(Command::FinalizePlan {
            plan_dir: key.clone(),
            run_uid: run_uid.to_owned(),
            expected_plan_oid: plan_oid,
            input: FinalizeInput::Automatic,
        })
        .await
    {
        Ok(_) => {
            println!("finalized {plan_dir} onto {base_branch}");
            0
        }
        Err(error) => {
            eprintln!("error: finalizing {plan_dir} failed: {error}");
            1
        }
    }
}

/// Read the retained plan ref's integration state, run, and tip.
///
/// Returns `None` when the ref is absent or its bundle does not load — both
/// cases belong to the ordinary open/start path, which reports them properly.
fn retained_finalization(repo_root: &Path, key: &PlanKey) -> Option<(String, String, String)> {
    use makina_core::plan::GitTreePlanFileSource;

    let tip = git_rev_parse(repo_root, &key.ref_name()).ok()?;
    let source = GitTreePlanFileSource::new(repo_root, &tip).ok()?;
    let PlanCandidate::Plan(plan) =
        load_plan(&source, key.clone(), &PlanReservations::default()).ok()?
    else {
        return None;
    };
    let state = match plan.status.integration_state {
        PlanIntegrationState::FinalizationPending => "finalization-pending",
        // Every task has landed but Phase P has not been published — either it
        // has not been attempted or an attempt failed. Finalization is still
        // the correct next step, so this must not be mistaken for a run to
        // start over.
        PlanIntegrationState::AwaitingIntegration => "awaiting-integration",
        PlanIntegrationState::Complete => "complete",
        _ => return None,
    };
    Some((state.to_owned(), plan.status.run.clone()?, tip))
}

/// Resolve one revision in `repo`, returning the trimmed object ID.
fn git_rev_parse(repo: &Path, revision: &str) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", revision])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
