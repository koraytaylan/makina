# Makina Plan 0018 — Arrow-Key Navigation & Focus Model

Keep `Tab` as the panel-focus toggle and add **arrow-key directional navigation**
on top of plan 0016's sidebar tree: `Right` expands a collapsed run then crosses
into the content pane; `Left` returns focus to the sidebar or collapses an
expanded run; `Up`/`Down` navigate the tree when the sidebar is focused but
**scroll the content** when the main pane is focused.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas. This plan builds on plan 0016's tree
(`App::focused_node()`/`TreeNode`/`tree_move`/`tree_toggle_expand`/`collapsed_runs`),
which must be merged before this plan runs.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0059 — Directional focus traversal

### arrow-focus-traversal — `Left`/`Right` move focus and expand/collapse

Bind `KeyCode::Left`/`KeyCode::Right` and add the handlers that expand/collapse a
run and cross focus between the sidebar and the content pane, reusing plan 0016's
tree API. `Tab` stays the panel-focus toggle.

**Steps:**

1. In `crates/makina/src/app.rs`, add two variants to `enum AppEvent` (next to
   `FocusNext`, `SelectUp`, `SelectDown`): `FocusRightOrExpand` (`→`) and
   `FocusLeftOrCollapse` (`←`), each documented per
   [ARCHITECTURE.md](ARCHITECTURE.md).

2. In `crates/makina/src/event.rs` `translate_key`, in the normal keymap
   `match key.code` (the block that binds `KeyCode::Tab => AppEvent::FocusNext` and
   the `Up`/`Down` arms), add `KeyCode::Right => AppEvent::FocusRightOrExpand` and
   `KeyCode::Left => AppEvent::FocusLeftOrCollapse` *before* the
   `_ => AppEvent::Tick` fallthrough. Leave the `Tab`, `Up`/`k`, and `Down`/`j`
   arms unchanged.

3. In `app.rs` `App::update`, add the `AppEvent::FocusRightOrExpand` handler: if
   `self.focused_node()` is a `TreeNode::Run` whose run's `RunId` is in
   `self.collapsed_runs` (i.e. a *collapsed* run), call `self.tree_toggle_expand()`
   to expand it; otherwise (an already-expanded run, or a `TreeNode::Task`) set
   `self.focused_panel = Panel::Main`. Return `true`. Read the run's `RunId` using
   whatever accessor plan 0016 uses in `tree_toggle_expand`/`visible_tree_nodes`
   (grep them) — do not invent a new field.

4. Add the `AppEvent::FocusLeftOrCollapse` handler: `match self.focused_panel` —
   `Panel::Main` ⇒ set `self.focused_panel = Panel::Sidebar` (no collapse);
   `Panel::Sidebar` ⇒ if `self.focused_node()` is a `TreeNode::Run` whose run is
   *expanded* (its `RunId` **not** in `collapsed_runs`), call
   `self.tree_toggle_expand()` to collapse it, else no-op. Return `true`. Leave
   `AppEvent::FocusNext` (Tab) untouched.

5. Add tests in `app.rs` (use the existing `App` test fixture/builder these tests
   already use for the sidebar):

   ```rust
   #[test]
   fn right_expands_then_focuses_content() { /* App, sidebar focused, cursor on a COLLAPSED run (id in collapsed_runs): FocusRightOrExpand => run removed from collapsed_runs (expanded), focused_panel still Sidebar; FocusRightOrExpand again => focused_panel == Panel::Main */ }
   #[test]
   fn left_returns_to_sidebar() { /* focused_panel = Panel::Main; FocusLeftOrCollapse => focused_panel == Panel::Sidebar (collapsed_runs unchanged) */ }
   #[test]
   fn left_collapses_expanded_run() { /* sidebar focused, cursor on an EXPANDED run (not in collapsed_runs): FocusLeftOrCollapse => run's RunId now in collapsed_runs, focused_panel still Sidebar */ }
   ```

   Also add an `event.rs` translation test (mirroring the existing
   `KeyCode::Tab`/`Up`/`Down` cases) asserting `KeyCode::Right` →
   `AppEvent::FocusRightOrExpand` and `KeyCode::Left` →
   `AppEvent::FocusLeftOrCollapse`.

- **Depends on:** — (but **requires plan 0016 merged**: uses
  `focused_node()`, `TreeNode`, `tree_toggle_expand`, `collapsed_runs`).
- **Done when:** the three `app.rs` tests and the `event.rs` translation test
  pass; `Right` on a collapsed run expands it and a second `Right` moves focus to
  `Panel::Main`; `Left` from `Panel::Main` returns to `Panel::Sidebar` and `Left`
  on an expanded run collapses it; `Tab` still toggles panels; cargo
  test/clippy/fmt green.

---

## 0060 — Up/Down: navigate vs scroll

### directional-up-down — Sidebar navigates nodes, content scrolls

Make `Up`/`Down` (and the `k`/`j` aliases) navigate the tree when the sidebar is
focused but **scroll the exchange/content pane** when the main pane is focused,
reusing plan 0016's `tree_move` and the existing scroll helpers.

**Steps:**

1. In `crates/makina/src/app.rs`, in the `AppEvent::SelectUp` handler's
   `match self.focused_panel`: route the `Panel::Sidebar` arm to
   `self.tree_move(-1)` (plan 0016) and the `Panel::Main` arm to `self.scroll_up()`
   — replacing the current `selected_task = current.saturating_sub(1)` mutation.

2. In the `AppEvent::SelectDown` handler's `match self.focused_panel`: route the
   `Panel::Sidebar` arm to `self.tree_move(1)` and the `Panel::Main` arm to
   `self.scroll_down(self.last_scroll_max.get())` — the same clamp the mouse-wheel
   `AppEvent::ScrollDown` handler uses — replacing the current `selected_task`
   increment.

3. Make no `event.rs` change for `Up`/`Down`/`k`/`j`: they still emit
   `AppEvent::SelectUp`/`SelectDown` (so `k`/`j` remain aliases); only the handlers
   change. If plan 0016 already routed the `Panel::Sidebar` arms through
   `tree_move`, that part is a no-op delta — confirm against the merged 0016 code
   and ensure only the `Panel::Main` arms change to the `scroll_*` calls.

4. Add tests in `app.rs`:

   ```rust
   #[test]
   fn up_down_navigates_tree_when_sidebar_focused() { /* Panel::Sidebar, cursor on first node; SelectDown => focused_node() advanced (via tree_move); SelectUp => back; assert through focused_node()/tree_cursor */ }
   #[test]
   fn up_down_scrolls_content_when_main_focused() { /* Panel::Main, app.last_scroll_max.set(n>0): SelectUp => exchange_auto_follow becomes false (scroll_up); SelectDown enough times => exchange_scroll advances toward/equals last_scroll_max; assert selected_task is NOT changed by these events */ }
   ```

- **Depends on:** arrow-focus-traversal (and **plan 0016** for `tree_move`).
- **Done when:** both tests pass; with the sidebar focused `Up`/`Down`/`k`/`j`
  walk the visible tree nodes via `tree_move`; with the main pane focused they
  scroll the exchange pane (mutating `exchange_scroll`/`exchange_auto_follow`) and
  leave `selected_task` unchanged; cargo test/clippy/fmt green.

---

**End of plan 0018 TASKS.** When every "Done when" bullet is green, the arrow keys
traverse the sidebar tree the way the layout implies — `Right` drills into a run
then crosses to the content, `Left` steps back or collapses — and `Up`/`Down`
navigate the tree or scroll the content depending on which pane has focus, while
`Tab` still flips between the two panels.
