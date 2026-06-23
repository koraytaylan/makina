# XAgent Plan 0033 — Mouse-Position-Aware Scroll Routing with Scrollbars

Add per-panel geometry tracking via a new `PanelGeometry` struct that maps panel names to their rendered rectangles; extend `App` to store geometry and per-panel scroll offsets for sidebar, exchange pane, dependency view, and plan accordion; modify `event.rs::translate_terminal_event` to capture mouse coordinates in new scroll events (`ScrollUpAt`, `ScrollDownAt`) and pass them to a hitbox-detection function that determines the target panel; update `app.rs::update` to dispatch scroll events to the appropriate panel based on hitbox matching; update rendering in `ui.rs` and `render_plan_accordion_pane` to read per-panel scroll offsets and apply clamping; integrate ratatui's `Scrollbar` widget to render on the right edge of scrollable panels when content height exceeds viewport height; ensure auto-follow logic in the exchange pane still works; wire all layers together and verify that wheel-scroll-over-sidebar scrolls sidebar, wheel-scroll-over-main scrolls main content, scrollbars appear only when needed, and all gate commands pass.

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
- **Render holds `&App` (immutable):** `pub fn render(app: &App, frame: &mut Frame)`
  at `ui.rs:58` and `ui.rs:3` documents the invariant that render holds no
  mutable borrow. The existing code records per-frame state through interior
  mutability — `last_scroll_max: std::cell::Cell<u16>` (`app.rs:1090`, written
  via `app.last_scroll_max.set(..)` at `ui.rs:1301`) and `selection_panes:
  std::cell::RefCell<..>` with `set_selection_panes(&self, ..)`. This plan keeps
  that signature. Therefore every field this plan writes *during render*
  (`panel_geometries`, `last_scroll_maxes`) is a `RefCell`, and its setter takes
  `&self`; only fields written exclusively from `App::update(&mut self, ..)`
  (`scroll_offsets`) stay plain. Render-time reads use `.borrow()`; render-time
  writes use `.borrow_mut()`.
- **Test fixtures:** the real constructor is
  `App::new(api: Arc<dyn Api>, initial_runs: Vec<RunView>, repo_root: PathBuf)`
  (`app.rs:1421`). Build apps in tests exactly as the existing `ui.rs` tests do:
  `let api = Arc::new(PlaceholderApi::empty()); let app = App::new(api, vec![],
  std::path::PathBuf::from("."));` and render through the `make_terminal`/
  `terminal.draw(|f| render(&app, f))` helper at `ui.rs:2665`. Never write
  `App::new(/* ... */)` with elided args.

---

## 0001 — Hitbox and Mouse Tracking

### panel-geometry-struct — Add ScrollablePanel enum, PanelGeometry struct, and App tracking

The `App` struct (locate by `pub fn new(api:` at `app.rs:1421` and the field block above it) holds state for the TUI, including selection panes and panel focus. Currently there is no mechanism to record the rendered rectangles of scrollable panels (sidebar, exchange pane, dependency view, plan accordion) for hitbox testing in the event loop. Mouse coordinates are available in the event stream but not acted upon.

This task introduces the `ScrollablePanel` enum (the single source of truth for panel identity, reused by the per-panel scroll state in `scrollable-panel-enum`), a `PanelGeometry` struct that pairs a `ScrollablePanel` with its rendered `Rect`, and a `panel_geometries` field on `App` so the render pass can record panel locations and the event loop can query "which panel is under this mouse coordinate?". Storing the enum directly (not a `&'static str` name) makes a panel rename a compile error instead of a silently dropped scroll.

`panel_geometries` is written *during render* (which holds `&App` — see the header note), so it is a `std::cell::RefCell`, mirroring the existing `selection_panes: std::cell::RefCell<..>` / `set_selection_panes(&self, ..)` pattern. Locate that pattern by grepping `set_selection_panes` and `selection_panes` and copy its shape exactly.

**Steps:**

1. In `crates/makina/src/app.rs`, add the `ScrollablePanel` enum above the `App` struct (grep for the `PlaceholderApi`/`App` definitions to find a stable insertion point near the other public types):
   ```rust
   /// Identifies a scrollable panel for per-panel scroll state and hitbox testing.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
   pub enum ScrollablePanel {
       /// The left sidebar listing runs and tasks.
       Sidebar,
       /// The exchange pane (prompts and responses).
       Exchange,
       /// The plan accordion pane (when a plan tab is active).
       PlanAccordion,
       /// The dependency-view overlay (when `DependencyViewMode` is not `Off`).
       DependencyView,
   }
   ```
2. Directly below it, add the `PanelGeometry` struct (it holds the enum, not a name string):
   ```rust
   /// Geometry of a single scrollable panel (used for mouse hitbox testing).
   #[derive(Debug, Clone, Copy)]
   pub struct PanelGeometry {
       /// Which panel this rectangle belongs to.
       pub panel: ScrollablePanel,
       /// The rendered rectangle of the panel content area.
       pub rect: ratatui::layout::Rect,
   }
   ```
3. In the `App` struct, add a new field next to `selection_panes` (grep `pub selection_panes` — it is a `std::cell::RefCell`, found near `app.rs:1217`):
   ```rust
   /// Rendered rectangles of each scrollable panel, recorded each frame for
   /// hitbox testing. `RefCell` so the `&App` render pass can rewrite it,
   /// mirroring `selection_panes`.
   pub panel_geometries: std::cell::RefCell<Vec<PanelGeometry>>,
   ```
4. In `App::new()` (the field-initializer block, found near `app.rs:1458` where `last_scroll_max: std::cell::Cell::new(0)` is initialized), initialize the new field:
   ```rust
   panel_geometries: std::cell::RefCell::new(Vec::new()),
   ```
5. Add a setter taking `&self` (mirror `set_selection_panes`, grep to place it next to that method near `app.rs:1648`):
   ```rust
   /// Record the geometries of rendered panels for hitbox testing.
   /// Takes `&self` (interior mutability) so the render pass can call it.
   pub fn set_panel_geometries(&self, geoms: Vec<PanelGeometry>) {
       *self.panel_geometries.borrow_mut() = geoms;
   }
   ```
6. Add a hitbox-testing method returning the enum:
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

- **Depends on:** —
- **Done when:** `ScrollablePanel` (deriving `Debug, Clone, Copy, PartialEq, Eq, Hash`) and `PanelGeometry { panel: ScrollablePanel, rect: Rect }` are defined and compile. `App` has a `panel_geometries: std::cell::RefCell<Vec<PanelGeometry>>` field initialized empty in `App::new`. A unit test records two `PanelGeometry` entries via `set_panel_geometries(&app, ..)` and asserts: (a) `app.panel_at(col, row)` returns `None` for a coordinate outside every rect, (b) it returns `Some(ScrollablePanel::Sidebar)` for a coordinate strictly inside the sidebar rect and `Some(ScrollablePanel::Exchange)` inside the exchange rect, (c) a coordinate exactly on `rect.x + rect.width` (the exclusive right edge) returns `None`. cargo test/clippy/fmt green.

