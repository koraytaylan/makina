# Architecture — Plan 0033 (deltas)

> The concrete deltas. This plan touches
> `crates/makina/src/app.rs`, `crates/makina/src/event.rs`, and
> `crates/makina/src/ui.rs`.
> Line numbers are hints; locate by symbol.

## 0001 — Hitbox and Mouse Tracking

Today `event.rs:1098–1099` discards the mouse coordinates carried by
`MouseEventKind::ScrollUp` / `ScrollDown` events, emitting bare `ScrollUp` /
`ScrollDown` variants with no location. The render pass `pub fn render(app:
&App, frame: &mut Frame)` (`ui.rs:58`) computes the layout `Rect` for every
panel as local variables that are discarded after drawing, so there is no
persistent record of which rectangle belongs to which logical panel. Ratatui's
`Rect` holds `x`, `y`, `width`, `height` — enough to test point-in-rectangle
membership.

**Render holds `&App`.** `ui.rs:3` documents the invariant that render takes an
immutable borrow and holds no mutable state. The existing code records per-frame
state through interior mutability — `last_scroll_max: std::cell::Cell<u16>`
(`app.rs:1090`) and `selection_panes: std::cell::RefCell<..>` with
`set_selection_panes(&self, ..)`. This plan keeps that signature, so every field
written *during render* (`panel_geometries`, `last_scroll_maxes`) is a `RefCell`
and its setter takes `&self`; fields written only from `App::update(&mut self)`
(`scroll_offsets`) stay plain.

**Edits:**

**Add `ScrollablePanel` and `PanelGeometry`.** In `crates/makina/src/app.rs`,
define the panel-identity enum once (reused as the `HashMap` key in 0002), and a
copy-able record pairing that enum with its rendered rectangle. Storing the enum
(not a `&'static str` name) makes a panel rename a compile error instead of a
silently dropped scroll:

```rust
/// Identifies a scrollable panel for per-panel scroll state and hitbox testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScrollablePanel {
    Sidebar,
    Exchange,
    PlanAccordion,
    DependencyView,
}

/// Geometry of a single scrollable panel (used for mouse hitbox testing).
#[derive(Debug, Clone, Copy)]
pub struct PanelGeometry {
    /// Which panel this rectangle belongs to.
    pub panel: ScrollablePanel,
    /// The rendered rectangle of the panel content area.
    pub rect: ratatui::layout::Rect,
}
```

Add `pub panel_geometries: std::cell::RefCell<Vec<PanelGeometry>>` to `App`
(next to `selection_panes`, near `app.rs:1217`), initialized to
`RefCell::new(Vec::new())` in `App::new()` (near `app.rs:1458`), with a `&self`
setter the render pass calls each frame (mirroring `set_selection_panes`):

```rust
/// Record the geometries of rendered panels for hitbox testing.
pub fn set_panel_geometries(&self, geoms: Vec<PanelGeometry>) {
    *self.panel_geometries.borrow_mut() = geoms;
}
```

**Hit-test a coordinate against the recorded geometries.** Add a `panel_at`
helper that returns the panel containing a `(col, row)`, or `None`:

```rust
/// Given a mouse column and row, return the panel under it (if any).
pub fn panel_at(&self, col: u16, row: u16) -> Option<ScrollablePanel> {
    self.panel_geometries
        .borrow()
        .iter()
        .find(|g| {
            col >= g.rect.x && col < (g.rect.x + g.rect.width)
                && row >= g.rect.y && row < (g.rect.y + g.rect.height)
        })
        .map(|g| g.panel)
}
```

**Carry coordinates on scroll events.** Extend `AppEvent` with
coordinate-bearing scroll variants so the event loop can route by position:

```rust
/// Scroll the panel under the cursor up by one line (mouse wheel up at col, row).
ScrollUpAt(u16, u16),
/// Scroll the panel under the cursor down by one line (mouse wheel down at col, row).
ScrollDownAt(u16, u16),
```

**Capture the mouse coordinates in `event.rs`.** At `event.rs:1097–1104`, the
wheel arms emit the new variants instead of discarding `m.column` / `m.row`; the
selection arms are unchanged:

