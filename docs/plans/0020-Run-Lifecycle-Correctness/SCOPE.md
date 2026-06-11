# Scope — Plan 0020

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

A full-codebase review (2026-06-11) found that the run lifecycle —
start/pause/cancel/finalize — has races and dishonest terminal states. Six
findings, all verified against the code:

1. **Supersede race (HIGH).** `start_run` (`orchestrator.rs:853`) enforces **no
   status precondition**, although `Command::StartRun`'s contract promises
   `InvalidCommand` "if the Run is already running, completed, or failed"
   (`api.rs:302–303`). Re-issuing `StartRun` — the documented resume path —
   takes and cancels the old handle (`orchestrator.rs:882–884`) and installs a
   new one. But the **old** detached background task (spawned at
   `orchestrator.rs:957–973`) is still draining; when it unwinds, its
   `state.finalize_run_status(run)` (`orchestrator.rs:444–535`) reads the
   entry's **current** handle — the *new* one, whose token is not cancelled
   (`orchestrator.rs:453–457`) — so the cancelled-early-return guard
   (`orchestrator.rs:469–471`) checks the wrong run's token. It then overwrites
   `entry.status` with a derived terminal (the new run is `Running`, so the
   `:507` guard passes — usually `Failed`), sets `entry.handle = None`
   (`orchestrator.rs:511`, destroying the new scheduler's control handle —
   `CancelRun` can no longer stop the run), writes `run.json` with `Failed`
   (`orchestrator.rs:518–529`), and evicts the audit registry mid-run
   (`orchestrator.rs:534`). The "defensive" old-handle cancel also aborts
   in-flight agent turns, contradicting the documented pause semantics
   ("In-flight agent turns are allowed to complete", `api.rs:311–312`).
2. **A paused run is finalized as Failed.** Pause only stops the fill phase
   (`supervisor.rs:1113–1114`); when the in-flight drivers drain, the scheduler
   exits and the end-of-run code runs as if terminal: `aggregate_run_status`
   (`supervisor.rs:977–986`) returns `Running` for a paused graph → a spurious
   `RunStatusChanged{Running}` emit (`supervisor.rs:957–965`); then
   `finalize_run_status` derives `Failed` ("all done? no → Failed",
   `orchestrator.rs:476–484`) and **unconditionally** writes `run.json` with
   status `Failed` + `ended_at` for a merely-paused run, clears the handle, and
   evicts the audit registry (`orchestrator.rs:511,518–529,534` — only the
   in-registry status field is protected by the `:507` guard).
3. **Cancel wedges tasks.** On cancel, aborted drivers' joins are dropped with
   no FSM event (`supervisor.rs:1355–1364`, "nothing to record"), leaving tasks
   `InProgress`/`InReview` in the shared graph. A subsequent `StartRun` launches
   a scheduler whose `next_ready_task_id` only picks `New | Ready`
   (`supervisor.rs:1388–1406`), so the wedged tasks are never re-driven and the
   run instantly re-finalizes `Failed`. Only a process-level reopen recovers
   (`persist.rs:265` `recover_for_resume`, applied in `open_run` at
   `orchestrator.rs:669`).
4. **Abort-flag conflation.** The scheduler's cancel arm is gated
   `if ctx.control.cancel.is_cancelled() && !stop_launching`
   (`supervisor.rs:1103–1106`), but a driver **panic** also sets
   `stop_launching = true` (`supervisor.rs:1366–1367`) — so a cancel issued
   *after* a panic never reaches `abort_all()` and silently waits for every
   in-flight agent.
5. **Disk RunId aliasing.** `runs()` assigns disk-loaded `RunView`s ids from a
   *peeked* `next_id` without reserving (`orchestrator.rs:1216`, `load`
   `Relaxed`; assignment in `run_metadata.rs:275–276`). The next `OpenRun`
   allocates the same numeric `RunId` (`orchestrator.rs:384`, `fetch_add`) for a
   live run — a TUI holding a disk view's id then addresses a *different* live
   run. The same disk run also gets different ids on every `runs()` call.
6. **Doc contract drift.** `Command::CancelRun` docs say the run "is removed
   from the orchestrator's open-run set; subsequent queries … return `None`"
   (`api.rs:326–327`), but `CoreApi` keeps the entry and marks it `Failed`
   (`orchestrator.rs:1010–1038`). `PauseRun`'s precondition ("`InvalidCommand`
   if the Run is not currently running", `api.rs:317`) is unenforced — pausing a
   `Pending`/`Completed` run happily sets `Paused` (`orchestrator.rs:983–1001`).

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0062–0064):