---

### scroll-events-with-coords — Add ScrollUpAt and ScrollDownAt events with mouse coordinates

Currently at `event.rs:1097–1104`, mouse wheel events discard the mouse coordinates `m.column` and `m.row` from the `MouseEvent` and emit only `ScrollUp` or `ScrollDown`, which carry no location information. To route scrolls to the correct panel, the event loop needs to pass the coordinates to the `update()` method.

Adding `ScrollUpAt(u16, u16)` and `ScrollDownAt(u16, u16)` variants to `AppEvent` allows the event loop to capture and route coordinates.

**Steps:**

1. In `crates/makina/src/app.rs`, locate the `AppEvent` enum (around line 598). Add two new variants before the existing `ScrollUp` and `ScrollDown` (or immediately after, depending on style):
   ```rust
   /// Scroll up at the given (column, row) — used for mouse-position-aware routing.
   ScrollUpAt(u16, u16),
   /// Scroll down at the given (column, row) — used for mouse-position-aware routing.
   ScrollDownAt(u16, u16),
   ```
2. Preserve the existing `ScrollUp` and `ScrollDown` variants for backward compatibility (tests and fallback).
3. In `crates/makina/src/event.rs`, locate the mouse event match at line 1097. Replace the `ScrollUp` and `ScrollDown` arms:
   ```rust
   CrosstermEvent::Mouse(m) => match m.kind {
       MouseEventKind::ScrollUp => AppEvent::ScrollUpAt(m.column, m.row),
       MouseEventKind::ScrollDown => AppEvent::ScrollDownAt(m.column, m.row),
       MouseEventKind::Down(MouseButton::Left) => AppEvent::SelectionStart(m.column, m.row),
       // ... rest unchanged ...
   },
   ```

- **Depends on:** panel-geometry-struct
- **Done when:** The new `ScrollUpAt` and `ScrollDownAt` variants are defined in `AppEvent`. The `event.rs` module compiles and passes `cargo test` (existing selection tests still pass). The new variants can be pattern-matched in `app.rs::update()` (verification occurs in task `scroll-event-dispatch`). cargo test/clippy/fmt green.

---

## 0002 — Per-Panel Scroll State

### scrollable-panel-enum — Replace exchange-only scroll fields with per-panel maps

Today the exchange pane has the only scroll-state fields: `exchange_scroll: u16` and `last_scroll_max: std::cell::Cell<u16>` (the latter at `app.rs:1090`, a `Cell` so the `&App` render pass can update the rendered bottom via `app.last_scroll_max.set(..)`). To support independent scrolling of sidebar, accordion, dependency view, and exchange pane, scroll state must be keyed by `ScrollablePanel` (the enum already defined in `panel-geometry-struct`).

This task replaces the single-panel fields with two `HashMap`s keyed by `ScrollablePanel`. Because the rendered scroll-max is written *during render* (which holds `&App`), `last_scroll_maxes` must be a `RefCell<HashMap<..>>` (it replaces the existing `Cell<u16>` write path at `ui.rs:1301`). `scroll_offsets` is written only from `App::update(&mut self, ..)` (the scroll event handlers), so it stays a plain `HashMap`.

**Steps:**

1. The `ScrollablePanel` enum is already declared in `panel-geometry-struct`; do not redeclare it. (Reference it here as `ScrollablePanel`.)
2. In the `App` struct, find the exchange scroll fields (grep `exchange_scroll` and `last_scroll_max`; `last_scroll_max: std::cell::Cell<u16>` is at `app.rs:1090`). Remove `exchange_scroll` and `last_scroll_max`, and add:
   ```rust
   /// Per-panel manual scroll offset (in lines from top). Key is the panel;
   /// a missing key defaults to 0. Written only from `App::update`, so a plain
   /// `HashMap` (no interior mutability needed).
   pub scroll_offsets: std::collections::HashMap<ScrollablePanel, u16>,

   /// Per-panel highest scroll offset the last render produced (the clamp
   /// ceiling for user input). `RefCell` because the `&App` render pass writes
   /// it each frame — this replaces the old `last_scroll_max: Cell<u16>` write
   /// path at `ui.rs:1301`. A missing key defaults to 0 (no scroll needed).
   pub last_scroll_maxes: std::cell::RefCell<std::collections::HashMap<ScrollablePanel, u16>>,
   ```
3. In `App::new()` (the field-initializer block near `app.rs:1458`, where `last_scroll_max: std::cell::Cell::new(0)` is initialized), remove the `exchange_scroll`/`last_scroll_max` initializers and add:
   ```rust
   scroll_offsets: std::collections::HashMap::new(),
   last_scroll_maxes: std::cell::RefCell::new(std::collections::HashMap::new()),
   ```
4. In the imports at the top of `app.rs`, ensure `HashMap` is imported (grep `use std::collections`); it is already imported via `use std::collections::{HashMap, HashSet};` — add `HashMap` only if absent.
5. Render-pass reads of `last_scroll_maxes` use `app.last_scroll_maxes.borrow().get(&panel).copied().unwrap_or(0)`; render-pass writes use `app.last_scroll_maxes.borrow_mut().insert(panel, max)`. Event-handler reads/writes of `scroll_offsets` use plain `.get`/`.entry`/`.insert` on `&mut self`.

- **Depends on:** panel-geometry-struct, scroll-events-with-coords
- **Done when:** `App` has `scroll_offsets: HashMap<ScrollablePanel, u16>` and `last_scroll_maxes: RefCell<HashMap<ScrollablePanel, u16>>`; the old `exchange_scroll` and `last_scroll_max` fields are gone and every reference to them across `app.rs`/`ui.rs` is migrated (grep `exchange_scroll`, `last_scroll_max` afterward must show no `\.set(`/`\.get()` against the deleted `Cell`). `App::new()` initializes both empty. A unit test asserts that reading a missing key — `app.scroll_offsets.get(&ScrollablePanel::Sidebar).copied().unwrap_or(0)` — returns `0` without panicking, and that `app.last_scroll_maxes.borrow_mut().insert(ScrollablePanel::Exchange, 7)` then `.borrow().get(..)` round-trips `7`. cargo test/clippy/fmt green.

---

### per-panel-scroll-methods — Update scroll_up, scroll_down, and effective_offset to use per-panel state

The methods `scroll_up(&mut self)` (`app.rs:1608`), `scroll_down(&mut self, scroll_max: u16)` (`app.rs:1623`), and `effective_offset(&self, scroll_max: u16) -> u16` (`app.rs:1635`) operate on `exchange_scroll` and `last_scroll_max`. With the new per-panel maps, `scroll_up`/`scroll_down` take a `ScrollablePanel` parameter, and a new `panel_offset` accessor centralizes the "read + clamp" logic so no render task re-implements it inline. `effective_offset` stays exchange-only.

Note `last_scroll_maxes` is now a `RefCell` (see `scrollable-panel-enum`), so reads from it use `.borrow().get(..)`.

**Steps:**

