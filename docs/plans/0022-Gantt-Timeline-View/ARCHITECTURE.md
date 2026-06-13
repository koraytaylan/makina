# Architecture — Plan 0022

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). This plan touches `makina-core` (`api.rs`, `orchestrator.rs`,
> `run_metadata.rs`) and the `makina` TUI crate (`ui.rs`, `Cargo.toml`).

## Current shape (what exists)

- **Domain `Task`** (`crates/makina-core/src/task.rs`, `pub struct Task`) already
  carries the timestamps we need:

  ```rust
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub started_at: Option<DateTime<Utc>>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub finished_at: Option<DateTime<Utc>>,
  ```

  `task.rs` imports `use chrono::{DateTime, Utc};` already.
- **`TaskView` DTO** (`crates/makina-core/src/api.rs`, `pub struct TaskView`):
  fields are `id, title, state, gate_iterations, review_iterations, depends_on,
  failure_reason` — **no timestamps**. `api.rs` does **not** currently import
  `chrono`.
- **Orchestrator conversion** (`crates/makina-core/src/orchestrator.rs`, the
  `.map(|task| TaskView { … })` closure inside the `RunView` builder): copies the
  seven fields above from each `Task` and **omits** `started_at`/`finished_at`.
- **Persistence** (`crates/makina-core/src/run_metadata.rs`):
  - `pub struct TaskSnapshot { id, title, state, gate_iterations,
    review_iterations, depends_on, failure_reason }` — `failure_reason` is the
    existing additive `#[serde(default, skip_serializing_if = "Option::is_none")]`
    field. `run_metadata.rs` imports `use chrono::{DateTime, Utc};` already.
  - `TaskSnapshot` is **written** in `orchestrator.rs` (the `.map(|t| TaskSnapshot
    { … })` block at finalization) and **read back** in
    `run_view_from_metadata` (`run_metadata.rs`, the `.map(|t| TaskView { … })`
    block) to reconstruct a `RunView` for a disk-loaded finished run.
- **Timeline render** (`crates/makina/src/ui.rs`, `fn render_dependency_view`, the
  `DependencyViewMode::Timeline` arm): computes `let levels =
  dependency_levels(&run.tasks);` and renders one `Line` per level as
  side-by-side `"[state] id"` spans via `task_state_badge`. The inner drawable
  rect is `let inner = block.inner(area);` (so `inner.width` / `inner.height` are
  the available columns/rows). `dependency_levels` is a pure helper just below.
- **`task_state_badge`** (`ui.rs`, `fn task_state_badge(&TaskState) ->
  (&'static str, Color)`): the canonical state→colour map (`InProgress` green,
  `InReview` yellow, `Done` cyan, `Failed` red, `New` DarkGray, `Skipped`
  DarkGray, `Ready` White — note `Ready` is **not** grey). The plan reuses the
  returned `Color` as-is, so this is just a colour-accuracy note.
- **Clock in the TUI:** the `makina` crate has **no `chrono` dependency**; it uses
  `std::time::SystemTime::now()` for exchange-entry timestamps. `chrono` is a
  workspace dependency (`chrono = { version = "0.4", features = ["serde"] }`)
  already used by `makina-core`.

## 0067 — Plumb task timestamps to the view

Edits in `crates/makina-core/src/api.rs`, `orchestrator.rs`, and
`run_metadata.rs`.