```rust
CrosstermEvent::Mouse(m) => match m.kind {
    MouseEventKind::ScrollUp => AppEvent::ScrollUpAt(m.column, m.row),
    MouseEventKind::ScrollDown => AppEvent::ScrollDownAt(m.column, m.row),
    // ... selection events unchanged ...
},
```

**Properties that make this safe:**

- `Rect::x`, `Rect::y`, `Rect::width`, `Rect::height` are stable ratatui 0.30
  fields, so the membership test is a pure arithmetic comparison.
- Panel geometries are recomputed every frame before events are processed, so
  the coordinates are always tested against the current layout.
- `panel_at` returns `None` when no panel contains the coordinate (a border, the
  status bar, or area outside the rendered panes) — callers must handle that
  case, which they do by dropping the scroll.

## 0002 — Per-Panel Scroll State

Today the exchange pane has the only scroll state: `exchange_scroll` and
`last_scroll_max: std::cell::Cell<u16>` (`app.rs:1090`, a `Cell` so the `&App`
render pass can update the rendered bottom via `.set(..)` at `ui.rs:1301`).
There are no scroll offsets for the sidebar, plan accordion, or dependency-view
overlay, so those panes cannot remember an independent position.

**Edits:**

**Reuse `ScrollablePanel` (defined in 0001).** The enum is the `HashMap` key; it
is declared once in 0001 and not redeclared here.

**Replace the single-panel fields with per-panel maps.** Swap
`exchange_scroll` / `last_scroll_max` for two maps keyed by `ScrollablePanel`; a
missing key defaults to `0`. `scroll_offsets` is written only from
`App::update(&mut self)` (the scroll handlers), so it is a plain `HashMap`.
`last_scroll_maxes` is written *during render* (which holds `&App`), so it is a
`RefCell<HashMap<..>>` — it replaces the old `last_scroll_max: Cell<u16>` write
path at `ui.rs:1301`. Auto-follow stays a separate boolean, so the exchange
pane's pin-to-bottom behavior is unchanged:

```rust
/// Per-panel manual scroll offset (in lines from top). Written only from
/// `App::update`, so plain. Auto-follow is tracked separately (`exchange_auto_follow`).
pub scroll_offsets: std::collections::HashMap<ScrollablePanel, u16>,

/// Per-panel highest scroll offset the last render produced (clamp ceiling).
/// `RefCell` because the `&App` render pass writes it each frame.
pub last_scroll_maxes: std::cell::RefCell<std::collections::HashMap<ScrollablePanel, u16>>,
```

`App::new()` initializes `scroll_offsets` as `HashMap::new()` and
`last_scroll_maxes` as `RefCell::new(HashMap::new())`; `HashMap` / `HashSet` are
already imported via `use std::collections::{HashMap, HashSet};`. Render-pass
reads use `app.last_scroll_maxes.borrow().get(&panel)`; render-pass writes use
`app.last_scroll_maxes.borrow_mut().insert(panel, max)`.

**Re-key the scroll helpers by panel.** `scroll_up`, `scroll_down`, and
`effective_offset` take a `ScrollablePanel` and operate on the corresponding map
entry; auto-follow logic is preserved for the exchange pane only. `scroll_up`
disengages auto-follow and anchors the manual offset to the last rendered
bottom:

```rust
/// Scroll the given panel up by one line, disengaging auto-follow if the panel
/// is the exchange pane.
pub fn scroll_up(&mut self, panel: ScrollablePanel) {
    if panel == ScrollablePanel::Exchange && self.exchange_auto_follow {
        self.exchange_auto_follow = false;
        // Anchor the manual offset to the last rendered bottom before stepping.
        let bottom = self
            .last_scroll_maxes
            .borrow()
            .get(&ScrollablePanel::Exchange)
            .copied()
            .unwrap_or(0);
        self.scroll_offsets.insert(ScrollablePanel::Exchange, bottom);
    }
    // A missing key defaults to 0 via or_insert, so new panels start at the top.
    let current = self.scroll_offsets.entry(panel).or_insert(0);
    *current = current.saturating_sub(1);
}
```

