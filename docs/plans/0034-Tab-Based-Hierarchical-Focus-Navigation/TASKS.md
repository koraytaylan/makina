# XAgent Plan 0034 — Tab-Based Hierarchical Focus Navigation

Extend the App focus model from a binary Panel (Sidebar ↔ Main) to a hierarchical FocusState that tracks region-level and nested accordion-section focus. Implement forward Tab and backward Shift+Tab traversal through a defined sequence: Sidebar → Main → accordion sections (if a plan tab is active) → back to Sidebar. Add visible focus borders/underlines to all focused regions. Wire Tab/Shift+Tab key events to the new focus-movement functions, implement test coverage for focus traversal order and wrapping, and verify Enter toggles focused accordion sections without breaking existing S/A/T/Z keybindings.

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

## 0001 — Focus Model Extension

### extend-focus-model — Extend App focus model with hierarchical FocusState

The App currently tracks focus as a binary Panel enum (Sidebar or Main) at `crates/makina/src/app.rs:1154`. When a plan tab with accordion sections is active, there is no way to track which accordion section owns focus. Tab navigation is blocked at the Main pane boundary because the model has nowhere to go next within the pane. We need to extend the focus model to include an optional accordion-section focus so Tab can traverse into and within those sections.

Once this model is in place, downstream tasks can implement Tab/Shift+Tab movement logic and visual focus indicators that rely on this hierarchical state.

**Steps:**

1. In `crates/makina/src/app.rs` after the Panel enum definition (line ~572), add a new FocusState enum:

   ```rust
   /// Hierarchical focus state: which nested item inside the focused panel owns focus.
   /// When `focused_panel == Panel::Main`, `focused_section` tracks which accordion
   /// section (if any) is active. When `focused_panel == Panel::Sidebar`,
   /// `focused_section` is ignored (the tree cursor owns focus).
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub enum FocusState {
       /// Sidebar tree node has focus; tree_cursor identifies the node.
       TreeNode,
       /// Main pane has focus, but no accordion section is focused yet (e.g., on first
       /// entry to Main when no plan tab is active).
       MainPane,
       /// A specific accordion section in the active plan tab has focus.
       AccordionSection(AccordionSection),
   }
   ```
2. In the App struct definition (starting ~line 1100), locate the `focused_panel: Panel` field (currently at line ~1154). Add a new field immediately after it:

   ```rust
   /// When focused_panel == Panel::Main, tracks which accordion section (if any) has focus.
   /// Defaults to None; Tab from Sidebar enters Main with focused_section = None, then
   /// subsequent Tab moves to the first accordion section (Scope).
   pub focused_section: Option<AccordionSection>,
   ```
3. In the App::new() constructor or impl Default, initialize focused_section to None:

   ```rust
   focused_section: None,
   ```
4. Add a helper method to App:

   ```rust
   /// Return the comprehensive focus state (region + nested section if applicable).
   pub fn focused_state(&self) -> FocusState {
       match self.focused_panel {
           Panel::Sidebar => FocusState::TreeNode,
           Panel::Main => self
               .focused_section
               .map(FocusState::AccordionSection)
               .unwrap_or(FocusState::MainPane),
       }
   }
   ```
5. Add a static method to App for the accordion section cycle order (used by Tab logic in task move-focus-forward):

   ```rust
   pub(crate) fn accordion_section_order() -> &'static [AccordionSection] {
       &[
           AccordionSection::Scope,
           AccordionSection::Architecture,
           AccordionSection::Tasks,
           AccordionSection::Status,
       ]
   }
   ```

- **Depends on:** —
- **Done when:** The code compiles without errors. The App struct has a focused_section: Option<AccordionSection> field initialized to None. The FocusState enum is defined with three variants and the focused_state() method returns the correct variant based on focused_panel and focused_section. The accordion_section_order() method returns the four sections in the correct order. No behavior change yet (this is data structure only); Tab still toggles Sidebar ↔ Main (old code path unchanged). cargo test/clippy/fmt green.

---