1. Update `pub fn scroll_up(&mut self)` (`app.rs:1608`) to take a panel:
   ```rust
   /// Scroll the given panel up by one line, disengaging auto-follow if the panel is the exchange pane.
   pub fn scroll_up(&mut self, panel: ScrollablePanel) {
       // Only the exchange pane has auto-follow logic.
       if panel == ScrollablePanel::Exchange && self.exchange_auto_follow {
           self.exchange_auto_follow = false;
           // Anchor the manual offset to the last rendered bottom.
           let bottom = self
               .last_scroll_maxes
               .borrow()
               .get(&ScrollablePanel::Exchange)
               .copied()
               .unwrap_or(0);
           self.scroll_offsets.insert(ScrollablePanel::Exchange, bottom);
       }
       let current = self.scroll_offsets.entry(panel).or_insert(0);
       *current = current.saturating_sub(1);
   }
   ```
2. Update `pub fn scroll_down(&mut self, scroll_max: u16)` (`app.rs:1623`):
   ```rust
   /// Scroll the given panel down by one line, clamped at scroll_max.
   pub fn scroll_down(&mut self, panel: ScrollablePanel, scroll_max: u16) {
       let current = self.scroll_offsets.entry(panel).or_insert(0);
       *current = current.saturating_add(1).min(scroll_max);
       if panel == ScrollablePanel::Exchange && *current == scroll_max {
           self.exchange_auto_follow = true;
       }
   }
   ```
3. Update `pub fn effective_offset(&self, scroll_max: u16) -> u16` (`app.rs:1635`) — still exchange-only:
   ```rust
   /// The effective scroll offset to render the exchange pane with, given the
   /// caller-computed scroll_max. When auto-following, returns scroll_max
   /// (pinned to the bottom); otherwise the manual offset clamped to scroll_max.
   pub fn effective_offset(&self, scroll_max: u16) -> u16 {
       if self.exchange_auto_follow {
           scroll_max
       } else {
           self.scroll_offsets
               .get(&ScrollablePanel::Exchange)
               .copied()
               .unwrap_or(0)
               .min(scroll_max)
       }
   }
   ```
4. Add a single generic accessor below `effective_offset` so all render tasks share one read+clamp path (the exchange pane delegates to `effective_offset` to keep auto-follow):
   ```rust
   /// The render-time scroll offset for any panel, clamped to scroll_max.
   /// The exchange pane delegates to `effective_offset` so auto-follow is honored;
   /// every other panel uses its stored offset clamped to scroll_max.
   pub fn panel_offset(&self, panel: ScrollablePanel, scroll_max: u16) -> u16 {
       if panel == ScrollablePanel::Exchange {
           self.effective_offset(scroll_max)
       } else {
           self.scroll_offsets
               .get(&panel)
               .copied()
               .unwrap_or(0)
               .min(scroll_max)
       }
   }
   ```
5. **Update the four existing non-test callers** whose signatures changed (the legacy `ScrollUp`/`ScrollDown` arms and the `SelectUp`/`SelectDown` arms — these all currently call the old no-arg/1-arg forms):
   - `app.rs:1721` `self.scroll_up();` → `self.scroll_up(ScrollablePanel::Exchange);`
   - `app.rs:1734` `self.scroll_down(self.last_scroll_max.get());` → read the exchange max from the RefCell map first, then `self.scroll_down(ScrollablePanel::Exchange, max);`
   - `app.rs:1808` `self.scroll_up();` (legacy `AppEvent::ScrollUp` arm) → `self.scroll_up(ScrollablePanel::Exchange);`
   - `app.rs:1816` `self.scroll_down(self.last_scroll_max.get());` (legacy `AppEvent::ScrollDown` arm) → read the exchange max from the RefCell map, then `self.scroll_down(ScrollablePanel::Exchange, max);`
   For the two `scroll_down` sites compute the ceiling as
   `let max = self.last_scroll_maxes.borrow().get(&ScrollablePanel::Exchange).copied().unwrap_or(0);`
   (replacing the deleted `self.last_scroll_max.get()`).
   The sole non-test `effective_offset` caller is `ui.rs:1302` (`app.effective_offset(scroll_max)`) and is handled in `apply-scroll-offsets-exchange`.
6. Update the existing scroll-state unit tests that set `app.last_scroll_max.set(..)` (grep — they are at `app.rs:4625,4697,4745,4871,4909,5740`) to seed the new map instead: `app.last_scroll_maxes.borrow_mut().insert(ScrollablePanel::Exchange, N);`, and update `app.update(AppEvent::ScrollUp/ScrollDown)` assertions to read `app.scroll_offsets.get(&ScrollablePanel::Exchange)`.

- **Depends on:** scrollable-panel-enum, scroll-events-with-coords
- **Done when:** `scroll_up(panel)`, `scroll_down(panel, scroll_max)`, and `panel_offset(panel, scroll_max)` exist with the signatures above; `effective_offset` is unchanged in behavior; and `app.rs` compiles with all four legacy/select callers (1721, 1734, 1808, 1816) updated to pass `ScrollablePanel::Exchange`. A unit test asserts `panel_offset(ScrollablePanel::Sidebar, 4)` returns the stored sidebar offset clamped to 4, and `panel_offset(ScrollablePanel::Exchange, max)` returns `max` when `exchange_auto_follow` is true. A test asserts that `scroll_down(ScrollablePanel::Sidebar, 10)` then reading `scroll_offsets[Exchange]` still yields `0` (panels are independent). The migrated existing auto-follow test (`exchange_scroll_offset_interaction_with_auto_follow` or equivalent) still passes. cargo test/clippy/fmt green.

---

## 0003 — Mouse-Position-Aware Scroll Event Dispatch

### scroll-event-dispatch — Implement ScrollUpAt/ScrollDownAt event dispatch in App::update

With `ScrollUpAt`/`ScrollDownAt` events, the panel geometry tracking, and per-panel scroll state in place, `App::update(&mut self, event) -> bool` (`app.rs:1673`) must handle the new events. The current `AppEvent::ScrollUp` arm is at `app.rs:1807` and `AppEvent::ScrollDown` at `app.rs:1811`; they dispatch unconditionally to the exchange pane. The new arms hit-test the coordinates and route to the panel under the cursor. Because `panel_at` now returns `Option<ScrollablePanel>` directly (see `panel-geometry-struct`), there is no string round-trip — route the enum straight through.

**Steps:**

1. In `App::update()`, after the existing `AppEvent::ScrollUp` / `AppEvent::ScrollDown` arms (`app.rs:1807`/`1811`), add:
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
2. Keep the existing bare `ScrollUp`/`ScrollDown` arms for backward compatibility; they already dispatch to the exchange pane (updated to pass `ScrollablePanel::Exchange` in `per-panel-scroll-methods`).

