# XAgent Plan 0031 — Sidebar-Unified Plan-Task Tree & Tabbed Content Navigation

This plan executes three interconnected workstreams: WS1 (`0001-Sidebar-Unified-Plan-Task-Tree`) integrates the modal plan picker into the always-present sidebar, making discovered plans top-level tree nodes that expand inline to reveal their tasks, eliminating the mode-switching cost. WS2 (`0002-Tabbed-Content-Pane`) replaces the fixed single detail/exchange pane with a tab-bar UI, allowing users to view and edit multiple tasks/plans in parallel without losing focus. WS3 (`0003-Model-Normalized-TASKS-Ingestion`) adds a model-driven normalizer that validates and repairs malformed or missing TASKS.md before the deterministic interpreter runs, generalizing plan 0028's generate-when-missing into a normalize-on-ingestion pattern that hardens the plan-opening path and keeps the deterministic-governance wedge (model assistance only at the front door).

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md) for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}`).
- **Depends on** lists *direct* prerequisites only; "—" means none.
- **Done when** is the verifiable criterion, and every task keeps the gate
  commands green — the full forms are `cargo test`,
  `cargo clippy -- -D warnings`, and `cargo fmt --check`; abbreviated as
  "cargo test/clippy/fmt green" thereafter.
- GPU tests self-skip without an adapter.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0001 — Sidebar Unified Plan-Task Tree

### tree-node-plan-variant — Add Plan Variant to TreeNode Enum

The `TreeNode` enum in `crates/makina/src/app.rs:581` currently has two variants: `Run { run: usize }` and `Task { run: usize, task: usize }`. These represent the two levels of the sidebar tree (runs and their tasks). To integrate discovered plans as top-level tree nodes, the enum must be extended with a third variant that identifies a plan by its index in the `discovered_plans` vector.

**Steps:**

1. In `crates/makina/src/app.rs`, locate the `TreeNode` enum definition (around line 581).
2. Add a new variant: `Plan { plan_idx: usize }` with a doc comment: /// A discovered plan at `discovered_plans[plan_idx]`.
3. Verify the enum definition now reads:
   ```rust
   pub enum TreeNode {
       /// The run at `runs[run]`.
       Run { run: usize },
       /// Task at `runs[run].tasks[task]`.
       Task { run: usize, task: usize },
       /// A discovered plan at `discovered_plans[plan_idx]`.
       Plan { plan_idx: usize },
   }
   ```

- **Depends on:** —
- **Done when:** the `TreeNode` enum compiles with the new `Plan { plan_idx: usize }` variant; cargo test/clippy/fmt green.

---

### add-collapsed-plans-state — Add collapsed_plans State to App

The App struct currently tracks `collapsed_runs: HashSet<RunId>` to remember which runs are collapsed in the sidebar tree. To support expandable/collapsible discovered plans (parallel to runs), a similar `collapsed_plans: HashSet<usize>` is needed to track which plans the user has collapsed by plan index.

**Steps:**

1. In `crates/makina/src/app.rs`, locate the `App` struct definition.
2. Find the `collapsed_runs: HashSet<RunId>` field (around line 930s).
3. Add a new field immediately after it:
   ```rust
   /// Plan indices currently collapsed in the sidebar tree (excludes expanded plans).
   /// Parallel to `collapsed_runs` but keyed by index into `discovered_plans`.
   pub collapsed_plans: HashSet<usize>,
   ```
4. In `App::new()`, initialize this new field to `HashSet::new()`.

- **Depends on:** tree-node-plan-variant
- **Done when:** the App struct compiles with the new `collapsed_plans` field initialized in `new()`; cargo test/clippy/fmt green.

---

### update-visible-tree-nodes-builder — Integrate Plans into visible_tree_nodes Builder

The `visible_tree_nodes` method in `crates/makina/src/app.rs:1139` flattens the run/task hierarchy into a linear `Vec<TreeNode>` for cursor-based sidebar navigation. To show discovered plans as top-level tree nodes (before runs), the builder must iterate over `discovered_plans` and insert plan nodes, then expand plan tasks if the plan is not collapsed.

**Steps:**

1. In `crates/makina/src/app.rs`, locate the `visible_tree_nodes` method (around line 1139).
2. Rewrite the method body to prepend discovered plans:
   ```rust
   pub fn visible_tree_nodes(&self) -> Vec<TreeNode> {
       let mut nodes = Vec::new();
       // Add discovered plans at the top, optionally expanded.
       for (plan_idx, plan) in self.discovered_plans.iter().enumerate() {
           nodes.push(TreeNode::Plan { plan_idx });
           // TODO (task tui-plan-tasks): if not collapsed, add plan's tasks as nested nodes.
           // For now, plans are leaf nodes; tasks are only shown after expansion is implemented.
       }
       // Add open runs and their expanded tasks (existing logic).
       for (run_idx, run) in self.runs.iter().enumerate() {
           nodes.push(TreeNode::Run { run: run_idx });
           if !self.collapsed_runs.contains(&run.id) {
               for task_idx in 0..run.tasks.len() {
                   nodes.push(TreeNode::Task {
                       run: run_idx,
                       task: task_idx,
                   });
               }
           }
       }
       nodes
   }
   ```

- **Depends on:** tree-node-plan-variant, add-collapsed-plans-state
- **Done when:** the `visible_tree_nodes` method compiles and returns a `Vec<TreeNode>` that includes discovered plans before open runs; sidebar tree renders with plan nodes appearing at the top; cargo test/clippy/fmt green.

---

### extend-sidebar-rendering-for-plans — Extend Sidebar Rendering to Show Plan Nodes

The sidebar rendering loop in `crates/makina/src/ui.rs:156–237` builds `ListItem`s for each `TreeNode`, currently handling `Run` and `Task` variants. When the new `Plan` variant is added, the renderer must handle it to show the plan's slug, a disclosure glyph (▸/▾), and an optional dim label for plans without tasks.

**Steps:**

1. In `crates/makina/src/ui.rs`, locate the tree-node rendering loop inside the sidebar block (around line 158–217).
2. In the `items` iterator's match statement, add a new arm for `TreeNode::Plan { plan_idx }`:
   ```rust
   TreeNode::Plan { plan_idx } => {
       let plan_entry = &app.discovered_plans[*plan_idx];
       let disclosure = if app.collapsed_plans.contains(plan_idx) {
           "▸ "
       } else {
           "▾ "
       };
       let mut line_spans = vec![
           Span::raw(disclosure),
           Span::raw(&plan_entry.slug),
       ];
       // Append "(no tasks — will plan)" hint for plans without TASKS.md.
       if !plan_entry.has_tasks {
           line_spans.push(Span::styled(
               " (no tasks — will plan)",
               Style::default().fg(Color::DarkGray),
           ));
       }
       let line = Line::from(line_spans);
       ListItem::new(line)
   }
   ```
3. Verify the sidebar renders plan nodes with disclosure glyphs and dim labels when appropriate.

- **Depends on:** update-visible-tree-nodes-builder
- **Done when:** the sidebar renders discovered plans as top-level tree nodes with disclosure glyphs and dim labels; plan nodes appear before open runs; cargo test/clippy/fmt green.

---

### remove-plan-picker-mode — Remove Mode::PlanPicker Modal and Related State

`Mode::PlanPicker` (`crates/makina/src/app.rs:417`) is the modal mode that overlays the plan-picker widget. Once plans are integrated into the sidebar tree, this mode and its rendering path are no longer needed. The `plan_cursor` state (for tracking the selected plan in the modal) can also be removed.

**Steps:**

1. In `crates/makina/src/app.rs`, locate the `Mode` enum (around line 403).
2. Remove the `PlanPicker,` variant.
3. Remove the `plan_cursor: usize` field from the `App` struct (around line 1107); plans are now navigated via the `tree_cursor` in the unified sidebar.
4. In `crates/makina/src/ui.rs`, locate the plan-picker rendering call (around line 115–117):
   ```rust
   if app.is_picking_plan() {
       render_plan_picker(app, frame, sidebar_area);
   } else {
       // render normal sidebar
   }
   ```
   Remove this conditional and always render the normal sidebar.
5. Optionally remove the `render_plan_picker` function and the `is_picking_plan()` helper method from App if they are no longer used elsewhere.

- **Depends on:** extend-sidebar-rendering-for-plans
- **Done when:** the `Mode::PlanPicker` variant is removed; `plan_cursor` state is gone; `render_plan_picker` is no longer called; the sidebar always shows the unified tree; cargo test/clippy/fmt green.

---

### add-plan-open-keybind — Add Keybind Handler for Opening Plans from Sidebar

When a user navigates to a `TreeNode::Plan` in the sidebar and presses Enter, the app should open that plan (invoking `CoreApi::open_run` with the plan dir's TASKS.md path). The event handler must distinguish between `TreeNode::Run`, `TreeNode::Task`, and `TreeNode::Plan` and dispatch the appropriate action.

**Steps:**

1. In `crates/makina/src/app.rs`, locate the `App::focused_node()` method (around line 1157).
2. In `App::update()` (the event handler), find the `AppEvent::ToggleTreeNode` / `AppEvent::SelectItem` handler (search for 'Enter' / 'Return' key handling).
3. Update the handler to match on `focused_node()` and dispatch accordingly:
   ```rust
   if let Some(focused_node) = self.focused_node() {
       match focused_node {
           TreeNode::Run { run } => { /* existing code: select run */ }
           TreeNode::Task { run, task } => { /* existing code: select task */ }
           TreeNode::Plan { plan_idx } => {
               // Dispatch open plan event
               return Some(AppEvent::OpenPlan(plan_idx));
           }
       }
   }
   ```
4. Handle the new `AppEvent::OpenPlan(plan_idx)` in the event loop (search for where `AppEvent::` patterns are matched):
   ```rust
   AppEvent::OpenPlan(plan_idx) => {
       if let Some(plan) = app.discovered_plans.get(plan_idx) {
           // Call API to open the plan's TASKS.md
           let task_list_path = plan.dir.join("TASKS.md");
           // Dispatch to API: return Some(api::Command::OpenRun { path: task_list_path })
       }
   }
   ```

- **Depends on:** remove-plan-picker-mode
- **Done when:** pressing Enter on a discovered plan node opens the plan via `CoreApi::open_run`; the plan dir's TASKS.md is read and ingested; cargo test/clippy/fmt green.

---

### test-sidebar-plan-integration — Test: Sidebar Shows Discovered Plans

A baseline test verifies that the sidebar tree correctly includes discovered plans before open runs, and that they render with disclosure glyphs.

**Steps:**

1. In `crates/makina/tests/` (or in the UI test suite), add a test:
   ```rust
   #[test]
   fn sidebar_shows_discovered_plans_before_runs() {
       let api = Arc::new(PlaceholderApi::new());
       let discovered_plans = vec![
           makina_core::orchestrator::PlanEntry {
               dir: PathBuf::from("docs/plans/0001-test"),
               slug: "0001-test".to_string(),
               has_tasks: true,
           },
       ];
       let mut app = App::new(api, vec![], PathBuf::from("."));
       app.discovered_plans = discovered_plans;

       let nodes = app.visible_tree_nodes();
       assert!(!nodes.is_empty());
       assert!(matches!(nodes[0], TreeNode::Plan { plan_idx: 0 }));
   }
   ```
2. Run the test: `cargo test sidebar_shows_discovered_plans_before_runs`

- **Depends on:** remove-plan-picker-mode, add-tab-content-enum
- **Done when:** the test passes; discovered plans appear in `visible_tree_nodes()` before open runs; the first node is a `TreeNode::Plan` when plans are discovered; cargo test/clippy/fmt green.

---

### integration-plan-open-from-sidebar — Integration: Open a Discovered Plan from Sidebar

An end-to-end integration test verifies that navigating to a discovered plan in the sidebar and pressing Enter opens the plan via `CoreApi::open_run()`.

**Steps:**

1. In `crates/makina-core/tests/` or `crates/makina/tests/`, create a test that:
   1. Instantiates the app with at least one discovered plan.
   2. Navigates the tree cursor to the plan node.
   3. Simulates an "open" keybind (Enter).
   4. Verifies that a `CoreApi::open_run()` call is dispatched with the plan's TASKS.md path.

   ```rust
   #[tokio::test]
   async fn can_open_discovered_plan_from_sidebar() {
       let api = Arc::new(TestApi::new());
       let discovered = vec![
           PlanEntry { dir: PathBuf::from("docs/plans/0001-test"), slug: "0001-test".to_string(), has_tasks: true },
       ];
       let mut app = App::new(api.clone(), vec![], PathBuf::from("."));
       app.discovered_plans = discovered;

       // Navigate to plan (tree_cursor points to plan node)
       let nodes = app.visible_tree_nodes();
       app.tree_cursor = if matches!(nodes[0], TreeNode::Plan { .. }) { Some(0) } else { None };

       // Simulate Enter on plan node
       // (This should dispatch AppEvent::OpenPlan or similar)
       // Verify that the API received an open_run call
       // ...
   }
   ```

- **Depends on:** test-sidebar-plan-integration
- **Done when:** the test passes; navigating to a discovered plan and pressing Enter opens the plan's TASKS.md via the API; the plan transitions to the open runs list; cargo test/clippy/fmt green.

---

## 0002 — Tabbed Content Pane

### add-tab-content-enum — Define TabContent Enum and Tab State

To support multiple open tabs in the main content pane, the App needs a data structure to identify and track which tabs are open. A `TabContent` enum represents the content of a tab (task or plan), and a `TabState` struct holds the set of open tabs and the currently active tab.

**Steps:**

1. In `crates/makina/src/app.rs`, before the `App` struct, add:
   ```rust
   /// Content displayed in a tab in the main pane.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
   pub enum TabContent {
       /// A task within a run: identified by plan slug and task ID.
       Task { plan_slug: String, task_id: TaskId },
       /// A discovered plan: identified by plan slug.
       Plan { plan_slug: String },
   }

   /// State for the tabbed content pane.
   #[derive(Debug, Clone)]
   pub struct TabState {
       /// Currently open tabs.
       pub open_tabs: Vec<TabContent>,
       /// Index of the active tab in `open_tabs`; `None` if no tabs are open.
       pub active_tab: Option<usize>,
   }

   impl TabState {
       pub fn new() -> Self {
           TabState {
               open_tabs: Vec::new(),
               active_tab: None,
           }
       }

       /// Open a new tab or switch to it if already open.
       pub fn open_tab(&mut self, content: TabContent) {
           if let Some(idx) = self.open_tabs.iter().position(|t| t == &content) {
               self.active_tab = Some(idx);
           } else {
               self.open_tabs.push(content);
               self.active_tab = Some(self.open_tabs.len() - 1);
           }
       }

       /// Close the tab at the given index. If it was the active tab, switch to an adjacent tab.
       pub fn close_tab(&mut self, idx: usize) {
           if idx < self.open_tabs.len() {
               self.open_tabs.remove(idx);
               if self.open_tabs.is_empty() {
                   self.active_tab = None;
               } else if let Some(active) = self.active_tab {
                   if active >= self.open_tabs.len() {
                       self.active_tab = Some(self.open_tabs.len() - 1);
                   }
               }
           }
       }
   }
   ```

- **Depends on:** tree-node-plan-variant, add-collapsed-plans-state, add-tab-state-to-app, update-visible-tree-nodes-builder, remove-plan-picker-mode, add-plan-open-keybind, test-sidebar-plan-integration
- **Done when:** the `TabContent` enum and `TabState` struct compile; `TabState::new()` creates an empty tab state; `open_tab()` and `close_tab()` methods work correctly; cargo test/clippy/fmt green.

---

### add-tab-state-to-app — Add TabState to App Struct

The App struct must hold a `TabState` instance to manage the set of open tabs and the currently active tab.

**Steps:**

1. In `crates/makina/src/app.rs`, locate the `App` struct definition.
2. Add a new field after the `settings` field (around line 1074):
   ```rust
   /// State for the tabbed main content pane.
   pub tabs: TabState,
   ```
3. In `App::new()`, initialize the field:
   ```rust
   tabs: TabState::new(),
   ```

- **Depends on:** add-tab-content-enum
- **Done when:** the App struct compiles with the `tabs: TabState` field; `App::new()` initializes `tabs`; cargo test/clippy/fmt green.

---

### add-open-tab-event — Add AppEvent Variants for Tab Operations

The event system must convey user interactions related to tabs: opening a new tab, closing the active tab, and switching between tabs.

**Steps:**

1. In `crates/makina/src/app.rs`, locate the `AppEvent` enum (around line 595).
2. Add new variants:
   ```rust
   /// Open a new tab with the given content (or switch to it if already open).
   OpenTab(TabContent),
   /// Close the active tab.
   CloseTab,
   /// Switch to the next tab (or wrap to the first).
   NextTab,
   /// Switch to the previous tab (or wrap to the last).
   PrevTab,
   ```

- **Depends on:** add-tab-state-to-app
- **Done when:** the `AppEvent` enum compiles with the new tab-related variants; cargo test/clippy/fmt green.

---

### handle-tab-events-in-update — Handle Tab Events in App::update()

The `App::update()` method must handle the new tab-related `AppEvent` variants and update the `tabs` state accordingly.

**Steps:**

1. In `crates/makina/src/app.rs`, locate the `App::update()` method.
2. Add handlers for the new events in the match statement:
   ```rust
   AppEvent::OpenTab(content) => {
       self.tabs.open_tab(content);
   }
   AppEvent::CloseTab => {
       if let Some(active) = self.tabs.active_tab {
           self.tabs.close_tab(active);
       }
   }
   AppEvent::NextTab => {
       if !self.tabs.open_tabs.is_empty() {
           let next = (self.tabs.active_tab.unwrap_or(0) + 1) % self.tabs.open_tabs.len();
           self.tabs.active_tab = Some(next);
       }
   }
   AppEvent::PrevTab => {
       if !self.tabs.open_tabs.is_empty() {
           let prev = self.tabs.active_tab.unwrap_or(0).saturating_sub(1);
           self.tabs.active_tab = Some(prev);
       }
   }
   ```

- **Depends on:** add-open-tab-event
- **Done when:** tab events are handled in `App::update()` and correctly manipulate `tabs` state; opening a tab adds it to the list and makes it active; closing the active tab removes it; cargo test/clippy/fmt green.

---

### add-tab-bar-renderer — Add Tab Bar Renderer Function

The main pane must render a visual tab bar above the content area, showing the open tabs with the active tab highlighted. A new renderer function is needed to build and render the tab bar.

**Steps:**

1. In `crates/makina/src/ui.rs`, add a new function after the existing renderer helpers:
   ```rust
   /// Render the tab bar showing open tabs above the main content pane.
   fn render_tab_bar(app: &App, frame: &mut Frame, area: Rect) {
       if app.tabs.open_tabs.is_empty() {
           return; // No tabs to render
       }

       let mut spans = Vec::new();
       for (idx, tab) in app.tabs.open_tabs.iter().enumerate() {
           let label = match tab {
               TabContent::Task { task_id, .. } => task_id.0.clone(),
               TabContent::Plan { plan_slug } => plan_slug.clone(),
           };
           let is_active = app.tabs.active_tab == Some(idx);
           let style = if is_active {
               Style::default().bg(Color::Cyan).fg(Color::Black)
           } else {
               Style::default().bg(Color::DarkGray).fg(Color::White)
           };
           spans.push(Span::styled(format!(" {} ", label), style));
           spans.push(Span::raw(" "));
       }
       let para = Paragraph::new(Line::from(spans));
       frame.render_widget(para, area);
   }
   ```

- **Depends on:** extend-sidebar-rendering-for-plans
- **Done when:** the `render_tab_bar` function compiles and renders a visible tab bar with the active tab highlighted; cargo test/clippy/fmt green.

---

### integrate-tab-bar-into-main-pane — Integrate Tab Bar into Main Pane Rendering

The main pane renderer (`crates/makina/src/ui.rs:240–392`) must be updated to render the tab bar at the top, then the tab content below it.

**Steps:**

1. In `crates/makina/src/ui.rs`, locate the main pane rendering section where the vertical layout is split into header/ingestion/exchange/error panes (around line 349–357).
2. Update the layout constraints to reserve 1 row for the tab bar at the top:
   ```rust
   let split = Layout::default()
       .direction(Direction::Vertical)
       .constraints([
           Constraint::Length(1),                 // tab bar (new)
           Constraint::Length(header_height),
           Constraint::Length(ingestion_pane_height),
           Constraint::Min(3),                    // exchange pane
           Constraint::Length(error_pane_height),
       ])
       .split(inner);

   let tab_area = split[0];
   let header_area = split[1];
   let ingestion_area = split[2];
   let exchange_area = split[3];
   let error_area = split[4];
   ```
3. Call the tab bar renderer:
   ```rust
   render_tab_bar(app, frame, tab_area);
   ```

- **Depends on:** add-tab-bar-renderer
- **Done when:** the main pane renders a tab bar at the top; the tab bar shows open tabs with the active tab highlighted; cargo test/clippy/fmt green.

---

### route-task-selection-to-tabs — Route Sidebar Task Selection to Tab Opening

Currently, when a user selects a task in the sidebar, the `selected_task` state is updated. With tabs, pressing Enter on a task should instead open a new tab. The keybind handler must dispatch an `OpenTab` event rather than updating `selected_task` directly.

**Steps:**

1. In the event handler (likely `crates/makina/src/event.rs` or within `App::update()`), locate where 'Return' / 'Enter' key is handled on a task node.
2. Replace the `selected_task` update with:
   ```rust
   if let Some(TreeNode::Task { run, task }) = app.focused_node() {
       let run_view = &app.runs[run];
       let task_view = &run_view.tasks[task];
       return Some(AppEvent::OpenTab(TabContent::Task {
           plan_slug: run_view.plan_slug.clone(), // or derive from task_list_path
           task_id: task_view.id.clone(),
       }));
   }
   ```

- **Depends on:** handle-tab-events-in-update
- **Done when:** pressing Enter on a sidebar task node opens a new tab for that task; the tab becomes active; `selected_task` state is no longer updated by sidebar selection; cargo test/clippy/fmt green.

---

### test-tab-state-operations — Test: Tab State Operations (open, close, switch)

A unit test verifies that `TabState` methods correctly manage the set of open tabs and the active-tab pointer.

**Steps:**

1. In `crates/makina/src/app.rs`, add a test module:
   ```rust
   #[cfg(test)]
   mod tab_state_tests {
       use super::*;

       #[test]
       fn open_tab_adds_new_tab() {
           let mut state = TabState::new();
           let content = TabContent::Plan { plan_slug: "0001-test".to_string() };
           state.open_tab(content);
           assert_eq!(state.open_tabs.len(), 1);
           assert_eq!(state.active_tab, Some(0));
       }

       #[test]
       fn open_existing_tab_switches_to_it() {
           let mut state = TabState::new();
           let content1 = TabContent::Plan { plan_slug: "0001".to_string() };
           let content2 = TabContent::Plan { plan_slug: "0002".to_string() };
           state.open_tab(content1);
           state.open_tab(content2);
           state.open_tab(content1); // Open again
           assert_eq!(state.open_tabs.len(), 2);
           assert_eq!(state.active_tab, Some(0)); // Switched back to first
       }

       #[test]
       fn close_tab_removes_it() {
           let mut state = TabState::new();
           let content1 = TabContent::Plan { plan_slug: "0001".to_string() };
           let content2 = TabContent::Plan { plan_slug: "0002".to_string() };
           state.open_tab(content1);
           state.open_tab(content2);
           state.close_tab(0);
           assert_eq!(state.open_tabs.len(), 1);
           assert_eq!(state.active_tab, Some(0)); // Still valid (now points to second tab)
       }
   }
   ```
2. Run the tests: `cargo test tab_state_tests`

- **Depends on:** add-open-tab-event, handle-tab-events-in-update, route-task-selection-to-tabs
- **Done when:** all three tests pass: `open_tab` adds/switches tabs, `close_tab` removes tabs and adjusts the active pointer, the active-tab index stays valid; cargo test/clippy/fmt green.

---

### integration-tabbed-pane-navigation — Integration: Tab Pane Rendering and Navigation

An integration test verifies that opening multiple tasks creates multiple tabs, renders the tab bar, and navigation keys switch between them.

**Steps:**

1. In `crates/makina/tests/`, add a render test:
   ```rust
   #[test]
   fn tabbed_pane_renders_open_tabs() {
       let mut terminal = make_terminal(100, 24);
       let api = Arc::new(PlaceholderApi::with_run());
       let mut app = App::new(api, vec![], PathBuf::from("."));

       // Open a couple of tabs
       app.tabs.open_tab(TabContent::Task {
           plan_slug: "0001".to_string(),
           task_id: TaskId("task-1".to_string()),
       });
       app.tabs.open_tab(TabContent::Task {
           plan_slug: "0001".to_string(),
           task_id: TaskId("task-2".to_string()),
       });

       terminal.draw(|frame| render(&app, frame)).unwrap();
       let screen = screen_of(&terminal);

       // Both tab titles should appear in the rendered output
       assert!(screen.contains("task-1"), "tab bar should show first task tab");
       assert!(screen.contains("task-2"), "tab bar should show second task tab");
   }
   ```
2. Run the test: `cargo test tabbed_pane_renders_open_tabs`

- **Depends on:** integration-plan-open-from-sidebar
- **Done when:** the test passes; multiple open tabs are rendered in the tab bar; the active tab is visually distinguished; cargo test/clippy/fmt green.

---

## 0003 — Model-Normalized TASKS.md Ingestion

### add-normalizer-struct — Create ModelNormalizer Struct

A new `ModelNormalizer` struct is needed to invoke the planner for repairing/generating malformed or missing TASKS.md. Like `ModelInterpreter`, it wraps an `AgentBackend` and provides an async method to normalize a task list.

**Steps:**

1. Create a new file `crates/makina-core/src/normalizer.rs`.
2. Add the struct and its methods:
   ```rust
   use std::sync::Arc;
   use async_trait::async_trait;
   use crate::backend::{AgentBackend, Prompt, SessionConfig};
   use crate::interpreter::InterpretError;
   use std::path::Path;

   /// Error type for normalization failures.
   #[derive(Debug, thiserror::Error)]
   pub enum NormalizeError {
       #[error("failed to read plan spec: {0}")]
       ReadError(#[from] std::io::Error),
       #[error("planner backend error: {0}")]
       BackendError(String),
       #[error("planner response invalid: {0}")]
       InvalidResponse(String),
   }

   pub struct ModelNormalizer {
       backend: Arc<dyn AgentBackend>,
   }

   impl ModelNormalizer {
       pub fn new(backend: Arc<dyn AgentBackend>) -> Self {
           ModelNormalizer { backend }
       }

       /// Normalize a malformed or missing TASKS.md by invoking the planner
       /// to repair/generate it from the SCOPE.md + ARCHITECTURE.md brief.
       pub async fn normalize(
           &self,
           plan_dir: &Path,
           slug: &str,
       ) -> Result<String, NormalizeError> {
           // 1. Read SCOPE.md + ARCHITECTURE.md
           let scope = tokio::fs::read_to_string(plan_dir.join("SCOPE.md")).await?;
           let arch = tokio::fs::read_to_string(plan_dir.join("ARCHITECTURE.md")).await?;
           let brief = format!("{scope}\n\n{arch}");

           // 2. Try to read existing TASKS.md to include error context
           let existing = match tokio::fs::read_to_string(plan_dir.join("TASKS.md")).await {
               Ok(content) => Some(content),
               Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
               Err(e) => return Err(NormalizeError::ReadError(e)),
           };

           // 3. Build the prompt for the planner
           let prompt_text = format!(
               "{}\n\n{}\n\nGenerate a canonical TASKS.md conforming to the Makina convention.",
               brief,
               if let Some(ref text) = existing {
                   format!("Existing TASKS.md (may be malformed):\n{}", text)
               } else {
                   "No existing TASKS.md found.".to_string()
               }
           );

           // 4. Call planner
           let config = SessionConfig::default();
           let prompt = Prompt::new(prompt_text, crate::constants::PLANNER_NORMALIZE_SYSTEM_PROMPT.into(), vec![]);
           let mut session = self.backend.spawn(config).await
               .map_err(|e| NormalizeError::BackendError(e.to_string()))?;
           let mut response = String::new();
           let mut stream = session.stream(prompt).await
               .map_err(|e| NormalizeError::BackendError(e.to_string()))?;
           while let Some(event) = stream.next().await {
               match event {
                   // ... accumulate response text
                   _ => {}
               }
           }

           Ok(response)
       }
   }
   ```
3. Add the new module to `crates/makina-core/src/lib.rs`: `pub mod normalizer;`

- **Depends on:** add-normalize-system-prompt
- **Done when:** the `ModelNormalizer` struct compiles; it provides an async `normalize()` method that reads the brief and returns normalized TASKS.md text; cargo test/clippy/fmt green.

---

### add-normalize-system-prompt — Add PLANNER_NORMALIZE_SYSTEM_PROMPT Constant

The normalizer uses a distinct system prompt to instruct the planner how to repair/generate TASKS.md. A new constant `PLANNER_NORMALIZE_SYSTEM_PROMPT` is added alongside the existing `PLANNER_SYSTEM_PROMPT` and `PLANNER_GENERATE_SYSTEM_PROMPT` constants.

**Steps:**

1. In `crates/makina-core/src/constants.rs` (or create it if it doesn't exist), add:
   ```rust
   pub const PLANNER_NORMALIZE_SYSTEM_PROMPT: &str = r#"
   You are a Makina task-list repair expert. Your job is to generate or repair a TASKS.md
   file that conforms to the Makina structured-text convention.

   You will be given:
   1. A SCOPE.md / ARCHITECTURE.md plan brief.
   2. Optionally, an existing but malformed TASKS.md with parse errors.

   Your task:
   Generate a **canonical**, well-formed TASKS.md that:
   - Follows the exact structure and heading format specified in `docs/spec/structured-text-convention.md`.
   - Includes all workstreams, tasks, and dependencies implied by the SCOPE/ARCHITECTURE.
   - Uses proper markdown: `## NNNN – Name` for workstream headers, `### kebab-id – Title` for task headers.
   - Ensures "Done when" bullets are clear and falsifiable.
   - Preserves the original task IDs and dependencies from the ARCHITECTURE when possible.

   Output ONLY the TASKS.md content; do not include any preamble or explanation.
   "#;
   ```

- **Depends on:** —
- **Done when:** the `PLANNER_NORMALIZE_SYSTEM_PROMPT` constant is defined; it clearly instructs the planner on the expected output format; cargo test/clippy/fmt green.

---

### add-is-plan-convention-helper — Add is_plan_convention_dir() Helper Function

The ingestion path must determine whether a directory follows the plan convention (has both SCOPE.md and ARCHITECTURE.md) to decide whether to invoke the normalizer. A helper function makes this check reusable.

**Steps:**

1. In `crates/makina-core/src/orchestrator.rs`, add:
   ```rust
   /// Check if `dir` follows the plan convention (both SCOPE.md and ARCHITECTURE.md exist).
   fn is_plan_convention_dir(dir: &Path) -> bool {
       dir.join("SCOPE.md").is_file() && dir.join("ARCHITECTURE.md").is_file()
   }
   ```

- **Depends on:** —
- **Done when:** the `is_plan_convention_dir()` function compiles and correctly returns `true` only when both SCOPE.md and ARCHITECTURE.md exist; cargo test/clippy/fmt green.

---

### integrate-normalizer-into-ingestion — Wire Normalizer into interpret_and_seed()

The `interpret_and_seed()` function (`crates/makina-core/src/orchestrator.rs:1024`) is the main ingestion entry point. It must be extended to invoke the normalizer on parse errors or missing TASKS.md for plan-convention dirs. This is the critical integration point.

**Steps:**

1. In `crates/makina-core/src/orchestrator.rs`, locate `interpret_and_seed()` (around line 1024).
2. Modify the read + interpret flow to include normalization on failure:
   ```rust
   let text = match tokio::fs::read_to_string(&task_list_path).await {
       Ok(text) => text,
       Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
           // Plan 0028 path: generate if it's a plan-convention dir
           if is_plan_convention_dir(task_list_path.parent().unwrap()) {
               let generated = /* existing generate_from_scope_arch logic */;
               tokio::fs::write(&task_list_path, &generated).await?;
               generated
           } else {
               return Err(ApiError::InvalidCommand { ... });
           }
       }
       Err(e) => return Err(ApiError::InvalidCommand { ... }),
   };

   // Try deterministic interpretation; on ParseError, normalize if plan-convention.
   let graph = match self.interpreter.interpret(&slug, &text).await {
       Ok(g) => g,
       Err(InterpretError::ParseError { location, context }) => {
           let plan_dir = task_list_path.parent().unwrap();
           if is_plan_convention_dir(plan_dir) {
               tracing::info!("Normalizing malformed TASKS.md at line {}: {}", location, context);
               match self.normalizer.normalize(plan_dir, &slug).await {
                   Ok(normalized) => {
                       tokio::fs::write(&task_list_path, &normalized).await?;
                       self.interpreter.interpret(&slug, &normalized).await?
                   }
                   Err(e) => {
                       tracing::warn!("Normalization failed: {e}; falling back to original error");
                       return Err(ApiError::InterpretError { ... });
                   }
               }
           } else {
               return Err(ApiError::InterpretError { ... });
           }
       }
       Err(e) => return Err(ApiError::InterpretError { ... }),
   };
   ```
3. Ensure `CoreApi` holds a `normalizer: Arc<ModelNormalizer>` instance (injected alongside the interpreter).

- **Depends on:** add-normalizer-struct, add-is-plan-convention-helper
- **Done when:** the `interpret_and_seed()` function compiles with normalizer integration; on a parse error for a plan-convention dir, the normalizer is invoked; the normalized TASKS.md is written and re-interpreted; cargo test/clippy/fmt green.

---

### add-normalizer-to-api-builder — Add Normalizer to CoreApi Builder

The `CoreApi` struct must be initialized with a `ModelNormalizer` instance so that `interpret_and_seed()` can use it. The builder pattern (similar to how the planner is injected) ensures the normalizer is available with the correct `AgentBackend`.

**Steps:**

1. In `crates/makina-core/src/api.rs`, locate the `CoreApi` struct definition.
2. Add a field: `normalizer: Arc<ModelNormalizer>,`
3. In the `CoreApi` builder (e.g., `CoreApi::new()` or the builder struct), instantiate the normalizer with the `AgentBackend`:
   ```rust
   let normalizer = Arc::new(ModelNormalizer::new(backend.clone()));
   // ... then include it in the CoreApi struct initialization
   ```

- **Depends on:** integrate-normalizer-into-ingestion
- **Done when:** the `CoreApi` struct holds a `normalizer` field; it is initialized in the builder; `interpret_and_seed()` can access it via `self.normalizer`; cargo test/clippy/fmt green.

---

### test-normalizer-basic — Test: ModelNormalizer Invocation

A test verifies that the normalizer can be instantiated and invoked (with a mock backend) without panicking. It confirms the basic flow is sound before integration testing.

**Steps:**

1. In `crates/makina-core/tests/`, create a new test file `normalizer_basic.rs` with:
   ```rust
   #[tokio::test]
   async fn normalizer_instantiates() {
       let backend = Arc::new(NoopBackend::new()); // or a mock
       let normalizer = ModelNormalizer::new(backend);
       // Just verify it constructs without panicking
       assert!(std::mem::size_of_val(&normalizer) > 0);
   }
   ```
2. Run the test: `cargo test normalizer_instantiates`

- **Depends on:** integration-plan-open-from-sidebar
- **Done when:** the test compiles and passes; the normalizer can be instantiated with a mock backend without errors; cargo test/clippy/fmt green.

---

### integration-normalizer-malformed-tasks — Integration: Normalizer Repairs Malformed TASKS.md

An integration test verifies that opening a plan dir with a malformed TASKS.md (e.g., task heading before section heading) triggers the normalizer, which repairs it and allows the run to open successfully.

**Steps:**

1. In `crates/makina-core/tests/`, create a test file `normalizer_integration.rs`:
   ```rust
   #[tokio::test]
   async fn normalizer_repairs_malformed_tasks() {
       // Setup: create a temp plan dir with SCOPE/ARCH but broken TASKS.md
       let tmp = TempDir::new().unwrap();
       let plan_dir = tmp.path();
       std::fs::write(plan_dir.join("SCOPE.md"), "# Scope\n\nA test plan.").unwrap();
       std::fs::write(plan_dir.join("ARCHITECTURE.md"), "# Architecture\n\nOne workstream.").unwrap();
       std::fs::write(
           plan_dir.join("TASKS.md"),
           "### task-1 – test task\n\n## 0001 – Workstream\n", // malformed: task before section
       )
       .unwrap();

       // Create API with mock planner that returns valid TASKS.md
       let api = Arc::new(TestApi::with_mock_planner(valid_tasks_response));
       let cmd = CoreApi::OpenRun { path: plan_dir.join("TASKS.md") };
       let result = api.execute(cmd).await;

       // Should succeed after normalization
       assert!(result.is_ok(), "open should succeed after normalization");
       // The written TASKS.md should be the normalized version
       let written = std::fs::read_to_string(plan_dir.join("TASKS.md")).unwrap();
       assert!(written.contains("## 0001"), "normalized TASKS.md should have section before task");
   }
   ```
2. Run the test: `cargo test normalizer_repairs_malformed_tasks`

- **Depends on:** integration-plan-open-from-sidebar
- **Done when:** the test passes; a malformed TASKS.md is detected, the normalizer repairs it, the repaired file is written, and the run opens successfully; cargo test/clippy/fmt green.

---

**End of plan 0031 TASKS.** When every "Done when" bullet is green, discovered
plans are navigable from the unified sidebar without mode-switching, multiple
tasks/plans live in parallel tabs, and a malformed or missing TASKS.md is
auto-repaired at the ingestion front door before the deterministic interpreter
runs — hardening the plan-opening path while keeping the governance wedge intact.
