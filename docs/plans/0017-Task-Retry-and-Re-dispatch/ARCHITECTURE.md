# Architecture — Plan 0017

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). Touches `makina-core` (engine) and `makina` (TUI).

## Current shape (what exists)

- **State machine** (`crates/makina-core/src/state_machine.rs`): `TaskEvent`
  (12 variants incl. `DependencyFailed`, `GateCapReached`, `ReviewCapReached`,
  `MergeConflict`, `HardError`, `WallClockCapReached`), `TaskState` (in
  `task.rs`: `New`/`Ready`/`InProgress`/`InReview`/`Done`/`Failed`/`Skipped`),
  `fn transition(from, event) -> Result<TaskState, IllegalTransition>`, and
  `fn is_terminal(state) -> bool` (`Done`/`Failed`/`Skipped` are terminal — no
  outgoing transitions).
- **Supervisor** (`crates/makina-core/src/actors/supervisor.rs`):
  - `apply_event_locked(graph, id, event)`, `task_state_locked(graph, id)`,
    `mark_finished_locked(graph, id)`, `set_failure_reason_locked(graph, id,
    kind, message)`.
  - `mark_dependents_skipped(graph, failed_task_id) -> Vec<TaskId>`: fixed-point
    reverse-edge sweep applying `DependencyFailed` (→ `Skipped`) to transitive
    dependents.
  - `aggregate_run_status(graph) -> api::RunStatus` (`Completed` if all `Done`,
    else `Failed` if any `Failed`, else `Running`).
  - The **scheduler** loop: dispatches `Ready` tasks onto a `JoinSet` under a
    concurrency permit, applies terminal events, and **breaks when the `JoinSet`
    is empty and no `Ready` task remains** (one-shot). The supervisor struct
    stays alive after the loop returns (it answers later asks).
  - `DriverGuard::drop` removes a task's worktree on failure.
- **Task record** (`crates/makina-core/src/task.rs`, `pub struct Task`):
  `state`, `gate_iterations`, `review_iterations`,
  `failure_reason: Option<FailureReason>`, `finished_at: Option<DateTime<Utc>>`,
  `depends_on: Vec<TaskId>`, `updated_at`.
- **Commands/Events** (`crates/makina-core/src/api.rs`): `enum Command`
  (`OpenRun`/`StartRun`/`PauseRun`/`CancelRun`/`ReinterpretRun`), `enum Event`
  (the live event stream the TUI consumes), and the `CoreApi` command-execution
  path that forwards commands to the supervisor actor.
- **Persistence** (`persist.rs` graph JSON at `.makina/tasks/{slug}.json`;
  `run_metadata.rs` `RunMetadata`/`TaskSnapshot` snapshot at
  `.makina/runs/{run_uid}/run.json`).
- **Worktrees** (`crates/makina-core/src/worktree.rs`): `WorktreeManager`
  `create`/`remove` at `.makina/worktrees/{plan_slug}--{task_id}/`.

## 0055 — FSM reset transitions

Edits in `crates/makina-core/src/state_machine.rs`.

- Add two `TaskEvent` variants and their transitions in `transition`:

  ```rust
  // Reset a permanently-failed task so it can run again (user-initiated retry).
  RetryRequested,   // Failed  -> New
  // Reset a dependency-skipped task because its blocking failure is being retried.
  DependencyReset,  // Skipped -> New
  ```

  `transition(Failed, RetryRequested) => Ok(New)`;
  `transition(Skipped, DependencyReset) => Ok(New)`. Every other
  `(state, RetryRequested|DependencyReset)` pair returns
  `Err(IllegalTransition)` exactly like the existing arms. These are the **only**
  legal exits from `Failed`/`Skipped`; `is_terminal` is unchanged (those states
  are still terminal *unless* a reset event is applied).

- Tests (`state_machine.rs`):

  ```rust
  #[test]
  fn retry_requested_resets_failed_to_new() { /* transition(Failed, RetryRequested) == Ok(New); transition(Done, RetryRequested) is Err */ }
  #[test]
  fn dependency_reset_unskips_to_new() { /* transition(Skipped, DependencyReset) == Ok(New); transition(Ready, DependencyReset) is Err */ }
  ```

## 0056 — Retry commands + graph reset

Edits in `crates/makina-core/src/api.rs` (the `Command`/`Event` enums),
`crates/makina-core/src/orchestrator.rs` (command handling — where `start_run`
etc. live), and `crates/makina-core/src/actors/supervisor.rs` (the under-lock
graph helpers), plus `persist.rs` wiring.