- **Depends on:** scroll-events-with-coords, per-panel-scroll-methods, panel-geometry-struct, scrollable-panel-enum
- **Done when:** The `ScrollUpAt`/`ScrollDownAt` arms compile and route via `panel_at`. Build the app fixture per the header note, seed geometries with `set_panel_geometries`, and seed `last_scroll_maxes` via `borrow_mut().insert(..)`. Tests assert: (`scroll_up_at_routes_to_sidebar`) `update(ScrollUpAt(col, row))` over the sidebar rect decrements `scroll_offsets[Sidebar]` and leaves `scroll_offsets[Exchange]` at 0; (`scroll_down_at_routes_to_exchange`) over the exchange rect increments `scroll_offsets[Exchange]`; a wheel-down over the accordion rect (with `last_scroll_maxes[PlanAccordion]` seeded) increments `scroll_offsets[PlanAccordion]`, and over the dependency rect increments `scroll_offsets[DependencyView]`; and `ScrollUpAt(col, row)` with coordinates outside every rect is a no-op (no panic, no state change). Existing `ScrollUp`/`ScrollDown` tests still pass. cargo test/clippy/fmt green.

---

## 0004 — Ratatui Scrollbar Widget Integration

### scrollbar-widget-exchange — Render vertical scrollbar on exchange pane

`render_exchange_pane(app: &App, frame: &mut Frame, area: Rect, focused: bool)` is at `ui.rs:1087`. It builds a `Block` with `Borders::TOP`, computes `let inner = block.inner(area);` (`ui.rs:1130`), renders the block to `area`, and applies `.scroll((scroll_offset, 0))` to a `Paragraph` rendered into **`inner`** (`ui.rs:1304–1307`). The locals `total_lines` and `scroll_max` are computed at `ui.rs:1296–1297`, `scroll_offset` at `ui.rs:1302`, and the rendered bottom is recorded at `ui.rs:1301` (currently `app.last_scroll_max.set(scroll_max)`, replaced in `scrollable-panel-enum` with the RefCell map write). The scrollbar must render into **`inner`** (the rows the paragraph scrolls), not `area` — rendering over `area` would put the begin-arrow on the TOP border/title row and misalign the thumb by one row.

**Steps:**

1. Add the `Scrollbar` widgets to the import. Grep `use ratatui::` and find the `widgets::{ ... }` group (it spans `ui.rs:43–45`, currently ending `... Paragraph, Wrap,`). Add `Scrollbar`, `ScrollbarOrientation`, and `ScrollbarState` to that exact group rather than assuming the line contents.
2. In `render_exchange_pane()`, immediately after `app.last_scroll_max.set(scroll_max)` is migrated, record this panel's scroll-max into the map this frame so the dispatch clamp and scrollbar have a real bound:
   ```rust
   app.last_scroll_maxes
       .borrow_mut()
       .insert(ScrollablePanel::Exchange, scroll_max);
   ```
   (This replaces the deleted `app.last_scroll_max.set(scroll_max)` at `ui.rs:1301`.)
3. After the paragraph is rendered into `inner`, render the scrollbar into the **same** `inner` rect:
   ```rust
   // Render scrollbar only when content exceeds the viewport.
   if scroll_max > 0 {
       let mut scrollbar_state =
           ScrollbarState::new(total_lines).position(scroll_offset as usize);
       let scrollbar = Scrollbar::default()
           .orientation(ScrollbarOrientation::VerticalRight)
           .begin_symbol(None)
           .end_symbol(None);
       frame.render_stateful_widget(scrollbar, inner, &mut scrollbar_state);
   }
   ```
   `total_lines` is already a `usize` (`ui.rs:1296`), so `ScrollbarState::new(total_lines)` is the real item count (no `+ height`, no `.min(1000)`). `.begin_symbol(None).end_symbol(None)` drops the default `▲`/`▼` arrows so only the track+thumb draw within `inner` (see ARCHITECTURE 0004 decision).

- **Depends on:** scrollable-panel-enum
- **Done when:** The exchange pane renders a vertical scrollbar in its `inner` right column when `scroll_max > 0`, and none when content fits. Using `make_terminal` + `terminal.draw(|f| render(&app, f))` (header note), a test renders a run whose exchange log far exceeds the pane height and asserts at least one of `█` (thumb) or `║` (track) appears in the exchange pane's rightmost inner column of `terminal.backend().buffer()`; a companion test renders a short log and asserts none of `█`/`║`/`▲`/`▼` appear in that column. A third test seeds `scroll_offsets[Exchange]` to a known mid value (with auto-follow off) and asserts the thumb glyph `█` falls in the expected row band, not at the top. cargo test/clippy/fmt green.

---

### scrollbar-widget-sidebar — Render vertical scrollbar on sidebar

In `render()`, the sidebar is rendered as a `List` from the `items: Vec<ListItem>` built around `ui.rs:161`, into `sidebar_area` (`ui.rs:91`) with the block `sidebar_block` (`Borders::ALL`), via `frame.render_stateful_widget(sidebar_list, sidebar_area, &mut list_state)` at `ui.rs:295`. When the list exceeds the viewport, a scrollbar indicates position. The content length is the **real item count** (`items.len()`), not a heuristic; and the scrollbar must render into the block's **inner** rect so it does not overwrite the box's right border and corners.

**Steps:**

1. In `render()`, after the sidebar `items` Vec is built (grep the `let items: Vec<ListItem>` near `ui.rs:161`) and the list is rendered at `ui.rs:295`, compute the item count and inner rect, and record the sidebar scroll-max into the map this frame:
   ```rust
   let sidebar_inner = sidebar_block.inner(sidebar_area);
   let total_items = items.len();
   let sidebar_visible = sidebar_inner.height as usize;
   let sidebar_scroll_max = total_items.saturating_sub(sidebar_visible) as u16;
   app.last_scroll_maxes
       .borrow_mut()
       .insert(ScrollablePanel::Sidebar, sidebar_scroll_max);
   ```
   (Compute `sidebar_inner`/`total_items` before `items` is moved into `List::new(items)`; if `items` is already moved, capture `total_items = items.len()` just before the `List::new` call.)
2. Render the scrollbar into `sidebar_inner` (preserving the border), guarded on overflow, with `content_length == total_items` (no `+ height`, no `.min(1000)`):
   ```rust
   if total_items > sidebar_visible {
       let sidebar_scroll_offset = app
           .scroll_offsets
           .get(&ScrollablePanel::Sidebar)
           .copied()
           .unwrap_or(0);
       let mut scrollbar_state =
           ScrollbarState::new(total_items).position(sidebar_scroll_offset as usize);
       let scrollbar = Scrollbar::default()
           .orientation(ScrollbarOrientation::VerticalRight)
           .begin_symbol(None)
           .end_symbol(None);
       frame.render_stateful_widget(scrollbar, sidebar_inner, &mut scrollbar_state);
   }
   ```

- **Depends on:** scrollable-panel-enum, scrollbar-widget-exchange
- **Done when:** Render 50 sidebar items into a `make_terminal(80, 10)` `TestBackend` (build the app via `App::new(Arc::new(PlaceholderApi::empty()), runs, PathBuf::from("."))` with enough expanded runs to exceed the height) and assert a thumb/track glyph (one of `█`, `║`, `▲`, `▼`) is present in the sidebar's inner rightmost column. Render 5 items into the same height-10 pane and assert none of those glyphs appear in that column. cargo test/clippy/fmt green.

