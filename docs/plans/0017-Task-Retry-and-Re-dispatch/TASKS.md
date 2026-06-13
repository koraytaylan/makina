# Makina Plan 0017 — Task Retry & Re-dispatch

When a task fails, it and all its dependents are stuck in terminal `Failed`/
`Skipped` with no way back. Add a **retry** that resets the failed task (and
un-skips its cascaded dependents), clears its failure metadata, recreates its
worktree, and **re-dispatches** on the live supervisor so the run resumes without
discarding completed work.

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

## 0055 — FSM reset transitions

### fsm-reset-transitions — Legal exits from `Failed` and `Skipped`

Add the only two transitions that leave the terminal failure states, so a reset
is auditable through the state machine instead of a behind-the-back mutation.

**Steps:**

1. In `crates/makina-core/src/state_machine.rs`, add two `TaskEvent` variants:
   `RetryRequested` and `DependencyReset`.

2. In the `transition(from, event)` match, add arms:
   `(TaskState::Failed, TaskEvent::RetryRequested) => Ok(TaskState::New)` and
   `(TaskState::Skipped, TaskEvent::DependencyReset) => Ok(TaskState::New)`. All
   other `(state, RetryRequested | DependencyReset)` combinations fall through to
   the existing `Err(IllegalTransition)` default. Leave `is_terminal` unchanged.

