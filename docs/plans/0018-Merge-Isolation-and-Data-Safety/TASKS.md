# Makina Plan 0018 — Merge Isolation & Data Safety

Move the squash-merge out of the operator's shared checkout into an ephemeral
staging worktree (the main checkout only ever sees a fast-forward, and no
`reset --hard`/`clean -fd` ever runs there), shield the merge from driver
cancellation, and stop the per-task wall-clock cap at approval so the merge
queue cannot cancel finished work mid-merge.

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

## 0057 — Merge in an ephemeral staging worktree

### merge-staging-worktree — Squash in a disposable worktree, advance the base by fast-forward only

`SquashMerger::squash_merge` (`merge.rs:227–277`) merges directly in
`repo_root` and recovers via `restore_base_branch` (`merge.rs:301–322`) —
`reset --hard` + `clean -fd` in the operator's checkout, which deletes operator
WIP on any failure. Rework the mechanics so all destructive recovery happens in
a disposable staging worktree.

**Steps:**

1. In `crates/makina-core/src/paths.rs`, add `pub fn merge_staging(repo_root:
   &Path) -> PathBuf` returning `.makina/worktrees/merge-staging` (doc-comment:
   no `--` in the name, so it cannot collide with a `{plan_slug}--{task_id}`
   slot), next to `paths::worktree` (`paths.rs:120–125`). Unit-test the path.

2. In `crates/makina-core/src/merge.rs`, rework `squash_merge` (signature,
   `MergeOutcome`, and `MergeError` shape unchanged) to, per merge: tear down
   any stale staging slot (tolerate "not found" — mirror
   `is_not_found_stderr`, `worktree.rs:418–430`); `git -C {repo_root} worktree
   add {staging} -b makina/merge-staging {base_branch}`; run
   `merge --squash {task_branch}` then `commit --allow-empty -m {message}`
   **in the staging worktree**. A squash conflict captures `details`
   (`combine_output`), tears down staging, and returns
   `Ok(MergeOutcome::Conflict { details })`; a commit failure tears down
   staging and returns `Err(GitCommandFailed)`. No git command with side
   effects on the working tree ever targets `repo_root`'s checkout.

3. Add the private `advance_base` helper: if
   `git -C {repo_root} symbolic-ref --quiet --short HEAD` equals
   `base_branch`, run `git -C {repo_root} merge --ff-only makina/merge-staging`
   (refusal → tear down staging, return `Err(GitCommandFailed)` with stderr —
   touch nothing else); otherwise run
   `git -C {repo_root} fetch . makina/merge-staging:{base_branch}` (ref-only,
   fast-forward-only). Tear down staging after a successful advance.

4. **Delete `restore_base_branch`.** Rewrite the module docs: the invariant
   block (`merge.rs:38–57`), restore docs (`merge.rs:281–300`), and concurrency
   caveat (`merge.rs:81–89`) now describe containment-by-staging; update the
   supervisor's mirror prose (`supervisor.rs:39`, `:75–84`, `:1730–1737`).
   Grep for `reset --hard` / `clean -fd` afterwards: zero hits outside tests.

- **Depends on:** —
- **Done when:** existing `tests/squash_merge.rs` and
  `merges_into_develop_are_serialized_and_clean` (`tests/concurrency.rs:619`)
  pass against the new mechanics; `restore_base_branch` no longer exists; no
  code path runs `reset --hard` or `clean -fd` in `repo_root`; cargo
  test/clippy/fmt green.

### operator-checkout-safety-tests — Prove operator WIP survives every failure path

**Steps:**

1. In `crates/makina-core/tests/squash_merge.rs`, add
   `merge_never_mutates_operator_checkout`: seed `repo_root` with an untracked
   `notes.txt` and an unstaged edit to a tracked file; the task branch also
   adds `notes.txt`; assert `squash_merge` returns a hard error (ff-only
   refusal), both WIP artifacts are byte-identical afterwards, `HEAD` is
   unchanged, and neither the staging worktree nor `makina/merge-staging`
   remains.

2. Add `conflict_is_contained_to_staging_worktree`: a genuine content conflict
   (develop and task branch edit the same line) with operator WIP present
   returns `MergeOutcome::Conflict`; assert the WIP survives and
   `git status --porcelain` output in `repo_root` is *unchanged* (WIP
   included), not merely clean.

3. Add `merge_advances_base_when_not_checked_out`: check `repo_root` out on a
   different branch; assert the merge advances the `develop` ref by exactly one
   squash commit (via the fetch path) and the checked-out tree is untouched.

4. Extend `squash_merge_conflict_leaves_develop_clean`
   (`tests/squash_merge.rs:268`) to seed WIP before the conflicting merge and
   assert it survives.

- **Depends on:** merge-staging-worktree
- **Done when:** all four tests pass;
  `approve_squash_merges_to_develop_and_tears_down_worktree`
  (`tests/squash_merge.rs:418`) still passes; cargo test/clippy/fmt green.

---

## 0058 — Cancellation shield + staging preflight

### shield-merge-from-cancellation — Spawn the merge; dropping the driver can no longer interrupt it

The merge critical section (`supervisor.rs:1744–1748`) is dropped at arbitrary
await points by the per-task timeout (`supervisor.rs:1194–1202`) and
`abort_all()` (`supervisor.rs:1103–1106`); tokio's `Command::output()` does not
kill the child on future-drop, so a dropped driver can leave a *completing*
squash with nobody to commit or clean up.

