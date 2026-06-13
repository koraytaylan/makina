# Architecture — Plan 0016

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). This plan touches only the `makina` (TUI) crate.

## Current shape (what exists)

- **Frame layout** (`crates/makina/src/ui.rs`, the top `render`/draw fn): a
  vertical split — title bar (1) · warning banner (0–1) · body · status bar (1).
  The **body** is a horizontal split `Percentage(30)` sidebar / `Percentage(70)`
  main.
- **Sidebar** (`ui.rs`, the `// ── Sidebar ──` block): a `List` titled `"Runs"`,
  one `ListItem` per `app.runs` entry built as `status_badge(run.status) + " " +
  run_label(run)`; highlighted via a `ListState` seeded from `app.selected_run`.
- **Main panel** (`ui.rs`, the main vertical split): header · **task table**
  (`Table`, up to ~10 rows, built from `run.tasks` with `task_state_badge` +
  `failure_kind_label`) · ingestion pane · exchange pane (`Min(3)`) · error pane.
- **App state** (`crates/makina/src/app.rs`): `pub runs: Vec<RunView>`,
  `pub selected_run: Option<usize>`, `pub selected_task: Option<usize>`,
  `pub focused_panel: Panel` (`enum Panel { Sidebar, Main }`).
- **Navigation** (`app.rs` `AppEvent::SelectUp`/`SelectDown`): when
  `Panel::Sidebar`, moves `selected_run` (and `clamp_selected_task` +
  `load_exchanges_for_selected_run`); when `Panel::Main`, moves `selected_task`.
  Keys bound in `crates/makina/src/event.rs` (`Up`/`k`, `Down`/`j`, `Tab`).
- **Reusable helpers** (`ui.rs`): `task_state_badge(&TaskState) -> (&str, Color)`,
  `failure_kind_label(&FailureKind) -> &str`, `status_badge(&RunStatus)`,
  `panel_block(title, focused)`, `run_label(&RunView)`, `spinner_frame(tick)`.

## 0052 — Sidebar tree state model

Edits in `crates/makina/src/app.rs`.

- **Expand state.** Track which runs are expanded. Use a set keyed by stable run
  identity (the `RunId`), not by index, so it survives run-list reordering:

  ```rust
  /// Run ids whose task children are collapsed in the sidebar tree.
  /// Absent ⇒ expanded (runs default to expanded).
  pub collapsed_runs: std::collections::HashSet<makina_core::api::RunId>,
  ```

