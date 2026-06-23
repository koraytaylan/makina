# XAgent Plan 0032 — Tabbed Plan-Detail Pane with Accordion Sections

Plan 0032 replaces the current single `plan_detail: Option<usize>` model (one plan view at a time) with a tabbed plan-viewing system. Pressing Enter on a plan node in the sidebar opens a new tab for that plan (or switches to an existing tab); tabs are rendered in a bar above the content area. Each plan tab displays four accordion sections—SCOPE, ARCHITECTURE, TASKS (with GATED/dependency markers), and STATUS—that expand and collapse independently via arrow keys or a toggle keybind. Accordion expand state is persisted per-tab in `App`. The change is additive: existing single-plan viewing becomes the first tab, and the tab bar renders a clear label and active-tab indicator. All gate commands remain green throughout.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md) for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}`).
- **Depends on** lists *direct* prerequisites only; "—" means none.
- **Done when** is the verifiable criterion, and every task keeps the gate
  commands green — the full forms are `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`;
  abbreviated as "cargo test/clippy/fmt green" thereafter.
- GPU tests self-skip without an adapter.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0002 — Accordion-Section Layout for Plan Metadata

### extend-plan-entry-with-spec-content — Extend PlanEntry to Cache SCOPE/ARCHITECTURE/STATUS

The `PlanEntry` struct (`crates/makina-core/src/orchestrator.rs:212–224`) currently holds only `slug`, `dir`, `has_tasks`, and `tasks` (parsed preview). To render SCOPE, ARCHITECTURE, and STATUS content in accordion sections, these files must be read and cached at discovery time (off the render thread). This change is the foundation for the accordion-section rendering in workstream 0002.

**Steps:**

1. In `crates/makina-core/src/orchestrator.rs`, locate the `PlanEntry` struct definition (line 212).
2. Add three new fields after the existing `tasks` field:
   ```rust
       /// SCOPE.md content (cached at discovery time; None if unreadable or absent).
       pub scope_text: Option<String>,
       /// ARCHITECTURE.md content (cached at discovery time; None if unreadable or absent).
       pub architecture_text: Option<String>,
       /// STATUS.md content (cached at discovery time; None if unreadable or absent).
       pub status_text: Option<String>,
   ```
3. In the `discover_plans` function (line ~354), locate where `PlanEntry { slug, dir, has_tasks, tasks }` is constructed.
4. Before constructing the `PlanEntry`, read the three spec files non-blocking (using `tokio::fs::read_to_string` and `.await.ok()` to convert errors to `None`):
   ```rust
   let scope_text = tokio::fs::read_to_string(dir.join("SCOPE.md")).await.ok();
   let architecture_text = tokio::fs::read_to_string(dir.join("ARCHITECTURE.md")).await.ok();
   let status_text = tokio::fs::read_to_string(dir.join("STATUS.md")).await.ok();
   ```
5. Update the `PlanEntry` construction to include these fields:
   ```rust
   PlanEntry { slug, dir, has_tasks, tasks, scope_text, architecture_text, status_text }
   ```

- **Depends on:** —
- **Done when:** the `PlanEntry` struct compiles with the three new `Option<String>` fields. The `discover_plans` function reads SCOPE.md, ARCHITECTURE.md, and STATUS.md files for each discovered plan (with `None` on read error). A unit test verifies that a plan entry with all three files readable has all three fields populated; a plan entry with missing files has `None` for those fields. cargo test/clippy/fmt green.

---

### add-accordion-state-to-app — Add Accordion State Tracking to App

Each plan tab must track which of its four accordion sections (SCOPE, ARCHITECTURE, TASKS, STATUS) are expanded or collapsed. This state is ephemeral (not persisted) and keyed by plan slug. Because there is exactly one tab per plan slug (`open_tab` dedups by `TabContent`), the slug uniquely identifies that tab's expand set — re-focusing a plan restores whichever sections it had open.

**Steps:**

1. In `crates/makina/src/app.rs`, define the accordion section enum before the `App` struct (around line 940):
   ```rust
   /// Accordion section identifier for plan tabs.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
   pub enum AccordionSection {
       /// SCOPE.md section
       Scope,
       /// ARCHITECTURE.md section
       Architecture,
       /// TASKS.md section (with GATED/dependency markers)
       Tasks,
       /// STATUS.md section
       Status,
   }
   ```
2. In the `App` struct definition (around line 944), add a new field after `tabs`:
   ```rust
       /// Accordion expand/collapse state for plan tabs.
       /// Keyed by plan slug; the set contains sections that are expanded.
       /// Sections not in the set are collapsed. All sections default to collapsed.
       pub accordion_state: HashMap<String, HashSet<AccordionSection>>,
   ```
3. In `App::new()` (around line 1390), initialize this field:
   ```rust
       accordion_state: HashMap::new(),
   ```
4. Verify no import is needed for `HashMap` and `HashSet` (they should already be imported for `collapsed_runs` and `collapsed_plans`).

- **Depends on:** —
- **Done when:** the `AccordionSection` enum compiles; the `App` struct has the `accordion_state` field and it is initialized to empty in `App::new()`. A unit test verifies that toggling a section (insert/remove from the set) works correctly. cargo test/clippy/fmt green.

---

### add-toggle-accordion-event — Add AppEvent Variant for Accordion Toggle

The event system must convey user interactions with accordion sections. When a user presses 's', 'a', 't', or 'z' (while the main pane is focused), an event should be dispatched to toggle the corresponding section.

**Steps:**

1. In `crates/makina/src/app.rs`, locate the `AppEvent` enum (around line 595).
2. Add a new variant after the existing tab-related events:
   ```rust
       /// Toggle the accordion section for the active plan tab.
       /// Only applies if the active tab is a plan tab; otherwise it is a no-op.
       ToggleAccordionSection(AccordionSection),
   ```

- **Depends on:** add-accordion-state-to-app
- **Done when:** the `AppEvent` enum compiles with the new `ToggleAccordionSection(AccordionSection)` variant. cargo test/clippy/fmt green.

---

### handle-accordion-toggle-in-update — Handle Accordion Toggle Events in App::update()

The `App::update()` method must process `ToggleAccordionSection` events, finding the active plan tab's slug, and toggling the section in the accordion state map.

**Steps:**

1. In `crates/makina/src/app.rs`, locate the `App::update()` method.
2. Add a handler for `AppEvent::ToggleAccordionSection(section)` in the event match statement:
   ```rust
   AppEvent::ToggleAccordionSection(section) => {
       // Only toggle if the active tab is a plan tab.
       if let Some(active_idx) = self.tabs.active_tab {
           if let Some(TabContent::Plan { plan_slug }) = self.tabs.open_tabs.get(active_idx) {
               let plan_slug = plan_slug.clone();
               let sections = self.accordion_state.entry(plan_slug).or_insert_with(HashSet::new);
               if sections.contains(&section) {
                   sections.remove(&section);
               } else {
                   sections.insert(section);
               }
           }
       }
   }
   ```

- **Depends on:** add-toggle-accordion-event
- **Done when:** the event handler compiles and correctly toggles accordion sections for the active plan tab. A unit test verifies toggling a section inserts it if absent and removes it if present. When no plan tab is active (or a task tab is active), the event is a no-op. cargo test/clippy/fmt green.

---

### create-accordion-renderer — Create render_plan_accordion_pane() Renderer

The new accordion renderer must display the four plan sections (SCOPE, ARCHITECTURE, TASKS, STATUS) as independently expandable regions, each with a collapse/expand indicator and content. The function must handle text wrapping, scrolling, and missing content gracefully.

**Steps:**

1. In `crates/makina/src/ui.rs`, add a new function after `render_plan_detail` (around line 1378): 
   ```rust
   /// Render a plan tab's accordion pane with SCOPE, ARCHITECTURE, TASKS, and STATUS sections.
   fn render_plan_accordion_pane(
       app: &App,
       plan: &makina_core::orchestrator::PlanEntry,
       frame: &mut Frame,
       area: Rect,
   ) {
       if area.height == 0 || area.width == 0 {
           return;
       }

       let mut lines = Vec::new();

       // Header
       lines.push(Line::from(vec![
           Span::styled("Plan: ", Style::default().fg(Color::DarkGray)),
           Span::styled(
               &plan.slug,
               Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
           ),
       ]));
       lines.push(Line::from(vec![
           Span::styled("Dir:  ", Style::default().fg(Color::DarkGray)),
           Span::styled(
               plan.dir.display().to_string(),
               Style::default().fg(Color::Gray),
           ),
       ]));
       lines.push(Line::from(""));

       // Get accordion state for this plan
       let expanded = app
           .accordion_state
           .get(&plan.slug)
           .cloned()
           .unwrap_or_default();

       // SCOPE section
       lines.extend(render_accordion_section(
           "SCOPE",
           AccordionSection::Scope,
           &expanded,
           plan.scope_text.as_deref().unwrap_or("(no SCOPE.md)"),
       ));
       lines.push(Line::from(""));

       // ARCHITECTURE section
       lines.extend(render_accordion_section(
           "ARCHITECTURE",
           AccordionSection::Architecture,
           &expanded,
           plan.architecture_text.as_deref().unwrap_or("(no ARCHITECTURE.md)"),
       ));
       lines.push(Line::from(""));

       // TASKS section
       let tasks_text = format_tasks_section(&plan.tasks);
       lines.extend(render_accordion_section(
           "TASKS",
           AccordionSection::Tasks,
           &expanded,
           &tasks_text,
       ));
       lines.push(Line::from(""));

       // STATUS section
       lines.extend(render_accordion_section(
           "STATUS",
           AccordionSection::Status,
           &expanded,
           plan.status_text.as_deref().unwrap_or("(no STATUS.md)"),
       ));
       lines.push(Line::from(""));

       // Footer help text
       lines.push(Line::from(Span::styled(
           "  [s] scope  [a] arch  [t] tasks  [z] status  [◄] [►] tabs  [Ctrl+W] close",
           Style::default().fg(Color::DarkGray),
       )));

       // Clamp scroll so the footer stays visible
       let total = lines.len() as u16;
       let scroll = total.saturating_sub(area.height);
       let para = Paragraph::new(lines)
           .wrap(Wrap { trim: false })
           .scroll((scroll, 0));
       frame.render_widget(para, area);
   }

   /// Render a single accordion section (SCOPE, ARCHITECTURE, TASKS, or STATUS).
   /// Returns a Vec<Line> containing the header (expanded/collapsed marker) and,
   /// if expanded, the content lines.
   fn render_accordion_section(
       title: &str,
       section: AccordionSection,
       expanded_set: &HashSet<AccordionSection>,
       content: &str,
   ) -> Vec<Line> {
       let mut result = Vec::new();
       let is_expanded = expanded_set.contains(&section);
       let marker = if is_expanded { "[-]" } else { "[+]" };

       // Section header
       result.push(Line::from(vec![
           Span::styled(
               marker,
               Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
           ),
           Span::raw(" "),
           Span::styled(
               title,
               Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
           ),
       ]));

       // Content (if expanded)
       if is_expanded {
           result.push(Line::from(""));
           for line in content.lines() {
               result.push(Line::from(format!("  {line}")));
           }
       }

       result
   }

   /// Format the tasks section content: task list with GATED markers and dependencies.
   fn format_tasks_section(tasks: &[makina_core::orchestrator::PlanTaskPreview]) -> String {
       if tasks.is_empty() {
           return "(no tasks)".to_string();
       }
       let mut text = format!("Tasks ({})", tasks.len());
       let gated = tasks.iter().filter(|t| t.gated).count();
       if gated > 0 {
           text.push_str(&format!("  · {} gated", gated));
       }
       text.push('\n');
       text.push('\n');
       for (i, t) in tasks.iter().enumerate() {
           text.push_str(&format!("  {}. ", i + 1));
           text.push_str(&t.id);
           text.push_str(&format!(" — {}", t.title));
           if t.gated {
               text.push_str("  GATED");
           }
           text.push('\n');
           if !t.depends_on.is_empty() {
               text.push_str(&format!("     depends on: {}", t.depends_on.join(", ")));
               text.push('\n');
           }
       }
       text
   }
   ```

- **Depends on:** add-accordion-state-to-app, extend-plan-entry-with-spec-content
- **Done when:** the `render_plan_accordion_pane` function compiles and renders a plan tab with four accordion sections. Sections render as `[+] SCOPE` (collapsed) or `[-] SCOPE` (expanded) with content indented below when expanded. Missing content renders as `(no SCOPE.md)`. The footer displays help text for keybindings. A test verifies rendering with long content does not panic and scrolling clamps correctly. cargo test/clippy/fmt green.

---

## 0001 — Tab-Based Plan Rendering

### remove-plan-detail-singleton — Remove plan_detail Singleton and Route to Tabs (GATED)

**Gate:** This task consumes work that must already be landed: `extend-plan-entry-with-spec-content` gives `PlanEntry` its `scope_text`, `architecture_text`, and `status_text` fields, and `create-accordion-renderer` provides the `render_plan_accordion_pane` plan-tab content renderer. Both are declared earlier (workstream 0002) and must land before this task routes plan viewing through them.

This task removes the singleton `plan_detail: Option<usize>` model and routes plan viewing through the tab infrastructure. Pressing Enter on a discovered plan node opens (or switches to) a tab, and the active plan tab renders via the new accordion pane instead of the old `render_plan_detail`.

**Steps:**

1. In `crates/makina/src/app.rs`, locate and remove the `plan_detail: Option<usize>` field from the `App` struct (around line 1012).
2. In `App::new()`, remove the `plan_detail: None,` initialization (around line 1417).
3. Search for all uses of `app.plan_detail` and `self.plan_detail` in the codebase:
   ```bash
   grep -n 'plan_detail' crates/makina/src/app.rs crates/makina/src/ui.rs crates/makina/src/event.rs
   ```
4. For each use that sets `plan_detail` (around lines 1267, 1276, 1284), replace with a no-op or route to `OpenTab` event. For example, when a `TreeNode::Plan` is focused and Enter is pressed, dispatch `AppEvent::OpenTab(TabContent::Plan { plan_slug: plan.slug.clone() })` instead of `self.plan_detail = Some(plan_idx)`.
5. In the focus-sync method (around line 1253 `sync_focus_with_sidebar`), update the logic to clear `plan_detail` (now remove this line since `plan_detail` no longer exists). Ensure the logic clears any plan tabs when a run/task is selected (or leave them open—consistency with 0031's decision to keep tabs independent of sidebar navigation).
6. In `crates/makina/src/ui.rs`, locate the call to `render_plan_detail` (around line 351). Replace it with logic that checks the active tab and routes to `render_plan_accordion_pane` if the active tab is a plan:
   ```rust
   if let Some(active_idx) = app.tabs.active_tab {
       if let Some(TabContent::Plan { plan_slug }) = app.tabs.open_tabs.get(active_idx) {
           // Find the matching plan in discovered_plans
           if let Some(plan) = app.discovered_plans.iter().find(|p| p.slug == *plan_slug) {
               render_plan_accordion_pane(app, plan, frame, content_area);
               return; // Plan pane rendered; skip rendering run/task content
           }
       }
   }
   // Normal run/task rendering follows...
   ```

- **Depends on:** extend-plan-entry-with-spec-content, create-accordion-renderer
- **Done when:** the `plan_detail` field is removed from the `App` struct and all uses are eliminated. Pressing Enter on a discovered plan node opens a new tab (or switches to an existing one) via `AppEvent::OpenTab`. The active plan tab is rendered using `render_plan_accordion_pane` instead of the old `render_plan_detail`. The sidebar focus and tab state remain independent (opening a tab does not close other tabs). A unit test verifies that navigating to a plan and pressing Enter opens a tab for that plan, and that pressing Enter *again* on the same plan focuses the existing tab — the open-tab count stays at 1 and the tab is not closed — rather than opening a duplicate (the "open if closed, focus if open" contract). This task lands as a unit or reverts and records the blocker. cargo test/clippy/fmt green.

---

### integrate-tab-bar-into-plan-pane — Integrate Tab Bar into Main Pane Layout (for Plan Tabs)

The tab bar (already implemented in Plan 0031 for task tabs) must render plan tabs alongside task tabs. The current tab bar renderer should already handle both `TabContent::Task` and `TabContent::Plan` variants, but its sizing and placement may need adjustment if the plan tab uses the full content area.

**Steps:**

1. In `crates/makina/src/ui.rs`, verify that `render_tab_bar` (implemented in Plan 0031) renders both task and plan tabs. The function should iterate over `app.tabs.open_tabs` and display each tab's label (task ID for `Task`, plan slug for `Plan`).
2. Verify that the main pane layout already reserves 1 row for the tab bar (check the vertical split in the main content area, around line 349). If not, add a `Constraint::Length(1)` for the tab bar at the top of the split.
3. Run a render test to confirm both task and plan tabs appear in the tab bar: `#[test] fn render_task_and_plan_tabs_together() { ... }`. Open a task tab and a plan tab, render the frame, and verify both labels appear in the tab bar.
4. Ensure the active tab is visually distinguished (different color/style) in the tab bar.

