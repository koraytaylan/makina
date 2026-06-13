# Makina Plan 0016 — Task-List Sidebar Tree

Turn the left sidebar from a flat list of runs into a **"Runs & Tasks" tree**:
each open run is an expandable parent with its tasks nested beneath it. Remove
the task table from the main panel so the exchange pane grows into the freed
space.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0052 — Sidebar tree state model

### sidebar-tree-model — Per-run expand state, flattened nodes, and a cursor

Give `App` the state a tree needs: which runs are collapsed, a flattened list of
visible nodes, and a single cursor that derives the existing
`selected_run`/`selected_task`.

**Steps:**

1. In `crates/makina/src/app.rs`, add to the `App` struct:
   `pub collapsed_runs: std::collections::HashSet<makina_core::api::RunId>` (a
   run **absent** from the set is expanded — runs default expanded) and
   `pub tree_cursor: Option<usize>` (index into the visible-node list). Initialise
   both in every `App` constructor (`App::new` / `App::with_config`): empty set,
   and `tree_cursor = if runs.is_empty() { None } else { Some(0) }`.

2. Add `pub enum TreeNode { Run { run: usize }, Task { run: usize, task: usize } }`
   (`#[derive(Debug, Clone, Copy, PartialEq, Eq)]`) and
   `pub fn visible_tree_nodes(&self) -> Vec<TreeNode>`: for each `run` index in
   `self.runs`, push `Run { run }`, then — unless that run's `RunId` is in
   `collapsed_runs` — push `Task { run, task }` for each task index.

3. Add `pub fn focused_node(&self) -> Option<TreeNode>`
   (`visible_tree_nodes().get(cursor).copied()`) and a private
   `fn sync_selection_from_cursor(&mut self)` that sets `selected_run`/
   `selected_task` from the focused node (`Run` ⇒ that run, `selected_task` =
   first task or `None`; `Task` ⇒ that run + task) and, when the resolved run
   changed, calls the existing `load_exchanges_for_selected_run()`.

4. Add `pub fn tree_move(&mut self, delta: isize) -> bool` (clamp the cursor to
   `0..visible_tree_nodes().len()`, then `sync_selection_from_cursor`; return
   whether the cursor moved) and `pub fn tree_toggle_expand(&mut self) -> bool`
   (resolve the focused node's run; insert/remove its `RunId` in `collapsed_runs`;
   move the cursor onto that run's header node; `sync_selection_from_cursor`;
   return `true`).

5. Add tests in `app.rs`:

   ```rust
   #[test]
   fn visible_nodes_expand_and_collapse() { /* App w/ 2 runs (run0: 2 tasks, run1: 1 task), both expanded => [Run0,Task00,Task01,Run1,Task10]; collapse run0 => [Run0,Run1,Task10] */ }
   #[test]
   fn tree_move_clamps_and_syncs_selection() { /* cursor at 0 (Run0); move +1 => Task00 => selected_run=0, selected_task=Some(0); move -1 at top stays; move past end clamps */ }
   #[test]
   fn toggle_expand_keeps_cursor_on_run_and_collapses() { /* focus Task01, toggle => run0 collapsed, cursor on Run0 header, selected_task=None */ }
   ```

- **Depends on:** —
- **Done when:** the three tests pass; `visible_tree_nodes`, `tree_move`, and
  `tree_toggle_expand` behave as specified; `selected_run`/`selected_task` are
  correctly derived from the cursor; cargo test/clippy/fmt green.

---

## 0053 — Render the sidebar tree

### render-sidebar-tree — Draw the tree; drop the main-panel task table

Render the flattened nodes as a tree in the sidebar and remove the now-redundant
task table from the main panel.

**Steps:**

1. In `crates/makina/src/ui.rs`, in the sidebar render block (the
   `panel_block("Runs", …)` / `List` over `app.runs`), retitle the block to
   `"Runs & Tasks"` and build one `ListItem` per `app.visible_tree_nodes()`:
   - **`TreeNode::Run { run }`:** a disclosure glyph — `"▾ "` when expanded,
     `"▸ "` when the run's `RunId` is in `app.collapsed_runs` — then
     `status_badge(&run.status)` (badge + colour) then a space then
     `run_label(run)`.
   - **`TreeNode::Task { run, task }`:** a two-space indent, then the same
     `state_cell` the old main-panel table built — `task_state_badge(&task.state)`
     with a `spinner_frame(app.tick)` prefix for `InProgress`/`InReview`, a space,
     the task title, and for `Failed` tasks a trailing
     `failure_kind_label(reason.kind)` when `task.failure_reason` is `Some`.

