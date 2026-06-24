# XAgent Plan 0035 — Four TUI Interaction Enhancements (Mouse Click, Arrow Keys, Markdown Rendering, Task Tabs)

This plan adds four focused TUI (ratatui) interaction improvements to the Makina viewer. First, detect mouse clicks on accordion section headers in plan tabs and toggle their expand/collapse state, completing the mouse-interaction story from plan 0019. Second, implement Right arrow as a Tab-equivalent (forward focus/expand) and Left arrow as Shift+Tab-equivalent (backward focus/collapse) for keyboard navigation parity with desktop conventions, reducing the need to reach for Tab. Third, properly render Markdown in task tabs' content pane (headings, bold/italic, lists, code blocks, links, rules) instead of displaying raw source, extending plan 0020's hardening to task-entry content. Fourth, when pressing Enter on a task in the sidebar, open a new tab displaying that task's full entry content (similar to plan-tab opening), enabling parallel task viewing and task-comparison workflows.

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

## 0001 — Accordion Click-to-Toggle

### accordion-click-detection — Accordion Header Click Detection and Toggle

Today, plan-accordion pane headers (SCOPE, ARCHITECTURE, TASKS, STATUS) render with `[+]` or `[-]` markers and are styled but non-interactive. Pressing 's'/'a'/'t'/'z' toggles the corresponding section via `AppEvent::ToggleAccordionSection(section)` (`event.rs:1238–1265`). However, mouse clicks on the headers are translated to text-selection events (`AppEvent::SelectionStart`, `event.rs:1100–1102`), not to toggle intents. Users expect to click the header to expand/collapse; the keyboard-only shortcut does not meet modern UX expectations.

This workstream adds mouse-click detection for accordion headers, dispatching a toggle intent when a click lands on a header. The header rendering happens in `render_accordion_section` (`ui.rs:1432–1461`), which builds each section header as a single `Line` containing the marker (`[+]` or `[-]`) and title. When rendering, the exact row (in the rendered pane) is deterministic, but the pane is scrollable, so the bounding box must be computed relative to the pane's visible area and stored in `App` for the event loop to hit-test against.

**Steps:**

1. In `crates/makina/src/app.rs`, add a new field to `App` to track accordion section header bounds: `pub accordion_header_bounds: Vec<(AccordionSection, Rect)>` (initialized to an empty Vec). This field holds the bounding box of each accordion section header visible in the current render, keyed by the section type. Clear it on each render so it reflects the current pane layout.
2. In `crates/makina/src/ui.rs`, modify `render_plan_accordion_pane` to compute and record header bounds. Before rendering each section header (in the blocks around lines 1367, 1378, 1392, 1403), calculate the row range of the header in the pane's rendered area. After rendering the paragraph, record the bounds: `app.accordion_header_bounds.push((section, rect))` where `rect` is the header's bounding box. Repeat for each of the four sections (SCOPE, ARCHITECTURE, TASKS, STATUS).
3. In `crates/makina/src/event.rs`, extend the mouse-event translation in `translate_terminal_event` (`event.rs:1097–1107`). After the existing `MouseEventKind::ScrollUp/Down` arms, add a new arm for left-button down: `MouseEventKind::Down(MouseButton::Left) => { /* hit-test accordion headers */ }`. If the click (m.column, m.row) falls within any bounds in `app.accordion_header_bounds`, emit `AppEvent::ToggleAccordionSection(section)`. Otherwise, emit `AppEvent::SelectionStart(m.column, m.row)`. This way, header clicks toggle; non-header clicks select text as before.
4. Add unit tests to `event.rs`: `test_accordion_header_click_toggles_section` should build a `CrosstermEvent::Mouse` with a click inside a known header region, assert the result is `AppEvent::ToggleAccordionSection(section)`, and verify a click outside any header is `AppEvent::SelectionStart`.
5. Add an app-level test to `app.rs`: `test_accordion_header_click_in_rendered_pane` opens a plan tab, verifies a section is collapsed, manually constructs an `AppEvent::ToggleAccordionSection(section)`, confirms the section expands, and verifies `accordion_header_bounds` was populated during render.

