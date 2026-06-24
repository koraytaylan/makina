# Architecture — Plan 0035 (deltas)

> The concrete deltas. This plan touches
> `crates/makina/src/event.rs`, `crates/makina/src/app.rs`,
> `crates/makina/src/ui.rs`, `crates/makina/src/markup.rs`,
> `crates/makina-core/src/orchestrator.rs`, and `README.md`.
> Line numbers are hints; locate by symbol.

## 0001 — Accordion Click-to-Toggle

Today the plan-accordion pane renders in `render_plan_accordion_pane`
(`crates/makina/src/ui.rs:1327–1425`): each section header is built by
`render_accordion_section` (`crates/makina/src/ui.rs:1432–1461`) as a single
`Line` carrying the `[+]` / `[-]` marker and the section title. The header is
styled but inert — only the keyboard toggles it, mapping `s` / `a` / `t` / `z` to
`AppEvent::ToggleAccordionSection(section)` (`crates/makina/src/event.rs:1238–1265`).
Mouse input takes a different path: `translate_terminal_event`
(`crates/makina/src/event.rs:1097–1107`) turns a left-button click into
`AppEvent::SelectionStart` / `SelectionExtend` / `SelectionEnd` for text
selection, and never dispatches a click as a toggle intent. There is nowhere for
a click on a header to land.

**Edits:**

**Record header bounds during render.** Add a per-frame field to `App`
(`crates/makina/src/app.rs`, beside the other render-derived hit-test state),
cleared at the top of each accordion render so it always reflects the current
layout:

```rust
/// Bounding box of each visible accordion section header, recorded during the
/// plan-accordion render so the event loop can hit-test mouse clicks against it.
/// Cleared and repopulated every frame, so resizes and pane reflows self-correct.
pub accordion_header_bounds: Vec<(AccordionSection, Rect)>,
```

**Push one entry per rendered header.** In `render_plan_accordion_pane`, after
laying out each of the four headers (SCOPE, ARCHITECTURE, TASKS, STATUS), record
its row range relative to the pane's visible area:

```rust
// The header occupies exactly one row in the pane; capture it for hit-testing.
app.accordion_header_bounds.push((section, header_rect));
```

**Hit-test left clicks before falling through to selection.** In
`translate_terminal_event`, add a `MouseEventKind::Down(MouseButton::Left)` arm
ahead of the existing selection arms. A click inside a recorded header toggles
that section; any other click stays a selection start:

```rust
MouseEventKind::Down(MouseButton::Left) => {
    // A click on a header toggles it; otherwise begin a text selection as before.
    if let Some((section, _)) = app
        .accordion_header_bounds
        .iter()
        .find(|(_, r)| r.x <= m.column && m.column < r.x + r.width
            && r.y <= m.row && m.row < r.y + r.height)
    {
        AppEvent::ToggleAccordionSection(*section)
    } else {
        AppEvent::SelectionStart(m.column, m.row)
    }
}
```

**Properties that make this safe:**

- Bounds are recomputed from scratch every frame, so terminal resizes and pane
  reflows never leave stale hit-test geometry — a click always tests against the
  layout the user is looking at.
- Only `Down(MouseButton::Left)` is intercepted; `Drag`, `Up`, and `Moved` stay
  on the existing selection path, so text selection in the content area is
  unaffected.
- The dispatch reuses the existing `AppEvent::ToggleAccordionSection` handler
  from plan 0032, so click and keyboard toggle share one mutation site and can
  never diverge.

## 0002 — Arrow-Key Hierarchical Navigation

Today Right/Left arrows are sidebar-only. In `translate_key`
(`crates/makina/src/event.rs:~1115`), the Right/Left arms
(`crates/makina/src/event.rs:1291–1293`) expand/collapse tree nodes and cross
focus when the sidebar is focused (`crates/makina/src/app.rs:1917–1965`); Tab
already switches panels via `AppEvent::FocusNext` and Shift+Tab reverses via
`BackTab` (`crates/makina/src/event.rs:1210–1217`). When the main pane is
focused, Right/Left fall through to a no-op, so a user in the main pane has no
arrow-key equivalent of Tab/Shift+Tab.

**Edits:**

**Make the Right/Left arms panel-aware.** In `translate_key`, branch on
`focused_panel`: in the main pane Right becomes the Tab-equivalent and Left the
Shift+Tab-equivalent; in the sidebar the existing expand/collapse-or-cross-focus
behavior is preserved verbatim:

```rust
// Right = Tab (forward) and Left = Shift+Tab (backward) in the main pane;
// in the sidebar they keep expanding/collapsing tree nodes as before.
KeyCode::Right => match focused_panel {
    Panel::Main => AppEvent::FocusNext,
    Panel::Sidebar => AppEvent::FocusRightOrExpand,
},
KeyCode::Left => match focused_panel {
    Panel::Main => AppEvent::FocusPrev,
    Panel::Sidebar => AppEvent::FocusLeftOrCollapse,
},
```

**Reuse the existing focus events.** `AppEvent::FocusNext` / `FocusPrev` already
exist and are handled in `App::update` (Tab/Shift+Tab route through them today),
so this task wires keys only — no new event variant or handler is introduced.

**Document the parity.** In `README.md`, update the keyboard-help text that
describes Tab panel switching to mention that Right/Left act as Tab/Shift+Tab in
the main pane.

**Properties that make this safe:**

- Sidebar behavior is byte-for-byte unchanged — the new logic only adds a
  `Panel::Main` arm, so existing arrow-key tree navigation cannot regress.
- The main-pane arrows route to `FocusNext` / `FocusPrev`, which are already
  exercised by the Tab/Shift+Tab tests, so the new bindings inherit proven
  forward/backward traversal.
- The change is confined to the key-translation layer; no focus-state machine is
  modified, so the navigation semantics are exactly Tab's and stay symmetric.

## 0003 — Markdown Content Rendering

Plan 0020 hardened `render_markdown` (`crates/makina/src/markup.rs:76–337`) to
render code blocks, lists, links, thematic breaks, and to respect the `width`
parameter for hard-wrapping; exchange-pane responses already flow through it.
Task-entry content does not: there is no dedicated pane that shows a task's entry
(role, title, steps, dependencies, notes) from the TASKS.md file, and where such
text would appear it is the raw Markdown source, not the structured render plan
0020 produces.

**Edits:**

**Cache the entry text at discovery.** `TaskView` (or the equivalent task struct)
gains a field for the raw Markdown entry, populated when a task is parsed so the
render thread never does IO:

```rust
/// Raw Markdown entry for this task (title, steps, deps, notes) from TASKS.md,
/// captured at parse time so rendering never touches disk.
pub entry_text: String,
```

In `crates/makina-core/src/orchestrator.rs`, extend the task-loading path to
extract each task's raw Markdown block and store it in `entry_text`.

**Add a task-entry pane mirroring the exchange pane.** In
`crates/makina/src/ui.rs`, add `render_task_entry_pane`, which draws a bordered
block, computes the inner area, and — for an existing task — builds a `Vec<Line>`
of the header (ID + title), state badge, dependencies / gated status, and the
entry text rendered through `render_markdown` at the pane's inner width:

```rust
/// Render a task's entry (metadata + Markdown body) into a bordered pane.
/// Width is taken from the pane's inner area so wrapping matches the pane, and
/// the body reuses plan 0020's hardened `render_markdown` — no new parser.
pub fn render_task_entry_pane(app: &App, run: &RunView, task_idx: usize, frame: &mut Frame, area: Rect) {
    // .. draw block, compute `inner` ..
    let body = markup::render_markdown(entry_text, base_style, inner.width);
    // .. push header / badge / deps lines, then `body`; placeholder if the task is absent ..
}
```

**Resolve the active task tab to a concrete task.** Add a
`find_task_in_selected_run` helper that maps a `TaskId` to indices in the
selected run:

```rust
/// Resolve a `TaskId` to `(run_idx, task_idx)` within the selected run, if present.
fn find_task_in_selected_run(app: &App, task_id: &TaskId) -> Option<(usize, usize)> { /* .. */ }
```

Then, in the main render path (`crates/makina/src/ui.rs`, the `render` function
around lines 350–490), dispatch to `render_task_entry_pane` when the active tab
is `TabContent::Task { .. }`.

**Properties that make this safe:**

- The body reuses the already-hardened, already-tested `render_markdown`; no new
  Markdown parser or renderer is introduced, so plan 0020's invariants carry over
  unchanged.
- Inner width is recomputed from the pane's borders each frame, so wrapping
  always matches the rendered pane and survives resizes.
- `entry_text` is captured once at discovery/parse time and is immutable at
  runtime, so the render path is pure and never blocks on disk IO.

## 0004 — Task Entry Opening in Tabs