---

### scrollbar-widget-accordion — Render vertical scrollbar on plan accordion pane

`render_plan_accordion_pane(..)` is at `ui.rs:1327`. It renders the plan sections (SCOPE, ARCHITECTURE, TASKS, STATUS) as one `Paragraph` built at `ui.rs:1411–1415` (`let total = lines.len() as u16; let scroll = total.saturating_sub(area.height); let para = Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((scroll, 0));`) and rendered directly into **`area`** (no block) at `frame.render_widget(para, area)` (`ui.rs:1416`). Because there is no block, a scrollbar drawn over the full `area` would overpaint the last column of wrapped text; reserve the rightmost column instead. The content length is the line count `lines.len()` directly (no `+ height`, no `.min(1000)`).

This task coordinates with `apply-scroll-offsets-accordion`, which rewrites the same `ui.rs:1411–1416` block to read the per-panel offset. To avoid both tasks editing the same lines independently, this scrollbar task owns the column split and the `last_scroll_maxes[PlanAccordion]` write; `apply-scroll-offsets-accordion` owns folding the offset into `.scroll(..)`. Implement them as one coherent block (this task runs first per the dependency order):

**Steps:**

1. In `render_plan_accordion_pane()`, locate the block at `ui.rs:1411–1416` by the `let total = lines.len()` / `frame.render_widget(para, area)` symbols.
2. Split `area` into a content column and a 1-cell scrollbar column, and record the scroll-max into the map this frame:
   ```rust
   let total_lines = lines.len() as u16;
   let accordion_scroll_max = total_lines.saturating_sub(area.height);
   app.last_scroll_maxes
       .borrow_mut()
       .insert(ScrollablePanel::PlanAccordion, accordion_scroll_max);

   // Reserve the rightmost column for the scrollbar so text is not overpainted.
   let cols = Layout::default()
       .direction(Direction::Horizontal)
       .constraints([Constraint::Min(0), Constraint::Length(1)])
       .split(area);
   let content_area = cols[0];
   let scrollbar_area = cols[1];
   ```
   Render the paragraph into `content_area` (the offset itself is applied in `apply-scroll-offsets-accordion`; until then keep the existing `.scroll((scroll, 0))` but render into `content_area`, not `area`).
3. After rendering the paragraph, draw the scrollbar into `scrollbar_area` when the content overflows, using the real line count as content_length:
   ```rust
   if accordion_scroll_max > 0 {
       let accordion_scroll_offset = app
           .scroll_offsets
           .get(&ScrollablePanel::PlanAccordion)
           .copied()
           .unwrap_or(0)
           .min(accordion_scroll_max);
       let mut scrollbar_state =
           ScrollbarState::new(total_lines as usize).position(accordion_scroll_offset as usize);
       let scrollbar = Scrollbar::default()
           .orientation(ScrollbarOrientation::VerticalRight)
           .begin_symbol(None)
           .end_symbol(None);
       frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
   }
   ```

- **Depends on:** scrollable-panel-enum, scrollbar-widget-exchange, scrollbar-widget-sidebar
- **Done when:** The accordion records `last_scroll_maxes[PlanAccordion] = lines.len() - area.height` each frame and renders a scrollbar into the reserved 1-cell right column only when `accordion_scroll_max > 0`. Using `make_terminal` + `terminal.draw(|f| render(&app, f))`, a test renders a plan with all four sections expanded into a short pane and asserts a thumb/track glyph (`█` or `║`) appears in the accordion pane's rightmost column of the buffer; a companion test renders a plan whose content fits and asserts none of `█`/`║`/`▲`/`▼` appear there. cargo test/clippy/fmt green.

---

## 0005 — Scroll Offset Clamping and Rendering

### apply-scroll-offsets-exchange — Apply per-panel scroll offset to exchange pane rendering

In `render_exchange_pane()`, the scroll offset is computed via `let scroll_offset = app.effective_offset(scroll_max);` at `ui.rs:1302` (the sole non-test `effective_offset` caller) and passed to the paragraph's `.scroll((scroll_offset, 0))` at `ui.rs:1306`. `effective_offset` already reads `scroll_offsets[Exchange]` and applies auto-follow (`per-panel-scroll-methods`), so the exchange render path needs no structural change — this task only verifies and tests it.

**Steps:**

1. Confirm `ui.rs:1302` still reads `let scroll_offset = app.effective_offset(scroll_max);` — no edit needed; it already routes through the new `scroll_offsets[Exchange]` + auto-follow logic.
2. Confirm the exchange offset is updated when `scroll_up`/`scroll_down` is called with `ScrollablePanel::Exchange` (implemented in `per-panel-scroll-methods`).
3. Add a TestBackend test (`exchange_scroll_offset_applied_to_rendering`): build the app fixture (header note), disable auto-follow (`app.exchange_auto_follow = false`), set `app.scroll_offsets.insert(ScrollablePanel::Exchange, N)` with a content log long enough that offset `N` skips known leading lines, render via `terminal.draw(|f| render(&app, f))`, and assert the exchange pane's first visible content row in `terminal.backend().buffer()` equals the log line originally at index `N` (not index 0).

- **Depends on:** per-panel-scroll-methods, scrollbar-widget-exchange
- **Done when:** With auto-follow off and `scroll_offsets[Exchange] = N`, the rendered exchange pane's first visible content row equals the line at index `N` (asserted against the TestBackend buffer); with auto-follow on, the bottom of the log is shown. Existing auto-follow tests still pass. cargo test/clippy/fmt green.

---

### apply-scroll-offsets-accordion — Apply per-panel scroll offset to plan accordion rendering

In `render_plan_accordion_pane()`, the accordion paragraph at `ui.rs:1411–1416` currently pins to the bottom: `let total = lines.len() as u16; let scroll = total.saturating_sub(area.height); let para = Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((scroll, 0));`. `Paragraph::scroll(self, ..) -> Self` is a **consuming builder**, so `para.scroll((..))` as a separate statement is a no-op that discards the result — the offset must be folded into the single chained `.scroll(..)`. This task replaces the footer-pinning default with a user-controlled, clamped offset (an intended behavior change: the accordion no longer auto-pins to the bottom).

`scrollbar-widget-accordion` (a prerequisite) already split `area` into `content_area` (left) + `scrollbar_area` (right, 1 col) and wrote `last_scroll_maxes[PlanAccordion] = total_lines.saturating_sub(area.height)`. This task edits the same block to render the paragraph into `content_area` with the per-panel offset.

**Steps:**

1. In `render_plan_accordion_pane()`, locate the block at `ui.rs:1411–1416` (and the `content_area`/`accordion_scroll_max` introduced by `scrollbar-widget-accordion`).
2. Delete the old `let scroll = total.saturating_sub(area.height);` footer-pin and build the paragraph with the clamped per-panel offset folded into the chained `.scroll(..)`:
   ```rust
   let accordion_scroll_offset = app.panel_offset(ScrollablePanel::PlanAccordion, accordion_scroll_max);
   let para = Paragraph::new(lines)
       .wrap(Wrap { trim: false })
       .scroll((accordion_scroll_offset, 0));
   frame.render_widget(para, content_area);
   ```
   (`accordion_scroll_max` is `total_lines.saturating_sub(area.height)`, computed once in `scrollbar-widget-accordion`; `panel_offset` reads `scroll_offsets[PlanAccordion]` clamped to it — see `per-panel-scroll-methods`.)
