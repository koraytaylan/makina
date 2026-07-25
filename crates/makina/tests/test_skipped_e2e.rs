//! Top-down integration test: scaffold → open plan → start run → observe states.
//!
//! This test simulates the EXACT scenario the user reports:
//! 1. A todo template project is created
//! 2. A run is opened for plan 0001-Todo-Core
//! 3. The run is started
//! 4. The agent backend (NoopBackend) responds "done" to everything
//! 5. We observe: why do tasks 2 and 3 show as "skipped"?

use makina_core::api::{Api, Command, CommandOutcome, RunStatus, TaskState};
use makina_core::audit::JsonlAuditSink;
use makina_core::backend::noop::NoopBackend;
use makina_core::config::Config;
use makina_core::dependency::EdgeInferrer;
use makina_core::interpreter::SourceProjectionUnavailable;
use makina_core::orchestrator::CoreApi;
use makina_core::plan::PlanKey;
use makina_core::repository_lease::RepositoryLeaseRegistry;
use makina_core::worktree::WorktreeManager;
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread")]
async fn todo_plan_first_run_no_skipped_tasks() {
    // ── Step 1: Create a fresh todo project (same as `makina create`) ──────
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("todo");
    makina::scaffold::scaffold_project(&target, "todo")
        .await
        .expect("scaffold must succeed");

    // ── Step 2: Build a CoreApi with NoopBackend (simulates agent) ──────────
    let backend: Arc<dyn makina_core::backend::AgentBackend> = Arc::new(NoopBackend::new());
    let config = Config::load_for_repo_with_paths(&target)
        .0
        .expect("config must load");
    let worktree_manager = WorktreeManager::new(target.clone(), config.base_branch.clone());
    let audit_sink = Arc::new(JsonlAuditSink::new(target.clone()));

    let ingestion_interpreter: Arc<dyn makina_core::interpreter::TaskListInterpreter> = Arc::new(
        EdgeInferrer::new(Arc::new(SourceProjectionUnavailable::new())),
    );
    let planner_interpreter: Arc<dyn makina_core::interpreter::TaskListInterpreter> = Arc::new(
        EdgeInferrer::new(Arc::new(SourceProjectionUnavailable::new())),
    );

    let api = CoreApi::with_repository_lease_registry(
        ingestion_interpreter,
        planner_interpreter,
        Arc::clone(&backend),
        Arc::clone(&backend),
        worktree_manager,
        config,
        audit_sink as Arc<dyn makina_core::audit::AuditRegistry>,
        Arc::new(RepositoryLeaseRegistry::new()),
    );

    // ── Step 3: Open plan 0001-Todo-Core ────────────────────────────────────
    let plan_key = PlanKey::parse("docs/plans/0001-Todo-Core").unwrap();
    let outcome = api
        .execute(Command::OpenPlan {
            plan_dir: plan_key.clone(),
        })
        .await
        .expect("OpenPlan must succeed");
    let run = match outcome {
        CommandOutcome::RunOpened { run } => run,
        _ => panic!("expected RunOpened, got {outcome:?}"),
    };

    // Check task states right after open (before StartRun).
    let view = api.run(run).await.expect("run must exist");
    println!("After OpenPlan — task states:");
    for task in &view.tasks {
        println!("  {}: {:?}", task.id.0, task.state);
    }
    for task in &view.tasks {
        assert_ne!(
            task.state,
            TaskState::Skipped,
            "task {} must NOT be Skipped right after OpenPlan (no run has happened yet)",
            task.id.0
        );
    }

    // ── Step 4: Start the run ───────────────────────────────────────────────
    let outcome = api
        .execute(Command::StartRun { run })
        .await
        .expect("StartRun must succeed");
    println!("StartRun outcome: {outcome:?}");

    // Poll task states every 500ms to see the progression.
    for i in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let view = api.run(run).await.expect("run must exist");
        let states: Vec<(String, TaskState)> = view
            .tasks
            .iter()
            .map(|t| (t.id.0.clone(), t.state.clone()))
            .collect();
        println!(
            "[{:.1}s] status={:?} tasks={:?}",
            i as f64 * 0.5,
            view.status,
            states
        );
        if matches!(view.status, RunStatus::Completed | RunStatus::Failed) {
            break;
        }
    }

    // ── Step 5: Check final task states ─────────────────────────────────────
    let view = api.run(run).await.expect("run must exist");
    println!("\nAfter StartRun + 3s — task states:");
    for task in &view.tasks {
        println!("  {}: {:?}", task.id.0, task.state);
    }
    println!("Run status: {:?}", view.status);

    // The NoopBackend responds to every prompt. The gates (cargo test/clippy/fmt)
    // will likely fail since NoopBackend doesn't actually edit files. But the
    // key assertion is: a task should only be Skipped if a transitive
    // dependency actually Failed. No task should be Skipped if all its
    // transitive dependencies are New/Ready/InProgress/Done.
    for task in &view.tasks {
        if task.state == TaskState::Skipped {
            // Check all transitive dependencies for any Failed.
            let mut queue: Vec<_> = task.depends_on.clone();
            let mut visited = std::collections::HashSet::new();
            let mut any_transitive_failed = false;
            while let Some(dep_id) = queue.pop() {
                if !visited.insert(dep_id.clone()) {
                    continue;
                }
                if let Some(dep) = view.tasks.iter().find(|t| t.id == dep_id) {
                    if dep.state == TaskState::Failed {
                        any_transitive_failed = true;
                        break;
                    }
                    queue.extend(dep.depends_on.iter().cloned());
                }
            }
            assert!(
                any_transitive_failed,
                "task {} is Skipped but none of its transitive dependencies Failed — \
                 this is the bug the user is reporting!\n\
                 Task states: {:?}",
                task.id.0,
                view.tasks
                    .iter()
                    .map(|t| (t.id.0.clone(), t.state.clone()))
                    .collect::<Vec<_>>()
            );
        }
    }
}
