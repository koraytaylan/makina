//! Integration test for `integration-tabbed-pane-navigation`.
//!
//! Verifies that opening multiple tasks creates multiple tabs, renders the tab bar,
//! and the active tab is visually distinguished.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use makina::app::{AccordionSection, App, TabContent};
use makina::ui;
use makina_core::api::{
    Api, ApiError, Command, CommandOutcome, EventStream, RunId, RunStatus, RunView, TaskId,
    TaskState, TaskView,
};
use makina_core::orchestrator::{PlanEntry, PlanTaskPreview};
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
                entry_text: String::new(),
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
                entry_text: String::new(),
            },
        ],
        report: makina_core::api::IngestionReport::default(),
    };
    App::new(api, vec![run], PathBuf::from("."))
}

/// Build an `App` with one running run and discovered plans.
fn make_app_with_tasks_and_plans() -> App {
    let api: Arc<dyn Api> = Arc::new(TestApi);
    let run = RunView {
        id: RunId(1),
        run_uid: "run1".to_string(),
        task_list_path: PathBuf::from("tasks.md"),
        status: RunStatus::Running,
        project: "test-project".to_string(),
        tasks: vec![TaskView {
            id: TaskId::new("task-1"),
            title: "First Task".into(),
            state: TaskState::InProgress,
            gate_iterations: 0,
            review_iterations: 0,
            depends_on: vec![],
            started_at: None,
            finished_at: None,
            failure_reason: None,
            entry_text: String::new(),
        }],
        report: makina_core::api::IngestionReport::default(),
    };
    let mut app = App::new(api, vec![run], PathBuf::from("."));

    // Add a discovered plan
    let plan = PlanEntry {
        slug: "0001-test-plan".to_string(),
        dir: PathBuf::from("docs/plans/0001-test"),
        has_tasks: true,
        tasks: vec![],
        scope_text: Some("This is a test plan.".to_string()),
        architecture_text: Some("Architecture details.".to_string()),
        status_text: Some("Status: complete.".to_string()),
    };
    app.discovered_plans.push(plan);

    app
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

#[test]
fn render_task_and_plan_tabs_together() {
    let mut terminal = {
        let backend = TestBackend::new(100, 24);
        Terminal::new(backend).unwrap()
    };
    let mut app = make_app_with_tasks_and_plans();

    // Open a task tab
    app.tabs.open_tab(TabContent::Task {
        plan_slug: "0001".to_string(),
        task_id: TaskId("task-1".to_string()),
    });

    // Open a plan tab
    app.tabs.open_tab(TabContent::Plan {
        plan_slug: "0001-test-plan".to_string(),
    });

    terminal.draw(|frame| ui::render(&app, frame)).unwrap();
    let screen = screen_of(&terminal);

    // Both task and plan tabs should appear in the rendered output
    assert!(screen.contains("task-1"), "tab bar should show task tab");
    assert!(
        screen.contains("0001-test-plan"),
        "tab bar should show plan tab"
    );

    // Verify that the plan tab is active (it was the last one opened)
    assert_eq!(
        app.tabs.active_tab,
        Some(1),
        "plan tab should be active after opening"
    );

    // Check that the buffer has style information showing the active tab is the plan tab
    let buffer = terminal.backend().buffer();
    let buffer_content = buffer.content();

    // Look for cells with the active tab styling (Cyan background)
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

/// Integration test: Plan Tabs Render with Accordion Sections
#[test]
fn plan_tabs_render_accordion_sections_without_panic() {
    let mut terminal = {
        let backend = TestBackend::new(120, 30);
        Terminal::new(backend).unwrap()
    };

    let api = Arc::new(TestApi);
    let mut app = App::new(api, vec![], PathBuf::from("."));

    let plan = PlanEntry {
        slug: "0031-test".to_string(),
        dir: PathBuf::from("docs/plans/0031"),
        has_tasks: true,
        tasks: vec![
            PlanTaskPreview {
                id: "task-1".to_string(),
                title: "First task".to_string(),
                gated: false,
                depends_on: vec![],
            },
            PlanTaskPreview {
                id: "task-2".to_string(),
                title: "Second task (GATED)".to_string(),
                gated: true,
                depends_on: vec!["task-1".to_string()],
            },
        ],
        scope_text: Some("This plan improves sidebar navigation.".to_string()),
        architecture_text: Some("Three workstreams...".to_string()),
        status_text: Some("✅ Complete.".to_string()),
    };
    app.discovered_plans.push(plan);

    // Open the plan tab
    app.tabs.open_tab(TabContent::Plan {
        plan_slug: "0031-test".to_string(),
    });

    // Expand all sections
    let expanded = app
        .accordion_state
        .entry("0031-test".to_string())
        .or_default();
    expanded.insert(AccordionSection::Scope);
    expanded.insert(AccordionSection::Architecture);
    expanded.insert(AccordionSection::Tasks);
    expanded.insert(AccordionSection::Status);

    // Render (should not panic)
    terminal.draw(|frame| ui::render(&app, frame)).unwrap();

    // Verify output contains section headers and content
    let screen = screen_of(&terminal);
    assert!(
        screen.contains("[-] SCOPE"),
        "SCOPE section should show expanded marker"
    );
    assert!(
        screen.contains("[-] ARCHITECTURE"),
        "ARCHITECTURE section should show expanded marker"
    );
    assert!(
        screen.contains("[-] TASKS"),
        "TASKS section should show expanded marker"
    );
    assert!(
        screen.contains("task-1"),
        "Task 1 should be visible in expanded TASKS"
    );
    assert!(
        screen.contains("GATED"),
        "Gated task marker should be visible"
    );
    assert!(
        screen.contains("[-] STATUS"),
        "STATUS section should show expanded marker"
    );
}

/// The real end-user flow: a discovered plan with task previews and **no runs**.
/// Opening the plan opens its plan tab; opening each task preview opens its OWN
/// distinct task tab (not a dedup of the plan tab), and the content pane shows
/// that task's detail. This is the behaviour that was missing — previously a
/// plan-task click resolved to the single plan tab, so no new tabs appeared.
#[test]
fn opening_plan_tasks_creates_distinct_task_tabs() {
    use makina::app::{AppEvent, TreeNode};

    let api: Arc<dyn Api> = Arc::new(TestApi);
    let mut app = App::new(api, vec![], PathBuf::from("."));
    app.discovered_plans.push(PlanEntry {
        slug: "0007-demo".to_string(),
        dir: PathBuf::from("docs/plans/0007"),
        has_tasks: true,
        tasks: vec![
            PlanTaskPreview {
                id: "wire-thing".to_string(),
                title: "Wire the thing".to_string(),
                gated: false,
                depends_on: vec![],
            },
            PlanTaskPreview {
                id: "gate-thing".to_string(),
                title: "Gate the thing".to_string(),
                gated: true,
                depends_on: vec!["wire-thing".to_string()],
            },
        ],
        scope_text: Some("scope".to_string()),
        architecture_text: None,
        status_text: None,
    });

    // Visible nodes: [Plan, PlanTask(wire), PlanTask(gate)] (plan expanded).
    let nodes = app.visible_tree_nodes();
    assert!(matches!(nodes[0], TreeNode::Plan { .. }));
    assert!(matches!(nodes[1], TreeNode::PlanTask { .. }));

    // Open the plan → exactly one plan tab.
    app.update(AppEvent::OpenTreeRow(0));
    assert_eq!(app.tabs.open_tabs.len(), 1);
    assert!(matches!(app.tabs.open_tabs[0], TabContent::Plan { .. }));

    // Open the first task preview → a SECOND, distinct task tab (the bug fix).
    app.update(AppEvent::OpenTreeRow(1));
    assert_eq!(
        app.tabs.open_tabs.len(),
        2,
        "a task preview must open its own tab, not refocus the plan tab"
    );
    assert!(
        matches!(&app.tabs.open_tabs[1], TabContent::PlanTask { task_id, .. } if task_id == "wire-thing")
    );
    assert_eq!(app.tabs.active_tab, Some(1), "the new task tab is active");

    // Open the second task preview → a THIRD tab.
    app.update(AppEvent::OpenTreeRow(2));
    assert_eq!(app.tabs.open_tabs.len(), 3);
    assert!(
        matches!(&app.tabs.open_tabs[2], TabContent::PlanTask { task_id, .. } if task_id == "gate-thing")
    );

    // Re-opening an already-open task focuses it (no duplicate).
    app.update(AppEvent::OpenTreeRow(1));
    assert_eq!(
        app.tabs.open_tabs.len(),
        3,
        "re-opening a task focuses its tab"
    );
    assert_eq!(app.tabs.active_tab, Some(1));

    // Render: the active task tab's content pane shows the task detail, and the
    // tab bar lists every open tab.
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| ui::render(&app, frame)).unwrap();
    let screen = screen_of(&terminal);
    assert!(
        screen.contains("wire-thing"),
        "active task tab renders its id"
    );
    assert!(
        screen.contains("Wire the thing"),
        "active task tab renders its title in the content pane"
    );
    assert!(
        screen.contains("gate-thing"),
        "tab bar shows the other task tab"
    );
    assert!(screen.contains("0007-demo"), "tab bar shows the plan tab");
}

/// Rendering records clickable bounds for every tab chip and every visible
/// sidebar row, so the event loop can turn a mouse click into an `ActivateTab`
/// / `OpenTreeRow` event.
#[test]
fn render_records_tab_and_sidebar_click_bounds() {
    let mut terminal = {
        let backend = TestBackend::new(100, 24);
        Terminal::new(backend).unwrap()
    };
    let mut app = make_app_with_tasks_and_plans();

    // Open a task tab and a plan tab so the tab bar has two chips.
    app.tabs.open_tab(TabContent::Task {
        plan_slug: "0001".to_string(),
        task_id: TaskId("task-1".to_string()),
    });
    app.tabs.open_tab(TabContent::Plan {
        plan_slug: "0001-test-plan".to_string(),
    });

    terminal.draw(|frame| ui::render(&app, frame)).unwrap();

    // One clickable bound per open tab, each exactly one row tall.
    let tab_bounds = app.tab_bounds.borrow();
    assert_eq!(tab_bounds.len(), 2, "two tab chips recorded");
    let mut indices: Vec<usize> = tab_bounds.iter().map(|(i, _)| *i).collect();
    indices.sort_unstable();
    assert_eq!(indices, vec![0, 1], "chip indices map to open-tab indices");
    for (_, r) in tab_bounds.iter() {
        assert_eq!(r.height, 1, "each tab chip is one row");
        assert!(r.width > 0, "each tab chip has positive width");
    }

    // One clickable bound per visible sidebar row.
    let node_bounds = app.sidebar_node_bounds.borrow();
    let visible = app.visible_tree_nodes().len();
    assert_eq!(
        node_bounds.len(),
        visible,
        "one clickable bound per visible sidebar row"
    );
    for (idx, r) in node_bounds.iter() {
        assert!(*idx < visible, "node index in range");
        assert_eq!(r.height, 1, "each sidebar row is one row tall");
    }
}