## 0002 — Tab/Shift+Tab Navigation Logic

### move-focus-forward — Implement move_focus_forward() for Tab key traversal

With the hierarchical focus model in place, we now implement forward Tab traversal. Tab should move focus in this sequence: Sidebar → Main (without section focus) → accordion sections (if a plan tab is active) → back to Sidebar. The logic must check whether a plan tab is active before entering accordion section cycling; if not, Tab from Main wraps directly back to Sidebar. This method is a pure state transition (no IO or rendering).

**Steps:**

1. Add the following method to the App impl block (in `crates/makina/src/app.rs`):

   ```rust
   /// Move focus forward (Tab key) through the hierarchy:
   /// Sidebar → Main (no section) → Accordion sections → Sidebar (wrap).
   /// Wrapping occurs only if a plan tab is active; otherwise Tab from Main goes to Sidebar.
   pub fn move_focus_forward(&mut self) {
       match self.focused_panel {
           Panel::Sidebar => {
               // From Sidebar, Tab always moves to Main pane.
               self.focused_panel = Panel::Main;
               self.focused_section = None; // Enter Main without a specific section focus.
           }
           Panel::Main => {
               // From Main, check if a plan tab is active.
               let has_active_plan_tab = self
                   .tabs
                   .active_tab
                   .and_then(|idx| self.tabs.open_tabs.get(idx))
                   .map(|tab| matches!(tab, TabContent::Plan { .. }))
                   .unwrap_or(false);

               if has_active_plan_tab {
                   // Plan tab is active; cycle accordion sections.
                   let sections = Self::accordion_section_order();
                   let next = match self.focused_section {
                       None => Some(sections[0]), // First entry: focus Scope.
                       Some(sec) => {
                           // Find the current section's position and move to the next.
                           sections
                               .iter()
                               .position(|&s| s == sec)
                               .and_then(|pos| {
                                   if pos + 1 < sections.len() {
                                       Some(Some(sections[pos + 1])) // Next section.
                                   } else {
                                       Some(None) // Signal to wrap to Sidebar.
                                   }
                               })
                               .unwrap_or(Some(sections[0])) // Fallback: reset to Scope.
                       }
                   };

                   match next {
                       Some(sec) => {
                           self.focused_section = Some(sec);
                       }
                       None => {
                           // Last section (Status); wrap to Sidebar.
                           self.focused_panel = Panel::Sidebar;
                           self.focused_section = None;
                       }
                   }
               } else {
                   // No plan tab active: wrap from Main to Sidebar.
                   self.focused_panel = Panel::Sidebar;
                   self.focused_section = None;
               }
           }
       }
   }
   ```
2. Verify the method compiles and handles all cases: Sidebar→Main, Main→accordion-section, accordion-section→accordion-section, last-accordion-section→Sidebar, Main-without-plan-tab→Sidebar.

- **Depends on:** extend-focus-model
- **Done when:** The code compiles without errors. The move_focus_forward() method exists on App. A unit test (see task move-focus-backward) exercises all traversal paths: Sidebar→Main, Main→Scope (with plan tab), Scope→Architecture→Tasks→Status (with plan tab), Status→Sidebar, and Main→Sidebar (no plan tab). Each transition correctly updates focused_panel and focused_section. cargo test/clippy/fmt green.

---

### move-focus-backward — Implement move_focus_backward() for Shift+Tab key traversal

With move_focus_forward in place, we now implement backward Shift+Tab traversal. Shift+Tab should reverse the forward order: Sidebar → last accordion section (Status, if plan tab active) → accordion sections in reverse → Main → Sidebar. Like move_focus_forward, it must check whether a plan tab is active. This method is a pure state transition with no IO or rendering.

**Steps:**

