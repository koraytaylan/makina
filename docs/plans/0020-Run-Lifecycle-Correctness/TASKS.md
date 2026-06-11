# Makina Plan 0020 — Run Lifecycle Correctness

Make start/pause/cancel/finalize honest: a stale finalizer can no longer
clobber a resumed run, a paused run is never persisted as `Failed`, cancel
gets a first-class `Cancelled` status and actually aborts even after a panic,
cancelled tasks are re-drivable in-process, and disk-loaded run ids stop
aliasing live runs.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0062 — Run epochs + enforced preconditions

### add-run-generation — Stale finalizers must no-op

Re-issuing `StartRun` leaves the *old* background task's
`finalize_run_status` (`orchestrator.rs:444`) free to overwrite the new run's
status, destroy its handle, write a bogus `run.json`, and evict the audit
registry mid-run (`orchestrator.rs:507–534`).

**Steps:**

1. In `crates/makina-core/src/orchestrator.rs`, add `generation: u64` to
   `RunEntry` (`orchestrator.rs:238`), initialized `0` at every `RunEntry {`
   literal in `open_run`.

2. In `start_run` (`orchestrator.rs:853`), under the registry lock, do
   `entry.generation += 1` and capture `let generation = entry.generation;`
   alongside the other pieces returned out of the lock. Pass it into the
   spawned wrapper (`orchestrator.rs:957–973`) and call
   `state.finalize_run_status(run, generation).await`.

3. In `finalize_run_status`, take `generation: u64` and return immediately if
   the entry is missing **or** `entry.generation != generation` — before
   reading the handle, deriving a status, writing `run.json`, or evicting the
   audit registry.

4. Add a test (use the `GatedBackend` pattern from
   `cancel_run_stops_execution_and_cleans_up`, `orchestrator.rs:2416`):

   ```rust
   #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
   async fn stale_finalizer_does_not_clobber_resumed_run() { /* start (gated), pause, StartRun again while the old scheduler drains, release; assert: status never flips to Failed, CancelRun still controls the run, no premature run.json, run completes */ }
   ```

- **Depends on:** —
- **Done when:** the test passes; a superseded scheduler's finalizer leaves
  status, handle, `run.json`, and audit registry untouched; cargo
  test/clippy/fmt green.

### enforce-lifecycle-preconditions — Enforce documented Start/Pause preconditions; drop the defensive cancel

`start_run` accepts any status despite `api.rs:302–303`; `pause_run` pauses
anything despite `api.rs:317`; and the old-handle cancel
(`orchestrator.rs:882–884`) aborts in-flight turns, contradicting
`api.rs:311–312`.

**Steps:**

1. In `start_run`, after the blocked-ingestion check
   (`orchestrator.rs:864–878`), return `ApiError::InvalidCommand` unless
   `entry.status` is `Pending` or `Paused` (0064 widens this to `Cancelled`).

2. In `pause_run` (`orchestrator.rs:983`), return `ApiError::InvalidCommand`
   unless `entry.status == RunStatus::Running`.

3. Delete the `old.cancel.cancel()` defensive cancel in `start_run`
   (`orchestrator.rs:882–884`); still *replace* the handle with a fresh one.
   With the precondition + epoch, the only overlap is resume-while-draining,
   which is safe uncancelled (see the locked decision in SCOPE.md).

4. Update `pause_run_sets_paused_and_does_not_complete_then_resume_completes`
   (`orchestrator.rs:2566`) to pause a *running* (gated) run, and add:

   ```rust
   #[tokio::test]
   async fn start_and_pause_preconditions_enforced() { /* StartRun on Running/Completed/Failed → InvalidCommand; PauseRun on Pending → InvalidCommand */ }
   ```

- **Depends on:** add-run-generation
- **Done when:** both tests pass; starting a running/terminal run and pausing
  a non-running run are rejected with `InvalidCommand`; resuming a paused run
  no longer aborts its in-flight turns; cargo test/clippy/fmt green.

