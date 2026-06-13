# Architecture — Plan 0018

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). This plan touches only the `makina` (TUI) crate
> (`crates/makina/src/event.rs` and `crates/makina/src/app.rs`) and builds on the
> tree introduced by plan 0016.

## Current shape (what exists)

- **Key translation** (`crates/makina/src/event.rs`, `translate_key` reached via
  `translate_terminal_event`): the normal keymap (the `else` arm's
  `match key.code`) binds `KeyCode::Tab => AppEvent::FocusNext`,
  `KeyCode::Up | KeyCode::Char('k') => AppEvent::SelectUp`, and
  `KeyCode::Down | KeyCode::Char('j') => AppEvent::SelectDown`, with a final
  `_ => AppEvent::Tick`. **`KeyCode::Left` and `KeyCode::Right` are not matched**,
  so they fall through to `AppEvent::Tick` today.
- **Panels** (`crates/makina/src/app.rs`): `pub enum Panel { Sidebar, Main }`;
  `pub focused_panel: Panel` on `App` (initialised to `Panel::Sidebar`).
- **Focus toggle** (`app.rs`, `AppEvent::FocusNext` handler): flips
  `self.focused_panel` between `Panel::Sidebar` and `Panel::Main`. Unchanged here.
- **Navigation handlers** (`app.rs`, `AppEvent::SelectUp` / `AppEvent::SelectDown`):
  each is a `match self.focused_panel { Panel::Sidebar => …, Panel::Main => … }`.
  Today the `Sidebar` arm moves `selected_run` (then `clamp_selected_task()` +
  `load_exchanges_for_selected_run()`); the `Main` arm moves `selected_task`.
- **Scroll plumbing** (`app.rs`): `pub fn scroll_up(&mut self)`,
  `pub fn scroll_down(&mut self, scroll_max: u16)`,
  `pub fn effective_offset(&self, scroll_max: u16) -> u16`, and fields
  `pub exchange_scroll: u16`, `pub exchange_auto_follow: bool`,
  `pub last_scroll_max: std::cell::Cell<u16>`. The `AppEvent::ScrollUp`/`ScrollDown`
  handlers (mouse wheel) already call `self.scroll_up()` /
  `self.scroll_down(self.last_scroll_max.get())`.