`scroll_down` clamps at `scroll_max` and re-engages auto-follow when the
exchange pane reaches the bottom; `effective_offset` (exchange-only) returns
`scroll_max` while auto-following and otherwise the clamped manual offset from
`scroll_offsets[Exchange]`. A single generic accessor `panel_offset(panel,
scroll_max)` centralizes the render-time read+clamp so no render site
re-implements it: it delegates to `effective_offset` for the exchange pane (to
honor auto-follow) and otherwise returns `scroll_offsets[panel].min(scroll_max)`.
The four existing non-test callers of the signature-changing methods —
`app.rs:1721` and `app.rs:1808` (`self.scroll_up()` → `self.scroll_up(Exchange)`),
and `app.rs:1734` and `app.rs:1816` (`self.scroll_down(self.last_scroll_max.get())`
→ read the exchange max from the RefCell map, then
`self.scroll_down(Exchange, max)`) — are all updated to the new signatures.

**Properties that make this safe:**

- `HashMap::entry(..).or_insert(0)` means a panel absent from the map starts at
  the top — new panels need no special initialization.
- Each panel's offset and max live under their own key, so scrolling one panel
  cannot perturb another.
- `exchange_auto_follow` remains in `App` and is still consulted when computing
  the exchange pane's effective offset, so existing pin-to-bottom behavior is
  preserved verbatim.

## 0003 — Mouse-Position-Aware Scroll Event Dispatch

Today `App::update(&mut self, event) -> bool` (`app.rs:1673`) handles
`AppEvent::ScrollUp` (`app.rs:1807`) / `AppEvent::ScrollDown` (`app.rs:1811`)
unconditionally, calling the exchange-pane scroll methods with no panel
parameter and no coordinate. Every wheel event scrolls the exchange pane
regardless of where the cursor sits.

**Edits:**

**Dispatch `ScrollUpAt` / `ScrollDownAt` by hitbox.** In `App::update()`, the
new arms hit-test the coordinate and route the returned `ScrollablePanel`
straight through — `panel_at` already returns `Option<ScrollablePanel>`, so
there is no string round-trip. The down arm looks up that panel's
`last_scroll_max` from the RefCell map for clamping:

```rust
AppEvent::ScrollUpAt(col, row) => {
    if let Some(panel) = self.panel_at(col, row) {
        self.scroll_up(panel);
    }
}
AppEvent::ScrollDownAt(col, row) => {
    if let Some(panel) = self.panel_at(col, row) {
        let scroll_max = self
            .last_scroll_maxes
            .borrow()
            .get(&panel)
            .copied()
            .unwrap_or(0);
        self.scroll_down(panel, scroll_max);
    }
}
```

**Keep the legacy arms as fallbacks.** The existing `ScrollUp` / `ScrollDown`
variants remain for test code and any legacy callers; they dispatch to the
exchange pane as before, so nothing that emitted them breaks.

**Properties that make this safe:**

- When `panel_at` returns `None` (cursor on a border or unhandled area) the
  scroll is dropped — no map entry changes.
- `panel_at` returns a `ScrollablePanel` directly, so the dispatch is a total
  match with no string round-trip — a panel rename is a compile error, never a
  silently dropped scroll.
- The clamp ceiling comes from the panel's own `last_scroll_max`, so a routed
  scroll can never exceed the content height of the targeted pane.

## 0004 — Ratatui Scrollbar Widget Integration

Today no `Scrollbar` widget is used anywhere; every scrollable pane (exchange,
sidebar, accordion, dependency view) truncates text and gives no visual feedback
about content height or scroll position. Ratatui 0.30's `widgets::Scrollbar` is
available in the workspace dependency but unimported.

**Edits:**

**Import the widgets.** Add `Scrollbar`, `ScrollbarOrientation`, and
`ScrollbarState` to the `widgets::{ ... }` group in
`crates/makina/src/ui.rs:43–45` (which currently ends `... Paragraph, Wrap,`).

