# Scope — Plan 0030

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Today a run has **no integration branch of its own**. Every task forks its
worktree branch straight off `base_branch` and squash-merges straight back into
`base_branch` *the moment the Reviewer approves it* — mid-run, one commit at a
time.

Two concrete sites make this true:

- **Forking.** `WorktreeManager::create` (`worktree.rs`) runs
  `git worktree add {path} -b task/{plan_slug}--{task_id} {base_branch}` — the
  task branch is cut from `self.base_branch` (e.g. `develop`).
- **Merging.** On `ReviewVerdict::Approve` the task driver
  (`supervisor.rs`, the `MergeOutcome::Merged` arm) calls
  `ctx.squash_merger.squash_merge(&branch, &message)` under the `merge_lock`.
  The `SquashMerger` operates in `repo_root`, which has `base_branch` checked
  out, so the squashed commit lands **directly on `develop`** — and squash is the
  *only* mode; there is **no `[merge]` config and no final-merge hook**.

The result: `develop` is mutated **continuously during a run**. A half-finished
run (some tasks `Done`, others `Failed` or never reached) has already pushed
commits onto the shared base branch, so `develop` can be left in an
intermediate, never-the-whole-feature state. There is also no choice in *how*
the completed work lands — every approved task is squashed, no merge-commit
option, no "leave it for me to merge" option.

This plan gives each run its **own per-plan integration branch** and makes the
**final landing configurable**:

1. **Per-plan integration branch.** At run start, create `plan/{plan_slug}` off
   `base_branch`. Task worktrees fork from `plan/{plan_slug}`; each approved task
   squash-merges **into `plan/{plan_slug}`**, not `base_branch`. `develop` is
   untouched for the duration of the run.
2. **Configurable final merge.** When the run completes **and every task reached
   `Done`**, land `plan/{plan_slug}` into `base_branch` per a `[merge]` config
   `final = "squash" | "merge-commit" | "manual"` (default `squash`). On **any**
   failed task, leave the plan branch in place regardless of mode.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0083–0084):

- **0083 — Plan integration branch.** At run start create `plan/{plan_slug}` off
  `base_branch` and make it the run's checkout + merge target. Thread the plan
  branch into `WorktreeManager` so task worktrees fork from it; retarget the
  per-task `SquashMerger` to merge into `plan/{plan_slug}`. Keep the `merge_lock`;
  restore `base_branch` in `repo_root` at run end.
- **0084 — Configurable final merge.** Add `FinalMerge { Squash, MergeCommit,
  Manual }` (default `Squash`) under a `[merge]` config section
  (`config.rs`) + `validate()`. At run completion, only when **all** tasks are
  `Done`, land `plan/{plan_slug}` into `base_branch` per the mode; `Manual` (and
  any failed run) leaves the branch and surfaces its name.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Task branches fork off `base_branch`; no per-run integration branch | `0083` |
| Approved tasks squash **directly into `develop`** mid-run | `0083` |
| Squash is the only mode; no `[merge]` config; no final-merge hook | `0084` |
| No way to leave the integration branch for a human to merge | `0084` |

## Locked decisions

- **One integration branch per run, named `plan/{plan_slug}`.** Created off
  `base_branch` at run start (`run_graph_inner`, before the scheduler). The
  `plan_slug` already threaded into the run (`DriverContext::plan_slug`) is the
  branch's identity — `plan/0030-plan-branch-and-configurable-merge`.
- **Tasks fork from — and squash into — the plan branch, never `base_branch`.**
  `WorktreeManager::create` cuts `task/...` off the plan branch; the per-task
  `SquashMerger` merges into the plan branch. `develop` is **read-only** for the
  whole run; it is touched **only** at the final-merge step.
- **The merger checkout follows the plan branch.** The `SquashMerger` (and the
  final merge) operate in `repo_root`'s working tree, which must have the *target*
  branch checked out. The run **checks out `plan/{plan_slug}` in `repo_root` for
  the duration of the run** and **restores `base_branch` at the end** (after the
  final merge). The per-task `merge_lock` still serializes the squash step.
- **`[merge].final` is `FinalMerge { Squash, MergeCommit, Manual }`, default
  `Squash`.** `Squash` ⇒ one squash commit of `plan/{plan_slug}` onto
  `base_branch`; `MergeCommit` ⇒ `git merge --no-ff plan/{plan_slug}`; `Manual`
  ⇒ leave the branch. The final merge runs **only when every task is `Done`**.
- **Any failure leaves the plan branch — in every mode.** If even one task ends
  non-`Done`, the run does **not** touch `base_branch`; `plan/{plan_slug}`
  survives for inspection / retry, and its name is surfaced (a `RunReport` field
  / a status event) exactly as `Manual` does.
- **Additive + back-compat.** A config with no `[merge]` section behaves as
  `final = "squash"`, matching the squash-only behaviour modulo the new plan
  branch. The `RunReadyTasks` ask path (empty `plan_slug`) keeps the legacy
  fork-and-merge-into-`base_branch` shape so the task-21–25 tests are unchanged.

## Out of scope

- Pushing `base_branch` or `plan/{plan_slug}` to a remote, or opening a PR for
  the integration branch.
- Per-task PRs or stacked-PR workflows.
- Agent-driven conflict reconciliation on the **final** merge (the per-task
  conflict seam in `merge.rs` is unchanged; a final-merge conflict fails the
  landing and leaves the plan branch, like `Manual`).
- Deleting `plan/{plan_slug}` after a successful squash/merge-commit landing
  (left as a follow-up; the branch is harmless and aids audit).
- Any TUI change beyond consuming the surfaced plan-branch name (the new
  `RunReport` field / status event); rendering it is a later UX plan.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
