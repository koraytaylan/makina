# Scope — Plan 0017

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

When a task fails, two things happen and **neither is recoverable from the UI**:

1. The task reaches `TaskState::Failed` (terminal).
2. The supervisor runs `mark_dependents_skipped` (`supervisor.rs`), a fixed-point
   sweep that drives **every transitive dependent** to `TaskState::Skipped` (also
   terminal) via `TaskEvent::DependencyFailed`.

The run then aggregates to `RunStatus::Failed` and the one-shot scheduler exits.
`Done`, `Failed`, and `Skipped` are all terminal in the state machine
(`state_machine.rs`), and the `Command` enum (`api.rs`) has **no retry surface**
— only `OpenRun`/`StartRun`/`PauseRun`/`CancelRun`/`ReinterpretRun`, and
`ReinterpretRun` only re-interprets a *Pending* run from its source file. So a
single transient failure (a flaky gate, a one-off hard error) permanently kills
the failed task **and everything downstream of it**, with the only escape being to
close and re-open the whole run — losing all completed work.

This plan adds a **retry** that resets a failed task (and un-skips its cascaded
dependents), clears its failure metadata, recreates its worktree, and
**re-dispatches** — re-entering the scheduler on the live supervisor — so the run
resumes from where it failed without discarding `Done` tasks.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0055–0058):

- **0055 — FSM reset transitions.** Add explicit events out of the terminal
  failure states: `RetryRequested` (`Failed → New`) and `DependencyReset`
  (`Skipped → New`). Keeping them in the state machine (rather than mutating state
  behind its back) keeps the reset auditable and unit-testable.
- **0056 — Retry commands + graph reset.** Add `Command::RetryTask { run, task }`
  and `Command::RetryFailedTasks { run }`. Under the graph lock, reset the target
  task(s) — clear `failure_reason`, zero `gate_iterations`/`review_iterations`,
  clear `finished_at`, `Failed → New` — then un-skip the transitive `Skipped`
  dependents (`Skipped → New`), and re-evaluate `New → Ready` for tasks whose
  dependencies are `Done`. Persist the reset state.
- **0057 — Re-dispatch.** Re-enter the supervisor's scheduler over the now-Ready
  tasks (the supervisor stays in memory after a one-shot run), flip the run
  `Failed → Running`, and let the normal dispatch path recreate worktrees and
  drive the reset tasks to terminal again. Emit the usual lifecycle events.
- **0058 — Retry in the TUI.** A single context-sensitive `[r]` key: on a focused
  **Failed task** retry that task (+ its skipped dependents); on a focused **run**
  node retry *all* failed tasks in that run. No-op with a status message when
  nothing is retryable. Advertise `[r]` in the status bar.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| `Failed`/`Skipped` are terminal with no reset transition | `0055` |
| No `Command` resets a failed task or its skipped dependents | `0056` |
| One-shot scheduler exits; nothing re-dispatches reset tasks | `0057` |
| No UI affordance to recover from a failure | `0058` |

## Locked decisions

- **Reset-in-place + re-enter the live scheduler** (not a fresh sub-run). The
  supervisor remains in memory after a run completes; retry resets the relevant
  task records and re-invokes the scheduler over them. This reuses the existing
  dispatch/worktree/gate machinery and keeps one run identity.
- **Auto re-dispatch.** Retry immediately resumes the run (`Failed → Running`) and
  runs the reset tasks — fewest keystrokes to recover. (A "reset but stay paused"
  variant is explicitly *not* built.)
- **Fresh budget on retry.** Reset zeroes `gate_iterations` and
  `review_iterations` so a retried task gets its full cap again, not the exhausted
  remainder.
- **Un-skip only the cascade of the retried task(s).** Re-running the
  `mark_dependents_skipped` cascade in reverse — a `Skipped` task is reset to
  `New` only if it became `Skipped` because (transitively) of a task being
  retried. Tasks `Skipped` for an unrelated still-failed prerequisite stay
  `Skipped`.
- **No new cancel surface.** This plan does not implement run-control cancel
  (the deferred "task 31" seam stays deferred); it adds only retry.
- **Persistence stays backward-compatible.** Reset rewrites the existing
  `.makina/tasks/*.json` graph and the plan-0010 run snapshot in place; no new
  persisted schema fields beyond what already exists.

## Out of scope

- Run-control pause/cancel implementation (the documented deferred seam).
- Partial / step-level retry (re-running only the reviewer, say); retry restarts
  the task from `New`.
- Automatic retry-on-failure / retry budgets (this is user-initiated only).
- Changing how failures are classified (`FailureKind`) or rendered (plan 0014).
- The sidebar tree itself (plan 0016); 0058 relies on 0016's focused-node concept
  to decide task-vs-run retry, and 0016 lands first.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