- **Depends on:** —
- **Done when:** Clicking on a rendered accordion section header (anywhere within the row containing `[+]`/`[-]` and the section title) toggles the section expand/collapse state in the plan-accordion pane. Clicks outside the header rows remain text-selection events. Tests `test_accordion_header_click_toggles_section` and `test_accordion_header_click_in_rendered_pane` pass. Existing accordion tests and mouse-selection tests remain green. cargo test/clippy/fmt green.

---

## 0002 — Arrow-Key Hierarchical Navigation

### arrow-key-navigation-parity — Arrow-Key Navigation Parity with Tab/Shift+Tab

Today, keyboard navigation uses Tab to switch between panels. In the sidebar, Right/Left arrows expand/collapse tree nodes or cross focus to the main pane. In the main pane, Right/Left are not mapped to any action. Users coming from browsers or text editors expect Right arrow to move focus forward (like Tab) and Left arrow to move focus backward (like Shift+Tab). This workstream extends the arrow-key handlers to support this parity when the main pane is focused, while preserving the existing sidebar behavior.

**Steps:**

1. In `crates/makina/src/event.rs`, locate the `translate_key` function (around line 1115). Find the two arms that handle Right and Left arrows (lines 1291–1293).
2. Replace these two arms with panel-aware logic: `KeyCode::Right => match focused_panel { Panel::Sidebar => AppEvent::FocusRightOrExpand, Panel::Main => AppEvent::FocusNext, },` and similarly for Left.
3. Verify that `AppEvent::FocusNext` and `AppEvent::FocusPrev` already exist in the `AppEvent` enum and are handled in `App::update`. They should be; if not, add them.
4. In `README.md`, find the keyboard-help text that documents the Tab keybinding for panel switching. Update it to mention arrow-key parity.
5. Add a unit test to `event.rs`: `test_arrow_keys_in_main_pane_emit_focus_events`. Create key-press events for Right and Left arrows with `focused_panel = Panel::Main`, assert the result is `AppEvent::FocusNext` and `AppEvent::FocusPrev`. Create the same with `focused_panel = Panel::Sidebar` and assert they still return `FocusRightOrExpand` and `FocusLeftOrCollapse`.
6. Add an integration-level app test to `app.rs`: `test_arrow_right_in_main_pane_cycles_tabs`. Open two plan tabs, set focus to the main pane, press Right arrow, and assert the active-tab index advances. Press Left arrow and assert it goes back.

- **Depends on:** accordion-click-detection
- **Done when:** In the main pane, Right arrow behaves like Tab (forward focus). Left arrow behaves like Shift+Tab (backward focus). In the sidebar, Right/Left arrows preserve their existing behavior. Tests pass. All existing arrow-key and Tab tests remain green. Documentation mentions arrow-key parity. cargo test/clippy/fmt green.

---

## 0003 — Markdown Content Rendering

### task-entry-markdown-rendering — Markdown Rendering in Task-Entry Panes

Plan 0020 hardened the `render_markdown` function in `markup.rs:76–337` to properly render code blocks, lists, links, thematic breaks, and respect the `width` parameter. This function is applied to exchange-pane responses. Task-entry content (the task metadata and steps from the TASKS.md file) is not yet rendered through this function; there is no dedicated task-entry pane yet. This workstream creates a new `render_task_entry_pane` function that displays task metadata and entry text, rendered with Markdown support and proper width-threading.

**Steps:**

