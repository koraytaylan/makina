//! Top-down integration test: scaffold → open plan → start run → observe states.
//!
//! This test simulates the EXACT scenario the user reports:
//! 1. A todo template project is created
//! 2. A run is opened for plan 0001-todo-core
//! 3. The run is started
//! 4. The agent backend (NoopBackend) responds "done" to everything
//! 5. We observe: why do tasks 2 and 3 show as "skipped"?

use makina_core::api::{Api, Command, CommandOutcome, RunStatus, TaskState};
use makina_core::audit::JsonlAuditSink;
use makina_core::backend::noop::NoopBackend;
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::dependency::EdgeInferrer;
use makina_core::interpreter::SourceProjectionUnavailable;
use makina_core::orchestrator::CoreApi;
use makina_core::plan::PlanKey;
use makina_core::repository_lease::RepositoryLeaseRegistry;
use makina_core::worktree::WorktreeManager;
use std::sync::Arc;

struct RestoreEnv(Option<std::ffi::OsString>);

impl Drop for RestoreEnv {
    fn drop(&mut self) {
        unsafe {
            match self.0.take() {
                Some(value) => std::env::set_var("GIT_CONFIG_GLOBAL", value),
                None => std::env::remove_var("GIT_CONFIG_GLOBAL"),
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn todo_plan_first_run_no_skipped_tasks() {
    // ── Step 1: Create a fresh todo project (same as `makina create`) ──────
    let tmp = tempfile::tempdir().expect("tempdir");
    let global_config = tmp.path().join("gitconfig");
    std::fs::write(
        &global_config,
        "[user]\n\tname = Skipped E2E Test\n\temail = skipped-e2e@example.invalid\n",
    )
    .expect("write test global Git identity");
    let prior = std::env::var_os("GIT_CONFIG_GLOBAL");
    unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", &global_config) };
    let _restore_global = RestoreEnv(prior);
    let target = tmp.path().join("todo");
    makina::scaffold::scaffold_project(&target, "todo")
        .await
        .expect("scaffold must succeed");

    // ── Step 2: Build a CoreApi with NoopBackend (simulates agent) ──────────
    let backend: Arc<dyn makina_core::backend::AgentBackend> = Arc::new(NoopBackend::new());
    let project_config_path = target.join(".makina/config.toml");
    let project_config = std::fs::read_to_string(&project_config_path)
        .expect("scaffolded project config must be readable");
    let project_config =
        ProjectConfig::from_toml_str(&project_config, &project_config_path.display().to_string())
            .expect("scaffolded project config must parse");
    // This test injects NoopBackend directly, so resolving the project settings
    // must not depend on an operator-installed backend CLI.
    let config = Config::resolve(GlobalConfig::default(), project_config);
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

    // ── Step 3: Open plan 0001-todo-core ────────────────────────────────────
    let plan_key = PlanKey::parse("docs/plans/0001-todo-core").unwrap();
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

    // ── Step 4: Start the run (model check is at the TUI layer, not the API) ─
    let _outcome = api
        .execute(Command::StartRun { run })
        .await
        .expect("StartRun must succeed at API level");

    // Poll until the run reaches a terminal state.
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let view = api.run(run).await.expect("run must exist");
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

    // The first task must be Failed (not Ready) and must have a failure_reason
    // that explains WHY it failed. This is what the TUI's task tab shows.
    let failed_task = view
        .tasks
        .iter()
        .find(|t| t.id.0 == "add-task-toggle")
        .expect("add-task-toggle must exist");
    assert_eq!(
        failed_task.state,
        TaskState::Failed,
        "add-task-toggle must be Failed, not Ready — \
         if it's Ready the commit_claim error wasn't handled properly"
    );
    let reason = failed_task
        .failure_reason
        .as_ref()
        .expect("failed task must have a failure_reason — the TUI task tab shows this");
    println!(
        "Failure reason: kind={:?} message={}",
        reason.kind, reason.message
    );
    assert!(
        !reason.message.is_empty(),
        "failure reason message must not be empty"
    );

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

/// Test that a failed task's tab shows its failure reason.
/// This verifies the UI rendering path, not just the API data.
#[tokio::test(flavor = "multi_thread")]
async fn tui_task_tab_shows_failure_reason_for_failed_task() {
    use makina::app::{App, AppEvent, Panel};
    use makina_core::api::{
        Api, ApiError, Command, CommandOutcome, EventStream, FailureKind, FailureReason, RunId,
        RunStatus, RunView, TaskId, TaskState, TaskView,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::Arc;

    // Minimal mock API that returns a run with a failed task.
    struct MockApi;
    #[async_trait::async_trait]
    impl Api for MockApi {
        async fn execute(&self, _command: Command) -> Result<CommandOutcome, ApiError> {
            Ok(CommandOutcome::Acknowledged)
        }
        async fn runs(&self) -> Vec<RunView> {
            vec![]
        }
        async fn run(&self, _id: RunId) -> Option<RunView> {
            Some(RunView {
                id: RunId(1),
                run_uid: String::new(),
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
                status: RunStatus::Failed,
                project: String::new(),
                tasks: vec![TaskView {
                    authored: None,
                    id: TaskId::new("add-task-toggle"),
                    title: "Add A Task Toggle Method".into(),
                    state: TaskState::Failed,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: Some(FailureReason {
                        kind: FailureKind::HardError,
                        message: "durable claim failed: git rev-parse failed".into(),
                    }),
                    entry_text: String::new(),
                }],
                report: makina_core::api::IngestionReport::default(),
            })
        }
        fn subscribe(&self) -> EventStream {
            Box::pin(futures::stream::iter(Vec::new()))
        }
    }

    let api: Arc<dyn Api> = Arc::new(MockApi);
    let run = api.run(RunId(1)).await.unwrap();
    let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));
    app.collapsed_runs.clear();
    app.selected_run = Some(0);
    app.selected_task = Some(0);
    app.focused_panel = Panel::Main;

    // Open a task tab so the main pane renders the task detail.
    app.update(AppEvent::OpenTab(makina::app::TabContent::Task {
        plan: makina::app::PlanIdentity::legacy("0001-Test"),
        run: RunId(1),
        task_id: TaskId::new("add-task-toggle"),
    }));

    // Render and check the screen.
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|f| makina::ui::render(&app, f)).unwrap();

    let buffer = terminal.backend().buffer().clone();
    let screen: String = buffer
        .content()
        .iter()
        .map(|c| c.symbol().chars().next().unwrap_or(' '))
        .collect();

    assert!(
        screen.contains("durable claim failed"),
        "the task tab must show the failure reason message; screen was:\n{screen}"
    );
}