3. Do **not** emit a standalone `para.scroll(..)` statement.

- **Depends on:** scrollable-panel-enum, scrollbar-widget-accordion, apply-scroll-offsets-exchange
- **Done when:** With `scroll_offsets[PlanAccordion] = 3` and a plan whose expanded content exceeds the pane height, the first rendered accordion content row (TestBackend buffer, header-note fixture) equals the wrapped line originally at index 3 (lines 0–2 skipped); with an offset greater than `accordion_scroll_max` it clamps so the first row equals the line at index `accordion_scroll_max`. cargo test/clippy/fmt green.

---

### apply-scroll-offsets-sidebar — Apply per-panel scroll offset to sidebar rendering

The sidebar is rendered as a `List` via `render_stateful_widget(sidebar_list, sidebar_area, &mut list_state)` at `ui.rs:295`, where `list_state` is built at `ui.rs:292–293` (`let mut list_state = ListState::default(); list_state.select(app.tree_cursor);`). Ratatui 0.30's `ListState` **does** expose a direct scroll-offset setter — `offset_mut(&mut self) -> &mut usize` and `with_offset(usize) -> Self` (verified in `ratatui-widgets` `src/list/state.rs`; `offset` is "Index of the first item to be displayed"). So the sidebar list content can and must be scrolled directly from `scroll_offsets[Sidebar]`. There is no "future work" fallback.

Note ratatui will still nudge the offset to keep the selected item visible, which is acceptable; the offset we set chooses the first visible row when the selection allows it.

**Steps:**

1. In `render()`, at `ui.rs:292–293`, replace the two-line `ListState` construction with one that also applies the offset:
   ```rust
   let sidebar_scroll_offset = app
       .scroll_offsets
       .get(&ScrollablePanel::Sidebar)
       .copied()
       .unwrap_or(0) as usize;
   let mut list_state = ListState::default()
       .with_selected(app.tree_cursor)
       .with_offset(sidebar_scroll_offset);
   ```
   (Equivalently, keep the existing `ListState::default()` + `select(..)` and add `*list_state.offset_mut() = sidebar_scroll_offset;` before the `render_stateful_widget` call at `ui.rs:295`.)
2. Remove any placeholder/`future work` comments — the offset is applied directly.

- **Depends on:** scrollable-panel-enum, scrollbar-widget-accordion, apply-scroll-offsets-accordion
- **Done when:** The sidebar `ListState` is constructed with `with_offset(scroll_offsets[Sidebar])` (or `offset_mut` set before render). A TestBackend test (`make_terminal(80, 10)`, header-note fixture) builds a sidebar with `N > visible_rows` items, sets `scroll_offsets[Sidebar] = 3`, renders via `terminal.draw(|f| render(&app, f))`, and asserts the first visible sidebar row in the buffer equals the label of the item originally at index 3 (not index 0); with offset 0 it equals the index-0 item's label. No "future work" language remains. cargo test/clippy/fmt green.

---

### apply-scroll-offsets-dependency — Apply per-panel scroll offset to dependency view rendering

`render_dependency_view(app: &App, frame: &mut Frame, area: Rect)` is at `ui.rs:679`. It computes `let inner = block.inner(area);` (`ui.rs:690`) and then, in **three** independent `match app.dependency_view` arms (List, Tree, Timeline), builds a fresh `let para = Paragraph::new(lines);` rendered into `inner`: at `ui.rs:735–736`, `ui.rs:764–765`, and `ui.rs:850–851`. There is no single shared `para`, none are `mut`, and none currently call `.scroll(`. Each arm must independently compute its own scroll-max from its `lines.len()`, record it into `last_scroll_maxes[DependencyView]`, and fold the clamped offset into a chained `.scroll(..)`.

**Steps:**

1. In each of the three arms (`ui.rs:735`, `764`, `850`), after `lines` is built and before constructing the paragraph, compute the scroll bound from that arm's content and record it this frame:
   ```rust
   let dep_total = lines.len() as u16;
   let dep_scroll_max = dep_total.saturating_sub(inner.height);
   app.last_scroll_maxes
       .borrow_mut()
       .insert(ScrollablePanel::DependencyView, dep_scroll_max);
   let dep_scroll_offset = app.panel_offset(ScrollablePanel::DependencyView, dep_scroll_max);
   ```
2. Build each arm's paragraph with the offset folded into the chained `.scroll(..)` (replacing the bare `let para = Paragraph::new(lines);`):
   ```rust
   let para = Paragraph::new(lines).scroll((dep_scroll_offset, 0));
   frame.render_widget(para, inner);
   ```
   Apply this in all three arms (List `ui.rs:735–736`, Tree `ui.rs:764–765`, Timeline `ui.rs:850–851`).

- **Depends on:** scrollable-panel-enum, apply-scroll-offsets-sidebar
- **Done when:** Each dependency-view arm records `last_scroll_maxes[DependencyView]` from its own `lines.len()` and renders its paragraph with `panel_offset(DependencyView, dep_scroll_max)` folded into `.scroll(..)`. A TestBackend test renders the List arm with more than `inner.height` synthetic dependency lines and `scroll_offsets[DependencyView] = 2`, then asserts the first visible dependency row in the buffer equals the line originally at index 2; with offset 0 it equals index 0. cargo test/clippy/fmt green.

---

## 0006 — Integration and Testing

### record-panel-geometries — Record panel geometries in the render pass

`ui.rs::render()` computes each panel's `Rect` inside **mutually-exclusive arms** of `match (active_plan_tab, app.selected_run())` (`ui.rs:362`): `plan_area` exists only in the `(Some(plan), _)` arm (`ui.rs:371`), while `exchange_pane_area` (`ui.rs:478–489`) and `dep_split[0]` (`ui.rs:486`, never bound to a named variable) exist only in the `(None, Some(run))` arm. `sidebar_area` (`ui.rs:91`) is in scope for the whole function. There is **no** `dep_area` variable and **no** single point at the end of render where `plan_area`/`exchange_pane_area`/`dep_split[0]` are all live — so a geometry block written "at the very end" cannot compile. The fix is to accumulate geometries into a `Vec` declared **before** the match and push each rect from inside the arm where it is in scope, then call the `&self` setter after the match. `PanelGeometry` now holds `panel: ScrollablePanel` (not a name string — see `panel-geometry-struct`), and `set_panel_geometries(&self, ..)` takes `&self` via `RefCell`.

**Steps:**

1. In `render()`, declare the accumulator and record the always-visible sidebar before the `match` at `ui.rs:362`:
   ```rust
   let mut panel_geoms: Vec<PanelGeometry> = vec![PanelGeometry {
       panel: ScrollablePanel::Sidebar,
       rect: sidebar_area,
   }];
   ```
