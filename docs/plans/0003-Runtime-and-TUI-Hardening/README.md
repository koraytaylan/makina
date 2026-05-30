# Plan 0003 — Runtime & TUI Hardening

The second post-MVP plan. It addresses the findings from the **plan-0002
dogfood run** (driving the plan-0002 task list through the TUI with a real
agent): the TUI dumped raw errors over the frame, the exchange pane garbled
agent diffs, a single task failure halted the whole run, parallelism wasn't
visible, and runtime state was scattered at the repo root with no run logs.

It does five things, in dependency order:

1. **Unify runtime state under `.makina/`** — relocate config, the task-graph
   artifact, worktrees, and the audit ledger into one repo-local `.makina/`
   home, introduce a persistent **run id**, and split committed vs. transient
   with an internal `.gitignore`.
2. **Add a logging subsystem** — a real `tracing` subscriber writing per-run /
   per-task logs under `.makina/runs/{run-id}/`, plus an in-frame collapsible
   error/log pane so nothing bypasses the TUI.
3. **Make the scheduler fail gracefully** — a task failure no longer stops the
   run; independent ready tasks keep going and the failed task's dependents are
   marked `Skipped` rather than left dangling.
4. **Polish the TUI** — ANSI/diff-aware exchange rendering, mouse-wheel
   scrolling, a meaningful run label, a `G`/`R` legend, and a
   list/tree/timeline dependency view (the timeline doubles as parallelism
   observability).
5. **Harden the run lifecycle** — guarantee no orphaned agent processes when the
   app quits, plan-scope the worktree/branch namespace, and let `create` reclaim
   its own stale worktrees so an interrupted run never poisons the next one.

## Sections

- [SCOPE.md](SCOPE.md) — what's in and out, the locked decisions, and how each
  workstream maps to the dogfood findings and the VISION principles.
- [ARCHITECTURE.md](ARCHITECTURE.md) — the concrete design deltas to plan-0001
  / plan-0002, with file-level seams (re-grounded against the just-merged
  plan-0002 code) and the decisions/open-questions record.
- [TASKS.md](TASKS.md) — the structured-text task list (sections 0012–0015),
  the executable spec; also a dogfood input Makina can build.

## Relationship to prior plans

Plan 0001 (`../0001-Initial/`) holds the stable VISION/ARCHITECTURE; plan 0002
(`../0002-Governance-and-Persistence/`) delivered the governance gateway and
task-graph persistence. Plan 0003 hardens what 0002 produced — notably it
**relocates** 0002's `.tasks/{slug}.json` persistence and `.tasks/{slug}/audit.jsonl`
ledger (now working code) under `.makina/`, and folds in two follow-ups the
plan-0002 final review flagged (async audit write, registry eviction).

## Status

Draft. Authored from the plan-0002 dogfood findings; design decisions were
settled in brainstorming and the file-level seams re-grounded post-merge.