- **`TaskView` gains two fields.** In `api.rs`, add a `chrono` import and two
  optional timestamp fields to `TaskView`, mirroring the additive style of the
  existing `failure_reason` field:

  ```rust
  use chrono::{DateTime, Utc};

  pub struct TaskView {
      // … id, title, state, gate_iterations, review_iterations, depends_on …

      /// When a Developer agent first picked up this task; `None` until then.
      /// Carried from the domain `Task` so the Gantt can scale a bar to the
      /// task's real start.
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub started_at: Option<DateTime<Utc>>,

      /// When the task reached a terminal state (`Done`/`Failed`); `None` while
      /// it is still running or not yet started.
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub finished_at: Option<DateTime<Utc>>,

      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub failure_reason: Option<FailureReason>,
  }
  ```

  Every existing `TaskView { … }` literal must add the two fields. `TaskView`
  does **not** derive `Default`, and `#[serde(default)]` relaxes only
  *deserialization* — it does **not** relax Rust struct-literal completeness, so
  any literal without a `..` spread fails to compile until the fields are added
  (the same blast radius plan 0014's `failure_reason` hit). `grep -rn 'TaskView
  {' crates/` finds ~67 literal sites across `api.rs`, `orchestrator.rs`,
  `app.rs`, `event.rs`, `ui.rs`, `placeholder.rs`, `run_metadata.rs`, and tests —
  update **all** of them (the in-crate test/doc literals in `api.rs` and the TUI
  test fixtures in `ui.rs`, e.g. `timeline_groups_independent_tasks_and_
  orders_dependents`, set them to `None` unless the test needs timing).

- **Orchestrator conversion populates them.** In `orchestrator.rs`, in the
  `.map(|task| TaskView { … })` closure, copy the domain timestamps straight
  through:

  ```rust
  .map(|task| TaskView {
      id: (&task.id).into(),
      title: task.title.clone(),
      state: task.state.into(),
      gate_iterations: task.gate_iterations,
      review_iterations: task.review_iterations,
      depends_on: task.depends_on.iter().map(Into::into).collect(),
      started_at: task.started_at,
      finished_at: task.finished_at,
      failure_reason: task.failure_reason.clone(),
  })
  ```

- **`TaskSnapshot` persists them.** In `run_metadata.rs`, add the same two fields
  to `TaskSnapshot` with the additive serde attributes so old `run.json` files
  (which lack them) still deserialise:

  ```rust
  pub struct TaskSnapshot {
      // … id, title, state, gate_iterations, review_iterations, depends_on …

      /// When the task first entered `InProgress`. Additive: old `run.json`
      /// files without it still load with `#[serde(default)]`.
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub started_at: Option<DateTime<Utc>>,

      /// When the task reached its terminal state. Additive, as above.
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub finished_at: Option<DateTime<Utc>>,

      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub failure_reason: Option<crate::api::FailureReason>,
  }
  ```

- **Write side.** In `orchestrator.rs`, the `.map(|t| TaskSnapshot { … })` block
  that captures per-task state at finalization adds `started_at: t.started_at,`
  and `finished_at: t.finished_at,` (the domain `Task` fields).

- **Read side.** In `run_metadata.rs`, `run_view_from_metadata`'s `.map(|t|
  TaskView { … })` block adds `started_at: t.started_at,` and `finished_at:
  t.finished_at,` so a reopened finished run carries its timing into the view.

This is a pure data plumb — no behavioural change to scheduling or rendering on
its own; the new fields are simply present and `None` for any task that never
started.

## 0068 — Render the Gantt

Edits in `crates/makina/src/ui.rs` (the `Timeline` arm + a new pure helper) and
`crates/makina/Cargo.toml`.

- **Add `chrono` to the TUI crate.** In `crates/makina/Cargo.toml`
  `[dependencies]`, add `chrono = { workspace = true }` (the workspace already
  pins `chrono` with the `serde` feature). The Gantt reads `chrono::Utc::now()`
  at render and works with the `DateTime<Utc>` fields now on `TaskView`.

- **A pure geometry helper.** Add a small, clock-free helper next to
  `dependency_levels` so the column math is unit-testable without a clock or a
  `Frame`. It takes the already-computed run span and a task's start/end and
  returns the inclusive `[start_col, end_col)` band within `width`:

  ```rust
  /// Map a task's wall-clock window onto bar columns within `width`.
  ///
  /// `span_start`/`span_end` are the run's overall window (with `span_end >
  /// span_start` guaranteed by the caller — degenerate spans are handled before
  /// this is reached). `start`/`end` are the task's clamped window. Returns
  /// `(start_col, end_col)` with `0 <= start_col <= end_col <= width`, scaling
  /// linearly by elapsed nanoseconds. No clock is read here.
  fn gantt_bar_cols(
      span_start: DateTime<Utc>,
      span_end: DateTime<Utc>,
      start: DateTime<Utc>,
      end: DateTime<Utc>,
      width: u16,
  ) -> (u16, u16) {
      let total = (span_end - span_start).num_nanoseconds().unwrap_or(1).max(1);
      let off = (start - span_start).num_nanoseconds().unwrap_or(0).max(0);
      let len = (end - start).num_nanoseconds().unwrap_or(0).max(0);
      let w = width as i128;
      let start_col = ((off as i128 * w) / total as i128) as u16;
      // Round the end up so a non-zero-duration task always shows ≥1 cell.
      let raw_end = (((off + len) as i128 * w) / total as i128) as u16;
      let end_col = raw_end.max(start_col.saturating_add(1)).min(width);
      (start_col.min(width), end_col)
  }
  ```

- **Rewrite the `Timeline` arm.** Replace the `dependency_levels` lane rendering
  with a Gantt over `run.tasks`. Pseudocode for the arm body:

  ```rust
  DependencyViewMode::Timeline => {
      let now = chrono::Utc::now();          // read the clock ONCE, at render
      let run = app.selected_run();
      let lines: Vec<Line> = match run {
          Some(run) if !run.tasks.is_empty() => {
              // Span: min started_at over started tasks → max(finished_at, or
              // `now` for any task still running).
              let span_start = run.tasks.iter().filter_map(|t| t.started_at).min();
              match span_start {
                  None => placeholder_line("  No timing yet."),  // nothing started
                  Some(span_start) => {
                      let mut span_end = run.tasks.iter()
                          .filter_map(|t| t.finished_at)
                          .max()
                          .unwrap_or(span_start);
                      let any_running = run.tasks.iter()
                          .any(|t| t.started_at.is_some() && t.finished_at.is_none());
                      if any_running { span_end = span_end.max(now); }
                      if span_end <= span_start { span_end = span_start + chrono::Duration::seconds(1); }

                      // label width: a few columns for the id, rest for the bar.
                      let bar_width = inner.width.saturating_sub(LABEL_COLS);
                      run.tasks.iter().map(|task| {
                          let (badge_unused, color) = task_state_badge(&task.state);
                          let label = truncate_label(&task.id.0, LABEL_COLS);
                          let bar: String = match task.started_at {
                              None => "·".repeat(bar_width as usize),   // ghost: not started
                              Some(start) => {
                                  let end = task.finished_at.unwrap_or(now);
                                  let (a, b) = gantt_bar_cols(span_start, span_end, start, end, bar_width);
                                  // spaces before, '█' across [a,b), spaces after
                                  render_bar_string(a, b, bar_width)
                              }
                          };
                          Line::from(vec![
                              Span::raw(label),
                              Span::styled(bar, Style::default().fg(color)),
                          ])
                      }).collect()
                  }
              }
          }
          _ => vec![placeholder_line("  No tasks.")],
      };
      frame.render_widget(Paragraph::new(lines), inner);
  }
  ```

  Notes:
  - `LABEL_COLS` is a small `const u16` (e.g. `12`); the bar gets `inner.width -
    LABEL_COLS` columns. Guard `bar_width == 0` (very narrow pane) by emitting just
    the label.
  - **Colour by state** comes from `task_state_badge`'s returned `Color`; the bar
    glyph is `'█'` for the filled span and `'·'` (dim) for a not-yet-started ghost
    slot. A still-running task's bar extends to the `now` column (its end is
    `finished_at.unwrap_or(now)`), so it visibly grows.
  - The placeholder/degenerate path (`span_start` is `None`, or no tasks) renders a
    single dim line — no division by zero, no panic.
  - The `dependency_levels` helper is now unused by the `Timeline` arm. Leave it in
    place only if another arm or test still references it; if nothing else uses it,
    remove it (and its test) so `clippy` stays green. (Verify with a grep before
    deleting — the `dependency_levels_assigns_parallel_siblings_same_level` test
    references it today.)

## Testing notes

- 0067 is pure data: `task_view_carries_timestamps` builds a `Task` with
  `started_at = Some(t0)`, `finished_at = Some(t1)`, runs it through the
  `TaskView` conversion — which lives in `orchestrator.rs` (the `.map(|task|
  TaskView { … })` RunView builder, ~`orchestrator.rs:297`), **not** `api.rs` —
  and checks the view carries `Some/Some` (a not-started `Task` yields
  `None/None`). Put the test in `orchestrator.rs` where that conversion is
  reachable. `snapshot_roundtrips_timestamps`
  writes a `RunMetadata` whose `TaskSnapshot` has the two fields set,
  `serde_json` round-trips it, and asserts they survive; a second case
  deserialises a JSON `TaskSnapshot` **lacking** the fields and asserts it loads
  with `None/None` (the `#[serde(default)]` contract).
- 0068 geometry is pure and clock-free: `gantt_positions_bars_by_time` calls
  `gantt_bar_cols` with a known span and known task windows for a fixed `width`
  and asserts exact `(start_col, end_col)`. `pending_task_has_no_solid_bar`
  renders (or inspects the arm output for) a task with `started_at = None` and
  asserts its row has no `'█'` cell. `empty_span_shows_placeholder` renders a run
  whose every task has `started_at = None` and asserts the "no timing yet"
  placeholder appears and no panic occurs.
- All work keeps `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.

## Interaction with prior plans

- Builds on the dependency-view machinery (`render_dependency_view`,
  `DependencyViewMode`, `task_state_badge`) already present. The sidebar tree
  helpers introduced by plan 0016 (`App::focused_node`, `TreeNode`, `tree_cursor`,
  `collapsed_runs`, the `Panel` enum) and the retry path (plan 0017) are
  unaffected — this plan changes only the dependency pane's `Timeline` arm and the
  `TaskView`/`TaskSnapshot` data shape. The additive serde fields preserve plan
  0010's persisted-run reload (`old_run_json_without_snapshot_still_loads`).
