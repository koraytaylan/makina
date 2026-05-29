# Scope — Plan 0002

> What this plan delivers, what it deliberately leaves out, and why.

## Why this plan

The MVP trial's summary judgment
([`trial-findings.md`](../../trial/trial-findings.md) §5):

> The two most important findings are the ACP permission gap and the missing
> `.tasks/{slug}.json` persistence. The ACP gap is the concrete, operational
> form of the governance problem the project exists to solve; the persistence
> gap means the VISION "tracked artifact / recoverable / diff-reviewable"
> principle is completely unmet at runtime.

Plan 0002 closes both, and pays down the two cheap debts the trial noted in
passing. Three workstreams:

1. **Cleanups** — a dedicated `MergeConflict` FSM event (stop overloading
   `ReviewCapReached`), and dedup the duplicated `extract_json_object` helper.
2. **Minimal viable governance** — intercept the ACP `session/request_permission`
   request, apply a worktree-scoped auto-allow policy, reply, and write an
   audit-log entry per decision. This is the action gateway at its smallest
   useful slice; it removes the `--yolo` workaround.
3. **Task-graph persistence** — the Supervisor writes `.tasks/{slug}.json` on
   every FSM transition and reads it back on `OpenRun`, with a simple
   crash-resume recovery rule.

## In scope

Exactly the tasks in [TASKS.md](TASKS.md) (sections 0009–0011). Nothing more.

## Out of scope — deferred to [FUTURE.md](../0001-Initial/FUTURE.md)

These are real directions the trial also motivated, intentionally not in this
increment:

- **Hard enforcement via sandboxing** — the gateway's "teeth". This plan's
  gateway is declarative-audit + auto-allow; it does not path-confine or
  network-restrict the agent. Sandboxing is the harder, platform-specific
  problem (Linux-first) and is scheduled after the gateway exists.
- **Full policy engine** — declared role-scoped rules, per-action policy,
  pluggable evaluation. This plan ships one deterministic rule behind a trait
  seam (`PermissionPolicy`) so the engine can replace it later without touching
  transport/client.
- **Hang detection**, **cost accounting / budget caps**, **richer reviewer-side
  rule kinds**, **agent-driven conflict reconciliation**, **multiple task
  sources** — all ranked lower than the two correctness gaps in the trial.
- **Auto-commit of `.tasks/`** and **WAL/replay crash recovery** — this plan
  writes the artifact but does not auto-commit it, and recovers via a simple
  state-reset rule rather than a write-ahead log.

## Mapping

| Workstream | Trial finding | FUTURE direction | VISION principle |
|------------|---------------|------------------|------------------|
| Minimal viable governance | §2 "ACP permission flow"; §4 item 1 | "Deterministic governance" (gateway + policy + audit) | Governance wedge; "Agents are external processes" |
| Task-graph persistence | §2 "`.tasks/{slug}.json` persistence never written"; §4 item 2 | "Crash recovery" (best-effort slice) | "Work state is a tracked artifact … reviewable via diff, recoverable from history" |
| `MergeConflict` event | §2 "FSM failure modeling — `ReviewCapReached` overloaded" | "State machine extensions" | "Failures are scoped" (failure cause stays inspectable) |
| Dedup `extract_json_object` | §2 "`extract_json_object` helper duplicated" | — | Maximalist core (no needless duplication) |

## Locked decisions

Settled during design; detail and rationale in [ARCHITECTURE.md](ARCHITECTURE.md).

- **Gateway policy:** worktree-scoped auto-allow, selecting the offered
  `allow_once` option (least privilege over `allow_always`).
- **Audit ledger:** per-run append-only JSONL at `.tasks/{slug}/audit.jsonl`,
  written by a **Supervisor-owned** sink so the "Supervisor is the only
  `.tasks/` writer" invariant still holds.
- **Policy home:** `makina-acp` for now (operates on ACP wire types); the
  `PermissionPolicy` trait is the migration seam to a future core-level engine.
- **Persistence write:** clone the graph under the lock, serialize + atomic
  temp-then-rename outside the lock; failures are best-effort (logged, never
  corrupt an in-memory run).
- **Resume:** reset non-terminal (`in-progress`/`in-review`) tasks to `ready`;
  preserve terminal states and cumulative iteration counters. The JSON artifact
  wins over an edited `.md` (it is the source of truth once emitted).
- **Git:** write-only this increment — `.tasks/{slug}.json` is written but not
  auto-committed; `.gitignore` already keeps `/.worktrees/` ignored and
  `.tasks/` tracked (verify only).

## Open question resolved first

One empirical unknown gates the gateway's `ClientCapabilities` literal: does
real `gemini --acp` emit `session/request_permission` with the current empty
capabilities, or must an `fs` capability be advertised? The first governance
task (`verify-permission-trigger`) answers it before the wire types are fixed.
The rest of the gateway design is unaffected either way.
