# Makina Plan 0030 — Per-Plan Integration Branch & Configurable Final Merge

Give each run its own `plan/{plan_slug}` integration branch: task worktrees fork
from it and approved tasks squash **into** it, so `base_branch` (`develop`) is
untouched mid-run. At run completion — and only when every task reached `Done` —
land the plan branch into `base_branch` per a configurable `[merge].final` mode
(squash / merge-commit / manual). Any failed task leaves the plan branch in every
mode.

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

## 0083 — Plan integration branch

### plan-integration-branch — Create `plan/{slug}`; fork + merge tasks into it

At run start, create `plan/{plan_slug}` off `base_branch`, check it out in
`repo_root`, fork task worktrees from it, and retarget the per-task squash-merge
into it — so `develop` is untouched mid-run. Keep the `merge_lock`; restore
`base_branch` at run end.

**Steps:**

1. In `crates/makina-core/src/worktree.rs`, add to `WorktreeManager` an optional
   fork point: `pub fork_branch: Option<String>` (a task branch forks from
   `fork_branch` when `Some`, else from `base_branch`). Keep `WorktreeManager::new`
   two-arg (set `fork_branch: None`) and add
   `pub fn with_fork_branch(self, branch: String) -> Self`. In `create`, replace
   the `&self.base_branch` start-point in the `worktree add` argv **and** in the
   human-readable command string with
   `self.fork_branch.as_deref().unwrap_or(&self.base_branch)` (also in the
   reclaim-on-conflict recreate path, so a reclaimed slot resets off the fork
   point).

2. In `worktree.rs`, add `pub async fn create_plan_branch(&self, plan_slug: &str)
   -> Result<String, WorktreeError>`: compute `branch = format!("plan/{plan_slug}")`;
   if `self.branch_exists(&branch).await?` run `git checkout {branch}` else run
   `git checkout -b {branch} {base_branch}` (both via `run_git`, with a
   human-readable command string); return `branch`. Add
   `pub async fn checkout(&self, branch: &str) -> Result<(), WorktreeError>` that
   runs `git checkout {branch}` via `run_git`.

3. In `crates/makina-core/src/actors/supervisor.rs`, in `run_graph_inner` (before
   building the `DriverContext`): call `worktree_manager.create_plan_branch(&plan_slug)`
   to get `plan_branch`; rebind `worktree_manager =
   worktree_manager.with_fork_branch(plan_branch.clone())`; build the per-task
   `squash_merger` with `SquashMerger::new(worktree_manager.repo_root.clone(),
   plan_branch.clone())` (it now targets the checked-out plan branch). Use this
   fork-bearing `worktree_manager` clone for both `SupervisorArgs` and the
   `DriverContext`. Keep `merge_lock` unchanged.

4. In `merge.rs`, no git-command change is needed (the merger operates on the
   checked-out branch, now `plan/{slug}`); update the `SquashMerger.base_branch`
   doc to note it is the *target* branch the merger lands onto (plan branch during
   a run). The per-task `squash_merge` call site in the `ReviewVerdict::Approve`
   arm is unchanged.

5. At the end of `run_graph_inner` (after the scheduler returns; the final merge in
   0084 will slot in before this), restore `base_branch` in `repo_root`:
   `worktree_manager.checkout(&worktree_manager.base_branch).await` (best-effort;
   `tracing::warn!` on error — never fail the run on the restore).

6. Leave the `RunReadyTasks` ask path (`run_ready_tasks` / `driver_context`, empty
   `plan_slug`) with `fork_branch: None` and the merger targeting `base_branch`, so
   `tests/squash_merge.rs` (commits land on `develop`) stays unchanged.

7. Add an integration test in `crates/makina-core/tests/` (new
   `plan_branch.rs`, reusing the temp-repo `git` helpers from `squash_merge.rs`)
   driving `run_graph` with a real `plan_slug`:

   ```rust
   #[tokio::test]
   async fn run_creates_plan_branch_and_merges_into_it() {
       /* setup_temp_repo (develop); NoopBackend dev + approve verdict;
          run_graph(graph, WorktreeManager::new(repo,"develop"), …, plan_slug="0030-demo", …);
          assert branch_exists(repo, "plan/0030-demo");
          assert the approved task's squash commit is on plan/0030-demo (its HEAD subject references the task);
          assert head_sha(develop) == develop_before (develop UNCHANGED mid/after-run, since final-merge is a later task);
          assert the task branch forked from plan/0030-demo, not develop */
   }
   ```

