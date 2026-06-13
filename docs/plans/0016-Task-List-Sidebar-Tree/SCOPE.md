# Scope — Plan 0016

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

The left sidebar is the app's primary navigation surface, but today it shows
only a flat **list of open runs** (`ui.rs` sidebar render, one `ListItem` per
`RunView`). A run's *tasks* — the thing the user actually watches — live in a
separate table crammed into the top of the main panel (`ui.rs`, capped at ~10
rows). The result: when one or two runs are open, the 30%-wide sidebar is mostly
**empty wasted space**, while the main panel's task table competes for room with
the exchange pane that the user came to read.

Two concrete problems:

1. **Sidebar space is wasted.** A 20+ row sidebar renders 1–3 run rows and
   nothing else; there is no use for the rest of the column.
2. **Tasks are buried in the main panel.** The task table (`ui.rs` main split)
   eats fixed vertical space (up to 10 rows + header) that pushes the exchange
   pane down, and it can't show all tasks of a large plan at once.

This plan turns the sidebar into a **"Runs & Tasks" tree**: each open run is an
expandable parent node, with its tasks nested beneath it. The task table is
removed from the main panel, which frees that space for a **larger exchange
pane**. Navigation, expand/collapse, and focus all happen in one tree.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0052–0054):

- **0052 — Sidebar tree state model.** Add per-run expand/collapse state and a
  unified *tree cursor* (a flattened walk over visible nodes — run rows plus the
  tasks of expanded runs) to `App`, deriving the existing `selected_run` /
  `selected_task` from it. Pure, unit-testable navigation logic.
- **0053 — Render the sidebar tree.** Replace the flat "Runs" list with a "Runs &
  Tasks" tree (runs as `▾`/`▸` parents, tasks nested with the existing state
  badge and plan-0014 failure label). **Remove the task table from the main
  panel** and let the exchange pane grow into the freed space.
- **0054 — Tree navigation & keys.** `Up`/`Down` walk visible nodes; `Space`
  toggles expand/collapse on a focused run node; `Tab` still toggles
  Sidebar↔Main focus; focusing a task node drives the main detail/exchange.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Sidebar shows only runs; rest of the column is wasted | `0052`, `0053` |
| Tasks are buried in a fixed-height main-panel table | `0053` |
| No way to navigate/expand tasks within the sidebar | `0052`, `0054` |

## Locked decisions

- **Tree, not two stacked lists.** Runs are expandable parents; tasks nest under
  the selected/expanded run. Runs default to **expanded** so a single-run session
  shows its task list immediately.
- **The task table leaves the main panel.** The main panel becomes header →
  detail (`gate ×n · review ×m`, failure reason, idle/countdown) → **exchange
  (larger)** → error pane. The run-level **ingestion-issues pane stays** in the
  main panel (it is per-run, shown only when a run has issues).
- **One cursor, derived selection.** A single tree cursor over the flattened
  visible-node list is the source of truth; `selected_run` and `selected_task`
  are *derived* from it so existing exchange-loading and detail-rendering keep
  working unchanged.
- **Reuse existing helpers.** Rows reuse `task_state_badge`, `failure_kind_label`,
  `status_badge` (for the run parent), `panel_block`, and `run_label`. No new
  badge vocabulary.
- **No data-model change in `makina-core`.** This is a TUI-only plan; `RunView` /
  `TaskView` are unchanged. Expand/collapse and cursor state live entirely on
  `App`.

## Out of scope

- Retrying failed tasks and re-dispatch (plan 0017; this plan only renders state).
- Reordering, filtering, or searching tasks in the tree.
- Horizontal resize of the 30/70 split or making it configurable.
- Persisting expand/collapse state across restarts.
- Any change to how exchanges are streamed or stored.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