**Arrowhead decision (applies to all four scrollbars).** ratatui 0.30's
`Scrollbar::default()` sets `begin_symbol = Some("▲")` and `end_symbol =
Some("▼")` (track `║`, thumb `█`). Those arrows occupy the first/last cells and
would collide with borders/title rows. This plan calls
`.begin_symbol(None).end_symbol(None)` on every scrollbar so only the track and
thumb render; each scrollbar's rect is the exact content strip the pane scrolls.

**Render a scrollbar on the exchange pane.** In `render_exchange_pane()`
(`ui.rs:1087`), the paragraph is drawn into `inner = block.inner(area)`
(`ui.rs:1130`), not `area`. After recording this frame's scroll-max
(`app.last_scroll_maxes.borrow_mut().insert(ScrollablePanel::Exchange,
scroll_max)`, replacing the old `app.last_scroll_max.set(scroll_max)` at
`ui.rs:1301`), render a vertical-right scrollbar into **`inner`** so the track
aligns with the paragraph rows and the (suppressed) arrows do not land on the
TOP border. `content_length` is the real line count `total_lines` (already a
`usize` at `ui.rs:1296`) — no `+ height`, no `.min(1000)`:

```rust
// Render scrollbar only when content exceeds the viewport.
if scroll_max > 0 {
    let mut scrollbar_state = ScrollbarState::new(total_lines).position(scroll_offset as usize);
    let scrollbar = Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None);
    frame.render_stateful_widget(scrollbar, inner, &mut scrollbar_state);
}
```

**Render scrollbars on the other scrollable panes.** Each pane records its own
`last_scroll_maxes[panel]` during render and uses the **real item/line count**
as `content_length`:

- **Sidebar** (`render()`, list at `ui.rs:295`): the list draws into
  `sidebar_area` inside a `Borders::ALL` block, so render the scrollbar into
  `sidebar_block.inner(sidebar_area)` to preserve the border. `content_length =
  items.len()`; record `last_scroll_maxes[Sidebar] =
  items.len().saturating_sub(inner.height)`.
- **Plan accordion** (`render_plan_accordion_pane`, `ui.rs:1411–1416`): the
  paragraph renders directly into `area` with no block, so split `area`
  horizontally into `[Min(0), Length(1)]` and render the scrollbar into the
  1-cell right column (text into the left). `content_length = lines.len()`;
  record `last_scroll_maxes[PlanAccordion] = lines.len().saturating_sub(area.height)`.
- **Dependency view** (`render_dependency_view`, three arms at `ui.rs:735/764/850`):
  each arm records `last_scroll_maxes[DependencyView]` from its own `lines.len()`
  and renders into `inner = block.inner(area)`.

**Properties that make this safe:**

- `Scrollbar` rendering reads scroll state only; the only mutation is the
  interior-mutable `last_scroll_maxes` write, which is the rendered bound the
  next event loop clamps against.
- When content fits, `scroll_max` is `0` and the scrollbar is skipped entirely,
  matching the locked decision that scrollbars appear only when needed.
- `ScrollbarState` clamps its position into `[0, content_length)`, so an
  out-of-range offset draws at the edge rather than panicking. Using the real
  item/line count as `content_length` (no `.min(1000)` cap) keeps the thumb size
  and position faithful for arbitrarily long content.

## 0005 — Scroll Offset Clamping and Rendering

Today `render_exchange_pane()` calls `app.effective_offset(scroll_max)` and
passes the result to `.scroll((offset, 0))` on the paragraph. The other panes
(sidebar, accordion, dependency view) do not yet read any per-panel offset, so
their scroll state has nowhere to land.

**Edits:**

**Keep the exchange pane reading through `effective_offset`.** The exchange
offset computation is unchanged — `effective_offset` already reads
`scroll_offsets[Exchange]` and applies auto-follow — so the auto-follow path is
preserved by construction:

```rust
// effective_offset reads scroll_offsets[Exchange] and honors auto-follow.
let scroll_offset = app.effective_offset(scroll_max);
```