- **Depends on:** —
- **Done when:** the test passes; a run creates and checks out `plan/{plan_slug}`
  off `base_branch`; task worktrees fork from `plan/{plan_slug}`; an approved task
  squash-merges into `plan/{plan_slug}`; `base_branch` is unchanged during the run
  (no final merge yet); `repo_root` is restored to `base_branch` at run end; the
  ask path is unchanged; cargo test/clippy/fmt green.

---

## 0084 — Configurable final merge

### final-merge-config — Add `[merge] final` (`FinalMerge`) and resolve it

**Steps:**

1. In `crates/makina-core/src/config.rs`, add
   `pub enum FinalMerge { Squash, MergeCommit, Manual }` with
   `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]`,
   `#[serde(rename_all = "kebab-case")]`, and `#[default] Squash`. Add
   `pub struct MergeConfig { #[serde(rename = "final")] pub final_: FinalMerge }`
   with `#[derive(Debug, Clone, Serialize, Deserialize, Default)]` +
   `#[serde(default)]`.

2. Add `pub merge: MergeConfig` (with `#[serde(default)]`) to `GlobalConfig` (and
   its `Default`), and `pub merge: Option<MergeConfig>` to `ProjectConfig`. In
   `Config::resolve`, compute `merge = project.merge.unwrap_or(global.merge)` and
   store `pub merge: MergeConfig` on `Config`. Mention the `[merge] final = "…"`
   shape in the `validate()` doc list (no new runtime check — the closed enum
   rejects unknown values at parse time).

3. Add tests in `config.rs`:

   ```rust
   #[test]
   fn final_merge_defaults_to_squash() { /* empty global+project => config.merge.final_ == FinalMerge::Squash */ }
   #[test]
   fn final_merge_parses_each_mode() { /* [merge] final="merge-commit" => MergeCommit; "manual" => Manual; "squash" => Squash */ }
   #[test]
   fn final_merge_rejects_unknown() { /* GlobalConfig::from_toml_str with final="rebase" => Err(ConfigError::Parse) */ }
   #[test]
   fn project_merge_overrides_global() { /* global final="manual", project final="squash" => resolved Squash */ }
   ```

- **Depends on:** plan-integration-branch
- **Done when:** the tests pass; `[merge] final` parses to `FinalMerge`; an absent
  `[merge]` resolves to `Squash`; an unknown value is a parse error; the project
  layer overrides the global; cargo test/clippy/fmt green.

### configurable-final-merge — Land `plan/{slug}` into `base_branch` by mode

At run completion, only when **every task is `Done`**, land `plan/{plan_slug}`
into `base_branch` per `config.merge.final_`; otherwise leave the branch and
surface its name.

**Steps:**

1. In `crates/makina-core/src/merge.rs`, add to `SquashMerger`:
   `pub async fn final_squash(&self, plan_branch: &str, message: &str) ->
   Result<MergeOutcome, MergeError>` (checks out `self.base_branch`, then runs the
   existing `merge --squash {plan_branch}` + `commit --allow-empty -m {message}`
   body; conflict path uses `restore_base_branch`); and
   `pub async fn final_merge_commit(&self, plan_branch: &str, message: &str) ->
   Result<MergeOutcome, MergeError>` (checks out `self.base_branch`, then
   `git merge --no-ff -m {message} {plan_branch}`; on non-zero exit
   `restore_base_branch().await?` then `Ok(MergeOutcome::Conflict { details })`).
   Build a **SEPARATE** `SquashMerger`-style instance for the final merge — e.g.
   `let final_merger = SquashMerger::new(repo_root.clone(),
   worktree_manager.base_branch.clone());` — constructed with the run's **true**
   `base_branch` as its target, **distinct from** the per-task `squash_merger`
   (which targets `plan/{plan_slug}`). Do **not** reuse the per-task merger:
   `SquashMerger.base_branch` is "the current merge target" (the plan branch for
   per-task merges; the true base for `final_merger`), and `restore_base_branch`
   (`reset --hard HEAD` + `clean -fd`) operates on **whatever is checked out** —
   it does not switch branches — so the checkout state and the merger's `base_branch`
   target must agree (the `final_*` methods check out the true `base_branch`
   first, so `final_merger.restore_base_branch` cleans the true base).

