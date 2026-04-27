# ADR-015: Supervisor transition ordering — persist, emit, then act

## Status

Accepted (2026-04-26).

## Context

Wave 3 (issue #12) ships the `TaskSupervisor` skeleton — a per-task FSM that walks an issue from
`INIT` through `MERGED` (or one of the terminal failure states). On every transition the supervisor
has three responsibilities, and the order in which they fire is load-bearing across daemon restarts,
TUI scrollback, and integration-test reconstruction:

1. **Persist** the new `Task` record through `Persistence.save()` so a crash mid-transition leaves a
   coherent on-disk view.
2. **Emit** a `state-changed` `TaskEvent` on the `EventBus` so subscribers (the TUI, the daemon
   server's IPC fan-out, and integration tests) observe the transition in real time.
3. **Act** — perform the observable side effect attached to the destination state (create a
   worktree, open a pull request, request reviewers, merge).

The wrong ordering produces visible bugs:

- **Act, then persist** loses the side effect on crash. The daemon comes back up, sees the task in
  the _prior_ state, and either retries the side effect (creating a duplicate worktree, opening a
  duplicate PR) or stalls because the FSM cannot tell whether the side effect already fired.
- **Emit before persist** lets a TUI subscriber render a state the store does not know about. A
  daemon restart then "rolls back" the visible state from the operator's point of view, which is
  worse than never showing the transition in the first place.
- **Emit, then act, then persist** combines both failure modes.

The Wave 4 stabilize loop adds further pressure: the conversations phase resolves review threads on
GitHub _only_ after the supervisor has re-published a `state-changed` event marking the phase.
Without a monotonic ordering, a flapping supervisor can resolve review threads on GitHub that the
persisted store still considers active, and the next daemon boot opens a new run that believes the
conversation is unresolved.

## Decision

The supervisor's `transition()` helper (`src/daemon/supervisor.ts`) is the single point of state
mutation. It enforces the order **persist, emit, then act**:

1. `await opts.persistence.save(updated)` — the new `Task` is durably on disk before anyone else
   sees it.
2. `tasks.set(updated.id, updated)` — the in-memory table reflects the same record.
3. `bus.publish({ kind: "state-changed", … })` — subscribers observe the transition.
4. The caller of `transition()` returns to the FSM `drive()` loop, which performs the destination
   state's side effect on the _next_ iteration (or, in compound states such as `PR_OPEN`, in a
   dedicated follow-up call).

The "act" step lives outside `transition()` on purpose: it is the destination state's
responsibility, not the transition mechanism's. Compound states (`PR_OPEN` → reviewer request →
`STABILIZING`) walk the FSM through two transitions so the persisted record carries every
intermediate observable a TUI subscriber may have rendered.

`SupervisorError` is the only exception type the supervisor throws to its caller, and it never fires
after the FSM has accepted a task: `start()` validates the duplicate-task invariant and mints the
task id synchronously before any `await`, so a duplicate-start either rejects with `SupervisorError`
_before_ a transition is persisted or runs to completion. FSM-internal failures (worktree-clone,
PR-create, merge) are absorbed into a `FAILED` transition with the underlying error captured on
`terminalReason`.

The `STABILIZING` sub-phases (`REBASE`, `CI`, `CONVERSATIONS`) ship as stubs in Wave 3: each phase
publishes its `state-changed` event with the matching `stabilizePhase` so the bus observes the
expected phase timeline, and the loop falls through to `READY_TO_MERGE` immediately. Wave 4 replaces
the stubs with the real per-phase logic; the persist-then-emit-then-act ordering is the contract
every phase honors.

## Consequences

- **Crash safety.** A `kill -9` between `persistence.save()` and the side effect leaves the task in
  the new state on disk; the next daemon boot replays the side effect against a record that already
  documents the destination, and the FSM idempotently resumes.
- **Subscriber consistency.** TUI scrollback never shows a state the store does not have.
  Persistence replay reconstructs the task identically to the in-memory snapshot returned by
  `start()` — the integration test in `tests/integration/supervisor_end_to_end_test.ts` asserts this
  by deep-equal comparing `persistence.loadAll()[0]` with the supervisor's `start()` result.
- **Side-effect coupling.** The Wave 4 GitHub-side actions (resolve review thread, re-request
  Copilot review) can rely on the bus event having been emitted and persisted before they fire. No
  extra synchronization is needed.
- **Cost.** Every transition is one synchronous disk write (atomic via the rename in
  `persistence.ts`). For a happy-path task this is ~12 writes; well below the IO budget for
  agent-paced workloads.

## Alternatives considered

- **Single combined "transition + side-effect" entry point.** Rejected because compound destination
  states (`PR_OPEN` → reviewer request → `STABILIZING`) need two persisted events. Bundling them
  into one helper would either combine multiple `state-changed` events into one envelope (breaking
  the bus's per-event ordering contract) or hide the intermediate state from subscribers (regressing
  on the consistency guarantee above).
- **Emit-then-persist with at-least-once replay.** Rejected because it requires every subscriber to
  be idempotent against re-deliveries of an event the daemon eventually drops on a crash.
  Subscribers (the TUI, the daemon server) are not idempotent today and adding the requirement would
  broaden the contract for marginal gains.

## References

- `src/daemon/supervisor.ts` — `transition()` and the `drive()` dispatch.
- `tests/integration/supervisor_end_to_end_test.ts` — the persistence-replay assertion.
- ADR-014 (Persistence durability protocol) — atomic-write semantics this ordering relies on.