- **Depends on:** remove-plan-detail-singleton, create-accordion-renderer
- **Done when:** the tab bar renders all open tabs (both task and plan tabs) with the active tab highlighted. A render test verifies opening a task tab and a plan tab displays both in the tab bar. The tab bar layout does not overflow or truncate tabs (clips gracefully if there are many). cargo test/clippy/fmt green.

---

## 0003 — Tab Navigation and Keybindings

### wire-accordion-keybindings — Wire Accordion Keybindings (s/a/t/z) to Events

When the main content pane is focused and the active tab is a plan tab, pressing 's', 'a', 't', or 'z' should toggle the corresponding accordion section. These keybindings must be added to the event loop.

**Steps:**

1. In `crates/makina/src/event.rs` (or in `app.rs` if keybind handling is there), locate the key-press event handler for the main pane.
2. Add branches for the accordion-toggle keys (matching only when `Panel::Main` is focused and the active tab is a plan tab):
   ```rust
   if app.focused_panel == Panel::Main {
       if let Some(active_idx) = app.tabs.active_tab {
           if matches!(app.tabs.open_tabs.get(active_idx), Some(TabContent::Plan { .. })) {
               match key_event.code {
                   KeyCode::Char('s') | KeyCode::Char('S') => {
                       return Some(AppEvent::ToggleAccordionSection(AccordionSection::Scope));
                   }
                   KeyCode::Char('a') | KeyCode::Char('A') => {
                       return Some(AppEvent::ToggleAccordionSection(AccordionSection::Architecture));
                   }
                   KeyCode::Char('t') | KeyCode::Char('T') => {
                       return Some(AppEvent::ToggleAccordionSection(AccordionSection::Tasks));
                   }
                   KeyCode::Char('z') | KeyCode::Char('Z') => {
                       return Some(AppEvent::ToggleAccordionSection(AccordionSection::Status));
                   }
                   _ => {}
               }
           }
       }
   }
   ```