- **Commands.** Add to `enum Command`:

  ```rust
  /// Reset a single `Failed` task (and its skipped dependents) and re-dispatch.
  RetryTask { run: RunId, task: TaskId },
  /// Reset every `Failed` task in the run (and their skipped dependents) and
  /// re-dispatch.
  RetryFailedTasks { run: RunId },
  ```

  Errors: `ApiError::UnknownRun` if the run isn't open; for `RetryTask`, an
  `ApiError::InvalidCommand` if the named task is not in state `Failed`.

- **Reset helper** (supervisor, under the graph lock). Add
  `fn reset_task_for_retry_locked(graph, id)`: require `task_state_locked(id) ==
  Failed`; clear `failure_reason = None`, `gate_iterations = 0`,
  `review_iterations = 0`, `finished_at = None`; apply
  `TaskEvent::RetryRequested` (→ `New`); bump `updated_at`.

- **Un-skip cascade** (the inverse of `mark_dependents_skipped`). Add
  `fn unskip_dependents_locked(graph, reset_task_ids: &[TaskId]) -> Vec<TaskId>`:
  a **fixed-point** sweep over the transitive forward-dependents of the reset
  task(s). Repeat until no change: for every task currently in `Skipped` that has
  some `depends_on` member already reset/revived, **revive it** (clear
  `finished_at`, apply `TaskEvent::DependencyReset` → `New`) **iff** *none* of its
  `depends_on` is currently in `Failed` **and** *none* is still in `Skipped`.
  Otherwise leave it `Skipped` (it is still legitimately blocked). The
  fixed-point + this guard makes authored order irrelevant and handles a task
  that depends on more than one failure correctly.

  Worked examples (deps written `child ← parent`):
  - `A ← B ← C` and `X ← Y`; fail `A` (⇒ `B`,`C` Skipped) and `X` (⇒ `Y` Skipped).
    `RetryTask(A)` resets `A→New`; the sweep revives `B` (its only dep `A` is now
    `New`), then `C` (dep `B` now `New`). `Y` is *not* a dependent of `A`, so it is
    never visited and stays `Skipped`.
  - `Z` depends on **both** `A` and `X`; fail `A` and `X` (⇒ `Z` Skipped).
    `RetryTask(A)` resets `A`; the sweep reaches `Z` (depends on `A`) but `Z` also
    depends on `X` which is still `Failed`, so the guard leaves `Z` Skipped.
  - `B` depends on both `A` and `D`, with `D` still `Failed`; `C ← B`. `RetryTask(A)`
    reaches `B`, but `B`'s dep `D` is still `Failed` → `B` stays Skipped; then `C`'s
    dep `B` is still `Skipped` → the guard leaves `C` Skipped too.

- **Re-evaluate readiness.** After reset/un-skip, for every task now in `New`
  whose `depends_on` are all `Done`, apply `TaskEvent::DependenciesSatisfied`
  (→ `Ready`) so the scheduler can pick them up (0057). Reuse the existing
  readiness check the scheduler uses for the initial `New → Ready` sweep.

- **Command handling lives in `orchestrator.rs`, not the supervisor.** Commands
  are dispatched by `CoreApi::execute` (the `match command { Command::StartRun =>
  self.start_run(run), … }`) to per-command methods `start_run` / `pause_run` /
  `cancel_run` / `reinterpret_run`. Add sibling methods
  `fn retry_task(&self, run, task)` and `fn retry_failed_tasks(&self, run)` and the
  two new `execute` arms. Each method: validates the run/task; locks the run's
  graph (the orchestrator holds it as `graph: Arc<AsyncMutex<TaskGraph>>`); runs
  the reset + `unskip_dependents_locked` + readiness sweep (the `_locked` helpers
  added to `supervisor.rs`); **persists** the updated graph (`persist.rs` save) and
  refreshes the run snapshot; emits `Event::TaskRetried { run, task }` (new `Event`
  variant) per reset task; then spawns re-dispatch (0057).

- **Tests** (supervisor, `NoopBackend`-style):

  ```rust
  #[test]
  fn reset_clears_failure_metadata() { /* drive a task to Failed w/ gate_iterations>0 + failure_reason; reset_task_for_retry_locked => state New, iterations 0, failure_reason None, finished_at None */ }
  #[test]
  fn unskip_revives_only_this_cascade() { /* graph: A->B->C and X->Y; fail A (B,C skipped), fail X (Y skipped); unskip dependents of A => B,C New, but Y stays Skipped */ }
  #[test]
  fn retry_task_rejects_non_failed() { /* RetryTask on a Done/InProgress task => InvalidCommand */ }
  ```