**Apply the accordion offset, clamped to its max.** The accordion paragraph at
`ui.rs:1411–1416` currently pins to the bottom (`let scroll =
total.saturating_sub(area.height); ... .scroll((scroll, 0))`). `Paragraph::scroll`
is a **consuming builder** (`scroll(self, ..) -> Self`), so the offset must be
folded into the single chained `.scroll(..)` — a separate `para.scroll((..))`
statement is a no-op. Replace the footer-pin with the clamped per-panel offset
via `panel_offset` (an intended behavior change: the accordion no longer
auto-pins to the bottom), rendering into the `content_area` reserved by the
scrollbar's column split:

```rust
let accordion_scroll_max = (lines.len() as u16).saturating_sub(area.height);
let accordion_scroll_offset = app.panel_offset(ScrollablePanel::PlanAccordion, accordion_scroll_max);
let para = Paragraph::new(lines)
    .wrap(Wrap { trim: false })
    .scroll((accordion_scroll_offset, 0));
frame.render_widget(para, content_area);
```

**Apply the sidebar offset directly to `ListState`.** ratatui 0.30's `ListState`
**does** expose offset control — `with_offset(usize) -> Self` and
`offset_mut(&mut self) -> &mut usize` (`ratatui-widgets` `src/list/state.rs`,
`offset` = "Index of the first item to be displayed"). So the sidebar list
content is scrolled directly: at `ui.rs:292–293`, build
`ListState::default().with_selected(app.tree_cursor).with_offset(scroll_offsets[Sidebar])`
(ratatui still nudges the offset to keep the selection visible, which is
acceptable). There is no "future work" deferral.

**Apply the dependency-view offset per arm.** `render_dependency_view` builds a
fresh paragraph in each of three arms (`ui.rs:735/764/850`); each folds
`panel_offset(DependencyView, dep_scroll_max)` into its own chained `.scroll(..)`.

**Properties that make this safe:**

- Clamping every offset to its panel's `scroll_max` (via `panel_offset`) rules
  out an out-of-bounds `.scroll()` argument.
- A panel absent from the map defaults to `0`, so an untouched pane renders from
  the top.
- The exchange pane's `effective_offset` path is untouched, so its auto-follow
  behavior is carried over unchanged.

## 0006 — Integration and Testing

Today the render pass computes every panel's `Rect` but discards it, and
workstreams 0001–0005 add the infrastructure without splicing it into the
rendering loop. Nothing yet calls `set_panel_geometries`, so `panel_at` always
sees an empty list.

**Edits:**

**Record the visible panel geometries each frame.** The panel rects live in
**mutually-exclusive arms** of `match (active_plan_tab, app.selected_run())`
(`ui.rs:362`): `plan_area` only in the `(Some(plan), _)` arm (`ui.rs:371`),
`exchange_pane_area` and the dependency rect (`dep_split[0]`, unnamed) only in
the `(None, Some(run))` arm (`ui.rs:478–489`). `sidebar_area` (`ui.rs:91`) is
function-wide. So geometry cannot be collected in one block "at the end" — there
is no point where all three rects are live, and there is no `dep_area` variable.
Instead, declare the accumulator before the match, push each rect from inside
the arm where it is in scope (binding `let dep_area = dep_split[0];` for the
dependency rect), and call the `&self` setter after the match:

```rust
// Before the match:
let mut panel_geoms = vec![PanelGeometry { panel: ScrollablePanel::Sidebar, rect: sidebar_area }];

// In the (Some(plan), _) arm, after `let plan_area = plan_split[1];`:
panel_geoms.push(PanelGeometry { panel: ScrollablePanel::PlanAccordion, rect: plan_area });

// In the (None, Some(run)) arm, inside the dependency-view else-branch:
let dep_area = dep_split[0];
render_dependency_view(app, frame, dep_area);
panel_geoms.push(PanelGeometry { panel: ScrollablePanel::DependencyView, rect: dep_area });
// ... and after render_exchange_pane(..):
panel_geoms.push(PanelGeometry { panel: ScrollablePanel::Exchange, rect: exchange_pane_area });

// After the match (set_panel_geometries takes &self via RefCell):
app.set_panel_geometries(panel_geoms);
```