2. In `crates/makina-core/src/actors/supervisor.rs`, extend `RunReport` with
   `pub plan_branch_left: Option<String>` (doc: `Some(plan_branch)` when the run
   left its integration branch unmerged — Manual, a failed run, or a final-merge
   conflict; `None` when squashed/merge-committed). Initialize it `None` at the
   single `scheduler` construction site (the `None => Ok(RunReport { … })` arm of
   the `fatal_error` match); `run_graph_inner` overwrites it after the final-merge
   decision.

3. In `run_graph_inner`, after `scheduler` returns and **before** restoring
   `base_branch` (step 5 of plan-integration-branch): compute
   `all_done = { let g = graph.lock().await; aggregate_run_status(&g) ==
   api::RunStatus::Completed }`. If `all_done`, match `config.merge.final_`:
   `Squash`/`MergeCommit` call the corresponding `final_*` method (on a
   `MergeOutcome::Merged` set `plan_branch_left = None`; on `Conflict` set
   `Some(plan_branch)`); `Manual` sets `Some(plan_branch)`. If not `all_done`, set
   `Some(plan_branch)` and do **not** touch `base_branch`. Stamp
   `report.plan_branch_left` accordingly.

4. In `api.rs`, add `Event::RunIntegrationBranchLeft { run: RunId, branch: String }`.
   In `run_graph_inner`, when `plan_branch_left` is `Some(branch)`, emit it via
   `control.emit(...)` just before the aggregate `RunStatusChanged`.

5. In `crates/makina-core/src/orchestrator.rs` `start_run`, capture the
   `run_graph` report (replace `let _ = run_graph(…)` with binding the result) and
   `tracing::info!` the `plan_branch_left` (best-effort surfacing; do not change
   `finalize_run_status`'s graph-derived status).

6. Add an integration test in `crates/makina-core/tests/plan_branch.rs`:

   ```rust
   #[tokio::test]
   async fn final_squash_lands_one_commit_on_base() {
       /* all tasks Done; [merge] final="squash"; after run: develop gained exactly one new commit (subject = plan message); plan_branch_left == None */
   }
   #[tokio::test]
   async fn final_merge_commit_creates_a_merge_commit() {
       /* final="merge-commit"; after run: develop HEAD is a merge commit (two parents / appears in `git rev-list --merges`); plan_branch_left == None */
   }
   #[tokio::test]
   async fn final_manual_leaves_branch_and_reports_name() {
       /* final="manual"; develop HEAD unchanged; branch_exists(repo,"plan/{slug}"); report.plan_branch_left == Some("plan/{slug}") */
   }
   #[tokio::test]
   async fn failed_task_leaves_branch_in_every_mode() {
       /* force one task Failed (reviewer reject to cap, or a hard error); for each of Squash/MergeCommit/Manual: develop HEAD unchanged AND plan/{slug} still exists AND plan_branch_left == Some */
   }
   ```

- **Depends on:** final-merge-config
- **Done when:** the four tests pass; with all tasks `Done`, `Squash` lands one
  squash commit on `base_branch`, `MergeCommit` creates a merge commit, and
  `Manual` leaves `plan/{slug}` + reports its name; a run with **any** failed task
  leaves `plan/{slug}` and does not touch `base_branch` in **every** mode;
  `plan_branch_left` is `None` only on a successful squash/merge-commit landing;
  cargo test/clippy/fmt green.

---

**End of plan 0030 TASKS.** When every "Done when" bullet is green, each run lives
on its own `plan/{plan_slug}` branch — `develop` stays stable for the whole run —
and the completed work lands exactly the way the project configured: a single
squash commit, a merge commit, or a branch left for a human to merge.