1. Add the following method to the App impl block (in `crates/makina/src/app.rs`):

   ```rust
   /// Move focus backward (Shift+Tab key) through the hierarchy in reverse:
   /// Sidebar → Status (if plan tab active) → Accordion sections in reverse → Main → Sidebar (wrap).
   pub fn move_focus_backward(&mut self) {
       match self.focused_panel {
           Panel::Sidebar => {
               // From Sidebar, Shift+Tab checks if a plan tab is active.
               let has_active_plan_tab = self
                   .tabs
                   .active_tab
                   .and_then(|idx| self.tabs.open_tabs.get(idx))
                   .map(|tab| matches!(tab, TabContent::Plan { .. }))
                   .unwrap_or(false);

               if has_active_plan_tab {
                   // Plan tab is active; jump to the last accordion section (Status).
                   let sections = Self::accordion_section_order();
                   self.focused_panel = Panel::Main;
                   self.focused_section = Some(sections[sections.len() - 1]);
               }
               // If no plan tab active, stay in Sidebar (no movement).
           }
           Panel::Main => {
               // From Main, check if a plan tab is active.
               let has_active_plan_tab = self
                   .tabs
                   .active_tab
                   .and_then(|idx| self.tabs.open_tabs.get(idx))
                   .map(|tab| matches!(tab, TabContent::Plan { .. }))
                   .unwrap_or(false);

               if has_active_plan_tab {
                   // Plan tab is active; step backward through accordion sections.
                   let sections = Self::accordion_section_order();
                   let next = match self.focused_section {
                       None => {
                           // No section focused yet; jump to the last one (Status).
                           Some(sections[sections.len() - 1])
                       }
                       Some(sec) => {
                           // Find the current section's position and move to the previous.
                           sections
                               .iter()
                               .position(|&s| s == sec)
                               .and_then(|pos| {
                                   if pos > 0 {
                                       Some(Some(sections[pos - 1])) // Previous section.
                                   } else {
                                       Some(None) // Signal to exit to Sidebar.
                                   }
                               })
                               .unwrap_or(Some(sections[0]))
                       }
                   };

                   match next {
                       Some(sec) => {
                           self.focused_section = Some(sec);
                       }
                       None => {
                           // First section (Scope); exit to Sidebar.
                           self.focused_panel = Panel::Sidebar;
                           self.focused_section = None;
                       }
                   }
               } else {
                   // No plan tab active: exit to Sidebar.
                   self.focused_panel = Panel::Sidebar;
                   self.focused_section = None;
               }
           }
       }
   }
   ```
2. Verify the method compiles and handles all cases: Sidebar→Status (with plan), Status→Tasks→Scope (with plan), Scope→Sidebar, Main→Sidebar (no plan), and Shift+Tab with no section focused yet should jump to Status.

- **Depends on:** extend-focus-model, move-focus-forward
- **Done when:** The code compiles without errors. The move_focus_backward() method exists on App. A unit test exercises all reverse traversal paths: Sidebar→Status (with plan tab), Status→Tasks→Architecture→Scope, Scope→Sidebar, Sidebar→stays-in-Sidebar (no plan tab), and Main→Sidebar (no plan tab). Each transition correctly updates focused_panel and focused_section. cargo test/clippy/fmt green.

---

## 0003 — Visual Focus Indicator

### add-visual-focus-indicator — Add visible focus indicator to accordion section headers

With Tab/Shift+Tab wired and focus moving, users have no visual feedback showing which accordion section is currently focused. The sidebar already has a visual indicator (highlight_symbol and highlight_style), but the accordion pane renders all sections in the same style regardless of focus. We need to add styling to make the currently focused section stand out — e.g., a bright background color or bold border on the section header.

**Steps:**

1. In `crates/makina/src/ui.rs`, locate the render_accordion_section function definition (starts at line ~1424). Update its signature to accept a `focused: bool` parameter:

   ```rust
   fn render_accordion_section(
       title: &str,
       section: AccordionSection,
       expanded: &HashSet<AccordionSection>,
       content: &str,
       focused: bool,  // new parameter
   ) -> Vec<Line<'static>> {
   ```
