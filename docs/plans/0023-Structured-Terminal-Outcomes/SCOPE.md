# Scope — Plan 0023

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

A full-codebase review (2026-06-11) found that the engine does not *know* why
a task failed — it **guesses**. Drivers return `Ok(TaskState)` plus stringly
errors, so the scheduler infers the failure reason from iteration counters
(`supervisor.rs:1248–1258`):

```rust
let reviewed = review_iterations_locked(&graph, &id).unwrap_or(0) > 0;
let reason = if reviewed {
    "review-cap-reached".to_string()
} else {
    "gate-cap-reached".to_string()
};
```

That inference is structurally lossy. Every `Ok(TaskState::Failed)` from a
driver — gate cap (`supervisor.rs:1677`), review cap (`supervisor.rs:1874`),
**and** merge conflict (`supervisor.rs:1819`) — lands in this one arm, so:

- A task failing via `MergeConflict` on its *first* approval has
  `review_iterations == 0` (only rejections increment it) → recorded as
  `"gate-cap-reached"`, even though the FSM applied the dedicated
  `MergeConflict` event (`supervisor.rs:1802`; `state_machine.rs:135,228`).
- A task rejected once that later exhausts the **gate** cap has
  `review_iterations == 1` → recorded as `"review-cap-reached"`.

Worse, the one datum the documented agent-reconciliation seam needs is thrown
away at the source: the merge-conflict arm does `let _ = details;`
(`supervisor.rs:1799`), discarding git's conflict output
(`merge.rs:144–148`) that the merger preserved precisely "for handing to an
agent-driven reconciliation step".

The root cause is stringly-typed plumbing on the whole reporting channel:
`DevelopAck = Result<DevelopOutcome, String>` (`developer.rs:182`),
`ReviewReply = Result<ReviewVerdict, String>` (`reviewer.rs:159`),
`task_driver(…) -> Result<TaskState, String>` (`supervisor.rs:1539`), the
scheduler's `JoinSet<(TaskId, Option<Result<TaskState, String>>)>`
(`supervisor.rs:1061`), and
`RunReport.failed_tasks: Vec<(TaskId, String)>` (`supervisor.rs:377`). A
`TaskState` carries no cause; a `String` carries no structure; so the
scheduler reconstructs causes from counters and synthesized literals.

This is the **engine-level complement to plan 0014**, which surfaces failure
reasons at the *view* layer and explicitly deferred this work: its scope
classifies "at the point of failure … even if the FSM event stays shared",
and its out-of-scope list names the engine-level split
(`doc/plan/0014-Failure-Reasons-and-Log-Access/SCOPE.md`, "Locked decisions" /
"Out of scope"). This plan is that deferred engine work: the driver →
scheduler → report channel becomes typed, so 0014's view mapping becomes a
direct conversion instead of string classification. (0014 is not yet
implemented — no `FailureReason` exists in the crates — see the interaction
note in [ARCHITECTURE.md](ARCHITECTURE.md).)

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0072–0073):

- **0072 — `TerminalOutcome` type.** `task_driver` returns
  `Ok(TerminalOutcome { state: TaskState, cause: TerminalCause })` with
  `TerminalCause = Completed | GateCap | ReviewCap | MergeConflict { details }
  | WallClock | Cancelled | HardError { message }`. The scheduler stops
  counter-guessing: driver arms state their own cause; the scheduler — which
  alone observes timeouts, hard-error joins, and aborts — wraps `WallClock` /
  `HardError` / `Cancelled`. `RunReport.failed_tasks` carries the typed cause,
  with a `Display` impl reproducing today's literal strings.
- **0073 — Thread the cause to consumers.** The per-task transition log
  records carry the cause, merge-conflict `details` finally reach `RunReport`
  (proven end-to-end), string-asserting tests assert on causes, and plan
  0014's `FailureReason` mapping becomes a direct `From<TerminalCause>` —
  whichever plan lands first.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Scheduler infers reasons from counters; merge conflicts mislabeled `gate-cap-reached`, gate caps mislabeled `review-cap-reached` | `0072` |
| Merge-conflict `details` discarded (`let _ = details;`) — the reconciliation seam's one input never leaves the driver | `0072`, `0073` |
| Stringly-typed driver/scheduler channel makes causes unrepresentable | `0072` |
| `RunReport.failed_tasks: Vec<(TaskId, String)>` forces 0014 into string classification | `0072`, `0073` |

## Locked decisions

- **Causes are constructed where the truth is known.** Driver arms produce
  `Completed` / `GateCap` / `ReviewCap` / `MergeConflict { details }` at the
  exact site that applies the matching FSM event; the scheduler wraps
  `WallClock` (timeout arm), `HardError` (driver `Err` arm), and `Cancelled`
  (abort bookkeeping). Nothing, anywhere, infers a cause from
  `review_iterations`.
- **The FSM is untouched.** `TerminalCause` is a *reporting* type on the
  driver → scheduler → report channel. The event set
  (`state_machine.rs:68` — including the dedicated `MergeConflict` event
  added by plan 0002) and every transition stay exactly as they are.
- **Hard errors stay `Err` at the driver boundary.** `task_driver` returns
  `Result<TerminalOutcome, String>`; its four hard-error paths keep returning
  `Err(String)` (the scheduler's `Err` arm already reads the terminal state
  back from the graph, `supervisor.rs:1277–1284`) and the scheduler wraps the
  message into `TerminalCause::HardError`. This keeps the change surface to
  the `Ok` channel where the guessing happens.
- **One typed field, `Display` for back-compat.** `failed_tasks` becomes
  `Vec<(TaskId, TerminalCause)>` — not a dual string+enum representation.
  `Display for TerminalCause` reproduces today's literals
  (`"gate-cap-reached"`, `"review-cap-reached"`, `"wall-clock-cap-reached"`,
  the hard-error message), so log lines and reason-string consumers keep
  their exact wording.

## Out of scope

- New FSM states or events — the FSM's event set is untouched; only the
  driver/scheduler reporting channel is typed.
- Agent-driven reconciliation itself (this plan delivers the conflict
  `details` to the seam; acting on them is future work).
- UI rendering of causes (plan 0014 owns failure UX; this plan hands it exact
  data).
- Run-level lifecycle states and cancel/resume correctness (plan 0020).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