2. In the `(Some(plan), _)` arm, right after `let plan_area = plan_split[1];` (`ui.rs:371`), push the accordion geometry:
   ```rust
   panel_geoms.push(PanelGeometry { panel: ScrollablePanel::PlanAccordion, rect: plan_area });
   ```
3. In the `(None, Some(run))` arm, bind a name for the dependency rect and record both the dependency and exchange geometries where they are computed (`ui.rs:478–489`):
   ```rust
   let exchange_pane_area = if app.dependency_view == DependencyViewMode::Off {
       exchange_area
   } else {
       let dep_height = (exchange_area.height / 2).max(3);
       let dep_split = Layout::default()
           .direction(Direction::Vertical)
           .constraints([Constraint::Length(dep_height), Constraint::Min(3)])
           .split(exchange_area);
       let dep_area = dep_split[0];
       render_dependency_view(app, frame, dep_area);
       panel_geoms.push(PanelGeometry { panel: ScrollablePanel::DependencyView, rect: dep_area });
       dep_split[1]
   };
   render_exchange_pane(app, frame, exchange_pane_area, main_focused);
   panel_geoms.push(PanelGeometry { panel: ScrollablePanel::Exchange, rect: exchange_pane_area });
   ```
   (This is the same code already at `ui.rs:478–489`, with `let dep_area = dep_split[0];` introduced and the two `panel_geoms.push(..)` lines added.)
4. After the `match` block (and before the error/status-bar rendering, or anywhere the borrow allows), record the geometries via the `&self` setter:
   ```rust
   app.set_panel_geometries(panel_geoms);
   ```
5. Ensure `PanelGeometry` and `ScrollablePanel` are imported (extend the existing `use crate::app::{ ... }` group near `ui.rs:49`).

- **Depends on:** panel-geometry-struct, scroll-events-with-coords, scrollable-panel-enum, per-panel-scroll-methods, scroll-event-dispatch, scrollbar-widget-exchange, scrollbar-widget-sidebar, scrollbar-widget-accordion
- **Done when:** `render()` records the visible panels' geometries into `panel_geometries` via `set_panel_geometries(&self, ..)`. Using the header-note fixture and `terminal.draw(|f| render(&app, f))`: a test with a selected run asserts `app.panel_geometries.borrow()` contains an entry whose `panel == ScrollablePanel::Sidebar` with `rect == sidebar_area` and one whose `panel == ScrollablePanel::Exchange` whose `rect` equals the computed `exchange_pane_area`; a test with no run selected asserts no `Exchange` entry is present; a test renders at one terminal size then a larger one and asserts the recorded sidebar `rect.height` changed accordingly. cargo test/clippy/fmt green.

---

### integration-test-scroll-routing — Comprehensive integration test for scroll routing and scrollbars

With all workstreams complete, the system routes wheel scroll events to the correct panel by mouse coordinate and renders scrollbars on scrollable panes. This task verifies the integrated behavior through tests that drive `App::update` directly with seeded geometries.

These tests must use the **real** constructor `App::new(api: Arc<dyn Api>, initial_runs: Vec<RunView>, repo_root: PathBuf)` (`app.rs:1421`) — never `App::new(/* ... */)`. Seed `last_scroll_maxes` through its `RefCell` (`borrow_mut().insert(..)`) and construct `PanelGeometry` with `panel: ScrollablePanel::..` (the struct holds the enum, not a name string).

**Steps:**

1. In the test module, add a fixture and imports mirroring the existing `ui.rs` tests (which build `App::new(Arc::new(PlaceholderApi::empty()), vec![], std::path::PathBuf::from("."))` at `ui.rs:2676`):
   ```rust
   use ratatui::layout::Rect;
   use std::sync::Arc;
   use crate::app::{App, AppEvent, PanelGeometry, ScrollablePanel};

   fn mk_app() -> App {
       // Reuse the same mock the ui.rs tests use; grep `PlaceholderApi` to confirm
       // its constructor (PlaceholderApi::empty()).
       let api = Arc::new(PlaceholderApi::empty());
       App::new(api, vec![], std::path::PathBuf::from("."))
   }
   ```

2. Test 1 — `scroll_up_at_coordinates_targets_sidebar`:
   ```rust
   #[test]
   fn scroll_up_at_coordinates_targets_sidebar() {
       let mut app = mk_app();
       app.set_panel_geometries(vec![
           PanelGeometry { panel: ScrollablePanel::Sidebar, rect: Rect::new(0, 1, 30, 59) },
           PanelGeometry { panel: ScrollablePanel::Exchange, rect: Rect::new(30, 1, 70, 59) },
       ]);
       app.last_scroll_maxes.borrow_mut().insert(ScrollablePanel::Sidebar, 10);
       app.scroll_offsets.insert(ScrollablePanel::Sidebar, 5);

       app.update(AppEvent::ScrollUpAt(10, 20)); // inside sidebar

       assert_eq!(app.scroll_offsets.get(&ScrollablePanel::Sidebar).copied().unwrap_or(0), 4);
       assert_eq!(app.scroll_offsets.get(&ScrollablePanel::Exchange).copied().unwrap_or(0), 0);
   }
   ```

3. Test 2 — `scroll_down_at_coordinates_targets_exchange`:
   ```rust
   #[test]
   fn scroll_down_at_coordinates_targets_exchange() {
       let mut app = mk_app();
       app.exchange_auto_follow = false; // exercise the manual-offset path
       app.set_panel_geometries(vec![
           PanelGeometry { panel: ScrollablePanel::Sidebar, rect: Rect::new(0, 1, 30, 59) },
           PanelGeometry { panel: ScrollablePanel::Exchange, rect: Rect::new(30, 1, 70, 59) },
       ]);
       app.last_scroll_maxes.borrow_mut().insert(ScrollablePanel::Exchange, 50);
       app.scroll_offsets.insert(ScrollablePanel::Exchange, 10);

       app.update(AppEvent::ScrollDownAt(60, 30)); // inside exchange

       assert_eq!(app.scroll_offsets.get(&ScrollablePanel::Exchange).copied().unwrap_or(0), 11);
   }
   ```

4. Test 3 — `scroll_at_coordinates_outside_panels_is_noop`:
   ```rust
   #[test]
   fn scroll_at_coordinates_outside_panels_is_noop() {
       let mut app = mk_app();
       app.set_panel_geometries(vec![
           PanelGeometry { panel: ScrollablePanel::Sidebar, rect: Rect::new(0, 1, 30, 59) },
       ]);
       app.scroll_offsets.insert(ScrollablePanel::Sidebar, 5);

       app.update(AppEvent::ScrollUpAt(100, 100)); // outside every rect

       assert_eq!(app.scroll_offsets.get(&ScrollablePanel::Sidebar).copied().unwrap_or(0), 5);
   }
   ```

