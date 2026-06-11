# Architecture — Plan 0020 (deltas)

> Edits in `crates/makina-core/src/orchestrator.rs`, `crates/makina-core/src/
> actors/supervisor.rs`, `crates/makina-core/src/api.rs`, `crates/makina-core/
> src/run_metadata.rs`, plus compile-minimal TUI touches (`crates/makina/src/
> ui.rs`). Line numbers are hints; locate by symbol.

## 0062 — Run epochs + enforced preconditions

Today `start_run` (`orchestrator.rs:853`) accepts any status, cancels whatever
handle exists (`orchestrator.rs:882–884`), and spawns a wrapper that calls
`finalize_run_status(run)` (`orchestrator.rs:972`) with no notion of *which*
execution it belongs to — so a stale finalizer operates on the superseding
run's entry (`orchestrator.rs:444–535`).

Edits:

- **`RunEntry` gains an epoch** (`orchestrator.rs:238`):

  ```rust
  /// Monotonic execution epoch: bumped by every accepted StartRun.  A
  /// finalizer that captured an older generation must not touch this entry.
  generation: u64,
  ```

  Initialize `0` at every `RunEntry` literal (locate by `RunEntry {` in
  `open_run`).

- **`start_run` enforces the documented precondition and stamps the epoch.**
  After the existing blocked-ingestion check (`orchestrator.rs:864–878`):

  ```rust
  match entry.status {
      RunStatus::Pending | RunStatus::Paused => {}
      ref other => {
          return Err(ApiError::InvalidCommand {
              reason: format!("cannot start: run is {other:?}"),
          })
      }
  }
  entry.generation += 1;
  let generation = entry.generation;
  // Replace — do NOT cancel — any previous handle: a paused run's old
  // scheduler may still be draining in-flight turns, which the pause
  // contract allows to complete (api.rs StartRun/PauseRun docs).
  entry.handle = Some(RunHandle { /* fresh token + flag */ });
  ```

  The `old.cancel.cancel()` at `orchestrator.rs:882–884` is deleted. Why this
  is safe is a locked decision in [SCOPE.md](SCOPE.md): the old scheduler's
  pause flag stays set, `next_ready_task_id` (`supervisor.rs:1388`) never picks
  its `InProgress`/`InReview` tasks, and its finalizer no-ops via the epoch.

- **`finalize_run_status(run, generation)`** (`orchestrator.rs:444`): first
  thing under the registry lock,

  ```rust
  let Some(entry) = runs.get(&run.0) else { return };
  if entry.generation != generation {
      return; // a newer StartRun owns this entry; stale finalizer backs off.
  }
  ```

  The spawned wrapper (`orchestrator.rs:957–973`) captures `generation` and
  passes it through. With the epoch guard in place, the handle-token sniffing
  at `orchestrator.rs:453–457`/`469–471` stays only until 0063 replaces it
  with the scheduler's explicit exit reason.

- **`pause_run` enforces its precondition** (`orchestrator.rs:983`): only a
  `Running` run may pause; anything else returns `ApiError::InvalidCommand`,
  matching `api.rs:317`. (Pause-before-start, exercised by the existing
  `pause_run_sets_paused_and_does_not_complete_then_resume_completes` test at
  `orchestrator.rs:2566`, becomes invalid — the test is updated to pause a
  *running* run, which is the documented semantics.)

## 0063 — Honest lifecycle states

Today the scheduler exits for three different reasons but reports none of
them: `run_graph` emits `aggregate_run_status` unconditionally unless the
token is cancelled (`supervisor.rs:957–965`), which is `Running` for a paused
graph (`supervisor.rs:985`); `finalize_run_status` then derives `Failed` for
that paused run and persists it (`orchestrator.rs:476–484,518–529`). And the
cancel arm's gate `is_cancelled() && !stop_launching` (`supervisor.rs:1103`)
conflates cancel with the panic path (`supervisor.rs:1366–1367`).

Edits:

- **`SchedulerExit` enum** in `supervisor.rs` near `RunReport`
  (`supervisor.rs:368`):

  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum SchedulerExit { Completed, Paused, Cancelled }
  ```

  `scheduler` (`supervisor.rs:1055`) and `run_graph` (`supervisor.rs:829`)
  return `Result<(RunReport, SchedulerExit), String>`. Computed at loop exit:
  `Cancelled` if `ctx.control.cancel.is_cancelled()`; else `Paused` if the
  pause flag is set **and** any task is still `New | Ready` (work remained
  that pause blocked); else `Completed` (a genuine drain — every reachable
  task is terminal). The ask path (`run_ready_tasks`, `supervisor.rs:695`)
  discards the exit to keep its `Reply = Result<RunReport, String>`
  (`supervisor.rs:654`) unchanged.

- **Separate `aborted` bool.** In `scheduler`, replace the overloaded gate at
  `supervisor.rs:1103–1106` with

  ```rust
  if ctx.control.cancel.is_cancelled() && !aborted {
      aborted = true;
      stop_launching = true;
      join_set.abort_all();
  }
  ```

  so a panic-set `stop_launching` (`supervisor.rs:1366–1367`) no longer
  swallows a later cancel's `abort_all()`.

- **`RunStatus::Cancelled`** added to the enum (`api.rs:206–217`).
  `cancel_run` sets and broadcasts `Cancelled` instead of `Failed`
  (`orchestrator.rs:1032,1034–1037`). Exhaustive matches to extend — grep
  confirmed these are the only three in `crates/makina/src`:
  - `status_badge` (`ui.rs:1494`) → `("[⊘]", Color::DarkGray)`;
  - `status_color` (`ui.rs:1505`) → `Color::DarkGray`;
  - `status_label` (`ui.rs:1543`) → `"Cancelled"`.
  Tests asserting `Failed`-on-cancel (e.g.
  `cancel_run_stops_execution_and_cleans_up`, `orchestrator.rs:2416`) assert
  `Cancelled` instead.

- **Emission + finalize keyed on the exit, not token sniffing.** `run_graph`
  emits its end-of-run `RunStatusChanged` (`supervisor.rs:957–965`) only on
  `SchedulerExit::Completed` (deriving `Completed`/`Failed` from the graph —
  the `Running` fallback arm of `aggregate_run_status` at `supervisor.rs:985`
  becomes unreachable and is removed). The orchestrator wrapper passes the
  exit into `finalize_run_status(run, generation, exit)`:
  - `Completed` → derive + record terminal status, clear handle, write
    `run.json`, evict audit (today's behavior, now only on genuine terminals);
  - `Paused` → **no-op**: status stays `Paused` (set by `pause_run`), handle
    retained, no `run.json`, no audit eviction — the run is alive;
  - `Cancelled` → status is already `Cancelled` (set by `cancel_run`); clear
    the spent handle, write `run.json` with `Cancelled`, evict audit (resume
    re-registers per task at `supervisor.rs:1622`).
  The handle-token check at `orchestrator.rs:453–457`/`469–471` is deleted.

## 0064 — Resume + identity

- **Re-drive wedged tasks on resume-after-cancel.** Widen `start_run`'s
  precondition to `Pending | Paused | Cancelled`. `start_run` is synchronous,
  so the recovery runs in the spawned async wrapper, *before* `run_graph`:
  when the prior status was `Cancelled`, lock the graph and apply
  `crate::persist::recover_for_resume` (`persist.rs:265` — it already maps
  `InProgress | InReview → Ready` on a `&mut TaskGraph`, exactly the wedged
  states from `supervisor.rs:1355–1364`). No new transition logic; the
  process-restart rule is reused in-memory.

- **Reserve disk-view ids.** Replace the peeked counter
  (`orchestrator.rs:1216`) and the `&mut u64` parameter of
  `load_disk_run_views` (`run_metadata.rs:254–258`) with a caller-supplied
  allocator, `ids: &mut dyn FnMut(&str) -> RunId`. `CoreState` gains
  `disk_ids: std::sync::Mutex<HashMap<String, u64>>` (run_uid → id); the
  closure consults the memo and on first sight allocates via
  `next_id.fetch_add(1, Ordering::Relaxed)` (`orchestrator.rs:384`) — so a
  disk view's id can never collide with a later `OpenRun` and is stable for
  the session.

- **Align the doc contracts.** `Command::CancelRun` (`api.rs:323–335`): the
  run is **kept** in the registry, marked `Cancelled`, still queryable, and
  resumable via `StartRun` — keep-and-mark is the better behavior (the TUI can
  inspect what completed before the cancel), so the docs change, not the code.
  `Command::StartRun` (`api.rs:296–307`) documents the `Cancelled` resume.
  The contradictory comment block inside `cancel_run`
  (`orchestrator.rs:1018–1030`) is rewritten to describe the epoch/exit-based
  finalize. `PauseRun` docs (`api.rs:309–321`) are already true once 0062
  enforces them.

## Test strategy

- `stale_finalizer_does_not_clobber_resumed_run`: `GatedBackend`
  (`orchestrator.rs:2420` pattern) holds task-a in flight; pause; resume
  (`StartRun`) while the old scheduler is still draining; release the gate.
  Assert: the old finalizer no-ops (status not flipped to `Failed`, handle
  still present — `CancelRun` still `Acknowledged`-and-effective), no
  premature `run.json`, and the run eventually reaches `Completed`.
- `paused_run_writes_no_terminal_run_json`: pause a running run with a held
  in-flight task; release; after the drain, assert status is still `Paused`,
  no `run.json` exists for the run_uid, no `RunStatusChanged{Running}` or
  terminal event was emitted after the `Paused` one, and resume completes.
- `cancel_after_panic_still_aborts`: a `PanicBackend`-style task (see
  `tests/continue_on_failure.rs`) plus a gated in-flight task; cancel after
  the panic is observed; assert the in-flight driver is aborted within a
  bounded window (the run does not wait for the gate).
- `start_and_pause_preconditions_enforced`: `StartRun` on a `Running` /
  `Completed` / `Failed` run and `PauseRun` on a `Pending` run all return
  `ApiError::InvalidCommand`.
- `cancelled_tasks_recover_on_restart` (in-memory): start with a gated task,
  cancel mid-flight (task wedged `InProgress`), `StartRun` again; assert the
  task is re-driven and the run reaches `Completed` — no process restart.
- `disk_run_ids_do_not_alias_live_runs`: seed a finished `run.json` on disk;
  open a live run; call `runs()` twice and then `OpenRun` again. Assert the
  disk view's id differs from every live id, is identical across both calls,
  and the new `OpenRun`'s id collides with nothing.

Existing tests extended rather than replaced:
`pause_run_sets_paused_and_does_not_complete_then_resume_completes`
(`orchestrator.rs:2566`, repointed at the enforced precondition),
`cancel_run_stops_execution_and_cleans_up` (`orchestrator.rs:2416`, asserts
`Cancelled`), and `tests/continue_on_failure.rs` (panic stays fatal with the
new `aborted` flag). `cargo test`,
`clippy --all-targets -D warnings`, and `fmt --check` stay green.

## Interaction with prior plans

- **0014 (failure reasons, not yet implemented):** independent. This plan adds
  `RunStatus::Cancelled` (run-level) compile-minimally; 0014's
  `FailureKind::Cancelled` is per-task and owns the failure UX.
- **0015 (idle detection):** independent — different layer (per-step stall vs
  run lifecycle).
- **Plan 0023 (sibling from the same review):** typed *task* terminal
  outcomes. 0063's `SchedulerExit` is run-level and shares no code with
  0023's `TerminalCause`; the plans land safely in either order. If 0023 lands
  first, its `Cancelled` cause becomes constructible once 0064's resume
  bookkeeping exists.
- **Persistence (plan 0002/0010 lineage):** `run.json` writes become
  terminal-only; `recover_for_resume` (`persist.rs:265`) is reused unchanged,
  now also in-memory on resume-after-cancel.
