# Scope — Plan 0014

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

When a task fails, the TUI shows `[✗ failed]` (`ui.rs:1529`) and nothing else.
The user cannot tell *why* from the interface: was it a gate cap, a reviewer cap,
a merge conflict, a hard error, or the wall-clock cap? The information **already
exists** — the supervisor records `(task_id, reason)` for every failed task in
`RunReport.failed_tasks: Vec<(TaskId, String)>` (`supervisor.rs:372`), where the
`String` is a short reason such as a driver hard-error message or a synthesized
cap literal like `"wall-clock-cap-reached"`. That reason simply never reaches the
view layer: `TaskView` (`api.rs:168`) has `gate_iterations` / `review_iterations`
but **no failure-reason field**, so the diagnosis is dropped on the floor.

Two adjacent visibility gaps compound it:

1. **The error pane is undiscoverable.** It toggles on `[e]` (`event.rs:505`) but
   that key is absent from the status-bar hint string (`ui.rs:405`). Errors
   accumulate silently; a user who doesn't read the source never learns the pane
   exists.
2. **Per-task logs are unreachable from the UI.** Detailed diagnostics are
   written to `.makina/runs/{run_id}/logs/{task}.log` (`log.rs`), but there is no
   command to open or tail them — the user must leave the app and hunt through the
   filesystem.

This plan threads the *already-captured* failure reason to the task view, renders
it inline, makes the error pane discoverable, and adds a key to open a task's log.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0047–0049):

- **0047 — Thread the failure reason to `TaskView`.** Add an
  `Option<FailureReason>` (a small enum with a message) to `TaskView`; populate it
  from the reason the supervisor already records on the failing transition, mapped
  to a stable discriminant (`GateCap`, `ReviewCap`, `MergeConflict`, `HardError`,
  `WallClockCap`). Persisted snapshots (plan 0010) carry it too.
- **0048 — Render the failure reason.** Show the reason in the task **detail**
  block (e.g. `failed: gate cap after ×5`) and as a short suffix/tooltip on the
  task row, so a failure is self-explaining without opening any pane.
- **0049 — Error-pane discoverability + open-log.** Add `[e] errors` to the
  status bar with an unseen-error badge, and add `[L]` to open the focused task's
  log file in `$PAGER` (suspend → spawn → restore the terminal).

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Failed tasks show no reason; the captured reason never reaches the UI | `0047`, `0048` |
| `[e]` error pane is undiscoverable; errors accumulate silently | `0049` |
| Per-task log files exist on disk but are unreachable from the UI | `0049` |

## Locked decisions

- **Reuse the reason the engine already has.** 0047 does not invent new failure
  detection; it surfaces `RunReport.failed_tasks` / the per-transition reason that
  already exists. Where the reason is currently a bare `String`, introduce a small
  `FailureReason` enum so the UI can colour/label it, keeping the human message.
- **Disambiguate `ReviewCapReached`.** It is overloaded today (reviewer
  exhaustion *and* merge conflict *and* hard merge error all reach `Failed` via
  it — `supervisor.rs:40,108`). 0047 maps these to distinct `FailureReason`
  discriminants at the point of failure so the *view* can tell them apart, even if
  the FSM event stays shared. (A full FSM event split is a larger change — out of
  scope; only the reason label is split here.)
- **Open logs read-only, via `$PAGER`.** No bespoke log viewer; suspend the TUI,
  spawn `$PAGER` (fallback `less`/`more`), restore on exit — the standard
  terminal-app idiom. The log path is derived, not stored in the view.

## Out of scope

- Splitting the FSM into dedicated `MergeConflict` / `HardError` *events*
  (engine-level refactor; only the *reason label* is disambiguated here).
- A scrollable in-pane log viewer or live log tail (this plan opens the file
  externally; richer in-app viewing is future work).
- Idle/hang detection (plan 0015) and first-run/config guidance (plan 0013).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