- **Flattened visible nodes.** A node is either a run header or one of its tasks
  (only present when the run is expanded). Build it on demand for render +
  navigation:

  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum TreeNode {
      /// The run at `runs[run]`.
      Run { run: usize },
      /// Task at `runs[run].tasks[task]`.
      Task { run: usize, task: usize },
  }

  impl App {
      /// Flatten open runs + the tasks of expanded runs into the visible-node
      /// order shown in the sidebar (run, then its tasks if expanded, repeat).
      pub fn visible_tree_nodes(&self) -> Vec<TreeNode> { /* … */ }
  }
  ```

- **Cursor.** Replace direct `selected_run`/`selected_task` mutation in the
  sidebar with a single cursor over `visible_tree_nodes()`. Keep
  `selected_run`/`selected_task` as **derived** fields updated from the focused
  node so exchange loading and the main detail keep working:

  ```rust
  /// Index into `visible_tree_nodes()` of the focused sidebar node.
  /// `None` when no runs are open.
  pub tree_cursor: Option<usize>,

  impl App {
      /// The node currently under the tree cursor, if any.
      pub fn focused_node(&self) -> Option<TreeNode> { /* visible_tree_nodes().get(cursor) */ }
      /// Recompute `selected_run`/`selected_task` from the focused node and,
      /// when the run changed, load that run's exchanges. Call after any cursor
      /// or expand/collapse change.
      fn sync_selection_from_cursor(&mut self) { /* … */ }
      /// Move the cursor by ±1 within the visible nodes (clamped), then
      /// `sync_selection_from_cursor`.
      pub fn tree_move(&mut self, delta: isize) -> bool { /* … */ }
      /// Toggle collapse on the focused node's run (Run node, or the parent of a
      /// Task node); keep the cursor on the run header; re-sync.
      pub fn tree_toggle_expand(&mut self) -> bool { /* … */ }
  }
  ```

- **Seeding & updates.** Where `selected_run` is first set (open/seed path) and
  wherever `runs` changes from api events, initialise/clamp `tree_cursor` to a
  valid node (prefer the previously-focused run/task identity; fall back to the
  first node). Collapsing a run whose task was focused moves the cursor to that
  run's header.

## 0053 — Render the sidebar tree

Edits in `crates/makina/src/ui.rs`.

- **Sidebar render.** Replace the flat `List` of runs with a `List` (or
  `Table`/`Paragraph` lines) over `app.visible_tree_nodes()`:
  - **Run node:** `▾`/`▸` disclosure (from `collapsed_runs`) + `status_badge(run)`
    + `run_label(run)`.
  - **Task node:** an indent (two spaces), `task_state_badge(task.state)` (with
    `spinner_frame` prefix for `InProgress`/`InReview`), the task title, and —
    for `Failed` tasks — the `failure_kind_label(reason.kind)` suffix, exactly as
    the old main-panel table built `state_cell`.
  - Highlight the node at `app.tree_cursor` with the existing cyan highlight
    style; keep `panel_block("Runs & Tasks", sidebar_focused)`.
  - Preserve the empty-state hint (`No runs open …`) when `app.runs.is_empty()`.

- **Main panel: drop the task table.** In the main vertical split, **remove the
  task-table region** (the `Constraint::Length(table_rows)` segment and the
  `Table`/rows builder). Keep header, ingestion pane, exchange pane, error pane;
  give the freed rows to the exchange pane (it already uses `Min(3)` — it simply
  gets more room once the table constraint is gone).

- **Header.** Keep the run header line, and since the focused node may be a task,
  ensure the detail block keys off `app.selected_task` exactly as today (already
  derived by 0052). No behavioural change to the exchange/detail rendering itself.

## 0054 — Tree navigation & keys

Edits in `crates/makina/src/app.rs` (`AppEvent` handlers) and
`crates/makina/src/event.rs` (key bindings).

- **Up/Down.** When `Panel::Sidebar`, route `SelectUp`/`SelectDown` to
  `app.tree_move(-1)` / `tree_move(1)` instead of the run-index logic. When
  `Panel::Main`, keep moving `selected_task` within the focused run (the main
  panel still scrolls the focused run's task selection for exchange viewing) —
  or, simpler and consistent, also route Main Up/Down through the cursor; pick
  the variant that keeps `selected_task` correct for the exchange pane and state
  it in the task.
- **Space → expand/collapse.** Add an `AppEvent` (e.g. `ToggleTreeNode`) and bind
  `Space` (normal mode, sidebar focus) to `app.tree_toggle_expand()`. No-op with
  a status message if the focused node is a task whose run can't collapse.
- **Tab.** Unchanged — toggles `Panel::Sidebar`↔`Panel::Main`.
- **Selection side-effects.** `tree_move`/`tree_toggle_expand` must call
  `sync_selection_from_cursor` so the main detail + exchange follow the focused
  node (Task → that task's exchange; Run → the run summary/ingestion), reusing the
  existing `load_exchanges_for_selected_run` path.

## Testing notes

- 0052 logic is pure: test `visible_tree_nodes`, `tree_move`, and
  `tree_toggle_expand` against a fixture `App` with two runs (one expanded, one
  collapsed) — assert node order, cursor clamping, and derived
  `selected_run`/`selected_task`.
- 0053 render: build a fixture `App`, render to a test `Buffer`, assert the
  sidebar shows a `▾`/`▸` run row and nested task rows with badges, and that the
  main panel no longer contains the task-table header.
- 0054: drive `SelectDown`/`ToggleTreeNode` through `app.update(...)` and assert
  cursor + derived selection move as expected.
