//! Integration tests for `integration-test-tab-traversal`.
//!
//! Verifies that Tab and Shift+Tab navigate through Sidebar → Main → accordion
//! sections (when a plan tab is active) with correct focus state transitions.
//! Tests document the expected Tab/Shift+Tab behavior: forward/backward traversal
//! through regions, accordion section cycling, wrapping at boundaries, and correct
//! handling when no plan tab is active.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use makina::app::{AccordionSection, App, AppEvent, Panel, TabContent};
use makina::ui;
use makina_core::api::{
    Api, ApiError, Command, CommandOutcome, EventStream, RunId, RunView, TaskId,
};
use makina_core::orchestrator::{PlanEntry, PlanTaskPreview};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Minimal `Api` double for the integration test.
struct PlaceholderApi;

#[async_trait]
impl Api for PlaceholderApi {
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

/// Build an `App` with a discovered plan.
fn make_app_with_plan() -> App {
    let api: Arc<dyn Api> = Arc::new(PlaceholderApi);
    let mut app = App::new(api, vec![], PathBuf::from("."));

    // Add a discovered plan with all sections
    let plan = PlanEntry {
        slug: "0034-test-plan".to_string(),
        dir: PathBuf::from("docs/plans/0034-test"),
        has_tasks: true,
        tasks: vec![
            PlanTaskPreview {
                id: "task-1".to_string(),
                title: "First task".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            },
            PlanTaskPreview {
                id: "task-2".to_string(),
                title: "Second task".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            },
        ],
        scope_text: Some("This plan implements tab-based focus navigation.".to_string()),
        architecture_text: Some("Architecture involves extending the focus model.".to_string()),
        status_text: Some("Status: Complete.".to_string()),
    };
    app.discovered_plans.push(plan);

    app
}

/// Test: Tab navigates Sidebar → Main when no plan tab is active
#[test]
fn test_tab_sidebar_to_main_no_plan() {
    let api = Arc::new(PlaceholderApi);
    let mut app = App::new(api, vec![], PathBuf::from("."));

    // Start with focus in Sidebar
    assert_eq!(
        app.focused_panel,
        Panel::Sidebar,
        "focus should start in Sidebar"
    );
    assert_eq!(
        app.focused_section, None,
        "focused_section should be None in Sidebar"
    );

    // Tab once: Sidebar → Main
    app.update(AppEvent::FocusNext);

    assert_eq!(
        app.focused_panel,
        Panel::Main,
        "Tab should move focus to Main"
    );
    assert_eq!(
        app.focused_section, None,
        "focused_section should remain None when entering Main without a plan tab"
    );

    // Tab again: Main → Sidebar (no plan tab, so wrap directly)
    app.update(AppEvent::FocusNext);

    assert_eq!(
        app.focused_panel,
        Panel::Sidebar,
        "Tab from Main with no plan tab should wrap to Sidebar"
    );
    assert_eq!(
        app.focused_section, None,
        "focused_section should remain None"
    );
}

/// Test: Tab navigates Sidebar → Main → accordion sections → Sidebar with plan tab active
#[test]
fn test_tab_full_cycle_with_plan_tab() {
    let mut app = make_app_with_plan();

    // Open a plan tab
    app.tabs.open_tab(TabContent::Plan {
        plan_slug: "0034-test-plan".to_string(),
    });
    app.tabs.active_tab = Some(0);

    // Start in Sidebar
    assert_eq!(app.focused_panel, Panel::Sidebar);
    assert_eq!(app.focused_section, None);

    // Tab 1: Sidebar → Main (no section focus yet)
    app.update(AppEvent::FocusNext);
    assert_eq!(app.focused_panel, Panel::Main);
    assert_eq!(
        app.focused_section, None,
        "entering Main should not immediately focus a section"
    );

    // Tab 2: Main → Scope (first accordion section)
    app.update(AppEvent::FocusNext);
    assert_eq!(app.focused_panel, Panel::Main);
    assert_eq!(
        app.focused_section,
        Some(AccordionSection::Scope),
        "Tab from Main (no section) should focus Scope"
    );

    // Tab 3: Scope → Architecture
    app.update(AppEvent::FocusNext);
    assert_eq!(app.focused_panel, Panel::Main);
    assert_eq!(
        app.focused_section,
        Some(AccordionSection::Architecture),
        "Tab from Scope should move to Architecture"
    );

    // Tab 4: Architecture → Tasks
    app.update(AppEvent::FocusNext);
    assert_eq!(app.focused_panel, Panel::Main);
    assert_eq!(
        app.focused_section,
        Some(AccordionSection::Tasks),
        "Tab from Architecture should move to Tasks"
    );

    // Tab 5: Tasks → Status
    app.update(AppEvent::FocusNext);
    assert_eq!(app.focused_panel, Panel::Main);
    assert_eq!(
        app.focused_section,
        Some(AccordionSection::Status),
        "Tab from Tasks should move to Status"
    );

    // Tab 6: Status → Sidebar (wrap to the beginning)
    app.update(AppEvent::FocusNext);
    assert_eq!(
        app.focused_panel,
        Panel::Sidebar,
        "Tab from Status should wrap to Sidebar"
    );
    assert_eq!(
        app.focused_section, None,
        "focused_section should be None after wrapping to Sidebar"
    );

    // Tab 7: Sidebar → Main (cycle back)
    app.update(AppEvent::FocusNext);
    assert_eq!(app.focused_panel, Panel::Main);
    assert_eq!(app.focused_section, None);
}

/// Test: Shift+Tab reverses forward traversal from Sidebar
#[test]
fn test_shift_tab_sidebar_to_status_with_plan_tab() {
    let mut app = make_app_with_plan();

    // Open a plan tab
    app.tabs.open_tab(TabContent::Plan {
        plan_slug: "0034-test-plan".to_string(),
    });
    app.tabs.active_tab = Some(0);

    // Start in Sidebar
    assert_eq!(app.focused_panel, Panel::Sidebar);
    assert_eq!(app.focused_section, None);

    // Shift+Tab from Sidebar with plan tab active should jump to Status (last section)
    app.update(AppEvent::FocusPrev);
    assert_eq!(app.focused_panel, Panel::Main);
    assert_eq!(
        app.focused_section,
        Some(AccordionSection::Status),
        "Shift+Tab from Sidebar with plan tab should jump to Status"
    );
}

/// Test: Shift+Tab reverses full backward traversal through accordion sections
#[test]
fn test_shift_tab_full_reverse_with_plan_tab() {
    let mut app = make_app_with_plan();

    // Open a plan tab
    app.tabs.open_tab(TabContent::Plan {
        plan_slug: "0034-test-plan".to_string(),
    });
    app.tabs.active_tab = Some(0);

    // Start in Status (last section)
    app.focused_panel = Panel::Main;
    app.focused_section = Some(AccordionSection::Status);

    // Shift+Tab 1: Status → Tasks
    app.update(AppEvent::FocusPrev);
    assert_eq!(app.focused_panel, Panel::Main);
    assert_eq!(
        app.focused_section,
        Some(AccordionSection::Tasks),
        "Shift+Tab from Status should move to Tasks"
    );

    // Shift+Tab 2: Tasks → Architecture
    app.update(AppEvent::FocusPrev);
    assert_eq!(app.focused_panel, Panel::Main);
    assert_eq!(
        app.focused_section,
        Some(AccordionSection::Architecture),
        "Shift+Tab from Tasks should move to Architecture"
    );

    // Shift+Tab 3: Architecture → Scope
    app.update(AppEvent::FocusPrev);
    assert_eq!(app.focused_panel, Panel::Main);
    assert_eq!(
        app.focused_section,
        Some(AccordionSection::Scope),
        "Shift+Tab from Architecture should move to Scope"
    );

    // Shift+Tab 4: Scope → Sidebar
    app.update(AppEvent::FocusPrev);
    assert_eq!(
        app.focused_panel,
        Panel::Sidebar,
        "Shift+Tab from Scope should exit to Sidebar"
    );
    assert_eq!(
        app.focused_section, None,
        "focused_section should be None in Sidebar"
    );
}

/// Test: Shift+Tab from Main with no section focused moves to Sidebar (no plan tab active)
#[test]
fn test_shift_tab_main_to_sidebar_no_plan() {
    let api = Arc::new(PlaceholderApi);
    let mut app = App::new(api, vec![], PathBuf::from("."));

    // Start in Main with no section focus and no plan tab
    app.focused_panel = Panel::Main;
    app.focused_section = None;

    // Shift+Tab should move to Sidebar
    app.update(AppEvent::FocusPrev);

    assert_eq!(
        app.focused_panel,
        Panel::Sidebar,
        "Shift+Tab from Main (no plan tab) should move to Sidebar"
    );
    assert_eq!(app.focused_section, None, "focused_section should be None");
}

/// Test: Shift+Tab from Sidebar with no plan tab is a no-op
#[test]
fn test_shift_tab_sidebar_noop_no_plan() {
    let api = Arc::new(PlaceholderApi);
    let mut app = App::new(api, vec![], PathBuf::from("."));

    // Start in Sidebar with no plan tab
    assert_eq!(app.focused_panel, Panel::Sidebar);

    // Shift+Tab should be a no-op
    app.update(AppEvent::FocusPrev);

    assert_eq!(
        app.focused_panel,
        Panel::Sidebar,
        "Shift+Tab from Sidebar with no plan tab should not move"
    );
    assert_eq!(
        app.focused_section, None,
        "focused_section should remain None"
    );
}

/// Test: Shift+Tab from Main with no section focused but with plan tab active
#[test]
fn test_shift_tab_main_to_status_with_plan_tab() {
    let mut app = make_app_with_plan();

    // Open a plan tab
    app.tabs.open_tab(TabContent::Plan {
        plan_slug: "0034-test-plan".to_string(),
    });
    app.tabs.active_tab = Some(0);

    // Start in Main with no section focus
    app.focused_panel = Panel::Main;
    app.focused_section = None;

    // Shift+Tab should jump to Status (last section)
    app.update(AppEvent::FocusPrev);

    assert_eq!(app.focused_panel, Panel::Main);
    assert_eq!(
        app.focused_section,
        Some(AccordionSection::Status),
        "Shift+Tab from Main (no section) with plan tab should jump to Status"
    );
}

/// Test: Tab wrapping at boundaries continues cycling
#[test]
fn test_tab_wrapping_multiple_cycles() {
    let mut app = make_app_with_plan();

    // Open a plan tab
    app.tabs.open_tab(TabContent::Plan {
        plan_slug: "0034-test-plan".to_string(),
    });
    app.tabs.active_tab = Some(0);

    // Do a full cycle: Sidebar → Main → Scope → Architecture → Tasks → Status → Sidebar
    app.update(AppEvent::FocusNext); // Sidebar → Main
    app.update(AppEvent::FocusNext); // Main → Scope
    app.update(AppEvent::FocusNext); // Scope → Architecture
    app.update(AppEvent::FocusNext); // Architecture → Tasks
    app.update(AppEvent::FocusNext); // Tasks → Status
    app.update(AppEvent::FocusNext); // Status → Sidebar

    // Now we're back in Sidebar; continue cycling
    assert_eq!(app.focused_panel, Panel::Sidebar);

    app.update(AppEvent::FocusNext); // Sidebar → Main
    assert_eq!(app.focused_panel, Panel::Main);
    assert_eq!(app.focused_section, None);

    app.update(AppEvent::FocusNext); // Main → Scope
    assert_eq!(
        app.focused_section,
        Some(AccordionSection::Scope),
        "second cycle should also move through sections"
    );
}

/// Test: Visual feedback via rendering when a section is focused
#[test]
fn test_focused_section_visual_indicator() {
    let mut terminal = {
        let backend = TestBackend::new(120, 30);
        Terminal::new(backend).unwrap()
    };

    let mut app = make_app_with_plan();

    // Open a plan tab
    app.tabs.open_tab(TabContent::Plan {
        plan_slug: "0034-test-plan".to_string(),
    });
    app.tabs.active_tab = Some(0);

    // Expand all sections
    let expanded = app
        .accordion_state
        .entry("0034-test-plan".to_string())
        .or_default();
    expanded.insert(AccordionSection::Scope);
    expanded.insert(AccordionSection::Architecture);
    expanded.insert(AccordionSection::Tasks);
    expanded.insert(AccordionSection::Status);

    // Move focus to Scope section
    app.focused_panel = Panel::Main;
    app.focused_section = Some(AccordionSection::Scope);

    // Render and check that content is displayed
    terminal.draw(|frame| ui::render(&app, frame)).unwrap();

    let buffer = terminal.backend().buffer();
    let buffer_content = buffer.content();

    // Look for SCOPE header in the rendered output
    let screen_text: String = buffer_content
        .iter()
        .map(|c| c.symbol().chars().next().unwrap_or(' '))
        .collect();

    assert!(
        screen_text.contains("SCOPE"),
        "rendered output should contain SCOPE header"
    );
    assert!(
        screen_text.contains("tab-based focus navigation"),
        "SCOPE section content should be visible"
    );

    // Now move focus to Architecture and re-render
    app.focused_section = Some(AccordionSection::Architecture);
    terminal.draw(|frame| ui::render(&app, frame)).unwrap();

    let buffer = terminal.backend().buffer();
    let buffer_content = buffer.content();
    let screen_text: String = buffer_content
        .iter()
        .map(|c| c.symbol().chars().next().unwrap_or(' '))
        .collect();

    assert!(
        screen_text.contains("ARCHITECTURE"),
        "rendered output should contain ARCHITECTURE header"
    );
    assert!(
        screen_text.contains("focus model"),
        "ARCHITECTURE section content should be visible"
    );
}

/// Test: Tab with plan tab not active (switched to different tab type)
#[test]
fn test_tab_behavior_when_plan_tab_not_active() {
    let mut app = make_app_with_plan();

    // Open a plan tab but then open a task tab
    app.tabs.open_tab(TabContent::Plan {
        plan_slug: "0034-test-plan".to_string(),
    });

    // Open a task tab (making it active)
    app.tabs.open_tab(TabContent::Task {
        plan_slug: "0034-test-plan".to_string(),
        task_id: TaskId::new("task-1"),
    });

    // Verify that the task tab is now active, not the plan tab
    assert_eq!(app.tabs.active_tab, Some(1));
    let active = app.tabs.active_tab.and_then(|i| app.tabs.open_tabs.get(i));
    assert!(
        active.is_some_and(|t| matches!(t, TabContent::Task { .. })),
        "active tab should be the task tab"
    );

    // Move to Main
    app.focused_panel = Panel::Main;
    app.focused_section = None;

    // Tab from Main should wrap to Sidebar (no plan tab active)
    app.update(AppEvent::FocusNext);

    assert_eq!(
        app.focused_panel,
        Panel::Sidebar,
        "Tab from Main with no active plan tab should wrap to Sidebar"
    );
    assert_eq!(
        app.focused_section, None,
        "focused_section should remain None"
    );
}

/// Test: Shift+Tab from Main (no section) with plan tab not active
#[test]
fn test_shift_tab_main_no_plan_active() {
    let mut app = make_app_with_plan();

    // Open a plan tab but then make a non-plan tab active
    app.tabs.open_tab(TabContent::Plan {
        plan_slug: "0034-test-plan".to_string(),
    });
    app.tabs.open_tab(TabContent::Task {
        plan_slug: "0034-test-plan".to_string(),
        task_id: TaskId::new("task-1"),
    });

    // Start in Main with no section focus
    app.focused_panel = Panel::Main;
    app.focused_section = None;

    // Shift+Tab should move to Sidebar (no plan tab active)
    app.update(AppEvent::FocusPrev);

    assert_eq!(
        app.focused_panel,
        Panel::Sidebar,
        "Shift+Tab from Main with no active plan tab should move to Sidebar"
    );
}