- **Depends on:** handle-accordion-toggle-in-update
- **Done when:** pressing 's', 'a', 't', or 'z' while the main pane is focused and a plan tab is active dispatches the corresponding toggle event. Pressing these keys while a task tab is active or the sidebar is focused has no effect. An integration test verifies the keybindings work (e.g., open a plan tab, press 's', assert `accordion_state` reflects the toggle). cargo test/clippy/fmt green.

---

## 0004 — Integration and Polish

### clamp-plan-tabs-on-discovery — Clamp Plan Tabs on Plan Re-discovery

When plans are re-discovered, the list of `discovered_plans` may change (new plans added, old plans removed, or indices shifted). Any open plan tabs that reference plans no longer in the discovered list must be closed or re-linked to maintain invariants.

**Steps:**

1. In `crates/makina/src/app.rs`, locate the `AppEvent::PlansDiscovered` handler (search for the event variant).
2. After updating `self.discovered_plans`, iterate over `self.tabs.open_tabs` and check each tab:
   ```rust
   for tab in &self.tabs.open_tabs {
       if let TabContent::Plan { plan_slug } = tab {
           // Check if this plan still exists in discovered_plans
           let still_exists = self.discovered_plans.iter().any(|p| p.slug == *plan_slug);
           if !still_exists {
               // Plan is no longer discovered; close it.
               // (The app will handle closing during the next tab cleanup phase.)
           }
       }
   }
   ```
