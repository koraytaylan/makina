# Architecture — Plan 0018 (deltas)

> Edits in `crates/makina-core/src/merge.rs` (staging-worktree merge mechanics),
> `crates/makina-core/src/paths.rs` (staging path helper), and
> `crates/makina-core/src/actors/supervisor.rs` (merge call site + scheduler
> deadline). Tests in `tests/squash_merge.rs`, `tests/concurrency.rs`,
> `tests/termination_caps.rs`. Line numbers are hints; locate by symbol.

## 0057 — Merge in an ephemeral staging worktree

Today `SquashMerger::squash_merge` (`merge.rs:227–277`) runs
`merge --squash` + `commit` directly in `repo_root`, and every failure path
calls `restore_base_branch` (`merge.rs:301–322`) — `reset --hard HEAD` +
`clean -fd` in the operator's checkout. Replace the mechanics so the operator's
checkout is never the merge arena:

- **New path helper** in `paths.rs` next to `paths::worktree`
  (`paths.rs:120–125`):

  ```rust
  /// `.makina/worktrees/merge-staging` — the disposable merge staging slot.
  /// No `--` in the name, so it can never collide with a
  /// `{plan_slug}--{task_id}` worktree.
  pub fn merge_staging(repo_root: &Path) -> PathBuf
  ```

- **Rework `squash_merge`** (signature and `MergeOutcome` unchanged). Per
  merge, all under the caller-held merge lock:

  ```text
  1. teardown_staging()                      # 0058 preflight; ignore "not found"
  2. git -C {repo_root} worktree add {staging} -b makina/merge-staging {base_branch}
  3. git -C {staging}  merge --squash {task_branch}
       non-zero → capture details → teardown_staging() → Ok(Conflict { details })
  4. git -C {staging}  commit --allow-empty -m {message}
       non-zero → teardown_staging() → Err(GitCommandFailed)
  5. advance the REAL base branch (see below)
  6. teardown_staging()
  ```

  `--allow-empty` keeps the no-op-task audit-trail behaviour (`merge.rs:28–36`).
  `teardown_staging` = `git worktree remove --force {staging}` +
  `git branch -D makina/merge-staging` + `git worktree prune`, each tolerating
  "not found" (mirror `is_not_found_stderr`, `worktree.rs:418–430`).