3. Add tests:

   ```rust
   #[test]
   fn retry_requested_resets_failed_to_new() { /* transition(Failed, RetryRequested) == Ok(New); transition(Done, RetryRequested) is Err; transition(InProgress, RetryRequested) is Err */ }
   #[test]
   fn dependency_reset_unskips_to_new() { /* transition(Skipped, DependencyReset) == Ok(New); transition(Ready, DependencyReset) is Err */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; `Failed --RetryRequested--> New` and
  `Skipped --DependencyReset--> New` are the only new legal transitions; every
  other pairing still errors; cargo test/clippy/fmt green.

---

## 0056 — Retry commands + graph reset

### retry-reset-and-commands — Reset a failed task and un-skip its cascade

Add the retry commands and the locked graph mutations that reset a failed task,
revive only its cascade of skipped dependents, and re-mark readiness — persisting
the result.

**Steps:**

1. In `crates/makina-core/src/api.rs`, add to `enum Command`:
   `RetryTask { run: RunId, task: TaskId }` and
   `RetryFailedTasks { run: RunId }`. Document the errors: `UnknownRun` when the
   run isn't open; `InvalidCommand` when `RetryTask`'s target is not `Failed` (and
   when the run is still actively `Running`, per 0057's guard).

2. In `crates/makina-core/src/actors/supervisor.rs`, add
   `fn reset_task_for_retry_locked(graph, id)`: assert the task is `Failed`; set
   `failure_reason = None`, `gate_iterations = 0`, `review_iterations = 0`,
   `finished_at = None`; apply `TaskEvent::RetryRequested`; bump `updated_at`
   (mirror `set_failure_reason_locked`'s lock discipline).

3. Add `fn unskip_dependents_locked(graph, reset_task_ids: &[TaskId]) -> Vec<TaskId>`
   — the inverse of `mark_dependents_skipped`. **Fixed-point** sweep over the
   transitive forward-dependents of the reset task(s): repeat until no change —
   for each task currently `Skipped` that depends on an already-reset/revived
   task, **revive it** (apply `TaskEvent::DependencyReset` → `New`, clear
   `finished_at`) **iff none** of its `depends_on` is currently in `Failed` **and
   none** is still in `Skipped`; otherwise leave it `Skipped`. (See
   ARCHITECTURE.md "Un-skip cascade" for the worked `A←B←C` / `X←Y` and
   multi-dependency examples — a task blocked by an unrelated still-`Failed` or
   still-`Skipped` prerequisite must stay `Skipped`.)

4. Add `fn remark_ready_locked(graph)` (or reuse the scheduler's initial readiness
   sweep): for every `New` task whose `depends_on` are all `Done`, apply
   `TaskEvent::DependenciesSatisfied` (→ `Ready`).

5. Handle the two commands in `crates/makina-core/src/orchestrator.rs`: add an
   `execute` arm for each and sibling methods `fn retry_task(&self, run, task)` /
   `fn retry_failed_tasks(&self, run)` next to `start_run`/`pause_run`/`cancel_run`.
   Lock the run's graph (`graph: Arc<AsyncMutex<TaskGraph>>`); for `RetryTask`
   reset the one task then `unskip_dependents_locked(&[task])` then
   `remark_ready_locked`; for `RetryFailedTasks` reset **every** `Failed` task,
   then a single un-skip (passing all reset ids) + readiness sweep. Persist the
   updated graph (`persist.rs` save) and refresh the run snapshot. Emit
   `Event::TaskRetried { run, task }` (add the variant to `enum Event`) for each
   reset task. Then trigger re-dispatch (0057).

6. Add tests (supervisor):

   ```rust
   #[test]
   fn reset_clears_failure_metadata() { /* task to Failed w/ gate_iterations>0 + failure_reason Some; reset_task_for_retry_locked => New, iterations 0, failure_reason None, finished_at None */ }
   #[test]
   fn unskip_revives_only_this_cascade() { /* A->B->C and X->Y; fail A (=> B,C Skipped) and X (=> Y Skipped); unskip_dependents_locked(A) => B,C New, Y still Skipped */ }
   #[test]
   fn retry_task_rejects_non_failed() { /* RetryTask targeting a Done/InProgress task => InvalidCommand */ }
   #[test]
   fn retry_persists_reset_graph() { /* after reset, reload the persisted graph => the task is New with cleared metadata */ }
   ```

- **Depends on:** fsm-reset-transitions
- **Done when:** all four tests pass; `RetryTask`/`RetryFailedTasks` exist and
  validate; reset clears metadata and `Failed → New`; only the retried task's
  cascade is un-skipped (unrelated `Skipped` stays); `New → Ready` is re-marked;
  the reset graph is persisted; cargo test/clippy/fmt green.

---

## 0057 — Re-dispatch

### retry-redispatch — Spawn a fresh scheduler over the reset graph

Resume the run by spawning the scheduler again over the reset graph — mirroring
`start_run` — rather than resurrecting the exited one.

**Steps:**

1. Study `start_run` in `crates/makina-core/src/orchestrator.rs`: it records a run
   handle, sets the run `Running`, and `tokio::spawn`s the scheduler (`run_graph`
   → `supervisor::scheduler`) over the shared `Arc<AsyncMutex<TaskGraph>>`. Make
   the retry methods (0056) do the **same spawn** after resetting the graph: spawn
   a fresh scheduler over the now-reset graph. Do **not** try to re-enter the
   already-returned scheduler. The dispatch path recreates each task's worktree via
   `WorktreeManager::create` (reclaim-on-conflict handles the stale slot from the
   failed attempt) and runs the developer→gate→reviewer loop as on a first run.

2. A fresh scheduler creates its own `Semaphore::new(concurrency)` locally
   (`supervisor.rs`). This is correct: the prior scheduler already exited, so a new
   `concurrency`-permit semaphore double-counts nothing. Do **not** relocate or
   share the old semaphore.

3. Flip the run `RunStatus::Failed → Running` when the retry spawn starts (emit
   `RunStatusChanged`, as `start_run` does); let `aggregate_run_status` recompute
   the terminal status when the scheduler drains. Reuse the run-control /
   event-sink plumbing `start_run` wires so the TUI animates the reset tasks.

4. Guard against races: only a run in `Failed`/`Paused`/`Completed` is retryable;
   reject a retry on a run still actively `Running` with `InvalidCommand` (the
   guard advertised in 0056's command docs).

5. Add tests (supervisor, scriptable backend that fails a task once then succeeds
   on retry; `tokio::time` pause where needed):

   ```rust
   #[test]
   fn retried_task_runs_to_terminal_again() { /* fail task A once (=> Failed, dependent B Skipped); RetryTask(A); A succeeds; assert A Done and B Done */ }
   #[test]
   fn retry_flips_run_status_running_then_completed() { /* run reaches Failed; RetryFailedTasks; observe RunStatus Running then Completed */ }
   #[test]
   fn retry_rejected_while_run_active() { /* run still Running => RetryTask => InvalidCommand */ }
   ```

- **Depends on:** retry-reset-and-commands
- **Done when:** all three tests pass; a retried task re-dispatches, recreates its
  worktree, and runs to terminal; its revived dependents run afterwards; the run
  status goes `Failed → Running → Completed/Failed`; a retry on an active run is
  rejected; no permit leaks; cargo test/clippy/fmt green.

---

## 0058 — Retry in the TUI

### retry-tui — Context-sensitive `[r]` on the sidebar tree

Wire a single `[r]` key that retries the focused failed task or all failures in
the focused run, using plan 0016's tree focus.

> **Requires plan 0016 merged to the base branch** — this task calls
> `app.focused_node()` and matches `TreeNode::{Run,Task}`, both introduced by plan
> 0016 (Task-List Sidebar Tree). Implement 0017 only after 0016 has landed.

**Steps:**

1. In `crates/makina/src/event.rs`, bind `r` (normal mode, sidebar focus) to emit
   a new `AppEvent::RetryFocused`. In `crates/makina/src/app.rs`, handle it via
   `self.focused_node()` (from plan 0016):
   - `TreeNode::Task { run, task }` with `state == Failed` ⇒ dispatch
     `Command::RetryTask { run, task }`;
   - `TreeNode::Run { run }` whose run has any `Failed` task ⇒ dispatch
     `Command::RetryFailedTasks { run }`;
   - otherwise emit a status message `nothing to retry here` and do nothing.
   Dispatch through the same api command channel the TUI uses for
   `StartRun`/`PauseRun`; emit a status message naming what was retried.

2. In `crates/makina/src/ui.rs`, add `[r] retry` to the status-bar hint string
   (coordinate with the existing `[v] view` / `[e] errors` / `[?] doctor` hints;
   do not clobber them or consume another letter beyond `r`).

3. Add tests (`app.rs`/`event.rs`):

   ```rust
   #[test]
   fn retry_key_on_failed_task_issues_retry_task() { /* fixture App: sidebar focus on a Failed task node; RetryFocused => the dispatched command is Command::RetryTask with the focused run/task */ }
   #[test]
   fn retry_key_on_run_node_issues_retry_failed() { /* focus a run node with a failed task; RetryFocused => Command::RetryFailedTasks */ }
   #[test]
   fn retry_key_noop_when_nothing_failed() { /* focus a Done task; RetryFocused => no command dispatched, status message set */ }
   #[test]
   fn status_bar_advertises_retry_key() { /* render the status bar; assert it contains "[r]" */ }
   ```

- **Depends on:** retry-redispatch (and plan 0016 merged to the base branch)
- **Done when:** all four tests pass; `[r]` retries the focused failed task or the
  focused run's failures and is a no-op-with-message otherwise; the status bar
  advertises `[r]`; the dispatched command matches the focused node; cargo
  test/clippy/fmt green.

---

**End of plan 0017 TASKS.** When every "Done when" bullet is green, a failed task
— and everything its failure skipped — can be retried with one keystroke: the
task resets with a fresh budget, its worktree is recreated, the run resumes, and
completed work is preserved.