3. Implement a helper method `close_tabs_for_missing_plans()` that removes any plan tabs whose slugs no longer exist:
   ```rust
   fn close_tabs_for_missing_plans(&mut self) {
       let valid_plans: HashSet<_> = self.discovered_plans.iter().map(|p| p.slug.clone()).collect();
       let mut indices_to_close = Vec::new();
       for (idx, tab) in self.tabs.open_tabs.iter().enumerate() {
           if let TabContent::Plan { plan_slug } = tab {
               if !valid_plans.contains(plan_slug) {
                   indices_to_close.push(idx);
               }
           }
       }
       // Close tabs in reverse order so indices don't shift
       for idx in indices_to_close.iter().rev() {
           self.tabs.close_tab(*idx);
       }
   }
   ```
4. Call this method in the `PlansDiscovered` handler after updating `discovered_plans`.

- **Depends on:** remove-plan-detail-singleton, add-toggle-accordion-event, handle-accordion-toggle-in-update, wire-accordion-keybindings
- **Done when:** when plans are re-discovered and a plan slug no longer appears in the list, any open tabs for that plan are closed. The `active_tab` index is clamped to stay valid. Accordion state for removed plans is also cleaned up (optional). A test verifies that re-discovering plans with a plan tab open for a removed plan closes the tab correctly. cargo test/clippy/fmt green.

