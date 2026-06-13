# Scope — Plan 0022

> Turn the dependency "Timeline" view from a lane diagram of scheduling levels
> into a real time-based Gantt chart, scaled to each task's wall-clock window.

## Why this plan

The dependency pane has a `DependencyViewMode::Timeline` mode
(`crates/makina/src/ui.rs`, the `Timeline` arm of `render_dependency_view`). Its
name promises a *timeline*, but it does not draw time at all. It calls
`dependency_levels(&run.tasks)` to bucket tasks by their **longest-path
dependency level** and renders each level as a row of side-by-side `"[state]
id"` badges. The result, as the feature report (#5) puts it, "looks like task
titles dumped next to each other": there is no time axis, no scale, and no way
to see when a task started, how long it ran, or which tasks overlapped in real
wall-clock time.

The data to do better already exists in the domain. `Task`
(`crates/makina-core/src/task.rs`) carries `started_at: Option<DateTime<Utc>>`
and `finished_at: Option<DateTime<Utc>>` (set when a Developer first picks the
task up and when it reaches a terminal state). But there is a **plumbing gap**:
the orchestrator's `TaskView` conversion (`crates/makina-core/src/
orchestrator.rs`, the `.map(|task| TaskView { … })` block) drops those two
fields, and `TaskView` (`crates/makina-core/src/api.rs`) has no slot for them at
all. The persisted `TaskSnapshot` (`crates/makina-core/src/run_metadata.rs`)
also omits them, so a *reopened* finished run could not be drawn on a time axis
even if the live view carried them.

So a true Gantt requires two paired pieces of work:

1. **Plumb the timestamps into the view (and persistence).** `TaskView` must
   carry `started_at`/`finished_at`; the orchestrator conversion must populate
   them; `TaskSnapshot` must persist them (additively, so old `run.json` files
   still load) and feed them back into the reconstructed `TaskView` for disk runs.
2. **Render a Gantt.** Rewrite the `Timeline` arm to compute the run's wall-clock
   span and draw one row per task — a short label plus a horizontal bar from the
   task's start offset to its end offset, scaled to the inner pane width and
   coloured by state — with sane handling of still-running and not-yet-started
   tasks and the degenerate "nothing has started" case.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0067–0068):

- **0067 — Plumb task timestamps to the view.** Add
  `started_at`/`finished_at` (`Option<DateTime<Utc>>`) to `TaskView`; populate
  them from `Task` in the orchestrator's `TaskView` conversion; add them to
  `TaskSnapshot` with `#[serde(default)]` so old snapshots still deserialise;
  write them at finalization and read them back into the reconstructed `TaskView`
  in `run_view_from_metadata` so the Gantt works on reopened runs too.
- **0068 — Render the Gantt.** Rewrite the `DependencyViewMode::Timeline` arm of
  `render_dependency_view` into a time-scaled Gantt: compute the run span, draw
  one labelled bar per task scaled to `inner.width`, colour by state, show
  not-yet-started tasks as a ghost slot, and show a friendly placeholder for a
  run with no timing yet. Read "now" at render in the TUI; keep the bar geometry
  in a pure, time-injected helper (no `Utc::now()` inside it).

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Timeline view renders dependency *levels*, not time — no axis, no scale | `0068` |
| `TaskView` has no timestamps; orchestrator conversion drops `started_at`/`finished_at` | `0067` |
| `TaskSnapshot` omits timestamps, so reopened runs can't be drawn on a time axis | `0067` |
| Still-running and not-yet-started tasks have no sensible time representation | `0068` |

## Locked decisions

- **Plumb before render.** 0068 depends on 0067: the Gantt needs real
  `started_at`/`finished_at` on `TaskView`. No synthesising timestamps in the TUI
  from ticks — use the domain values plumbed through.
- **Additive, back-compatible persistence.** The two new `TaskSnapshot` fields use
  `#[serde(default, skip_serializing_if = "Option::is_none")]`, exactly mirroring
  the existing `failure_reason` field, so pre-existing `run.json` files (which
  have no timestamps) still load and the `old_run_json_without_snapshot_still_loads`
  contract is preserved.
- **The run span comes from real timestamps.** Span start = the minimum
  `started_at` over tasks that have started; span end = the maximum of every
  `finished_at`, widened to the current time when at least one task is still
  running (so a live bar grows toward "now"). "Now" is read **at render** in the
  TUI and passed into a pure geometry helper — the helper never calls a clock, so
  it stays deterministically testable (the project's `no Date::now in pure
  helpers` rule).
- **Colour by state, reuse the badge vocabulary.** Bars are coloured from the same
  `task_state_badge` colour mapping already used everywhere (`Done` cyan,
  `InProgress` green, `Failed` red, …). A not-yet-started task (`started_at` is
  `None`) renders as an empty/ghost slot rather than a solid bar.
- **Degenerate span is a friendly line, not a panic.** If no task has started (the
  span is empty or zero-width), the arm renders a single dim "no timing yet" line
  instead of dividing by zero or drawing garbage.
- **TUI gains a `chrono` dependency.** Because `TaskView` now carries
  `DateTime<Utc>` and the Gantt reads `Utc::now()` at render, the `makina` crate
  adds `chrono` (already a workspace dependency used by `makina-core`). The pure
  geometry helper itself takes plain `DateTime<Utc>` arguments and returns column
  indices; it does not touch the clock.

## Out of scope

- Changing how `started_at`/`finished_at` are *set* on the domain `Task` (the
  supervisor already sets them; this plan only reads/plumbs/persists them).
- A scrollable/zoomable time axis, tick labels, or a configurable axis range — the
  Gantt fills the available `inner` width for the whole run span, no panning.
- The `List` and `Tree` dependency modes (unchanged) and the mode-cycling keybind.
- Persisting per-task worktree paths or any new field beyond the two timestamps.
- A relative-time legend / "x ago" formatting beyond what's needed for the bar.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
