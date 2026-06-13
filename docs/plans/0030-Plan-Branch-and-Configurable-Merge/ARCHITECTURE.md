# Architecture — Plan 0030

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). This plan touches only the `makina-core` crate
> (`config.rs`, `worktree.rs`, `merge.rs`, `actors/supervisor.rs`,
> `orchestrator.rs`, `api.rs`).

## Current shape (what exists)

- **Task branches fork off `base_branch`.** `WorktreeManager`
  (`crates/makina-core/src/worktree.rs`) holds `repo_root` + `base_branch` and
  its `create(plan_slug, task_id)` runs
  `git -C {repo_root} worktree add {path} -b task/{plan_slug}--{task_id} {base_branch}`
  (the `run_git(&["worktree","add", …, "-b", &branch, &self.base_branch])` call).
  There is **no plan branch** anywhere.
- **Approved tasks squash directly into `base_branch`.** In the task driver
  (`actors/supervisor.rs`, the `ReviewVerdict::Approve` arm), `branch =
  format!("task/{}--{task_id}", ctx.plan_slug)`, then under the **develop merge
  lock** `let _merge_guard = ctx.merge_lock.lock().await;
  ctx.squash_merger.squash_merge(&branch, &message).await`. On
  `MergeOutcome::Merged` the task goes `InReview → Done` (`ReviewerApproved`).
- **`SquashMerger`** (`crates/makina-core/src/merge.rs`) holds `repo_root` +
  `base_branch`; `squash_merge` runs `git -C {repo_root} merge --squash
  {task_branch}` then `git -C {repo_root} commit --allow-empty -m {msg}`, and on
  any failure calls `restore_base_branch` (`merge --abort` / `reset --hard HEAD`
  / `clean -fd`). It operates on **whatever branch `repo_root` has checked
  out** — expected to be `base_branch`. Squash is the **only** mode.
- **Run wiring.** `run_graph` → `run_graph_inner`
  (`actors/supervisor.rs`) builds the `SquashMerger` from
  `worktree_manager.{repo_root, base_branch}`, builds the `DriverContext`
  (which carries `worktree_manager`, `squash_merger`, `merge_lock`, `config`,
  `plan_slug`), runs `scheduler`, then derives the aggregate `RunStatus`. The
  `scheduler` returns `RunReport { outcomes, failed_tasks }`.
- **Config.** `crates/makina-core/src/config.rs`: `CapsConfig`,
  `GlobalConfig`, `ProjectConfig`, and the resolved `Config { …, gates,
  base_branch }`. `Config::resolve(global, project)` merges them; `validate()`
  checks caps/concurrency/`base_branch`/gates. There is **no `[merge]`
  section**.
- **Run completion.** `orchestrator.rs`: `start_run` `tokio::spawn`s
  `run_graph(…)` and **discards** its `RunReport` (`let _ = run_graph(…)`),
  then calls `finalize_run_status(run)`, which derives `Completed` (all tasks
  `Done`) / `Failed` from the live graph and writes `run.json`.
- **Events.** `api.rs` `Event` enum already has `TaskIdle { run, task,
  idle_secs }` (plan 0015) as a precedent for a small status event; `RunStatus`
  is `{ Pending, Running, Paused, Completed, Failed }`.

## 0083 — Plan integration branch

Goal: each run executes on its own `plan/{plan_slug}` branch — tasks fork from
it and squash into it — so `base_branch` is untouched until the final merge
(0084).

### A new git helper for the plan branch + checkout (`worktree.rs`)

`WorktreeManager` already owns `repo_root`; give it the verbs to create the
plan branch and to switch the `repo_root` checkout. These are thin wrappers
over `run_git` (which already returns `WorktreeError` on non-zero exit):

```rust
impl WorktreeManager {
    /// Create `plan/{plan_slug}` off `base_branch` and check it out in
    /// `repo_root`. Idempotent-on-restart: if the branch already exists, just
    /// check it out (a reclaimed run resumes on the same integration branch).
    pub async fn create_plan_branch(&self, plan_slug: &str) -> Result<String, WorktreeError> {
        let branch = format!("plan/{plan_slug}");
        if self.branch_exists(&branch).await? {
            self.run_git(&["checkout", &branch], /* human */).await?;
        } else {
            self.run_git(&["checkout", "-b", &branch, &self.base_branch], /* human */).await?;
        }
        Ok(branch)
    }

    /// Check out `branch` in `repo_root` (used to restore `base_branch` at run end).
    pub async fn checkout(&self, branch: &str) -> Result<(), WorktreeError> {
        self.run_git(&["checkout", branch], /* human */).await.map(|_| ())
    }
}
```

Note `branch_exists` already exists (private) and is reused.

### Fork task worktrees from the plan branch, not `base_branch` (`worktree.rs`)