- **Advancing the base branch** (new private helper `advance_base`). Determine
  whether `base_branch` is checked out in `repo_root` via
  `git -C {repo_root} symbolic-ref --quiet --short HEAD`:
  - **Checked out** (the documented normal case — `merge.rs:8–10`):
    `git -C {repo_root} merge --ff-only makina/merge-staging`. A fast-forward
    cannot produce conflict markers; git *refuses* (non-zero) if operator WIP
    on affected tracked paths or an untracked file would be overwritten, or if
    `develop` diverged. On refusal: `teardown_staging()`, return
    `Err(GitCommandFailed)` with the captured stderr — **no reset, no clean,
    nothing in the operator's checkout is touched**.
  - **Not checked out** (operator parked on another branch / detached):
    `git -C {repo_root} fetch . makina/merge-staging:{base_branch}` — updates
    the ref only, fast-forward-only by default, refuses checked-out branches
    (which we've excluded) and touches no working tree.

  The staging branch is created *from the `base_branch` tip* at step 2 under
  the merge lock, and the base only advances through this path, so the
  fast-forward succeeds by construction unless the operator intervened — in
  which case refusing loudly is the correct behaviour.

- **Delete `restore_base_branch`** and rewrite the module docs: the invariant
  section (`merge.rs:38–57`), the restore docs (`merge.rs:281–300`), and the
  concurrency caveat (`merge.rs:81–89`) now describe containment-by-staging
  instead of restore-by-reset. Update the supervisor's mirror prose
  (`supervisor.rs:39`, `:75–84`, `:1730–1737`).

- **Supervisor call site** (`supervisor.rs:1744–1748`) is unchanged by 0057
  (same signature/outcomes); the hard-error arm (`supervisor.rs:1750–1770`) and
  conflict arm (`:1789–1821`) keep working, with `develop` now untouched
  instead of restored.

## 0058 — Cancellation shield + staging preflight

- **Shield the merge critical section.** At `supervisor.rs:1744–1748`, move the
  lock + merge onto a spawned task and await its handle (a dropped
  `JoinHandle` detaches — the inner task runs to completion):

  ```rust
  let merge_outcome = {
      let merge_lock = Arc::clone(&ctx.merge_lock);
      let merger = ctx.squash_merger.clone(); // SquashMerger is Clone (merge.rs:176)
      let (branch, message) = (branch.clone(), message.clone());
      let handle = tokio::spawn(async move {
          // The shielded task owns the lock span: even if the driver is
          // dropped (timeout/abort_all), the merge finishes (or fails) and
          // the lock is released — never a half-merged base or a leaked lock.
          let _merge_guard = merge_lock.lock().await;
          merger.squash_merge(&branch, &message).await
      });
      match handle.await {
          Ok(result) => result,
          // Panic inside the merge task: treat as a hard merge error so the
          // existing HardError arm (supervisor.rs:1750–1770) drives the task
          // terminal. (JoinError::is_cancelled is impossible — nobody aborts
          // this handle.)
          Err(join_err) => return Err(format!("squash-merge task failed for {task_id}: {join_err}")),
      }
  };
  ```

  Extract the spawn-and-detach pattern as a tiny documented helper (e.g.
  `fn shield<F>(fut: F) -> JoinHandle<F::Output>`) so the contract ("dropping
  the handle does not cancel the work") is unit-testable in isolation.

- **Preflight + self-heal in `SquashMerger`.** Step 1 of the 0057 sequence: a
  stale `merge-staging` worktree/branch (a previous process crashed or a
  pre-0058 driver was dropped mid-merge) is removed before `worktree add`, and
  after creation a `git -C {staging} status --porcelain` must come back empty
  (defensive; warn + recreate once if not). With 0057 the staging slot is
  disposable, so self-heal is a cheap delete-and-recreate — never a
  `reset`/`clean` and never in `repo_root`.

## 0059 — Merge-phase budget

The scheduler wraps each driver in
`tokio::time::timeout(wall_clock, task_driver(…))` (`supervisor.rs:1194–1202`)
and maps an elapse to `WallClockCapReached` in the `Some(Ok((id, None)))` arm
(`supervisor.rs:1307–1354`, benign-race handling at `:1326–1331`). The deadline
spans the merge-lock queue, so a finished, approved task can be cancelled
mid-merge purely because *other* tasks merged first.

- **Per-task `merge_phase` flag.** The scheduler's fill phase
  (`supervisor.rs:1184–1203`) creates an `Arc<AtomicBool>` per driver and
  passes a clone into `task_driver` (third parameter, alongside `ctx` and
  `task_id`). The driver sets it on entering the Approve arm
  (`supervisor.rs:1724`), *before* awaiting the merge lock — so both the queue
  wait and the merge execution are covered.

- **Grace-await instead of drop.** Replace the `timeout` wrapper with a
  pinned `select!`:

  ```rust
  let driver = task_driver(&driver_ctx, &driver_id, merge_phase_for_driver)
      .instrument(task_span);
  tokio::pin!(driver);
  tokio::select! {
      result = &mut driver => (driver_id, Some(result)),
      _ = tokio::time::sleep(wall_clock) => {
          if merge_phase.load(Ordering::SeqCst) {
              // The work is done and approved; the clock stopped at approval.
              // Let the terminal merge phase finish rather than failing a
              // completed task (and recording Failed for work that landed).
              (driver_id, Some(driver.await))
          } else {
              (driver_id, None) // genuine cap elapse; driver dropped as today
          }
      }
  }
  ```

  The `Some(Ok((id, None)))` timeout arm and its benign-race handling are
  unchanged. Cancel (`abort_all`, `supervisor.rs:1103–1106`) still aborts the
  whole spawned future — that is 0058's job: the *git* state is shielded; only
  the task's recorded state can lag, exactly as for any cancel.

## Test strategy

- `merge_never_mutates_operator_checkout` (`tests/squash_merge.rs`): repo with
  operator WIP — an untracked `notes.txt` *and* an unstaged edit to a tracked
  file — plus a task branch that adds `notes.txt`. The squash lands in staging,
  the `--ff-only` advance is refused, `squash_merge` returns a hard error; the
  untracked file and the edit are byte-identical, `HEAD` is unchanged, and no
  staging worktree/branch remains.
- `conflict_is_contained_to_staging_worktree`: a real content conflict returns
  `MergeOutcome::Conflict`; operator WIP survives, `git status` in `repo_root`
  is unchanged (not merely "clean" — *unchanged*, WIP included).
- `merge_advances_base_when_not_checked_out`: `repo_root` parked on another
  branch; the merge advances the `develop` ref by exactly one squash commit via
  the fetch path and touches no working tree.
- Update `squash_merge_conflict_leaves_develop_clean`
  (`tests/squash_merge.rs:268`) to additionally seed WIP and assert it
  survives; `approve_squash_merges_to_develop_and_tears_down_worktree`
  (`tests/squash_merge.rs:418`) and `merges_into_develop_are_serialized_and_clean`
  (`tests/concurrency.rs:619`) must stay green unmodified in substance.
- `shield_survives_handle_drop` (unit, near the helper): drop the shield's
  `JoinHandle` mid-flight; observe (via a channel) that the inner future still
  completes.
- `cancel_mid_merge_leaves_develop_clean` (`tests/concurrency.rs`): N tasks
  held at simultaneous approval (barrier backend), fire
  `control.cancel` while merges are in flight, drain; assert
  `git status --porcelain` in `repo_root` is empty and `develop`'s history is a
  prefix of complete squash commits — never a staged half-merge.
- `cap_elapse_during_merge_grace_awaits_completion`
  (`tests/termination_caps.rs`): a `commit-msg` hook sleeps past
  `caps.wall_clock_secs` so the deadline elapses *during* the merge phase; the
  task still terminates `Done` with its squash commit on `develop` (under the
  pre-0059 code this records `Failed` via `WallClockCapReached`).

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay green.

## Interaction with prior plans

- Replaces the merge mechanics behind the seams plans 0001/0014 rely on:
  `MergeOutcome` and the supervisor's conflict/hard-error arms are unchanged,
  so 0014's `MergeConflict` failure classification is untouched. 0015's idle
  watchdog is orthogonal (per-step stream silence vs. terminal merge phase).
- Within this review: independent of plan 0019 (slug safety — the staging slot
  uses a fixed name, no slug in its path); plan 0023 (structured failure
  reasons) will later classify the new ff-refusal error; plan 0020
  (run-lifecycle races) builds on 0058's shield discipline.
- Internal ordering: 0058's preflight assumes 0057's staging slot; 0059 is
  mechanically independent but only fully safe combined with 0058 (a cancel
  during the grace window still relies on the shield).
