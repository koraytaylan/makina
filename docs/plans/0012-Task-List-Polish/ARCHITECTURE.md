# Architecture — Plan 0012 (deltas)

> Two isolated edits in `crates/makina/src/ui.rs` (+ a touch of `app.rs`). Line
> numbers are hints; locate by symbol.

## 0042 — Remove the G/R columns

Today the task table builds a four-column header and rows (ui.rs:285–330):

```
Task | State | G | R
```

where the `G`/`R` cells render `task.gate_iterations` / `task.review_iterations`
(`TaskView`, api.rs:178–184), and a conditional legend
`G = gate iterations  R = review iterations` is appended to the status bar
(ui.rs:407–433) only when some count is non-zero.

Edits:

- In `table_header`, drop the `Cell::from("G")` and `Cell::from("R")` cells.
- In the row builder, drop the two corresponding `Cell::from(... .to_string())`
  cells.
- Update the table column constraints/widths to the two remaining columns
  (`Task`, `State`) so they reflow.
- Remove the `show_legend` computation and the `legend` string from the status
  bar.
- **Relocate the counts** into the task detail rendering (wherever the selected
  task's detail block is drawn): show e.g. `gate ×{n} · review ×{m}` (dim when
  both are 0). The `TaskView` fields are unchanged and remain part of plan 0010's
  persisted snapshot.

## 0043 — Make dependency views discoverable

The machinery already exists:

- `DependencyViewMode { Off, List, Tree, Timeline }` (app.rs:336).
- `AppEvent::CycleDependencyView` handler (app.rs:734) and the `v`/`V` binding
  (event.rs:419).
- Renderers for each mode (ui.rs:479–717).

Add discoverability only:

- **Status-bar hint.** In the status-bar string (ui.rs:433) add `[v] view`
  alongside the existing hints. Keep the string within width; coordinate with
  plan 0011's editor hotkey so keys don't collide (0012 owns this string).
- **Current-view indicator.** Render the active mode label — `View: Off` /
  `List` / `Tree` / `Timeline` — in the status bar (or as the dependency
  sub-pane title, e.g. `Dependencies — Tree`), computed from
  `app.dependency_view`.
- When `dependency_view == Off`, still show `[v] view` so the feature is
  discoverable from the default state.

No change to the view renderers or the dependency data model.

## Test strategy

- `task_table_has_no_gate_review_columns`: render the table; assert the header
  cells are exactly `Task`, `State` (no `G`/`R`).
- `task_detail_shows_iteration_counts`: assert the detail block contains the
  relocated `gate ×… · review ×…` text for a task with non-zero counts.
- `status_bar_advertises_view_key`: assert the status-bar buffer contains
  `[v]` and the current view label; cycling `v` updates the label.

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay green.

## Interaction with prior plans

- Independent of 0009/0010. Shares the status bar with plan 0011's editor hotkey
  — 0012 owns the status-bar string; 0011 slots its key in without collision.