1. In `crates/makina/src/ui.rs`, create a new public function `render_task_entry_pane(app: &App, run: &RunView, task_idx: usize, frame: &mut Frame, area: Rect)`. This function mirrors `render_exchange_pane`: it takes a run and task index, computes the inner area of a bordered block, and renders content. The function should render a bordered block, compute the inner area, and if the task exists, build a `Vec<Line>` containing: Task header (ID and title), state badge, dependencies and gated status, task entry text (rendered via `render_markdown`). If the task does not exist, render a placeholder.
2. In `render_task_entry_pane`, thread the width through to `render_markdown`. Compute `inner.width`, pass it to `render_markdown(entry_text, base_style, inner.width)`. This ensures text wraps to the actual pane width.
3. Ensure `TaskView` (or the equivalent task struct) has an `entry_text` field. If it does not, add it.
4. In `crates/makina-core/src/orchestrator.rs`, extend the task-loading logic to parse and cache the task-entry text. When a task is parsed from the TASKS.md file, extract the raw Markdown entry for that task and store it in the `TaskView` struct.
5. In the main render path (`crates/makina/src/ui.rs`, the `render` function around line 350–490), add a dispatch for task-tab rendering. When the active tab is `TabContent::Task { plan_slug, task_id }`, call `render_task_entry_pane` instead of (or in addition to) the exchange pane.
6. Add a helper function `find_task_in_selected_run(app: &App, task_id: &TaskId) -> Option<(usize, usize)>` that searches the currently-selected run for a task matching `task_id` and returns `(run_idx, task_idx)`.
7. Add unit tests to `ui.rs`: `test_task_entry_pane_renders_markdown` should set up a task with markdown entry text, call `render_task_entry_pane`, and assert the resulting `Line`s contain properly-rendered spans. `test_task_entry_pane_respects_pane_width` should verify that text wraps to the pane's inner width.

- **Depends on:** accordion-click-detection
- **Done when:** Task-entry panes render when a `TabContent::Task` tab is active. The task's entry text is rendered through `render_markdown` with the pane's inner width. Code blocks, lists, links, and rules render as readable structure. Tests pass. Existing exchange-pane and markdown tests remain green. cargo test/clippy/fmt green.

---

## 0004 — Task Entry Opening in Tabs

### task-entry-tab-opening — Task Entry Opening in Tabs

Pressing Enter on a task node (`TreeNode::Task { run, task }`) in the sidebar opens a tab (`AppEvent::OpenTab(TabContent::Task { plan_slug, task_id })`). However, the content pane does not yet render the task *entry* when a task tab is active; the render path needs to be extended to call the new `render_task_entry_pane` function. This workstream ensures that when a task tab is opened, it is populated with the task-entry view, completing the UI flow for task-based tab opening and enabling side-by-side task comparison.

**Steps:**

1. Verify that `event.rs:423–435` correctly dispatches `AppEvent::OpenTab(TabContent::Task { plan_slug, task_id })` when Enter is pressed on a `TreeNode::Task` node in the sidebar. This should already work (part of plan 0032).
2. In the main render path (`ui.rs:350–490`), verify that the active-tab dispatch includes logic to determine the active tab content.
3. Extend the active-tab render dispatch to handle `TabContent::Task`. Add a match arm that finds the task in the selected run and renders the task-entry pane.
4. Ensure the tab state persists across navigation. When the user opens a task tab, then navigates the sidebar, the tab should remain open and switchable.
5. Add an integration test to `app.rs`: `test_task_tab_opens_and_renders_entry`. Construct an app with an open run and tasks, open a task tab, verify the tab appears in `app.tabs.open_tabs`, render the frame, and assert the rendered output contains the task ID and entry content.

- **Depends on:** task-entry-markdown-rendering, arrow-key-navigation-parity
- **Done when:** Pressing Enter on a task node in the sidebar opens a task tab that renders the task-entry pane (with the task's metadata, ID, title, and entry text rendered as Markdown). Multiple task tabs can be open simultaneously and are switchable. The task-entry content matches the task definition from the TASKS.md file. Test passes. Existing tab tests remain green. cargo test/clippy/fmt green.

---

**End of plan 0035 TASKS.** When every "Done when" bullet is green, four
complementary TUI interaction enhancements ship: clicking accordion section
headers in plan/task tabs toggles expand/collapse; Right/Left arrows work as
Tab/Shift+Tab equivalents in the main pane for forward/backward focus; Markdown
in task-entry tabs renders headings, code blocks, lists, links, and rules
properly; and pressing Enter on a task in the sidebar opens a new tab displaying
the task entry with full metadata and rendered content — all with the gate
commands green.
