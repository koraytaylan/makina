//! Integration test for `integration-tabbed-pane-navigation`.
//!
//! Verifies that opening multiple tasks creates multiple tabs, renders the tab bar,
//! and the active tab is visually distinguished.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use makina::app::{App, TabContent};
use makina::ui;
use makina_core::api::{
    Api, ApiError, Command, CommandOutcome, EventStream, RunId, RunStatus, RunView, TaskId,
    TaskState, TaskView,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Minimal `Api` double for the integration test.
struct TestApi;

#[async_trait]
impl Api for TestApi {
    async fn execute(&self, _command: Command) -> Result<CommandOutcome, ApiError> {
        Err(ApiError::Internal {
            reason: "stub".into(),
        })
    }

    async fn runs(&self) -> Vec<RunView> {
        vec![]
    }

    async fn run(&self, _id: RunId) -> Option<RunView> {
        None
    }

    fn subscribe(&self) -> EventStream {
        Box::pin(futures::stream::empty())
    }
}

/// Build an `App` with one running run that has two tasks.
fn make_app_with_tasks() -> App {
    let api: Arc<dyn Api> = Arc::new(TestApi);
    let run = RunView {
        id: RunId(1),
        run_uid: "run1".to_string(),
        task_list_path: PathBuf::from("tasks.md"),
        status: RunStatus::Running,
        project: "test-project".to_string(),
        tasks: vec![
            TaskView {
                id: TaskId::new("task-1"),
                title: "First Task".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
            },
            TaskView {
                id: TaskId::new("task-2"),
                title: "Second Task".into(),
                state: TaskState::Ready,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
            },
        ],
        report: makina_core::api::IngestionReport::default(),
    };
    App::new(api, vec![run], PathBuf::from("."))
}

/// Helper to extract screen content from a terminal.
fn screen_of(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().chars().next().unwrap_or(' '))
        .collect()
}

#[test]
fn tabbed_pane_renders_open_tabs() {
    let mut terminal = {
        let backend = TestBackend::new(100, 24);
        Terminal::new(backend).unwrap()
    };
    let mut app = make_app_with_tasks();

    // Open a couple of tabs
    app.tabs.open_tab(TabContent::Task {
        plan_slug: "0001".to_string(),
        task_id: TaskId("task-1".to_string()),
    });
    app.tabs.open_tab(TabContent::Task {
        plan_slug: "0001".to_string(),
        task_id: TaskId("task-2".to_string()),
    });

    terminal.draw(|frame| ui::render(&app, frame)).unwrap();
    let screen = screen_of(&terminal);

    // Both tab titles should appear in the rendered output
    assert!(
        screen.contains("task-1"),
        "tab bar should show first task tab"
    );
    assert!(
        screen.contains("task-2"),
        "tab bar should show second task tab"
    );
}

#[test]
fn tabbed_pane_active_tab_is_distinguished() {
    let mut terminal = {
        let backend = TestBackend::new(100, 24);
        Terminal::new(backend).unwrap()
    };
    let mut app = make_app_with_tasks();

    // Open two tabs
    app.tabs.open_tab(TabContent::Task {
        plan_slug: "0001".to_string(),
        task_id: TaskId("task-1".to_string()),
    });
    app.tabs.open_tab(TabContent::Task {
        plan_slug: "0001".to_string(),
        task_id: TaskId("task-2".to_string()),
    });

    // Verify that second tab is active (it was the last one opened)
    assert_eq!(
        app.tabs.active_tab,
        Some(1),
        "second tab should be active after opening"
    );

    terminal.draw(|frame| ui::render(&app, frame)).unwrap();

    // Render the screen and verify both tabs are present
    let screen = screen_of(&terminal);
    assert!(
        screen.contains("task-1"),
        "tab bar should show first task tab"
    );
    assert!(
        screen.contains("task-2"),
        "tab bar should show second task tab"
    );

    // Check that the buffer has style information showing the active tab
    // (we can't directly check colors from screen string, but we verify the structure)
    let buffer = terminal.backend().buffer();
    let buffer_content = buffer.content();

    // Look for cells with the active tab styling (Cyan background)
    // The active tab should have a different style than inactive tabs
    let mut found_styled_cells = false;
    for cell in buffer_content.iter() {
        if !cell.symbol().is_empty() && cell.style().bg == Some(ratatui::style::Color::Cyan) {
            found_styled_cells = true;
            break;
        }
    }

    assert!(
        found_styled_cells,
        "active tab should be styled with Cyan background"
    );
}