---

## 0063 — Honest lifecycle states

### scheduler-exit-reason — Return why the scheduler exited; split the aborted flag

The scheduler conflates cancel with the panic path
(`supervisor.rs:1103–1106,1366–1367`) and exits without saying why, forcing
`run_graph`/`finalize_run_status` to guess from a paused graph
(`supervisor.rs:957–965,977–986`).

**Steps:**

1. In `crates/makina-core/src/actors/supervisor.rs`, add near `RunReport`
   (`supervisor.rs:368`):

   ```rust
   pub enum SchedulerExit { Completed, Paused, Cancelled }
   ```

2. In `scheduler` (`supervisor.rs:1055`), add a local `aborted: bool` and
   re-gate the cancel arm (`supervisor.rs:1103–1106`) on
   `is_cancelled() && !aborted` (still setting `stop_launching = true`), so a
   panic-set `stop_launching` (`supervisor.rs:1366–1367`) cannot swallow the
   `abort_all()`.

3. Compute the exit at loop end (`Cancelled` if the token is cancelled, else
   `Paused` if the pause flag is set and any task is still `New | Ready`,
   else `Completed`) and return
   `Result<(RunReport, SchedulerExit), String>` from both `scheduler` and
   `run_graph` (`supervisor.rs:829`). The ask path (`run_ready_tasks`,
   `supervisor.rs:695`) discards the exit, keeping its reply type
   (`supervisor.rs:654`) unchanged.

4. Gate the end-of-run `RunStatusChanged` emit (`supervisor.rs:957–965`) on
   `SchedulerExit::Completed`; remove the now-unreachable `Running` fallback
   from `aggregate_run_status` (`supervisor.rs:985`).

