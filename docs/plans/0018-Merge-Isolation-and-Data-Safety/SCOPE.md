# Scope — Plan 0018

> What this plan delivers, what it leaves out, and the decisions behind it.
> Origin: full-codebase review, 2026-06-11. Highest-severity finding of the
> review.

## Why this plan

The squash-merge runs in the **operator's shared main checkout**, and its
failure and cancellation paths can destroy user work or leave `develop` dirty.

1. **Data loss in the operator's checkout.** On any merge failure,
   `restore_base_branch` (`merge.rs:301–322`) runs `git reset --hard HEAD` +
   `git clean -fd` in `repo_root` — the user's own working tree. The doc claims
   `clean -fd` removes "untracked files/dirs the squash introduced"
   (`merge.rs:295`), but git cannot distinguish squash-introduced untracked
   files from pre-existing operator WIP: `clean -fd` deletes **every**
   non-ignored untracked file and `reset --hard` discards **every**
   staged/unstaged modification. The worst case is self-inflicted: the operator
   has an untracked file X; an approved task branch adds X; `merge --squash`
   correctly *refuses* ("untracked working tree files would be overwritten",
   non-zero exit) → the refusal is classified `Conflict` (`merge.rs:241–247`)
   → the restore runs → Makina deletes the very file git just protected. The
   module itself acknowledges the shared-checkout hazard (`merge.rs:81–89`);
   this fires whenever the operator has WIP while a run is active.

2. **Cancellation corrupts the shared checkout.** Driver futures are dropped at
   arbitrary await points: by the per-task
   `tokio::time::timeout(wall_clock, task_driver(…))` (`supervisor.rs:1194–1202`)
   and by `join_set.abort_all()` on cancel (`supervisor.rs:1103–1106`). The
   merge critical section —
   `let _merge_guard = ctx.merge_lock.lock().await;`
   `ctx.squash_merger.squash_merge(&branch, &message).await`
   (`supervisor.rs:1744–1748`) — is **not cancellation-safe**: a drop between
   `git merge --squash` and the commit/restore leaves the shared `develop`
   checkout with staged squash changes, and the merge guard is released, so
   every later merge runs against a dirty index. Worse, tokio's
   `Command::output()` does **not** kill the child on future-drop
   (`kill_on_drop` defaults off — see `run_git_raw`, `merge.rs:329–337`), so
   the squash can even *complete* after the driver is gone. The "develop is
   NEVER left broken" invariant (`merge.rs:38–57`) only holds when
   `squash_merge` runs to completion.

3. **The wall-clock cap spans the merge queue.** The per-task timeout wraps the
   *whole* driver lifecycle, including waiting on the `merge_lock`
   (`supervisor.rs:417`, acquired at `:1746`). With N concurrent approvals the
   queue wait alone can blow the cap (`caps.wall_clock_secs`, default 1800 s —
   `config.rs:261`) on a task whose work is already done and approved —
   triggering finding 2 in the worst possible place.

This plan moves the merge into an ephemeral staging worktree (the operator's
checkout only ever sees a fast-forward), shields the merge from cancellation,
and exempts the terminal merge phase from the per-task wall-clock cap.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0057–0059):

- **0057 — Merge in an ephemeral staging worktree.** `SquashMerger` creates a
  disposable worktree on a throwaway branch at the `base_branch` tip, runs
  `merge --squash` + `commit` *there*, then advances the real base branch with
  a fast-forward-only operation. Conflicts and failures are contained to the
  disposable worktree (delete it; never reset/clean the operator's files).
  `restore_base_branch` is deleted — the invariant becomes structural.
- **0058 — Cancellation shield + staging preflight.** Run the merge critical
  section on a `tokio::spawn`ed task and await the `JoinHandle` (dropping a
  `JoinHandle` detaches, it does not cancel), so scheduler aborts/timeouts can
  never interrupt a merge mid-flight; add a preflight that reclaims/recreates a
  stale staging worktree before merging.
- **0059 — Merge-phase budget.** Stop the per-task wall-clock cap at approval:
  the scheduler's deadline, when it elapses during the merge phase
  (lock queue + merge execution), grace-awaits the driver to completion instead
  of dropping it.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| `restore_base_branch` runs `reset --hard` + `clean -fd` in the operator's checkout; deletes WIP on any merge failure | `0057` |
| Untracked-overwrite refusal classified `Conflict` → restore deletes the file git protected | `0057` |
| Driver drop mid-merge leaves `develop` with a staged squash; later merges run on a dirty index | `0058` |
| Per-task wall-clock cap spans the merge-lock queue; queue wait can cancel a finished task mid-merge | `0059` |

## Locked decisions

- **NO `reset --hard` / `clean -fd` ever runs in the operator's checkout.**
  This is the non-negotiable invariant of 0057. All destructive recovery
  happens in the disposable staging worktree (which is simply deleted).
- **The operator's checkout only ever sees a fast-forward.** When `base_branch`
  is checked out in `repo_root`, the landed squash commit reaches it via
  `git merge --ff-only` (cannot create conflict markers; refuses rather than
  overwrites). When it is *not* checked out, the ref is advanced with
  `git fetch . <staging>:<base_branch>` (fast-forward-only by default, never
  touches any working tree). If the fast-forward is refused (operator WIP on
  affected paths, or a manual commit on `develop`), surface a hard `MergeError`
  **without cleaning anything** — the operator resolves it; Makina never
  destroys state to make progress.
- **Dedicated staging helper inside `merge.rs`, not `WorktreeManager`.**
  `WorktreeManager` is shaped around `task/{plan_slug}--{task_id}` branches and
  reclaim semantics (`worktree.rs:159–242`); the merge staging slot is a single
  fixed path (`.makina/worktrees/merge-staging`) on a fixed branch
  (`makina/merge-staging`), created and torn down per merge under the merge
  lock. Reuse the path-helper pattern (`paths.rs`), not the manager.
- **Shield = `tokio::spawn` + await the `JoinHandle`.** No `select!` tricks, no
  `kill_on_drop` changes: the merge critical section (lock acquisition + git
  commands) moves onto its own task, which runs to completion regardless of
  what happens to the driver. The merge lock is held and released *by the
  shielded task*, so a cancelled driver can never leak a held lock or a
  half-merged state.
- **Stop the clock at approval, with an unbounded grace.** 0059 implements
  "exclude the merge phase from the cap" as: the driver flips a per-task
  `merge_phase` flag on entering the Approve arm (before awaiting the merge
  lock); the scheduler replaces `timeout(…)` with a `select!` whose elapsed-arm
  grace-awaits the driver when the flag is set. No separate merge budget knob:
  merges are serialized and short, and 0058 already guarantees git consistency
  if anything hangs — a new cap config would be speculative.

## Out of scope

- **Agent-driven conflict reconciliation.** A conflicting merge still fails the
  task safely via `MergeConflict` (`supervisor.rs:1789–1821`); the
  reconciliation seam (`merge.rs:59–69`, `trial-findings.md` §7) stays a seam.
- **Structured failure reasons** for the new error paths (plan 0023 of this
  review); 0057/0058 keep the existing string-reason plumbing.
- **Run-lifecycle races** around start/cancel/finalize (plan 0020 of this
  review); 0058 only shields the merge critical section.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