**Steps:**

1. In `crates/makina-core/src/actors/supervisor.rs`, add a small documented
   helper `fn shield<F>(fut: F) -> tokio::task::JoinHandle<F::Output>` (a named
   wrapper over `tokio::spawn` whose doc states the contract: dropping the
   returned handle detaches — it never cancels the work).

2. At the merge call site (`supervisor.rs:1744–1748`), move the lock + merge
   into `shield(async move { let _merge_guard = merge_lock.lock().await;
   merger.squash_merge(&branch, &message).await })` — cloning
   `Arc::clone(&ctx.merge_lock)`, `ctx.squash_merger` (`Clone`,
   `merge.rs:176`), `branch`, and `message` into the task — and `await` the
   handle. Map `Err(join_err)` (a panic in the merge task) to the existing
   hard-error return so the `HardError` arm (`supervisor.rs:1750–1770`) drives
   the task terminal. Update the merge-lock doc block
   (`supervisor.rs:1510–1530`): the lock span now lives on the shielded task.

3. Add `shield_survives_handle_drop` (unit test next to the helper): the
   shielded future signals over a channel after a short sleep; drop the handle
   immediately; assert the signal still arrives.

4. In `crates/makina-core/tests/concurrency.rs`, add
   `cancel_mid_merge_leaves_develop_clean`: hold N tasks at simultaneous
   approval with the barrier backend, fire `control.cancel` while merges are in
   flight, drain the run; assert `git status --porcelain` in `repo_root` is
   empty and every commit on `develop` is a complete squash commit (no staged
   half-merge, regardless of interleaving).

- **Depends on:** —
- **Done when:** both tests pass; a cancel or wall-clock elapse during a merge
  can no longer leave `develop` mid-squash or leak the merge lock; cargo
  test/clippy/fmt green.

### staging-preflight-self-heal — Reclaim a stale staging slot before merging

**Steps:**

1. In `crates/makina-core/src/merge.rs`, make the staging teardown the *first*
   step of every `squash_merge` (preflight): remove a leftover
   `merge-staging` worktree/branch from a crashed prior process before
   `worktree add` (idempotent, "not found" tolerated).

2. After creating the staging worktree, verify
   `git -C {staging} status --porcelain` is empty; if not (defensive), warn via
   `tracing::warn!` and recreate the slot once before proceeding. Never touch
   `repo_root`'s checkout to self-heal.

3. Add `stale_staging_slot_is_reclaimed` (`tests/squash_merge.rs`): pre-create
   a dirty `merge-staging` worktree + branch by hand, then run a normal merge;
   assert it succeeds and exactly one squash commit lands.

- **Depends on:** merge-staging-worktree
- **Done when:** the test passes; a merge after a simulated crash needs no
  manual cleanup; cargo test/clippy/fmt green.

---

## 0059 — Merge-phase budget

### merge-phase-cap-exemption — Stop the per-task wall-clock at approval

The per-task deadline (`supervisor.rs:1194–1202`) spans the merge-lock queue
wait, so with N concurrent approvals a finished task can be cancelled mid-merge
(`Some(Ok((id, None)))` arm, `supervisor.rs:1307–1354`) purely from queueing.

**Steps:**

1. In `crates/makina-core/src/actors/supervisor.rs`, give each driver an
   `Arc<AtomicBool>` `merge_phase` flag: created in the scheduler's fill phase
   (`supervisor.rs:1184–1203`), passed into `task_driver` as a parameter, and
   set (`store(true, SeqCst)`) at the top of the `ReviewVerdict::Approve` arm
   (`supervisor.rs:1724`) — *before* awaiting the merge lock, so the queue wait
   is covered.

2. Replace the `tokio::time::timeout(wall_clock, …)` wrapper with a pinned
   `tokio::select!` over the instrumented driver and
   `tokio::time::sleep(wall_clock)`: on elapse with `merge_phase` set,
   grace-await the driver to completion and return its real result; on elapse
   without it, return `(driver_id, None)` exactly as today. The
   `Some(Ok((id, None)))` arm and its benign-race handling
   (`supervisor.rs:1326–1331`) stay unchanged. Update the wall-clock doc
   prose (`supervisor.rs:49–53`, `:111–115`) to state the cap stops at
   approval.

3. In `crates/makina-core/tests/termination_caps.rs`, add
   `cap_elapse_during_merge_grace_awaits_completion`: install a `commit-msg`
   hook in the temp repo that sleeps past a small `caps.wall_clock_secs`, run a
   single task to approval; assert it terminates `Done` with its squash commit
   on `develop` (pre-fix this records `Failed` / `wall-clock-cap-reached`).
   Also assert an ordinary pre-merge stall still fails via the cap (existing
   cap tests stay green).

- **Depends on:** shield-merge-from-cancellation
- **Done when:** the new test passes and the existing wall-clock cap tests in
  `tests/termination_caps.rs` still pass; an approved task is never failed by
  merge-queue wait; cargo test/clippy/fmt green.

---

**End of plan 0018 TASKS.** When every "Done when" bullet is green, the
operator's checkout can never lose work to a Makina merge, a cancelled or
timed-out run can never leave `develop` mid-squash, and a task that earned its
approval can never be failed by the merge queue.