5. Add tests:

   ```rust
   #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
   async fn cancel_after_panic_still_aborts() { /* PanicBackend task + gated task; cancel after the panic; the gated driver is aborted within a bounded window */ }
   #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
   async fn paused_scheduler_exit_emits_no_terminal_status() { /* pause with a held in-flight task, release, drain; no RunStatusChanged{Running|Completed|Failed} after the Paused emit */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; the scheduler reports
  `Completed | Paused | Cancelled`; cancel aborts even after a panic; a paused
  drain emits no terminal status; cargo test/clippy/fmt green.

### run-status-cancelled — `RunStatus::Cancelled` + terminal-only finalize

**Steps:**

1. Add `Cancelled` to `RunStatus` (`api.rs:206–217`); `cancel_run` sets and
   broadcasts `Cancelled` instead of `Failed`
   (`orchestrator.rs:1032,1034–1037`).

2. Update the only three exhaustive `RunStatus` matches in
   `crates/makina/src` (verified by grep): `status_badge` (`ui.rs:1494`,
   `("[⊘]", Color::DarkGray)`), `status_color` (`ui.rs:1505`,
   `Color::DarkGray`), `status_label` (`ui.rs:1543`, `"Cancelled"`). No other
   TUI work (plan 0014 owns failure UX).

3. Thread the exit into `finalize_run_status(run, generation, exit)`:
   `Completed` → today's derive/record/`run.json`/evict; `Paused` → full
   no-op (status stays `Paused`, handle retained, no `run.json`, no audit
   eviction); `Cancelled` → clear the spent handle, write `run.json` with
   `Cancelled`, evict audit. Delete the handle-token sniffing
   (`orchestrator.rs:453–457,469–471`).

4. Update `cancel_run_stops_execution_and_cleans_up`
   (`orchestrator.rs:2416`) to assert `Cancelled`, and add:

   ```rust
   #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
   async fn paused_run_writes_no_terminal_run_json() { /* pause, drain, assert status Paused, no run.json for the run_uid, resume completes and only then writes run.json */ }
   ```

- **Depends on:** scheduler-exit-reason, add-run-generation
- **Done when:** both tests pass; a cancelled run reads `Cancelled`
  everywhere; a paused run survives its drain with status `Paused` and no
  `run.json`; cargo test/clippy/fmt green.

---

## 0064 — Resume + identity

### resume-cancelled-runs — Re-drive wedged tasks on StartRun after cancel

Aborted drivers leave tasks `InProgress`/`InReview`
(`supervisor.rs:1355–1364`) that `next_ready_task_id`
(`supervisor.rs:1388–1406`) never re-picks; today only a process reopen
(`orchestrator.rs:669` → `persist.rs:265`) recovers them.

**Steps:**

1. Widen `start_run`'s precondition to `Pending | Paused | Cancelled`.

2. In the spawned wrapper (`orchestrator.rs:957`), when the prior status was
   `Cancelled`, lock the graph and apply
   `crate::persist::recover_for_resume(&mut g)` (`persist.rs:265` already
   maps `InProgress | InReview → Ready`) before calling `run_graph`.

3. Add a test:

   ```rust
   #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
   async fn cancelled_tasks_recover_on_restart() { /* gated task; cancel mid-flight (task wedged InProgress); StartRun again in the SAME process; the task is re-driven and the run completes */ }
   ```

- **Depends on:** run-status-cancelled
- **Done when:** the test passes; resume-after-cancel re-drives wedged tasks
  without a process restart; cargo test/clippy/fmt green.

### reserve-disk-run-ids — Disk views get real, stable ids

**Steps:**

1. In `crates/makina-core/src/orchestrator.rs`, add
   `disk_ids: std::sync::Mutex<HashMap<String, u64>>` to `CoreState`
   (`orchestrator.rs:327`), mapping `run_uid` → allocated id.

2. Change `load_disk_run_views` (`run_metadata.rs:254–258`) to take
   `ids: &mut dyn FnMut(&str) -> RunId` instead of `next_id: &mut u64`; in
   `runs()` (`orchestrator.rs:1189–1256`), delete the peeked
   `next_id.load(Relaxed)` (`orchestrator.rs:1216`) and pass a closure that
   memoizes via `disk_ids`, allocating with
   `next_id.fetch_add(1, Ordering::Relaxed)` (`orchestrator.rs:384`) on first
   sight of a `run_uid`.

3. Add a test:

   ```rust
   #[tokio::test]
   async fn disk_run_ids_do_not_alias_live_runs() { /* seed a run.json on disk; open a live run; runs() twice then OpenRun: disk id ∉ live ids, identical across calls, new OpenRun id collides with nothing */ }
   ```

- **Depends on:** —
- **Done when:** the test passes; a disk view's id never collides with a live
  run and is stable for the session; cargo test/clippy/fmt green.

### align-lifecycle-docs — Make the api.rs lifecycle contracts true

**Steps:**

1. Rewrite `Command::CancelRun` docs (`api.rs:323–335`): the run is **kept**
   in the registry, marked `Cancelled`, remains queryable, and is resumable
   via `StartRun` — keep-and-mark is deliberately preferred over the
   documented remove-from-set (the TUI can inspect what completed before the
   cancel), so the docs change, not the code.

2. Update `Command::StartRun` docs (`api.rs:296–307`) to include the
   `Cancelled` resume path, and verify `PauseRun` docs (`api.rs:309–321`) now
   match the enforced behavior.

3. Rewrite the stale comment block in `cancel_run`
   (`orchestrator.rs:1018–1030`) and the `finalize_run_status` doc comment
   (`orchestrator.rs:435–443`) to describe the epoch + exit-reason design.

- **Depends on:** run-status-cancelled
- **Done when:** every lifecycle claim in `api.rs` matches an enforced
  behavior with a test somewhere in this plan; cargo test/clippy/fmt green.

---

**End of plan 0020 TASKS.** When every "Done when" bullet is green, a resumed
run cannot be clobbered by its predecessor, paused means paused, cancelled
means cancelled (and is resumable), and every `RunId` names exactly one run.
