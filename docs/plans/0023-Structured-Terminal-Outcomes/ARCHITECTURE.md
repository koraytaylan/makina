# Architecture — Plan 0023 (deltas)

> Edits live almost entirely in `crates/makina-core/src/actors/supervisor.rs`
> (types + `task_driver` + `scheduler`), with assertion updates in
> `crates/makina-core/tests/continue_on_failure.rs`. Line numbers are hints;
> locate by symbol.

## 0072 — `TerminalOutcome` type

Today (`task_driver`, `supervisor.rs:1539–1911`):

- `Ok` exits return a bare `TaskState`: `Done` after a clean merge
  (`supervisor.rs:1786`), `Failed` for the gate cap (`supervisor.rs:1677`),
  the merge conflict (`supervisor.rs:1819`, after `let _ = details;` at
  `supervisor.rs:1799`), and the review cap (`supervisor.rs:1874`).
- `Err(String)` exits cover worktree-create failure (`supervisor.rs:1612`),
  develop hard errors (`supervisor.rs:1684`), reviewer dispatch failure
  (`supervisor.rs:1719`), and hard merge errors (`supervisor.rs:1768`).
- The scheduler then *guesses*: its `Ok(Failed)` arm derives the reason from
  `review_iterations` (`supervisor.rs:1248–1258`), its `Err` arm records the
  raw string (`supervisor.rs:1299`), its timeout arm synthesizes
  `"wall-clock-cap-reached"` (`supervisor.rs:1347`), and a fill-phase
  `advance_to_ready` failure records `e.to_string()` (`supervisor.rs:1150`).

Edits:

- **New types** in `supervisor.rs` near `RunReport` (`supervisor.rs:368`):

  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum TerminalCause {
      Completed,
      GateCap,
      ReviewCap,
      MergeConflict { details: String },
      WallClock,
      Cancelled,
      HardError { message: String },
  }

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct TerminalOutcome { pub state: TaskState, pub cause: TerminalCause }

  impl fmt::Display for TerminalCause { /* today's literals:
      GateCap → "gate-cap-reached", ReviewCap → "review-cap-reached",
      WallClock → "wall-clock-cap-reached", MergeConflict → "merge-conflict",
      HardError → the message, Cancelled → "cancelled", Completed → "completed" */ }
  ```

- **`task_driver` returns `Result<TerminalOutcome, String>`**
  (`supervisor.rs:1539`). Each `Ok` arm states its cause at the site that
  applies the FSM event: `{ Done, Completed }` (`supervisor.rs:1786`);
  `{ Failed, GateCap }` (`supervisor.rs:1677`); `{ Failed, ReviewCap }`
  (`supervisor.rs:1874`); and the conflict arm replaces `let _ = details;`
  (`supervisor.rs:1799`) with `{ Failed, MergeConflict { details } }` —
  carrying the git output the merger preserved (`merge.rs:144–148`). The
  `terminal_state` local becomes a `TerminalOutcome`. `Err` paths are
  unchanged.

- **The scheduler stops guessing.** The `JoinSet` item type
  (`supervisor.rs:1061`) becomes
  `(TaskId, Option<Result<TerminalOutcome, String>>)`; the local
  `failed_tasks` (`supervisor.rs:1075`) and
  `RunReport.failed_tasks` (`supervisor.rs:377`) become
  `Vec<(TaskId, TerminalCause)>`. Arm by arm:
  - `Ok(Ok(outcome))` (`supervisor.rs:1234–1269`): delete the
    `review_iterations` inference (`supervisor.rs:1248–1258`); on
    `outcome.state == Failed`, push `outcome.cause`. The
    `mark_dependents_skipped` flow is unchanged.
  - `Ok(Err(e))` (`supervisor.rs:1270–1306`): push
    `TerminalCause::HardError { message: e }`.
  - Timeout arm (`supervisor.rs:1307–1353`): push `TerminalCause::WallClock`
    instead of the literal at `supervisor.rs:1347`.
  - Fill-phase `advance_to_ready` failure (`supervisor.rs:1136–1151`): push
    `HardError { message: e.to_string() }`.
  - Aborted-join arm (`supervisor.rs:1355–1364`): still records nothing — but
    `TerminalCause::Cancelled` now exists for it, so cancel-aware recording
    (plan 0020's resume bookkeeping, or 0014's per-task `Cancelled` reason)
    has a typed slot waiting instead of a string convention.

- **`RunReport` docs** (`supervisor.rs:368–378`) updated: the reason is now a
  `TerminalCause`; `Display` yields the legacy literal.

## 0073 — Thread the cause to consumers

- **Transition log records carry the cause.** The terminal
  `tracing::info!(… "task state transition")` emissions in the driver — gate
  cap (`supervisor.rs:1671–1676`), merge conflict (`supervisor.rs:1813–1818`),
  review cap (`supervisor.rs:1868–1873`) — gain a `cause = %outcome.cause`
  field, so per-task logs (`.makina/runs/{run_uid}/logs/{task}.log`) name the
  real cause. The audit trail and run log inherit it through the same
  subscriber (`supervisor_tracing_transitions.rs` guards the shape).

- **String-asserting tests assert causes.** `tests/continue_on_failure.rs`
  asserts non-empty reason strings (`continue_on_failure.rs:416–440,511–534`);
  rewrite those to `matches!(cause, TerminalCause::HardError { .. })` (the
  injected failures are driver hard errors), keeping a `to_string()`
  non-emptiness check so `Display` stays honest.

- **Plan 0014 seam — handle both orderings.** 0014 is *not yet implemented*
  (verified: no `FailureReason`/`FailureKind` in the crates). Its design maps
  `RunReport.failed_tasks` strings into a `FailureKind` at the view layer:
  - if **0023 lands first** (likely): 0014 implements
    `From<TerminalCause> for FailureReason` directly —
    `GateCap → GateCap`, `ReviewCap → ReviewCap`,
    `MergeConflict { details } → MergeConflict` (details become the message),
    `WallClock → WallClockCap`, `HardError { message } → HardError`,
    `Cancelled → Cancelled` — no string classification ever exists;
  - if **0014 lands first**: its string classifier is deleted here and
    replaced by that same `From` impl.
  Record the mapping contract in the `TerminalCause` doc comment so 0014's
  implementer finds it.

- **Conflict details proven end-to-end.** The orchestrator currently discards
  the report (`let _ = run_graph(…)`, `orchestrator.rs:958`) — the run-level
  consumer arrives with 0014. Until then the supervisor write path is the
  consumer of record: an integration test drives a real two-branch conflict
  through `run_graph` and asserts the report's `MergeConflict { details }`
  carries git's non-empty conflict output.

## Test strategy

- `merge_conflict_records_merge_conflict_cause`: drive two tasks that edit
  the same file to a squash-merge conflict (reuse the conflicting-worktree
  setup from `tests/squash_merge.rs`); the conflicted task's cause is
  `MergeConflict { .. }` — **not** `GateCap`, despite
  `review_iterations == 0`.
- `gate_cap_after_review_rejection_records_gate_cap`: a backend whose
  reviewer rejects once and whose gates then never pass; when the gate cap
  fires, the cause is `GateCap` — **not** `ReviewCap`, despite
  `review_iterations == 1`.
- `conflict_details_reach_run_report`: the end-to-end conflict's `details`
  is non-empty and contains git's conflict output.
- `display_matches_legacy_literals`: `Display` of `GateCap` / `ReviewCap` /
  `WallClock` equals the exact pre-plan strings, so log greps and dashboards
  keep working.

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay
green.

## Interaction with prior plans

- **0014 (failure reasons, unimplemented):** this plan is the engine half
  0014 explicitly deferred (its SCOPE locks "reuse the reason the engine
  already has" and rules the engine-level split out of scope). Landed in
  either order, the view mapping ends as `From<TerminalCause>`; see the seam
  note in 0073. Nothing here renders anything.
- **Plan 0020 (sibling from the same review):** run-level lifecycle. 0020's
  `SchedulerExit` is per-run; `TerminalCause` is per-task; no shared code,
  safe in either order. Once 0020's cancel bookkeeping records aborted tasks,
  `TerminalCause::Cancelled` is the type it records into.
- **Plan 0002 lineage:** the dedicated `MergeConflict` FSM event
  (`state_machine.rs:135,228,301`) finally gets *reported* truthfully — the
  FSM itself is untouched.