`create` currently passes `&self.base_branch` as the `worktree add` start
point. Give `WorktreeManager` a **fork point** distinct from the merge base.
The cheapest change that keeps the ask-path legacy behaviour: add an explicit
`fork_branch: Option<String>` field (or thread a parameter) so `create` cuts
from the plan branch when set, else falls back to `base_branch`:

```rust
pub struct WorktreeManager {
    pub repo_root: PathBuf,
    pub base_branch: String,
    /// Branch new task branches fork from. `None` ⇒ fork from `base_branch`
    /// (legacy ask-path). The run sets this to `plan/{plan_slug}`.
    pub fork_branch: Option<String>,
}
```

In `create`, replace `&self.base_branch` in the `worktree add` argv with
`self.fork_branch.as_deref().unwrap_or(&self.base_branch)` (and the same in the
human-readable command string). The reclaim-on-conflict path resets the slot
"fresh off the fork point" with the same expression. `WorktreeManager::new`
keeps its two-arg signature with `fork_branch: None`; the run sets the field
(or use a `with_fork_branch(self, branch)` builder to keep `new` callers — the
many tests — unchanged).

### Retarget the per-task merge into the plan branch (`merge.rs` + `supervisor.rs`)

The `SquashMerger` merges into whatever `repo_root` has checked out. Because the
run now **checks out `plan/{plan_slug}` in `repo_root`** (below), the existing
`squash_merge` lands the squashed commit on the **plan branch** with **no change
to `merge.rs`'s git commands** — only `SquashMerger::base_branch` (a label used
in docs/messages) should be set to the plan branch so diagnostics read true:

```rust
// run_graph_inner, after create_plan_branch:
let squash_merger = SquashMerger::new(
    worktree_manager.repo_root.clone(),
    plan_branch.clone(), // was worktree_manager.base_branch.clone()
);
```

`restore_base_branch` (name is historical) does `reset --hard HEAD` / `clean
-fd` on the checked-out branch — which is now the plan branch — so a per-task
conflict still leaves the **plan branch** clean, never corrupting it, and never
touches `develop`. Keep the `merge_lock` exactly as-is (it serializes the squash
step against the single shared `repo_root` checkout).

### Wire the plan branch into the run (`supervisor.rs::run_graph_inner`)

Before building the `DriverContext` (and thus before the `scheduler`):

```rust
// 1. Create + check out the per-plan integration branch off base_branch.
let plan_branch = worktree_manager
    .create_plan_branch(&plan_slug)
    .await
    .map_err(|e| format!("failed to create plan branch: {e}"))?;

// 2. Task worktrees fork from the plan branch.
let worktree_manager = worktree_manager.with_fork_branch(plan_branch.clone());

// 3. The per-task merger targets the plan branch (now checked out in repo_root).
let squash_merger = SquashMerger::new(worktree_manager.repo_root.clone(), plan_branch.clone());

// 4. A SEPARATE merger for the FINAL merge (0084), targeting the TRUE base branch.
let final_merger = SquashMerger::new(
    worktree_manager.repo_root.clone(),
    worktree_manager.base_branch.clone(),
);
```

The two mergers are distinct instances with distinct `base_branch` targets:
`squash_merger` targets `plan/{plan_slug}` (the per-task merge target, checked
out during the run), `final_merger` targets the true `base_branch` (its `final_*`
methods check it out before landing). See 0084 — keep their checkout contexts and
`base_branch` targets aligned so `restore_base_branch` cleans the right branch.

The `SupervisorArgs.worktree_manager` handed to the spawned `Supervisor` actor
must be the fork-branch-bearing clone too (so the hub's own `on_start`-derived
merger and any actor-path worktree calls agree). At the end of
`run_graph_inner` — after the scheduler returns and **after** the 0084 final
merge — restore `base_branch` so `repo_root` is left as callers expect:

```rust
// Run end: leave repo_root back on base_branch (best-effort; warn on failure).
if let Err(e) = worktree_manager.checkout(&worktree_manager.base_branch).await {
    tracing::warn!(error = %e, "failed to restore base branch in repo_root");
}
```

The ask path (`run_ready_tasks` / `driver_context`, empty `plan_slug`) does
**not** create a plan branch: keep `fork_branch: None` and the merger targeting
`base_branch`, so the task-21–25 integration tests (`tests/squash_merge.rs`
asserting commits land on `develop`) are byte-for-byte unchanged.

