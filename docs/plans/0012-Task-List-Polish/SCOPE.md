# Scope — Plan 0012

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Two small, self-contained defects in the task-list view (`crates/makina/src/ui.rs`)
make it noisier and less discoverable than it should be:

1. **The "G" and "R" columns mean nothing to a reader.** They show
   `gate_iterations` and `review_iterations` — internal retry counters — as bare
   numbers in the task table. They read as cryptic noise; the legend that
   explains them only appears once a count is non-zero.

2. **The dependency views are invisible.** Plan 0003 fully built `List`, `Tree`,
   and `Timeline` dependency views (`DependencyViewMode` in `app.rs`, cycled by
   the `v` key, rendered in `ui.rs`). They work and are tested — but **nothing in
   the UI hints they exist**: the status bar lists `[o]`, `[s/p/c]`, `[Tab]`,
   `[q]` but never `[v]`, and there is no indicator of which view is active. A
   feature you planned and built is effectively hidden.

This plan removes the noise and surfaces the hidden feature. It is deliberately
tiny and independent of the other 0009–0011 plans.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0042–0043):

- **0042 — Remove G/R columns.** Drop the two columns and their conditional
  legend from the task table; relocate the gate/review iteration counts into the
  task **detail** so the information is still available, just not cluttering the
  table. The underlying `TaskView` fields stay (used by detail and by plan 0010's
  persistence).
- **0043 — Make dependency views discoverable.** Add a `[v]` hint to the status
  bar and a current-view indicator (`Off` / `List` / `Tree` / `Timeline`) so the
  existing views are findable, with a small header label on the dependency
  sub-pane.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| "G" and "R" columns are meaningless and should go away | `0042` |
| Planned timeline/tree views exist but the UI gives no hint | `0043` |

## Locked decisions

- **Remove from the table, keep the data.** The G/R columns leave the table, but
  `gate_iterations`/`review_iterations` are shown in the task detail (e.g.
  `gate ×2 · review ×1`) so nothing is lost and the counters remain visible when
  relevant.
- **Don't touch the view rendering.** The `List`/`Tree`/`Timeline` renderers and
  the `v` keybinding work and are out of scope to change — 0043 only adds
  discoverability (hint + indicator + sub-pane title).
- **Coordinate hotkeys.** The status-bar additions here and plan 0011's editor
  hotkey must not collide; 0012 owns the status-bar string and reserves keys.

## Out of scope

- New view modes, table redesign, or sorting/filtering of the task list.
- Any change to the dependency-graph data model or layout algorithms.
- Exchange pane, persistence, and provider config (plans 0009–0011).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
