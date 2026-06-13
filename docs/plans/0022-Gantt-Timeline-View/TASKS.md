# Makina Plan 0022 — Gantt Timeline View

Turn the dependency **Timeline** view from a longest-path lane diagram into a
real **time-based Gantt**: plumb each task's `started_at`/`finished_at` into the
view (and persist them), then draw one labelled, state-coloured bar per task,
scaled to its real wall-clock window across the pane width.

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

## 0067 — Plumb task timestamps to the view

### plumb-task-timestamps — `started_at`/`finished_at` on `TaskView` (+ persist)

The domain `Task` already carries `started_at`/`finished_at`
(`crates/makina-core/src/task.rs`), but `TaskView` has no slot for them and the
orchestrator conversion drops them. Carry them through to the view and persist
them so the Gantt works on both live and reopened runs.

**Steps:**

1. In `crates/makina-core/src/api.rs`, add `use chrono::{DateTime, Utc};` (it is
   not currently imported there) and add to `pub struct TaskView`, mirroring the
   additive style of the existing `failure_reason` field:
   `#[serde(default, skip_serializing_if = "Option::is_none")] pub started_at:
   Option<DateTime<Utc>>` and the same for `pub finished_at:
   Option<DateTime<Utc>>`.

   > **Blast radius — every `TaskView { … }` literal must be updated.**
   > `TaskView` does **not** derive `Default`, and `#[serde(default)]` relaxes
   > only *deserialization* — it does **not** relax Rust struct-literal
   > completeness. So every `TaskView { … }` literal in the codebase that lacks
   > a `..` spread must add `started_at`/`finished_at` or it will not compile —
   > exactly the situation plan 0014's `failure_reason` field hit. `grep -rn
   > 'TaskView {' crates/` (currently ~67 literal sites across `api.rs`,
   > `orchestrator.rs`, `app.rs`, `event.rs`, `ui.rs`, `placeholder.rs`,
   > `run_metadata.rs`, and tests) and update **all** of them; set both fields
   > to `None` in test/doc literals unless the test needs timing.

2. In `crates/makina-core/src/orchestrator.rs`, in the `.map(|task| TaskView {
   … })` closure (the `RunView` builder), add `started_at: task.started_at,` and
   `finished_at: task.finished_at,` so the live view carries the domain
   timestamps.

3. In `crates/makina-core/src/run_metadata.rs`, add the same two fields to `pub
   struct TaskSnapshot` with `#[serde(default, skip_serializing_if =
   "Option::is_none")]` (so old `run.json` files without them still
   deserialise — same attribute the existing `failure_reason` snapshot field
   uses).

4. Populate the snapshot on **write**: in `orchestrator.rs`, the `.map(|t|
   TaskSnapshot { … })` block at finalization adds `started_at: t.started_at,`
   and `finished_at: t.finished_at,`. Populate on **read**: in
   `run_metadata.rs`'s `run_view_from_metadata`, the `.map(|t| TaskView { … })`
   block adds `started_at: t.started_at,` and `finished_at: t.finished_at,`.

5. Add tests in `orchestrator.rs` (where the `TaskView` conversion lives — the
   `.map(|task| TaskView { … })` RunView builder, ~`orchestrator.rs:297`) and
   `run_metadata.rs`:

   ```rust
   #[test]
   fn task_view_carries_timestamps() { /* in orchestrator.rs: a Task with started_at=Some(t0), finished_at=Some(t1) run through the orchestrator's TaskView conversion => both Some; a not-started Task => both None */ }
   #[test]
   fn snapshot_roundtrips_timestamps() { /* RunMetadata w/ a TaskSnapshot { started_at: Some, finished_at: Some } -> serde_json round-trip preserves both; a TaskSnapshot JSON lacking both fields still deserialises with None/None */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; `TaskView` and `TaskSnapshot` carry
  `started_at`/`finished_at`; the orchestrator live conversion and the
  `run_view_from_metadata` disk conversion both populate them; a snapshot lacking
  the fields still loads (the `old_run_json_without_snapshot_still_loads` contract
  remains green); **every `TaskView { … }` literal across the workspace has been
  updated so the whole build compiles** (no `missing field` errors); cargo
  test/clippy/fmt green.

---

## 0068 — Render the Gantt

### render-gantt-timeline — Time-scaled bars in the Timeline view

Rewrite the `DependencyViewMode::Timeline` arm of `render_dependency_view`
(`crates/makina/src/ui.rs`) from a `dependency_levels` lane diagram into a
time-scaled Gantt, with a pure clock-free geometry helper.

**Steps:**

1. In `crates/makina/Cargo.toml` `[dependencies]`, add `chrono = { workspace =
   true }` (the workspace already pins `chrono` with the `serde` feature; the TUI
   crate does not yet depend on it). It is needed to read `chrono::Utc::now()` at
   render and to work with the `DateTime<Utc>` fields now on `TaskView`.

2. In `ui.rs`, add a pure helper `fn gantt_bar_cols(span_start: DateTime<Utc>,
   span_end: DateTime<Utc>, start: DateTime<Utc>, end: DateTime<Utc>, width: u16)
   -> (u16, u16)` next to `dependency_levels`. It maps the task window onto
   `[start_col, end_col)` within `width` by linear scaling over elapsed
   nanoseconds (callers guarantee `span_end > span_start`), clamps to
   `0..=width`, and rounds a non-zero-duration task up to at least one cell. **It
   reads no clock** (the project's `no Date::now in pure helpers` rule).

3. Rewrite the `DependencyViewMode::Timeline` arm: read `let now =
   chrono::Utc::now();` **once** at the top of the arm. For the selected run with
   non-empty tasks, compute `span_start = min(started_at over started tasks)` and
   `span_end = max(finished_at over finished tasks)`, widened to `now` when any
   task is started-but-not-finished; if `span_start` is `None` (nothing started)
   emit a single dim `"  No timing yet."` line. Otherwise render one `Line` per
   task: a fixed-width id/title label (`const LABEL_COLS: u16`) followed by a bar
   span over `inner.width.saturating_sub(LABEL_COLS)` columns — `'█'` across
   `gantt_bar_cols(...)`'s `[start_col, end_col)` coloured by
   `task_state_badge(&task.state)`'s `Color`, with a not-yet-started task
   (`started_at == None`) drawn as a dim `'·'` ghost slot and a still-running
   task's bar extending to the `now` column (`end = finished_at.unwrap_or(now)`).
   Keep the existing `_ => "  No tasks."` placeholder for the no-run/empty case.

4. The `Timeline` arm no longer calls `dependency_levels`. Grep for other users:
   if nothing else references `dependency_levels` (the
   `dependency_levels_assigns_parallel_siblings_same_level` test does today),
   either keep both for the `List`/`Tree` documentation or remove the now-dead
   helper and its test so `clippy -D warnings` stays green — do not leave an
   unused function.

5. Add tests in `ui.rs`:

   ```rust
   #[test]
   fn gantt_positions_bars_by_time() { /* gantt_bar_cols with span [t0, t0+100s], width=100; a task [t0+20s, t0+50s] => (20, 50) (±rounding); assert exact start/end cols */ }
   #[test]
   fn pending_task_has_no_solid_bar() { /* run w/ one started task and one task started_at=None; render Timeline; assert the pending task's row contains no '█' cell (ghost only) */ }
   #[test]
   fn empty_span_shows_placeholder() { /* run whose every task has started_at=None; render Timeline; assert "No timing yet" appears and render does not panic */ }
   ```

- **Depends on:** plumb-task-timestamps
- **Done when:** the three tests pass; the Timeline view renders one labelled,
  state-coloured bar per task scaled to its real `started_at`/`finished_at` window
  across `inner.width`; a still-running task's bar extends to the current time; a
  not-yet-started task shows a ghost slot with no solid bar; a run with no started
  tasks shows the "No timing yet" placeholder instead of panicking; the geometry
  helper reads no clock; cargo test/clippy/fmt green.

---

**End of plan 0022 TASKS.** When every "Done when" bullet is green, the Timeline
view is a real Gantt: each task is a bar positioned and sized by its actual
wall-clock window — overlap, duration, and live progress are visible at a glance —
instead of a row of badges bucketed by dependency level.