**Defensive note — why the run-end restore is safe.** The final checkout/restore
of `base_branch` in `repo_root` at run end is safe because by that point the
`scheduler` has **already returned**: there are no concurrent task drivers and no
in-flight per-task merges, and the `merge_lock` is therefore **free** (nothing
holds it). The shared `repo_root` checkout is no longer contended, so the
`checkout(&base_branch)` can run unguarded; the checkout is **idempotent** (if
`base_branch` is already current, or already restored after the final merge, it is
a no-op). This same ordering is what lets the 0084 final merge run between the
scheduler return and this restore — the final merge (via `final_merger`) checks
out the true `base_branch`, lands the plan branch, and leaves `repo_root` on
`base_branch`; the run-end restore then idempotently re-asserts that state. And as
noted above, the ask path (`run_ready_tasks`, empty `plan_slug`, no `fork_branch`)
keeps the **legacy behavior** — fork from `base_branch`, merge into `base_branch`,
no plan branch, no run-end restore — so existing tests are unaffected.

## 0084 — Configurable final merge

Goal: when the run completes with **every task `Done`**, land `plan/{plan_slug}`
into `base_branch` per `[merge].final`; otherwise (or in `Manual`) leave the
branch and surface its name.

### `FinalMerge` enum + `[merge]` config (`config.rs`)

Add the enum and a `MergeConfig`, default `Squash`, in the global layer (and a
project override mirroring the `CapsOverride` pattern), then thread the resolved
value into `Config`:

```rust
/// How a completed run lands its `plan/{plan_slug}` branch into `base_branch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FinalMerge {
    /// One squash commit of the plan branch onto base_branch (the legacy shape).
    #[default]
    Squash,
    /// `git merge --no-ff plan/{slug}` — a true merge commit on base_branch.
    MergeCommit,
    /// Leave plan/{slug}; surface its name for a human to merge.
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MergeConfig {
    /// How the completed run's integration branch lands into base_branch.
    pub final_: FinalMerge, // serde rename to `final` (reserved word).
}
```

`final` is a Rust keyword, so the field is `final_` with
`#[serde(rename = "final")]`. Add `pub merge: MergeConfig` to `GlobalConfig`
(with `#[serde(default)]`) and a `pub merge: Option<MergeConfig>` to
`ProjectConfig`; in `Config::resolve` pick `project.merge.unwrap_or(global.merge)`
and store `pub merge: MergeConfig` on `Config`. `validate()` needs no new check
(the enum is closed — any TOML value outside the three is a serde parse error),
but document the `[merge] final = "..."` shape in the `validate` doc list for
parity with `base_branch`.

### Final-merge git verbs (`merge.rs`)

The plan-branch landing is the same `repo_root`-checkout discipline as the
squash merger, but the *target* is `base_branch` and the *source* is the plan
branch — so we must first check out `base_branch`, then merge the plan branch
in. Add a small `FinalMerger` (or extend `SquashMerger`) with one method per
non-manual mode, reusing `restore_base_branch` on conflict:

```rust
impl SquashMerger {
    /// Squash-land `plan_branch` onto `base_branch` as ONE commit.
    /// (Checks out base_branch first, then `merge --squash` + `commit`.)
    pub async fn final_squash(&self, plan_branch: &str, message: &str)
        -> Result<MergeOutcome, MergeError> { /* checkout base; reuse squash_merge body */ }

    /// `git merge --no-ff {plan_branch}` onto base_branch (a real merge commit).
    pub async fn final_merge_commit(&self, plan_branch: &str, message: &str)
        -> Result<MergeOutcome, MergeError> {
        // git -C {repo} checkout {base_branch}
        // git -C {repo} merge --no-ff -m {message} {plan_branch}
        // on non-zero exit: restore_base_branch(); Ok(Conflict { … })
    }
}
```

Here `self.base_branch` is the **true** base branch (`develop`) — so build the
final merger from the worktree manager's original `base_branch`, **not** the
plan branch used by the per-task `squash_merger`. Both share `restore_base_branch`
(now operating on `base_branch`, since these methods check it out first).

**Construct a SEPARATE `SquashMerger`-style instance for the final merge** —
e.g. a `final_merger` distinct from the per-task `squash_merger`:

```rust
// run_graph_inner — built alongside the per-task merger, but with a different target:
let squash_merger = SquashMerger::new(repo_root.clone(), plan_branch.clone());      // per-task: targets plan/{slug}
let final_merger  = SquashMerger::new(repo_root.clone(), worktree_manager.base_branch.clone()); // final: targets the TRUE base
```

Do **not** reuse the per-task `squash_merger` for the final merge: its
`base_branch` is set to the plan branch (so its diagnostics + `restore_base_branch`
assume the plan branch is checked out), whereas the final merge checks out and
targets the *true* `base_branch`. Conflating them would make `restore_base_branch`
operate on the wrong branch. Concretely, **`SquashMerger.base_branch` is "the
current merge target"**: the plan branch for the per-task merger, the true base
for `final_merger`. And `restore_base_branch` (`reset --hard HEAD` + `clean -fd`)
operates on **whatever is checked out** — it does not switch branches — so the
`repo_root` checkout state and the merger's `base_branch` target **must agree**.
The per-task merger runs while `plan/{slug}` is checked out; `final_merger`'s
methods check out the true `base_branch` first, so its `restore_base_branch`
cleans the true base. Keep the two instances and their checkout contexts aligned.

### Run the final merge at completion (`supervisor.rs::run_graph_inner`)

After `scheduler` returns its `RunReport` and before restoring `base_branch`,
decide the landing. The gate is **all tasks `Done`** — reuse the same predicate
`aggregate_run_status(&graph) == api::RunStatus::Completed` (it already means
"every task is `Done`"):

```rust
let report = scheduler(ctx, config.concurrency).await;

let all_done = {
    let g = graph.lock().await;
    aggregate_run_status(&g) == api::RunStatus::Completed
};

let plan_branch_left = if all_done {
    match config.merge.final_ {
        FinalMerge::Squash      => { final_merger.final_squash(&plan_branch, &msg).await?; None }
        FinalMerge::MergeCommit => { final_merger.final_merge_commit(&plan_branch, &msg).await?; None }
        FinalMerge::Manual      => Some(plan_branch.clone()),
    }
} else {
    // ANY failure: never touch base_branch; leave the plan branch.
    Some(plan_branch.clone())
};
```

A `MergeOutcome::Conflict` from a final `Squash`/`MergeCommit` is treated like
`Manual` (leave the branch, surface its name) rather than corrupting
`base_branch` — `restore_base_branch` already cleaned the working tree.

### Surface the plan-branch name (`supervisor.rs` `RunReport` + `api.rs` Event)

Add a field to `RunReport` so the orchestrator can report which branch (if any)
was left for a human, and emit a status event so the TUI sees it live:

```rust
// supervisor.rs RunReport:
pub struct RunReport {
    pub outcomes: Vec<(TaskId, TaskState)>,
    pub failed_tasks: Vec<(TaskId, String)>,
    /// `Some(plan_branch)` when the run left its integration branch unmerged
    /// (Manual mode, a failed run, or a final-merge conflict); `None` when it was
    /// squashed/merge-committed into base_branch.
    pub plan_branch_left: Option<String>,
}
```

`scheduler` does not know the plan branch, so its single `RunReport`
construction site (the `None => Ok(RunReport { … })` arm) initializes
`plan_branch_left: None`; `run_graph_inner` overwrites it on the returned report
after the final-merge decision (the ask path, which never calls
`run_graph_inner`, simply keeps `None`). When `Some(branch)`, emit a new

```rust
// api.rs Event:
RunIntegrationBranchLeft { run: RunId, branch: String },
```

just before the aggregate `RunStatusChanged`. The orchestrator's `start_run`
spawn currently does `let _ = run_graph(…)`; capture the report and (best-effort)
thread `plan_branch_left` into the persisted `run.json` / a `tracing::info!` so
the surfaced name is observable without re-deriving it.

## Testing notes

- **0083 (plan branch).** Drive the full Supervisor loop over a temp repo (the
  `tests/squash_merge.rs` harness pattern) **through `run_graph`** with a real
  `plan_slug`: assert `plan/{slug}` exists after start; assert the task branch
  forked from `plan/{slug}` (its merge-base with `plan/{slug}` is `plan/{slug}`'s
  tip-at-fork, not `develop`); assert the approved task's squash commit landed on
  `plan/{slug}`; assert `develop`'s HEAD is **unchanged** from before the run.
- **0084 (final merge).** With all tasks `Done`: `Squash` ⇒ exactly one new
  commit on `develop` whose subject is the plan message; `MergeCommit` ⇒ a merge
  commit (`git rev-list --merges` shows the new commit; two parents); `Manual`
  ⇒ `develop` HEAD unchanged, `plan/{slug}` still exists, `RunReport
  .plan_branch_left == Some("plan/{slug}")`. With a forced failed task: in
  **every** mode `develop` is unchanged and the plan branch survives.
- **Config.** `FinalMerge` parses from `[merge] final = "squash" | "merge-commit"
  | "manual"`; an absent `[merge]` resolves to `Squash`; an unknown value is a
  parse error; the project layer overrides the global.
- All tests keep `cargo test`, `clippy --all-targets -D warnings`, and
  `fmt --check` green. Use the existing temp-repo `git` helpers; no real remote.

## Interaction with prior plans

- Reuses the `plan_slug` already threaded by the audit/worktree plans
  (`DriverContext::plan_slug`) as the integration-branch identity — no new slug
  derivation. Reuses `merge.rs`'s `restore_base_branch` invariant for both the
  per-task (now plan-branch) merges and the final merge. Reuses
  `aggregate_run_status` (== `Completed` ⇔ all `Done`) as the all-tasks-done
  gate, keeping the "any failure leaves the branch" rule consistent with how the
  scheduler already classifies a run.