---

### test-accordion-section-state — Test: Accordion Section State Persistence Across Tabs

A baseline test verifies that opening two plan tabs and expanding different sections in each preserves the section state when switching tabs.

**Steps:**

1. In `crates/makina/tests/` (or in `ui_tests.rs`), add a test: 
   ```rust
   #[test]
   fn accordion_sections_persist_per_tab() {
       let api = Arc::new(PlaceholderApi::new());
       let plan1 = makina_core::orchestrator::PlanEntry {
           slug: "0001-test".to_string(),
           dir: PathBuf::from("docs/plans/0001"),
           has_tasks: true,
           tasks: vec![],
           scope_text: Some("Scope for plan 1.".to_string()),
           architecture_text: Some("Architecture for plan 1.".to_string()),
           status_text: Some("Status for plan 1.".to_string()),
       };
       let plan2 = makina_core::orchestrator::PlanEntry {
           slug: "0002-test".to_string(),
           dir: PathBuf::from("docs/plans/0002"),
           has_tasks: true,
           tasks: vec![],
           scope_text: Some("Scope for plan 2.".to_string()),
           architecture_text: None,
           status_text: None,
       };
       let mut app = App::new(api, vec![plan1, plan2], PathBuf::from("."));

       // Open first plan tab and expand SCOPE
       app.tabs.open_tab(TabContent::Plan { plan_slug: "0001-test".to_string() });
       app.accordion_state
           .entry("0001-test".to_string())
           .or_insert_with(HashSet::new)
           .insert(AccordionSection::Scope);

       // Open second plan tab and expand TASKS
       app.tabs.open_tab(TabContent::Plan { plan_slug: "0002-test".to_string() });
       app.accordion_state
           .entry("0002-test".to_string())
           .or_insert_with(HashSet::new)
           .insert(AccordionSection::Tasks);

       // Switch back to first tab
       app.tabs.active_tab = Some(0);

       // Verify first tab's state is preserved
       assert!(
           app.accordion_state
               .get("0001-test")
               .map(|s| s.contains(&AccordionSection::Scope))
               .unwrap_or(false),
           "Plan 1's SCOPE should remain expanded"
       );
       assert!(
           !app.accordion_state
               .get("0001-test")
               .map(|s| s.contains(&AccordionSection::Tasks))
               .unwrap_or(true),
           "Plan 1's TASKS should remain collapsed"
       );

       // Switch to second tab and verify its state
       app.tabs.active_tab = Some(1);
       assert!(
           app.accordion_state
               .get("0002-test")
               .map(|s| s.contains(&AccordionSection::Tasks))
               .unwrap_or(false),
           "Plan 2's TASKS should remain expanded"
       );
   }
   ```