2. Highlight the node at `app.tree_cursor` using the existing cyan
   `highlight_style` + `highlight_symbol("▶ ")`, seeding the `ListState` from
   `app.tree_cursor` instead of `app.selected_run`. Keep the
   `app.runs.is_empty()` empty-state hint unchanged.

3. In the **main-panel** vertical split (the `Layout` with header / task table /
   ingestion / exchange / error), **delete the task-table region**: remove its
   `Constraint` from the split and the `Table`/rows builder that uses
   `task_state_badge`. Leave header, ingestion pane, exchange pane (`Min(3)`), and
   error pane; the exchange pane absorbs the freed rows. Keep the focused-task
   detail block (gate/review · failure reason · idle/countdown) — it keys off
   `app.selected_task`, still set by 0052.

4. Add tests in `ui.rs`:

   ```rust
   #[test]
   fn sidebar_renders_run_and_nested_tasks() { /* App: 1 run expanded w/ a Done + a Failed task; render; assert buffer contains "Runs & Tasks", a "▾" run row, "✓"/"[✗ failed]" task rows, and the failure label */ }
   #[test]
   fn collapsed_run_hides_its_tasks() { /* collapse the run; render; assert "▸" shows and the task titles are absent from the sidebar columns */ }
   #[test]
   fn main_panel_no_longer_renders_task_table_header() { /* render full frame; assert the old task-table column header text is absent from the main area */ }
   ```

- **Depends on:** sidebar-tree-model
- **Done when:** the three tests pass; the sidebar renders runs as `▾`/`▸`
  parents with nested task rows (badges, spinner, failure label); collapsing a run
  hides its tasks; the main panel no longer renders the task table and the
  exchange pane occupies the freed space; cargo test/clippy/fmt green.

---

## 0054 — Tree navigation & keys

### sidebar-tree-navigation — Walk the tree, expand/collapse, keep focus working

Wire keys to the cursor so the tree is navigable and expand/collapse works.

**Steps:**

1. In `crates/makina/src/app.rs`, in the `AppEvent::SelectUp`/`SelectDown`
   handlers, when `self.focused_panel == Panel::Sidebar` route to
   `self.tree_move(-1)` / `self.tree_move(1)` (replacing the run-index increment
   block). Keep the `Panel::Main` arm moving `selected_task` within the focused
   run for the exchange pane; if you instead route Main through the cursor, ensure
   `selected_task` still points at the focused task.

2. Add an `AppEvent::ToggleTreeNode` variant and handle it by calling
   `self.tree_toggle_expand()`. In `crates/makina/src/event.rs`, bind `Space`
   (normal mode) to emit `ToggleTreeNode` when the sidebar is focused. Leave
   `Tab` (panel focus toggle) and existing bindings unchanged.

3. Ensure `runs`-update paths (api-event handlers that replace/extend
   `self.runs`) clamp `tree_cursor` to a valid node and call
   `sync_selection_from_cursor` (prefer keeping the previously-focused run/task
   identity; else fall back to node 0).

4. Add tests in `app.rs`:

   ```rust
   #[test]
   fn select_down_in_sidebar_walks_tree_nodes() { /* Panel::Sidebar, cursor Run0; SelectDown => Task00; SelectDown => Task01 (asserts via focused_node + selected_task) */ }
   #[test]
   fn toggle_tree_node_collapses_focused_run() { /* focus a task; ToggleTreeNode => run collapsed, focused_node is the Run header */ }
   #[test]
   fn cursor_survives_runs_update() { /* simulate a runs replacement that keeps the focused run; assert cursor still focuses the same run identity */ }
   ```

- **Depends on:** render-sidebar-tree
- **Done when:** the three tests pass; `Up`/`Down` walk the visible tree nodes
  when the sidebar is focused; `Space` toggles expand/collapse; `Tab` still
  switches panels; the focused task still drives the main detail/exchange; the
  cursor stays valid across run-list updates; cargo test/clippy/fmt green.

---

**End of plan 0016 TASKS.** When every "Done when" bullet is green, the sidebar
shows every open run as an expandable tree of its tasks — filling the column that
used to be wasted — and the main panel gives the exchange pane the room the task
table used to take.