- **Tree (introduced by plan 0016, merged before this plan runs):** on `App`,
  `pub collapsed_runs: HashSet<makina_core::api::RunId>` (a run *absent* ⇒
  expanded), `pub tree_cursor: Option<usize>`, `pub enum TreeNode { Run { run },
  Task { run, task } }`, `pub fn focused_node(&self) -> Option<TreeNode>`,
  `pub fn tree_move(&mut self, delta: isize) -> bool`, and
  `pub fn tree_toggle_expand(&mut self) -> bool` (which insert/remove the focused
  run's `RunId` in `collapsed_runs` and re-sync selection).

## 0059 — Directional focus traversal

Edits in `crates/makina/src/event.rs` (bindings) and `crates/makina/src/app.rs`
(`AppEvent` variants + handlers). Requires plan 0016's tree API.

- **New `AppEvent` variants.** Add to `enum AppEvent` (`app.rs`), next to
  `FocusNext`/`SelectUp`/`SelectDown`:

  ```rust
  /// `→` — on a collapsed run, expand it; otherwise cross focus into
  /// [`Panel::Main`]. See plan 0018.
  FocusRightOrExpand,
  /// `←` — from [`Panel::Main`], return focus to [`Panel::Sidebar`];
  /// otherwise collapse the focused expanded run. See plan 0018.
  FocusLeftOrCollapse,
  ```

- **Bind the arrows.** In `event.rs` `translate_key`, in the normal keymap
  `match key.code` (the block that already binds `KeyCode::Tab`,
  `KeyCode::Up | KeyCode::Char('k')`, and `KeyCode::Down | KeyCode::Char('j')`),
  add two arms before the `_ => AppEvent::Tick` fallthrough:

  ```rust
  KeyCode::Right => AppEvent::FocusRightOrExpand,
  KeyCode::Left => AppEvent::FocusLeftOrCollapse,
  ```

  Leave `KeyCode::Tab => AppEvent::FocusNext` and the `Up`/`Down` arms exactly as
  they are.

- **`Right` handler** (`app.rs`, in the big `match` inside `App::update`). Use
  0016's `focused_node()` and `collapsed_runs`:

  ```rust
  AppEvent::FocusRightOrExpand => {
      // On a *collapsed* run, the first `Right` expands it; on an already-
      // expanded run, or a task leaf, `Right` crosses into the content pane.
      let collapsed_run = matches!(
          self.focused_node(),
          Some(TreeNode::Run { run })
              if self.runs.get(run).is_some_and(|r| self.collapsed_runs.contains(&r.id)),
      );
      if collapsed_run {
          self.tree_toggle_expand(); // expand it; cursor stays on the run header
      } else {
          self.focused_panel = Panel::Main;
      }
      true
  }
  ```

  > **`RunView` id field.** The disclosure check needs the run's stable id. Plan
  > 0016 keys `collapsed_runs` on `makina_core::api::RunId`; reuse whatever
  > accessor 0016 uses to read it from a `RunView` (grep 0016's
  > `tree_toggle_expand` / `visible_tree_nodes` for the exact field/expression —
  > the snippet above writes `r.id` as a placeholder for that accessor).

- **`Left` handler** (`app.rs`):

  ```rust
  AppEvent::FocusLeftOrCollapse => {
      match self.focused_panel {
          // From the content pane, `Left` steps back to the sidebar (no collapse).
          Panel::Main => self.focused_panel = Panel::Sidebar,
          // In the sidebar, `Left` collapses an *expanded* run; a collapsed run
          // or a task leaf is a no-op.
          Panel::Sidebar => {
              let expanded_run = matches!(
                  self.focused_node(),
                  Some(TreeNode::Run { run })
                      if self.runs.get(run).is_some_and(|r| !self.collapsed_runs.contains(&r.id)),
              );
              if expanded_run {
                  self.tree_toggle_expand(); // collapse it
              }
          }
      }
      true
  }
  ```

- **`Tab` unchanged.** `AppEvent::FocusNext` and its handler stay as-is — these
  arrow variants are additive.

## 0060 — Up/Down: navigate vs scroll

Edits in `crates/makina/src/app.rs`, in the existing `AppEvent::SelectUp` and
`AppEvent::SelectDown` handlers. Requires 0059 (so focus can be in `Panel::Main`)
and plan 0016's `tree_move`.

- **Replace both `match self.focused_panel` bodies.** Sidebar focus delegates to
  0016's `tree_move`; main focus scrolls the content pane via the existing scroll
  helpers (the same ones the mouse-wheel `ScrollUp`/`ScrollDown` handlers call),
  *not* the old `selected_task` mutation:

  ```rust
  AppEvent::SelectUp => {
      match self.focused_panel {
          // Sidebar: walk the visible tree nodes (plan 0016).
          Panel::Sidebar => { self.tree_move(-1); }
          // Main: scroll the focused exchange pane up one line.
          Panel::Main => self.scroll_up(),
      }
      true
  }
  AppEvent::SelectDown => {
      match self.focused_panel {
          Panel::Sidebar => { self.tree_move(1); }
          // Main: scroll down; `last_scroll_max` is the bottom the render pass
          // last recorded (same clamp the mouse-wheel `ScrollDown` arm uses).
          Panel::Main => self.scroll_down(self.last_scroll_max.get()),
      }
      true
  }
  ```

- **`k`/`j` stay aliases.** No `event.rs` change for `Up`/`Down`/`k`/`j` — they
  still emit `SelectUp`/`SelectDown`; only the *handlers* change. So `k`/`j`
  navigate the tree in the sidebar and scroll the content in the main pane, exactly
  like the arrows.

- **Why `tree_move` here.** 0016 already wires `SelectUp`/`SelectDown` to
  `tree_move` for the sidebar arm; if 0016's wiring is already in place this task's
  sidebar arm is a no-op delta and the work is purely the `Panel::Main` →
  `scroll_*` change. Confirm against the merged 0016 code and only edit the
  `Panel::Main` arm if 0016 already routes the sidebar arm through `tree_move`.

## Testing notes

- 0059 logic is pure and synchronous: build a fixture `App` with two runs (the
  first collapsed via `collapsed_runs`, the second expanded), drive
  `FocusRightOrExpand` / `FocusLeftOrCollapse` through `app.update(...)`, and
  assert `focused_panel`, `collapsed_runs` membership, and `focused_node()`.
- 0060: with `Panel::Sidebar`, assert `SelectDown`/`SelectUp` move `focused_node()`
  (delegating to `tree_move`); with `Panel::Main`, assert they mutate
  `exchange_scroll` / `exchange_auto_follow` (the scroll path) and **do not** move
  `selected_task`. Seed `last_scroll_max` (`app.last_scroll_max.set(n)`) so
  `scroll_down` has a non-zero clamp, mirroring the existing
  `exchange_scroll_*` tests in `app.rs`.
- All edits keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