- **0062 — Run epochs + enforced preconditions.** Add a monotonically
  increasing `generation: u64` to `RunEntry`; `start_run` increments it and the
  spawned wrapper captures it; `finalize_run_status(run, generation)` no-ops if
  the entry's generation has moved on. Enforce the documented status
  preconditions in `start_run` (only `Pending`/`Paused` may start) and
  `pause_run` (only `Running` may pause). Drop the defensive old-handle cancel.
- **0063 — Honest lifecycle states.** Thread *why the scheduler exited* out of
  `scheduler` (a `Completed | Paused | Cancelled` exit enum), add
  `RunStatus::Cancelled` to `api.rs` (TUI updated compile-minimally), and only
  emit terminal `RunStatusChanged` + write `run.json` on genuine terminal
  exits. Paused runs keep status `Paused`, write no `run.json`, and retain
  their handle. Fix the abort-flag conflation with a separate `aborted` bool.
- **0064 — Resume + identity.** On `StartRun` of a previously-cancelled run,
  apply `recover_for_resume`-equivalent transitions to the in-memory wedged
  tasks so resume actually re-drives them; reserve disk-view `RunId`s through
  the real allocator (memoized `fetch_add`); align the `api.rs` doc contracts
  with the implemented registry semantics.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Stale finalizer clobbers a resumed run; `StartRun` preconditions unenforced; defensive cancel aborts in-flight turns | `0062` |
| Paused run finalized as `Failed`; spurious `RunStatusChanged{Running}` | `0063` |
| Cancel leaves tasks wedged `InProgress`/`InReview`; in-process resume re-finalizes instantly | `0064` |
| Cancel after a panic never aborts in-flight drivers | `0063` |
| Disk-view `RunId`s alias live runs and drift between calls | `0064` |
| `CancelRun`/`PauseRun` docs contradict the implementation | `0062`, `0064` |

## Locked decisions

- **Epochs, not handle identity.** Stale finalizers are detected by comparing a
  captured `generation` against the entry's current one — the *entire* finalize
  (status write, handle clear, `run.json`, audit evict) no-ops on mismatch. We
  do not compare token pointers or handle identity: the handle is exactly the
  thing the race destroys.
- **Drop the defensive old-handle cancel.** With preconditions, `StartRun` is
  only legal on `Pending`/`Paused` (0064 adds `Cancelled`). The one live-overlap
  case left is resume-while-draining: a paused run's old scheduler may still be
  draining in-flight turns. That overlap is *safe without cancelling*: the old
  scheduler's pause flag stays set (it launches nothing new), the new
  scheduler's `next_ready_task_id` never picks the old one's `InProgress`/
  `InReview` tasks, both mutate the graph under the shared async mutex, and the
  old finalizer no-ops via the epoch. Cancelling the old handle would abort
  in-flight turns — exactly what `api.rs:311–312` forbids.
- **Pause is not terminal.** A paused-exit scheduler triggers *no* terminal
  emit, *no* `run.json` write, *no* audit eviction, and *no* handle clear.
  `run.json` is written only on genuine terminal exits (Completed/Failed/
  Cancelled); cross-process pause survival already rides on the persisted task
  artifact (`persist.rs`), not on `run.json`.
- **Cancelled is a first-class status, and cancel keeps the entry.** The
  implemented keep-and-mark behavior is *better* than the documented
  remove-from-set behavior (the TUI can inspect what completed before the
  cancel), so 0064 fixes the **docs**, not the code — and 0063's
  `RunStatus::Cancelled` makes the mark honest instead of `Failed`.
- **Sequencing of the cancel→resume path.** After 0062 (before 0064 lands),
  `StartRun` on a cancelled run is rejected. That temporarily closes today's
  re-start-after-cancel path — which only produced an instant re-finalize to
  `Failed` anyway (finding 3) — and 0064 reopens it properly with in-memory
  task recovery.
- **Disk ids come from the real allocator, memoized.** Disk-view `RunId`s are
  allocated with the same `next_id` `fetch_add` the registry uses (so they can
  never collide with a live run) and memoized per `run_uid` in `CoreState` (so
  the same disk run keeps one id for the whole session). No second id
  namespace: one counter, one uniqueness invariant.

## Out of scope

- Structured per-task failure reasons / typed terminal outcomes (plan 0023 —
  the engine-level reporting channel; this plan is about run-level lifecycle).
- TUI rendering of the new `Cancelled` status beyond the compile-minimum match
  arms (plan 0014 owns failure UX).
- Crash recovery beyond what `persist.rs` already does (`recover_for_resume`
  on reopen is reused, not extended).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