Only panes actually drawn this frame are recorded, so a hidden dependency view
or unselected run contributes no geometry.

**Cover routing and scrollbars end to end.** Add tests asserting that a scroll
over sidebar coordinates moves only the sidebar offset, a scroll over exchange
coordinates moves only the exchange offset, a scroll outside every panel is a
no-op, exchange auto-follow survives the new routing, two panels keep
independent offsets, and the scrollbar widget renders when content is tall and is
absent when it fits.

**Properties that make this safe:**

- Geometries are recollected every frame, so they always reflect the current
  layout, including resize and dynamic pane visibility.
- Hit-testing runs in the event loop after rendering, so coordinates are tested
  against the most recent geometry.
- The tests assert both the happy paths (correct panel scrolls, scrollbar
  appears/disappears) and the edge cases (no panel under the cursor, auto-follow
  preservation), locking in the invariants above.

## Test strategy

- **0001 (hitbox).** Unit tests assert `panel_at` returns `None` when no panel
  contains the coordinate (including a coordinate exactly on the exclusive right
  edge `rect.x + rect.width`) and `Some(ScrollablePanel::..)` when the coordinate
  falls inside a recorded rect; the `event.rs` change keeps existing selection
  tests green.
- **0002 (per-panel state).** A unit test confirms a missing map key defaults to
  `0` without panicking, and that scrolling one panel leaves another's offset
  untouched. The updated auto-follow test passes the panel parameter and still
  asserts the exchange pane pins to the bottom.
- **0003 (dispatch).** `scroll_up_at_routes_to_sidebar` and
  `scroll_down_at_routes_to_exchange` assert the routed offsets change; a
  coordinate outside every panel is asserted to be a no-op. Legacy
  `ScrollUp` / `ScrollDown` tests still pass.
- **0004 (scrollbars).** Per pane (exchange, sidebar, accordion) a test renders
  tall content through `render(&app, f)` on a `TestBackend` and asserts a real
  scrollbar glyph (`█` thumb or `║` track — never `│`/`▐`) appears on the pane's
  right content column of the buffer; a companion test renders short content and
  asserts none of `█`/`║`/`▲`/`▼` appear there.
- **0005 (offsets).** Tests set a non-zero offset for the exchange and accordion
  panes and assert the rendered paragraph reflects it, with clamping preventing
  out-of-bounds scroll.
- **0006 (integration).** `scroll_up_at_coordinates_targets_sidebar`,
  `scroll_down_at_coordinates_targets_exchange`,
  `scroll_at_coordinates_outside_panels_is_noop`,
  `exchange_auto_follow_preserved_with_routing`, and
  `multiple_panels_maintain_independent_scroll` exercise the wired system; a
  geometry-recording test asserts `panel_geometries` reflects the current layout.
- All tests keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.

## Interaction with prior work

- **0027 / sidebar expansion.** Expanded runs and tasks can push the sidebar
  list past the viewport; 0033 gives that pane its own `scroll_offsets[Sidebar]`
  entry, applies it directly via `ListState::with_offset` / `offset_mut`
  (ratatui 0.30 exposes both — list content **is** scrolled, not deferred), and
  draws a right-edge scrollbar inside the sidebar block's inner rect so the
  border is preserved.
- **0032 / tabbed plan accordion.** The plan accordion pane introduced in 0032
  becomes independently scrollable here via `scroll_offsets[PlanAccordion]` and a
  right-edge scrollbar; this plan reads the same `render_plan_accordion_pane`
  entry point and adds offset application plus the scrollbar, introducing no new
  accordion mechanism.
- **0019 / mouse scroll + selection.** Extends 0019's wheel handling: the bare
  `ScrollUp` / `ScrollDown` variants remain as fallbacks, and the new
  `ScrollUpAt` / `ScrollDownAt` variants carry the cursor coordinate so the
  dependency-view overlay (visible when `DependencyViewMode` is not `Off`) and
  the other panes route correctly instead of every wheel event hitting the
  exchange pane.
