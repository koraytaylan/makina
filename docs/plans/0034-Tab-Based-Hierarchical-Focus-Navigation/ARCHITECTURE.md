# Architecture — Plan 0034 (deltas)

> The concrete deltas. This plan touches
> `crates/makina/src/app.rs`, `crates/makina/src/ui.rs`,
> `crates/makina/src/event.rs`, and the integration suite under
> `crates/makina/tests/integration_tests.rs`.
> Line numbers are hints; locate by symbol.

## 0001 — Focus Model Extension

Today `crates/makina/src/app.rs:1154` holds `focused_panel: Panel`, a binary
enum whose two variants — `Panel::Sidebar` and `Panel::Main`
(`crates/makina/src/app.rs:569–572`) — are the only focus state the app tracks.
The accordion expand/collapse map (`crates/makina/src/app.rs:1176–1179`, keyed
by plan slug) and the `AccordionSection` enum (`crates/makina/src/app.rs:932–940`,
variants `Scope`, `Architecture`, `Tasks`, `Status`) already exist, but nothing
records *which* accordion section owns focus. With only `focused_panel`, Tab has
nowhere to go inside the Main pane.

**Edits:**

**Add a `FocusState` enum.** Declared once after the `Panel` enum
(`crates/makina/src/app.rs:~572`), it is the comprehensive focus descriptor that
0002 traverses and 0003 reads when styling. Its variants are exhaustive — there
is no fallthrough state:

```rust
/// Hierarchical focus: which nested item inside the focused panel owns focus.
/// When `focused_panel == Panel::Main`, the variant carries the focused
/// accordion section (if any); in the Sidebar the tree cursor owns focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusState {
    /// Sidebar tree node has focus; `tree_cursor` identifies the node.
    TreeNode,
    /// Main pane has focus, but no accordion section is focused yet.
    MainPane,
    /// A specific accordion section in the active plan tab has focus.
    AccordionSection(AccordionSection),
}
```

**Add the `focused_section` field.** Inserted immediately after `focused_panel`
(`crates/makina/src/app.rs:~1154`), initialized to `None` in `App::new()`. It is
written only by the Tab/Shift+Tab logic of 0002, so no legacy code path can
populate it:

```rust
/// When `focused_panel == Panel::Main`, which accordion section (if any) has
/// focus. Defaults to `None`; the first Tab into Main focuses the first section.
pub focused_section: Option<AccordionSection>,
```

**Project the two fields into one `FocusState`.** A `focused_state()` accessor
folds `focused_panel` and `focused_section` into the comprehensive variant —
`None` in Main collapses to `MainPane`, a `Some` to `AccordionSection`:

```rust
/// Return the comprehensive focus state (region + nested section if any).
pub fn focused_state(&self) -> FocusState { /* match focused_panel, map focused_section */ }
```

**Pin the cycle order in one place.** A static `accordion_section_order()`
returns `[Scope, Architecture, Tasks, Status]` — the single source of truth that
both 0002 traversal methods index into, so forward and backward order can never
drift apart:

```rust
/// The fixed Tab cycle order through accordion sections (mirrors the plan doc
/// hierarchy: SCOPE → ARCHITECTURE → TASKS → STATUS).
pub(crate) fn accordion_section_order() -> &'static [AccordionSection] { /* &[Scope, ..] */ }
```

**Properties that make this safe:**

- The `FocusState` variants are exhaustive, so every match over focus is total
  with no fallthrough.
- `focused_section` defaults to `None` and is written only by the Tab/Shift+Tab
  methods added in 0002, so no legacy code can corrupt it.
- This task is data-structure-only: the old `focused_panel` toggle path is left
  in place, so behavior is unchanged until 0002 and 0004 wire the new methods in.

## 0002 — Tab/Shift+Tab Navigation Logic

The new model from 0001 is inert until something moves focus through it. Today
the only focus movement is the binary toggle the legacy Tab handler performs
(`crates/makina/src/app.rs:1684–1689`); there is no method that advances or
retreats through the Sidebar → Main → accordion-sections sequence, and no logic
that consults the active tab to decide whether accordion sections are reachable
at all.

**Edits:**

**Add `move_focus_forward()` (Tab).** A pure state transition over
`focused_panel` / `focused_section`. From Sidebar it enters Main with no section
focus; from Main it cycles accordion sections only when a plan tab is active —
otherwise it wraps straight to Sidebar. The active-tab probe matches
`TabContent::Plan { .. }` on the tab at `self.tabs.active_tab` indexed into
`self.tabs.open_tabs`:

```rust
/// Move focus forward: Sidebar → Main → accordion sections → Sidebar (wrap).
/// Accordion sections are entered only when a plan tab is active; the last
/// section (Status) wraps to Sidebar, never to Main or Scope.
pub fn move_focus_forward(&mut self) { /* match focused_panel { Sidebar => Main; Main => cycle or wrap } */ }
```

**Add `move_focus_backward()` (Shift+Tab).** The exact reverse: from Sidebar with
a plan tab active it jumps directly to the last section (Status); from Main it
steps to the previous section, and exits the first section (Scope) to Sidebar.
Both methods index the same `accordion_section_order()` slice from 0001, so a
stale `focused_section` resets to a valid endpoint rather than panicking:

```rust
/// Move focus backward: Sidebar → Status → sections in reverse → Main → Sidebar.
/// Shift+Tab from Sidebar with a plan tab active jumps straight to Status;
/// from the first section (Scope) it exits to Sidebar.
pub fn move_focus_backward(&mut self) { /* mirror of move_focus_forward */ }
```

**Properties that make this safe:**

- Traversal order is deterministic and sourced from the single
  `accordion_section_order()` slice, so forward and backward stay symmetric.
- Wrapping is explicit at every boundary (last section → Sidebar, Sidebar →
  Main/Status), and each branch leaves `focused_panel` / `focused_section` in a
  consistent pair — no unreachable or half-written state.
- Both methods consult the active tab content and enter accordion focus only when
  a `TabContent::Plan` tab is active, so a non-plan tab cannot strand focus in a
  section that is not rendered.

## 0003 — Visual Focus Indicator

Today the sidebar already shows focus — `render` sets `highlight_symbol "▶ "`
and `highlight_style` on the tree list (`crates/makina/src/ui.rs:280–288`). The
accordion pane does not: `render_plan_accordion_pane`
(`crates/makina/src/ui.rs:1327–1410`) renders all four section headers through
`render_accordion_section` (`crates/makina/src/ui.rs:~1424`) in one uniform
style, so a focused section is visually indistinguishable from the rest.

**Edits:**

**Thread a `focused` flag into `render_accordion_section`.** The header style
gains a focus branch — `Color::DarkGray` background plus `Modifier::BOLD` — that
mirrors the sidebar's existing highlight convention. The flag is the only new
input; the marker and content rendering are unchanged:

```rust
fn render_accordion_section(/* .., */ focused: bool) -> Vec<Line<'static>> {
    let mut header_style = Style::default().fg(Color::Cyan);
    if focused {
        // Distinctive background + bold marks the Tab-focused section header.
        header_style = header_style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
    }
    /* .. build header Line with header_style, then content lines .. */
}
```

**Pass the per-section focus flag from the pane.** Each of the four
`render_accordion_section` calls in `render_plan_accordion_pane` derives its flag
from `app.focused_section`, so exactly one header can be focused at a time:

```rust
// One flag per section; e.g. for SCOPE:
let scope_focused = matches!(app.focused_section, Some(AccordionSection::Scope));
// .. then pass `scope_focused` into the SCOPE render call (likewise ARCHITECTURE/TASKS/STATUS).
```

**Properties that make this safe:**

- Focus styling is applied only when `app.focused_section` matches that section,
  so unfocused headers render exactly as before.
- `render` keeps its immutable `&App` borrow — the change reads `focused_section`
  and writes no state, touching only the header `Style`.
- The chosen `DarkGray` background plus bold is distinct from the default cyan
  header yet not jarring, matching the locked indicator decision in SCOPE.

## 0004 — Keyboard Integration & Testing

Today Tab is mapped to `AppEvent::FocusNext` in the keymap
(`crates/makina/src/event.rs:1207`), and the `FocusNext` handler in `update()`
(`crates/makina/src/app.rs:1684–1689`) performs the legacy binary toggle. There
is no event for Shift+Tab, and the new `move_focus_forward` / `move_focus_backward`
methods from 0002 are unreachable. The S/A/T/Z accordion toggles
(`crates/makina/src/event.rs:1228–1251`) and Enter
(`AppEvent::ToggleTreeNode`) are independent of this path.

**Edits:**

**Add a `FocusPrev` event.** A new `AppEvent` variant beside `FocusNext`; because
`update()` matches `AppEvent` exhaustively, omitting its arm is a compile error:

```rust
pub enum AppEvent {
    // .. existing variants ..
    FocusNext, // Tab
    FocusPrev, // Shift+Tab
}
```

**Split Tab by the Shift modifier.** The keymap arm at
`crates/makina/src/event.rs:1207` checks `KeyModifiers::SHIFT` to choose the
event:

```rust
KeyCode::Tab => {
    if key.modifiers.contains(KeyModifiers::SHIFT) { AppEvent::FocusPrev } else { AppEvent::FocusNext }
}
```

**Route both events to the 0002 methods.** The `update()` handler replaces the
legacy toggle so `FocusNext` / `FocusPrev` call the pure traversal methods:

```rust
AppEvent::FocusNext => { self.move_focus_forward(); true }
AppEvent::FocusPrev => { self.move_focus_backward(); true }
```

**Let Enter toggle the focused section.** The `AppEvent::ToggleTreeNode` handler
gains a branch: when `focused_panel == Panel::Main` and `focused_section` is
`Some`, Enter flips that section's entry in the slug-keyed `accordion_state`
map — the same map the S/A/T/Z keys mutate, so the two input methods stay
orthogonal and conflict-free.

**Cover traversal and toggling end to end.** Integration tests in
`crates/makina/tests/integration_tests.rs` drive `App::update` with
`FocusNext` / `FocusPrev` and assert `focused_panel` / `focused_section` at each
step: Sidebar → Main → Scope → Architecture → Tasks → Status → Sidebar forward,
the reverse under Shift+Tab, the no-plan-tab wrap (Main → Sidebar directly), and
Enter expanding/collapsing the focused section in `accordion_state`.

**Properties that make this safe:**

- The new `AppEvent::FocusPrev` variant is matched exhaustively in `update()`, so
  a missing handler fails to compile.
- `move_focus_forward` / `move_focus_backward` are pure state transitions with no
  IO or rendering, so wiring them in cannot introduce side effects.
- Enter's accordion branch and the S/A/T/Z toggles mutate the same
  `accordion_state` map but are reached by disjoint key handlers, so neither input
  method shadows the other.

## Test strategy

- **0001 (focus model).** Unit tests assert `focused_state()` returns `TreeNode`
  in the Sidebar, `MainPane` when Main is focused with `focused_section == None`,
  and `AccordionSection(..)` when a section is set; `accordion_section_order()` is
  asserted to be `[Scope, Architecture, Tasks, Status]`.
- **0002 (traversal).** Unit tests exercise every forward path (Sidebar → Main,
  Main → Scope, Scope → Architecture → Tasks → Status, Status → Sidebar,
  Main-without-plan-tab → Sidebar) and every backward path (Sidebar → Status,
  Status → Tasks → Architecture → Scope, Scope → Sidebar, Sidebar-without-plan-tab
  stays put), asserting the `focused_panel` / `focused_section` pair after each.
- **0003 (indicator).** A render test on a `TestBackend` with a focused section
  asserts the focused header carries the bold/`DarkGray` styling and the other
  three headers do not.
- **0004 (integration).** `tab_navigates_sidebar_to_main_to_accordion_to_sidebar`
  and its Shift+Tab counterpart drive `update` through the full cycle and wrap; a
  separate test asserts Enter toggles only the focused section's `accordion_state`
  entry, and that S/A/T/Z still toggle without moving focus.
- All tests keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.

## Interaction with prior work

- **0031 / unified sidebar navigation & tabs.** The active-tab probe in 0002
  reuses 0031's `TabContent::Plan { .. }` variant and the `self.tabs.active_tab`
  index into `self.tabs.open_tabs` to decide whether accordion sections are
  reachable; no new tab mechanism is added.
- **0032 / tabbed plan accordion sections.** This plan reuses 0032's
  `AccordionSection` enum, slug-keyed `accordion_state` map, and
  `render_plan_accordion_pane` entry point. It adds the orthogonal
  `focused_section` cursor, Tab/Shift+Tab traversal into those sections, the
  focused-header styling, and an Enter toggle — leaving 0032's S/A/T/Z quick
  toggles intact as a faster alternative.
- **0018 / arrow-key navigation.** Tab/Shift+Tab move focus *between* regions and
  sections; arrow keys remain reserved for sidebar tree navigation and pane
  scrolling, so the two schemes do not contend for the same keys.