## 0057 — Re-dispatch (spawn a fresh scheduler over the reset graph)

Edits in `crates/makina-core/src/orchestrator.rs` (the spawn path) and
`crates/makina-core/src/actors/supervisor.rs` (the scheduler entry, if a small
refactor is needed to invoke it on the existing shared graph).

- **Spawn a fresh scheduler — mirror `start_run`.** Re-dispatch is **not** a
  resurrection of the exited scheduler. `start_run` (`orchestrator.rs`) records a
  run handle, sets the run `Running`, and `tokio::spawn`s the scheduler
  (`run_graph` → `supervisor::scheduler(ctx, concurrency)`) over the shared
  `Arc<AsyncMutex<TaskGraph>>`. The retry methods do the same after resetting the
  graph: spawn the scheduler again over the now-reset graph. The scheduler
  dispatches whatever is `Ready`, recreates each task's worktree
  (`WorktreeManager::create` — its reclaim-on-conflict handles a stale slot from
  the failed attempt), runs the developer→gate→reviewer loop, and drains.
- **Fresh semaphore is correct.** `scheduler` creates its own
  `Arc::new(Semaphore::new(concurrency))` locally (`supervisor.rs`). Because the
  prior scheduler already exited (the run was terminal), a new spawn getting a
  fresh `concurrency`-permit semaphore is exactly right — there is no concurrent
  scheduler to double-count against. Do **not** try to relocate or share the old
  semaphore.
- **Run status.** Flip the run `RunStatus::Failed → Running` when the retry spawn
  starts (emit `RunStatusChanged` from the retry method, as `start_run` does); on
  drain, `aggregate_run_status` recomputes `Completed`/`Failed`. Reuse the same
  run-control / event-sink plumbing `start_run` wires so the TUI animates the
  reset tasks.
- **Idempotence / races.** Reject a retry that targets a task while the run is
  still actively `Running` those tasks (only `Failed`/`Paused`/`Completed` runs
  are retryable); state this guard in the command validation.
- **Tests:**

  ```rust
  #[test]
  fn retried_task_runs_to_terminal_again() { /* backend: a task that fails once then a retry that succeeds => after RetryTask the task reaches Done and dependents that were Skipped reach Done */ }
  #[test]
  fn retry_flips_run_status_running_then_completed() { /* run Failed -> RetryFailedTasks -> observe Running then Completed */ }
  ```

## 0058 — Retry in the TUI

Edits in `crates/makina/src/event.rs`, `crates/makina/src/app.rs`,
`crates/makina/src/ui.rs`. Builds on plan 0016's sidebar tree (focused node).

- **Key.** Bind `r` (normal mode, sidebar focus) to a context-sensitive retry
  using `app.focused_node()` (from 0016):
  - focused `TreeNode::Task { run, task }` whose `state == Failed` ⇒ execute
    `Command::RetryTask { run, task }`;
  - focused `TreeNode::Run { run }` whose run has any `Failed` task ⇒ execute
    `Command::RetryFailedTasks { run }`;
  - otherwise emit a status message (`nothing to retry here`).
- **Plumb the command.** Add an `AppEvent` (e.g. `RetryFocused`) handled in
  `app.rs` that resolves the focused node and dispatches the right `Command`
  through the existing api command channel (the same path `StartRun`/`PauseRun`
  use from the TUI). Emit a status message naming what was retried.
- **Status bar.** Add `[r] retry` to the status-bar hint string in `ui.rs`
  (coordinate with the `[v] view` / `[e] errors` / `[?] doctor` hints already
  there from plans 0012–0014; do not clobber them).
- **Tests** (`event.rs`/`app.rs`):

  ```rust
  #[test]
  fn retry_key_on_failed_task_issues_retry_task() { /* focused failed task => AppEvent resolves to Command::RetryTask with the right run/task */ }
  #[test]
  fn retry_key_on_run_node_issues_retry_failed() { /* focused run w/ failures => Command::RetryFailedTasks */ }
  #[test]
  fn retry_key_noop_when_nothing_failed() { /* focused Done task => no command, status message emitted */ }
  #[test]
  fn status_bar_advertises_retry_key() { /* render; assert status bar contains "[r]" */ }
  ```

## Testing notes

- Use the existing `NoopBackend`/scriptable backend used by current supervisor
  tests to script "fail then succeed" without real agents; use `tokio::time`
  pause where timing matters. No real worktrees in unit tests — assert on graph
  state transitions and emitted events, mirroring existing supervisor tests.