2. Inside render_accordion_section, update the header rendering to apply focus styling:

   ```rust
   let marker = if expanded.contains(&section) { "[−]" } else { "[+]" };
   let mut header_style = Style::default().fg(Color::Cyan);
   if focused {
       // Apply a distinctive background and bold modifier when focused.
       header_style = header_style
           .bg(Color::DarkGray)
           .add_modifier(Modifier::BOLD);
   }
   let header = Line::from(Span::styled(
       format!(" {marker} {title}"),
       header_style,
   ));
   ```
3. In render_plan_accordion_pane (starts at line ~1327), update all four calls to render_accordion_section to pass the focused flag. For example:

   ```rust
   let scope_focused = matches!(app.focused_section, Some(AccordionSection::Scope));
   lines.extend(render_accordion_section(
       "SCOPE",
       AccordionSection::Scope,
       &expanded,
       plan.scope_text.as_deref().unwrap_or("(no SCOPE.md)"),
       scope_focused,  // pass focus flag
   ));
   lines.push(Line::from(""));

   let arch_focused = matches!(app.focused_section, Some(AccordionSection::Architecture));
   lines.extend(render_accordion_section(
       "ARCHITECTURE",
       AccordionSection::Architecture,
       &expanded,
       plan.architecture_text.as_deref().unwrap_or("(no ARCHITECTURE.md)"),
       arch_focused,  // pass focus flag
   ));
   lines.push(Line::from(""));

   let tasks_focused = matches!(app.focused_section, Some(AccordionSection::Tasks));
   lines.extend(render_accordion_section(
       "TASKS",
       AccordionSection::Tasks,
       &expanded,
       &tasks_text,
       tasks_focused,  // pass focus flag
   ));
   lines.push(Line::from(""));

   let status_focused = matches!(app.focused_section, Some(AccordionSection::Status));
   lines.extend(render_accordion_section(
       "STATUS",
       AccordionSection::Status,
       &expanded,
       plan.status_text.as_deref().unwrap_or("(no STATUS.md)"),
       status_focused,  // pass focus flag
   ));
   ```

- **Depends on:** extend-focus-model
- **Done when:** The code compiles without errors. render_accordion_section accepts a focused: bool parameter and applies a distinct style (background + bold) to the section header when focused is true. render_plan_accordion_pane passes the correct focused flag (based on app.focused_section) to each of the four section renders. Manual testing or a UI test shows the focused accordion section has a visually distinct header (bright background or underline) compared to unfocused sections. cargo test/clippy/fmt green.

---

## 0004 — Keyboard Integration & Testing

### wire-tab-shift-tab-events — Wire Tab and Shift+Tab key events to focus-movement functions

The move_focus_forward and move_focus_backward methods are implemented but not yet connected to keyboard events. Currently Tab maps to AppEvent::FocusNext (a simple enum variant). We need to add a new AppEvent::FocusPrev variant for Shift+Tab, update the keymap in event.rs to emit both events, and update the app.rs event handlers to call the new methods.

**Steps:**

1. In `crates/makina/src/app.rs` near the AppEvent enum definition (starting ~line 598), add a new variant after FocusNext:

   ```rust
   pub enum AppEvent {
       // ... existing variants ...
       FocusNext,  // Tab key
       FocusPrev,  // Shift+Tab key
       // ... rest of the enum ...
   }
   ```
2. In `crates/makina/src/event.rs` at line ~1207 where Tab is currently handled, update the keymap to distinguish Tab from Shift+Tab:

   ```rust
   KeyCode::Tab => {
       if key.modifiers.contains(KeyModifiers::SHIFT) {
           AppEvent::FocusPrev
       } else {
           AppEvent::FocusNext
       }
   }
   ```
3. In `crates/makina/src/app.rs` in the update() method, locate the FocusNext handler (currently ~line 1684). Replace it with:

   ```rust
   AppEvent::FocusNext => {
       self.move_focus_forward();
       true
   }
   AppEvent::FocusPrev => {
       self.move_focus_backward();
       true
   }
   ```

