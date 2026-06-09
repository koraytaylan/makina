# Makina Plan 0012 — Task-List Polish

Remove the meaningless "G"/"R" columns from the task table (relocating the
counts into the task detail), and make the already-built dependency views
(List/Tree/Timeline, cycled by `v`) discoverable in the UI.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas. Both changes are in the `makina` binary crate.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0042 — Remove the G/R columns

### remove-gate-review-columns — Drop G/R from the table; show counts in detail

The task table renders four columns — `Task | State | G | R` — where `G`/`R` are
`gate_iterations` / `review_iterations`, plus a conditional legend in the status
bar. They read as noise. Remove them from the table and surface the counts in the
task detail instead.

**Steps:**

1. Open `crates/makina/src/ui.rs` and find the task-table builder (the
   `table_header` `Row::new(vec![ … ])` with `Cell::from("G")` and
   `Cell::from("R")`).

2. Delete the `Cell::from("G")…` and `Cell::from("R")…` header cells, leaving
   `Task` and `State`.

3. In the row builder, delete the two corresponding
   `Cell::from(task.gate_iterations.to_string())…` and
   `…review_iterations…` cells.

4. Update the table's column-width `Constraint`s to the two remaining columns so
   they reflow (e.g. `[Constraint::Min(20), Constraint::Length(12)]`).

5. Remove the `show_legend` computation and the `legend` string interpolation
   from the status-bar builder (the `"  │  G = gate iterations  R = review iterations"`).

6. In the task **detail** rendering (the block that shows the selected task's
   title/state), add a dim line with the relocated counts:

   ```rust
   let counts = format!("gate ×{}  ·  review ×{}", task.gate_iterations, task.review_iterations);
   let style = if task.gate_iterations + task.review_iterations == 0 {
       Style::default().fg(Color::DarkGray)
   } else {
       Style::default().fg(Color::Yellow)
   };
   detail_lines.push(Line::from(Span::styled(counts, style)));
   ```

7. Add tests:

   ```rust
   #[test]
   fn task_table_has_no_gate_review_columns() { /* render table; assert header cells are exactly Task, State */ }
   #[test]
   fn task_detail_shows_iteration_counts() { /* render detail for a task with gate=2, review=1; assert "gate ×2" and "review ×1" appear */ }
   ```

   The `TaskView::gate_iterations` / `review_iterations` fields stay (used by the
   detail and by plan 0010's persisted snapshot).

- **Depends on:** —
- **Done when:** `task_table_has_no_gate_review_columns` and
  `task_detail_shows_iteration_counts` pass; `grep -n 'Cell::from("G")\|Cell::from("R")' crates/makina/src/ui.rs` returns nothing; existing table tests updated and green; cargo test/clippy/fmt green.

---

## 0043 — Make the dependency views discoverable

### surface-dependency-view-key — Advertise `[v]` and show the current view

`DependencyViewMode { Off, List, Tree, Timeline }` is fully implemented and cycled
by the `v` key, but the UI never hints it exists. Add a status-bar hint, a
current-view label, and a sub-pane title.

**Steps:**

1. Open `crates/makina/src/ui.rs` and find the status-bar format string (the one
   containing `[o] open  [s/p/c] start/pause/cancel  [Tab] panel  [q/^C] quit`).

2. Add `[v] view` to the hint list. **Reserve the status-bar string for this
   plan** — plan 0011 will slot its editor hotkey in here later, so leave room
   and do not consume an additional letter key beyond `v`.

3. Compute the current-view label from `app.dependency_view` and render it in the
   status bar:

   ```rust
   let view = match app.dependency_view {
       DependencyViewMode::Off => "off",
       DependencyViewMode::List => "list",
       DependencyViewMode::Tree => "tree",
       DependencyViewMode::Timeline => "timeline",
   };
   // … include `format!("  │  view: {view}")` in the status-bar line
   ```

4. When the dependency sub-pane is rendered (any mode other than `Off`), set its
   block title to `Dependencies — {view}` so the active mode is labelled.

5. Add tests:

   ```rust
   #[test]
   fn status_bar_advertises_view_key() { /* render status bar; assert it contains "[v]" */ }
   #[test]
   fn status_bar_shows_current_view_label() { /* set dependency_view = Tree; assert "view: tree" (or "Tree") appears */ }
   ```

- **Depends on:** —
- **Done when:** `status_bar_advertises_view_key` and
  `status_bar_shows_current_view_label` pass; `grep -n '\[v\]' crates/makina/src/ui.rs` matches; cycling `v` updates the rendered label; cargo test/clippy/fmt green.

---

**End of plan 0012 TASKS.** When every "Done when" bullet is green, the task table
is clean and the timeline/tree/list views are findable from the UI.