Today pressing Enter on a `TreeNode::Task { run, task }` node in the sidebar
already dispatches `AppEvent::OpenTab(TabContent::Task { plan_slug, task_id })`
(`crates/makina/src/event.rs:423–435`, from plan 0032's tab infrastructure), and
the tab is recorded in `app.tabs.open_tabs`. But when that tab is active the main
content pane does not render the task *entry* — the render path has no arm that
resolves the tab to the task and draws the entry view from 0003.

**Edits:**

**Render the task-entry pane for an active task tab.** In the active-tab dispatch
of the main render path (`crates/makina/src/ui.rs:350–490`), add a
`TabContent::Task` arm that resolves the task via 0003's
`find_task_in_selected_run` and renders it:

```rust
// An active task tab shows the task entry (metadata + Markdown body), not the log.
if let Some(TabContent::Task { task_id, .. }) = active_tab_content(app) {
    if let Some((run_idx, task_idx)) = find_task_in_selected_run(app, task_id) {
        render_task_entry_pane(app, &app.runs[run_idx], task_idx, frame, content_area);
        return;
    }
}
```

**Reuse the existing event plumbing.** No change to `crates/makina/src/event.rs`
for opening — Enter already dispatches `OpenTab(TabContent::Task { .. })`, and
`TabState::open_tab` dedups by `TabContent` equality, so reopening an
already-open task focuses its tab rather than duplicating it. Multiple task tabs
therefore coexist and stay switchable for side-by-side comparison.

**Properties that make this safe:**

- `TabContent::Task` already exists and is tested (plan 0032), so this task only
  completes its render side — no new tab mechanism, identity, or dedup logic is
  added.
- Entry rendering is delegated to 0003's `render_task_entry_pane`, so the same
  Markdown and width-threading invariants apply; the two workstreams share one
  rendering site.
- Task entries are immutable at runtime and the sidebar remains the selector, so
  opening, switching, and re-selecting tabs cannot mutate the underlying task or
  strand a tab on a stale task.

## Test strategy

- **0001 (click-to-toggle).** `test_accordion_header_click_toggles_section`
  builds a `CrosstermEvent::Mouse` left-click inside a known header region and
  asserts the result is `AppEvent::ToggleAccordionSection(section)`, while a click
  outside any header is `AppEvent::SelectionStart`. An app-level
  `test_accordion_header_click_in_rendered_pane` opens a plan tab, confirms a
  section is collapsed, dispatches the toggle, asserts the section expands, and
  asserts `accordion_header_bounds` was populated during render.
- **0002 (arrow parity).** `test_arrow_keys_in_main_pane_emit_focus_events`
  asserts Right/Left with `focused_panel == Panel::Main` yield `FocusNext` /
  `FocusPrev`, and with `Panel::Sidebar` still yield
  `FocusRightOrExpand` / `FocusLeftOrCollapse`. An app-level
  `test_arrow_right_in_main_pane_cycles_tabs` opens two plan tabs, focuses the
  main pane, and asserts Right advances the active tab and Left steps back.
- **0003 (Markdown rendering).** `test_task_entry_pane_renders_markdown` sets up a
  task with Markdown entry text, calls `render_task_entry_pane`, and asserts the
  resulting `Line`s carry rendered spans (not raw source);
  `test_task_entry_pane_respects_pane_width` asserts the body wraps to the inner
  width. An orchestrator unit test asserts a parsed task populates `entry_text`.
- **0004 (task tabs).** `test_task_tab_opens_and_renders_entry` builds an app with
  an open run and tasks, opens a task tab, asserts it appears in
  `app.tabs.open_tabs`, renders a frame, and asserts the output contains the task
  ID and entry content; existing tab tests stay green.
- All tests keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.

## Interaction with prior work

- **0032 / tabbed plan accordion sections.** 0001 reuses 0032's
  `AccordionSection` enum, slug-keyed `accordion_state` map, and the
  `ToggleAccordionSection` handler — adding mouse hit-testing as a second input
  path into the same toggle, leaving the `s` / `a` / `t` / `z` keys intact. 0004
  builds on 0032's `TabContent::Task` variant, `TabState`, and `OpenTab` dedup;
  this plan only completes the render side for task tabs and adds no new tab
  system.
- **0020 / markdown rendering hardening.** 0003 reuses `markup::render_markdown`
  unchanged, threading the task pane's inner width through it; no new parser or
  renderer is introduced, and the deferred items (syntax highlighting, clickable
  links) stay deferred.
- **0034 / Tab-based hierarchical focus navigation.** 0002 reuses 0034's
  `FocusNext` / `FocusPrev` events and forward/backward traversal, mapping
  Right/Left onto them in the main pane; arrow keys keep their sidebar
  tree-navigation role, so the two schemes do not contend for the same keys.