- **Depends on:** move-focus-forward, move-focus-backward
- **Done when:** The code compiles without errors. Tab key emits AppEvent::FocusNext, Shift+Tab emits AppEvent::FocusPrev. The update() method calls move_focus_forward() on FocusNext and move_focus_backward() on FocusPrev. Manual testing (or integration test below) shows Tab and Shift+Tab navigate through Sidebar, Main, accordion sections, and wrap correctly. cargo test/clippy/fmt green.

---

### integration-test-tab-traversal — Integration test: Tab traversal through all regions with visual feedback

With focus movement logic, keyboard wiring, and visual indicators in place, we need to verify that the entire feature works end-to-end. An integration test will exercise the Tab key from Sidebar through Main and accordion sections, ensuring the focus model transitions are correct and test assertions document the expected behavior. This test serves as both verification and a behavioral spec for future maintainers.

**Steps:**

1. In `crates/makina/tests/integration_tests.rs` (or a new file `focus_navigation.rs` in the tests directory), add comprehensive integration tests for Tab and Shift+Tab navigation. The tests should: (1) start with focus in Sidebar, (2) Tab to Main, (3) with a plan tab active, Tab into and cycle through accordion sections (Scope → Architecture → Tasks → Status), (4) Tab from Status wraps back to Sidebar, (5) Tab again returns to Main, (6) verify Shift+Tab reverses the direction at all boundaries, and (7) verify Tab from Main with no plan tab active wraps directly to Sidebar.

- **Depends on:** wire-tab-shift-tab-events, add-visual-focus-indicator
- **Done when:** All integration tests pass (cargo test test_tab_* and test_shift_tab_*). The tests document the expected Tab/Shift+Tab behavior: forward/backward traversal through regions, accordion section cycling, wrapping at boundaries, and correct handling when no plan tab is active. Test assertions confirm focused_panel and focused_section are updated correctly at each step. cargo test/clippy/fmt green. Manual smoke test confirms Tab and Shift+Tab navigate visibly through the TUI with focus indicator highlighting.

---

### verify-accordion-entry-toggle — Verify Enter toggles focused accordion section and keyboard-only navigation works

With Tab moving focus to accordion sections, users should be able to toggle (expand/collapse) them using Enter or a keyboard shortcut. Plan 0032 already implements S/A/T/Z toggles for quick section access. This task verifies that Enter also toggles the focused section (a standard UI convention) and that the keybindings do not conflict. We also ensure the visual focus indicator, Tab navigation, and section toggling all work together end-to-end.

**Steps:**

1. In `crates/makina/src/app.rs`, update the AppEvent::ToggleTreeNode handler to support accordion sections. When focused_panel is Main and focused_section is Some, pressing Enter should toggle that section's expand/collapse state in the accordion_state HashMap.
2. Verify that the S/A/T/Z keybindings (from event.rs, lines ~1228–1251) still work correctly and do not conflict with Tab/Shift+Tab navigation. These should remain available for quick section toggling without moving focus.
3. Add unit tests to verify Enter toggles the focused accordion section: starting with a focused section (e.g., Scope), pressing Enter should add/remove it from the accordion_state for the active plan tab's slug. Test both expand and collapse transitions.

- **Depends on:** wire-tab-shift-tab-events, add-visual-focus-indicator
- **Done when:** The code compiles without errors. Enter (AppEvent::ToggleTreeNode) toggles the focused accordion section when focused_panel is Main and focused_section is Some. The S/A/T/Z keybindings continue to work as before (no conflicts). Unit tests pass: expanding and collapsing the focused section via Enter works correctly. cargo test/clippy/fmt green. Manual end-to-end test shows Tab moves focus with visible indicator, and Enter toggles the focused section; S/A/T/Z toggles still work independently.

---

**End of plan 0034 TASKS.** When every "Done when" bullet is green, users can
navigate the TUI entirely by keyboard: Tab moves focus forward through Sidebar,
Main, and nested accordion sections with a visible focus indicator; Shift+Tab
reverses; focused accordion sections toggle via Enter; the hardcoded S/A/T/Z
keybindings remain available; and all gate commands remain green.