2. Run the test: `cargo test accordion_sections_persist_per_tab`

- **Depends on:** add-accordion-state-to-app, extend-plan-entry-with-spec-content
- **Done when:** the test passes: opening multiple plan tabs, expanding different sections in each, and switching between tabs preserves each tab's expand state. cargo test/clippy/fmt green.

---

### integration-plan-tabs-rendering — Integration: Plan Tabs Render with Accordion Sections

An end-to-end integration test verifies that opening plan tabs, expanding sections, and rendering produces the expected visual output with no panics or truncation.

**Steps:**

1. In `crates/makina/tests/`, create a test:
   ```rust
   #[test]
   fn plan_tabs_render_accordion_sections_without_panic() {
       let mut terminal = make_terminal(120, 30); // Wide, tall terminal
       let api = Arc::new(PlaceholderApi::new());
       let plan = makina_core::orchestrator::PlanEntry {
           slug: "0031-test".to_string(),
           dir: PathBuf::from("docs/plans/0031"),
           has_tasks: true,
           tasks: vec![
               makina_core::orchestrator::PlanTaskPreview {
                   id: "task-1".to_string(),
                   title: "First task".to_string(),
                   gated: false,
                   depends_on: vec![],
               },
               makina_core::orchestrator::PlanTaskPreview {
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
       let mut app = App::new(api, vec![plan], PathBuf::from("."));

       // Open the plan tab
       app.tabs.open_tab(TabContent::Plan { plan_slug: "0031-test".to_string() });

       // Expand all sections
       let expanded = app
           .accordion_state
           .entry("0031-test".to_string())
           .or_insert_with(HashSet::new);
       expanded.insert(AccordionSection::Scope);
       expanded.insert(AccordionSection::Architecture);
       expanded.insert(AccordionSection::Tasks);
       expanded.insert(AccordionSection::Status);

       // Render (should not panic)
       terminal.draw(|frame| render(&app, frame)).unwrap();

       // Verify output contains section headers and content
       let screen = screen_of(&terminal);
       assert!(screen.contains("[-] SCOPE"), "SCOPE section should show expanded marker");
       assert!(screen.contains("[-] ARCHITECTURE"), "ARCHITECTURE section should show expanded marker");
       assert!(screen.contains("[-] TASKS"), "TASKS section should show expanded marker");
       assert!(screen.contains("task-1"), "Task 1 should be visible in expanded TASKS");
       assert!(screen.contains("GATED"), "Gated task marker should be visible");
       assert!(screen.contains("[-] STATUS"), "STATUS section should show expanded marker");
   }
   ```