5. Test 4 — `exchange_auto_follow_preserved_with_routing`:
   ```rust
   #[test]
   fn exchange_auto_follow_preserved_with_routing() {
       let mut app = mk_app();
       app.set_panel_geometries(vec![
           PanelGeometry { panel: ScrollablePanel::Exchange, rect: Rect::new(30, 1, 70, 59) },
       ]);
       app.last_scroll_maxes.borrow_mut().insert(ScrollablePanel::Exchange, 100);
       app.exchange_auto_follow = true;

       app.update(AppEvent::ScrollDownAt(60, 30)); // stays pinned
       assert_eq!(app.effective_offset(100), 100, "auto-follow should pin to scroll_max");

       app.update(AppEvent::ScrollUpAt(60, 30)); // disengages
       assert!(!app.exchange_auto_follow, "scroll up should disengage auto-follow");
   }
   ```

6. Test 5 — `multiple_panels_maintain_independent_scroll`:
   ```rust
   #[test]
   fn multiple_panels_maintain_independent_scroll() {
       let mut app = mk_app();
       app.exchange_auto_follow = false;
       app.set_panel_geometries(vec![
           PanelGeometry { panel: ScrollablePanel::Sidebar, rect: Rect::new(0, 1, 30, 59) },
           PanelGeometry { panel: ScrollablePanel::Exchange, rect: Rect::new(30, 1, 70, 59) },
       ]);
       app.last_scroll_maxes.borrow_mut().insert(ScrollablePanel::Sidebar, 20);
       app.last_scroll_maxes.borrow_mut().insert(ScrollablePanel::Exchange, 100);

       app.update(AppEvent::ScrollDownAt(10, 20));
       app.update(AppEvent::ScrollDownAt(10, 20));
       app.update(AppEvent::ScrollDownAt(60, 30));
       app.update(AppEvent::ScrollDownAt(60, 30));
       app.update(AppEvent::ScrollDownAt(60, 30));

       assert_eq!(app.scroll_offsets.get(&ScrollablePanel::Sidebar).copied().unwrap_or(0), 2);
       assert_eq!(app.scroll_offsets.get(&ScrollablePanel::Exchange).copied().unwrap_or(0), 3);
   }
   ```

- **Depends on:** scroll-event-dispatch, record-panel-geometries, apply-scroll-offsets-exchange, apply-scroll-offsets-accordion, apply-scroll-offsets-sidebar, apply-scroll-offsets-dependency
- **Done when:** All five tests compile (real `App::new`, `PanelGeometry { panel: .. }`, `RefCell` seeding) and pass, verifying: (1) scrolling over sidebar moves only the sidebar offset, (2) scrolling over exchange moves only the exchange offset, (3) scrolling outside every rect is a no-op, (4) exchange auto-follow is preserved through routing, (5) two panels keep independent offsets. cargo test/clippy/fmt green.

---

### scrollbar-rendering-test — Test scrollbar rendering on all scrollable panels

Scrollbars are added to exchange pane, sidebar, and accordion; their rendering must be verified against the **TestBackend buffer**, not prose. There is no usable `Frame::new` public constructor and no `render_stateful_widget` hook — drive everything through the public `render(&app, f)` inside `terminal.draw`, exactly as the existing tests do (`make_terminal` at `ui.rs:2665`, `use ratatui::backend::TestBackend` at `ui.rs:2656`). The render targets (`exchange_area`, `plan_area`, `sidebar_area`) are private locals of `render()`, so do **not** call the `render_*` helpers directly; instead assert on the known pane column ranges of the full-frame buffer.

ratatui 0.30's default vertical `Scrollbar` uses track `║` (U+2551) and thumb `█` (U+2588) (and arrows `▲`/`▼`, which these tasks disable via `.begin_symbol(None).end_symbol(None)`). Assert on `█`/`║` — never `│` or `▐`.

**Steps:**

1. In the `ui.rs` test module, add a small helper that scans a column range for any scrollbar glyph:
   ```rust
   fn col_has_scrollbar(buf: &ratatui::buffer::Buffer, x: u16, y0: u16, y1: u16) -> bool {
       (y0..y1).any(|y| {
           let s = buf[(x, y)].symbol();
           s == "█" || s == "║" || s == "▲" || s == "▼"
       })
   }
   ```

2. Test 1 — `exchange_pane_scrollbar_renders_when_tall`: build the header-note fixture with a run whose exchange log far exceeds the pane height, `terminal.draw(|f| render(&app, f))`, then assert `col_has_scrollbar` is `true` on the exchange pane's rightmost inner column over its row range (the exchange pane occupies the right content column; compute its x from the terminal width minus 1, y-range from below the header to above the status bar). Companion `exchange_pane_no_scrollbar_when_short`: a run with a short log, assert `col_has_scrollbar` is `false` over the same column/rows.

3. Test 2 — `sidebar_scrollbar_renders_when_tall`: build a fixture with enough expanded runs that the sidebar item count exceeds the pane height in `make_terminal(80, 10)`, render, and assert `col_has_scrollbar` is `true` on the sidebar inner rightmost column (`sidebar_area.x + sidebar_area.width - 2` for the inner column inside the `Borders::ALL` block) over the sidebar rows. Companion test with few items asserts `false`.

4. Test 3 — `accordion_scrollbar_renders_when_tall`: open a plan tab whose expanded sections exceed the pane height, render, and assert `col_has_scrollbar` is `true` on the accordion's reserved rightmost column over its rows. Companion test with content that fits asserts `false`.

- **Depends on:** scrollbar-widget-exchange, scrollbar-widget-sidebar, scrollbar-widget-accordion, apply-scroll-offsets-exchange, apply-scroll-offsets-accordion, apply-scroll-offsets-dependency
- **Done when:** For each of exchange, sidebar, and accordion, one test asserts a scrollbar glyph (`█` or `║`) is present on the pane's right column when content is tall, and a companion test asserts no `█`/`║`/`▲`/`▼` is present there when content fits — all via `TestBackend` buffer inspection through `render(&app, f)`. No `Frame::new` or `render_stateful_widget hook` language remains. cargo test/clippy/fmt green.

---

### verify-gate-commands — Verify all gate commands pass

This is a final verification task to ensure the entire plan meets the project's quality gates. All changes must pass the project's standard CI checks.

**Steps:**

1. Run `cargo test` and verify all tests pass (both existing and new).
2. Run `cargo clippy --all-targets -- -D warnings` and fix any warnings.
3. Run `cargo fmt --check` and run `cargo fmt` if needed.
4. Verify that the build completes without errors.

- **Depends on:** integration-test-scroll-routing, scrollbar-rendering-test
- **Done when:** `cargo test` passes with all tests (existing and new) green. `cargo clippy --all-targets -- -D warnings` reports no warnings. `cargo fmt --check` reports no formatting issues. The binary builds successfully with `cargo build`.

---

**End of plan 0033 TASKS.** When every "Done when" bullet is green, multiple
independent panels (sidebar, exchange pane, plan accordion, dependency view)
each scroll independently in response to mouse-wheel events positioned over
them; vertical scrollbars render on the right edge of each scrollable panel to
indicate content height and current scroll position; exchange-pane auto-follow
logic is preserved; and all gate commands remain green.