2. Run the test: `cargo test plan_tabs_render_accordion_sections_without_panic`

- **Depends on:** test-accordion-section-state, create-accordion-renderer, integrate-tab-bar-into-plan-pane
- **Done when:** the test renders a plan tab with all accordion sections expanded without panicking. The screen output contains section headers with expand/collapse markers and the expanded content (SCOPE text, TASKS with GATED markers, etc.). Scrolling and text wrapping work correctly for long sections. cargo test/clippy/fmt green.

---

### verification-plan-tab-workflow — Verification: Plan Tab Workflow End-to-End

A manual verification step confirms that the complete workflow—discovering plans, opening tabs, expanding sections, navigating tabs, and closing tabs—works as expected in the running app.

**Steps:**

1. Build and run the app with a project that has multiple discovered plans: `cargo build && ./target/debug/makina`
2. In the sidebar, navigate (arrow keys) to a discovered plan node.
3. Press Enter to open the plan in a new tab. Verify the tab appears in the tab bar above the content area with the plan slug as the label.
4. Verify the plan content renders with four accordion section headers: `[+] SCOPE`, `[+] ARCHITECTURE`, `[+] TASKS`, `[+] STATUS` (all collapsed by default).
5. Press 's' to expand the SCOPE section. Verify the SCOPE.md content appears below the header (indented).
6. Press 't' to expand the TASKS section. Verify the tasks list appears with GATED markers and dependency chains.
7. Press 'a' to expand the ARCHITECTURE section. Verify the ARCHITECTURE.md content appears.
8. Press 'z' to expand the STATUS section. Verify the STATUS.md content appears (or "(no STATUS.md)" if missing).
9. With the plan tab still focused, navigate back to the *same* plan node in the sidebar and press Enter again. Verify NO second tab for that plan is created (the tab count is unchanged), the existing tab is simply re-focused, and its expand state is preserved — confirming "open if closed, focus if open" and that Enter never closes the tab.
10. Navigate to another discovered plan and open it with Enter. Verify a second tab appears in the tab bar with the second plan's slug.
11. Press Alt+Left (or equivalent keybinding) to switch back to the first plan tab. Verify the first tab's expand state is preserved (SCOPE, TASKS, ARCHITECTURE still expanded).
12. Press Ctrl+W to close the active tab. Verify the tab is removed from the tab bar and the app switches to the remaining tab (or shows the run/task view if no tabs remain).
13. Verify the app does not crash or show stale data when switching tabs or re-discovering plans.

- **Depends on:** integration-plan-tabs-rendering, add-accordion-state-to-app, add-toggle-accordion-event, handle-accordion-toggle-in-update, create-accordion-renderer, integrate-tab-bar-into-plan-pane, wire-accordion-keybindings, clamp-plan-tabs-on-discovery
- **Done when:** all steps complete without errors. The app opens multiple plan tabs, expands accordion sections independently, preserves section state when switching tabs, and closes tabs cleanly. Re-pressing Enter on an already-open plan focuses its existing tab instead of duplicating or closing it (step 9). The tab bar correctly shows open plan tabs and indicates the active tab. Keybindings for accordion toggle and tab navigation work as expected. cargo test/clippy/fmt green.

---

**End of plan 0032 TASKS.** When every "Done when" bullet is green, multiple
plan tabs are open and viewable in parallel; each tab displays SCOPE,
ARCHITECTURE, TASKS, and STATUS as independent accordion sections that expand
and collapse per-tab; tab navigation and accordion keybindings work without
conflict; and the single-plan-view bottleneck is eliminated — improving UX for
comparing multiple plans while keeping the gate commands green.
