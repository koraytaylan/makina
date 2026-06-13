# Scope — Plan 0029

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Today **all** of Makina's per-project state lives inside the target repo's own
`.makina/` directory. The pure path helpers in `crates/makina-core/src/paths.rs`
root everything there: committed artifacts (`config_file` → `.makina/config.toml`,
`task_graph` → `.makina/tasks/{slug}.json`) **and** transient runtime state
(`run_dir` → `.makina/runs/{run_id}`, `audit_log`, `task_log`, `run_logs_dir`,
and `worktree` → `.makina/worktrees/{plan_slug}--{task_id}`). The two kinds are
held apart only by `.makina/.gitignore`, which lists `/runs/` and `/worktrees/`.

That single-directory layout has two concrete problems:

1. **Runtime state pollutes the repo.** Worktrees and run logs are checkouts and
   transient junk, yet they sit inside the project tree. Every Makina run writes
   into the repo being worked on; `git worktree add` registers paths under the
   repo; a stray `git clean -dffx` or a forgotten `.gitignore` entry exposes them
   to the user's working tree. A worktree *under* the repo it checks out is also
   conceptually backwards — nested checkouts of the same repo.
2. **Worktree dir + branch names are long and collision-by-construction.** The
   scheme `{plan_slug}--{task_id}` (e.g.
   `.makina/worktrees/0003-runtime-and-tui-hardening--sample-task`, branch
   `task/0003-runtime-and-tui-hardening--sample-task`) bakes the *entire* plan
   slug and task id into both the directory and the branch. These are long,
   awkward to `cd` into, and noisy in `git branch` output.

This plan **moves runtime state out of the repo** into a per-project namespace
under `~/.makina/projects/`, **keeps committed artifacts where they are**, and
**shortens** the worktree directory + branch names to a bounded, still-unique
form. It is **pre-release and non-backwards-compatible**: there is **no
migration** — existing on-disk runtime state is abandoned.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0080–0082):

- **0080 — Relocate runtime state.** Add `paths::project_ns(repo_root)` and
  `paths::state_root(repo_root)` (rooted at `$HOME/.makina/projects/{ns}`) and
  re-root the transient helpers — `worktree`, `run_dir`, `run_logs_dir`,
  `task_log`, `audit_log` — at `state_root` instead of `repo_root/.makina`.
  **Keep** `config_file` and `task_graph` under `repo_root/.makina` (committed
  artifacts). Update `WorktreeManager::worktree_path`, `crates/makina/src/log.rs`,
  and every caller.
- **0081 — Shorten worktree names.** Add
  `paths::short_worktree_name(plan_slug, task_id)` producing
  `{plan#}-{task-trunc}-{hash4}` and use it in `paths::worktree` and in
  `worktree.rs`'s branch construction so **create and remove derive the same
  name**. Stay `[a-z0-9-]`-valid.
- **0082 — Update gitignore + test.** With `worktrees/` and `runs/` gone from the
  repo's `.makina/`, drop the `/runs/` + `/worktrees/` rules from
  `.makina/.gitignore` (it now guards only committed `config.toml` + `tasks/`) and
  rewrite `persist.rs::gitignore_worktrees_ignored_tasks_not_ignored` to assert
  the new reality.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Worktrees + run logs live inside the target repo (`.makina/runs`, `.makina/worktrees`) | `0080` |
| Run metadata (`run.json`) + audit log are transient yet committed-adjacent in-repo | `0080` |
| Worktree dir + branch names bake in the full `{plan_slug}--{task_id}` | `0081` |
| `.makina/.gitignore` + its invariant test still guard in-repo runtime state | `0082` |

## Locked decisions

- **Per-project namespace, hashed.** Runtime state lives under
  `$HOME/.makina/projects/{ns}/`, where `ns = "{repo_basename}-{hash6}"` and
  `hash6` is a **6-char lowercase-hex** hash of the **canonicalized absolute**
  repo path. The basename is for human legibility; the hash disambiguates two
  repos that share a basename. Read `$HOME` exactly the way `config.rs` already
  does — `std::env::var_os("HOME")`, **no `dirs` crate** (confirmed: config.rs's
  `home_dir()` uses `HOME` directly and `dirs` is not a dependency).
- **Move the transient, keep the committed.** `worktrees/` and `runs/` (run logs
  **and** run metadata `run.json` — both transient, both gitignored today) move to
  `~/.makina/projects/{ns}/`. `config.toml` and `tasks/*.json` are **committed
  artifacts** and **stay** at `repo_root/.makina/`. `config_file` and `task_graph`
  are therefore unchanged.
- **Shorter name scheme.** Worktree dir + branch use
  `{plan#}-{task-trunc}-{hash4}` — e.g. `0016-sidebar-tree-nav-a1b2`, branch
  `task/0016-sidebar-tree-nav-a1b2`. `plan#` is the leading 4-digit number parsed
  from `plan_slug`; `task-trunc` is the task id truncated to a small fixed budget
  (~20 chars); `hash4` is a **4-char hex** hash of the full
  `"{plan_slug}--{task_id}"` for uniqueness. The result stays `[a-z0-9-]`-valid so
  `validate_task_id`'s charset rules still hold for the composite.
- **Determinism + symmetry.** `short_worktree_name` and `project_ns` are pure and
  deterministic for a given input, so `create` and `remove` compute the **same**
  path and branch, and replay/log readers resolve the same `state_root`.
- **No migration, no compatibility shim.** Pre-release: old in-repo `.makina/runs`
  / `.makina/worktrees` are simply abandoned. No reader looks in the old location.

## Out of scope

- Migrating or importing pre-existing in-repo runtime state (none is read).
- Changing where committed artifacts live (`config.toml`, `tasks/*.json` stay).
- A configurable state root or `$XDG_STATE_HOME` support (always `$HOME/.makina`).
- Garbage-collecting or pruning `~/.makina/projects/` across projects.
- Any change to the worktree create/remove git mechanics beyond the new name.
- The global config file (`~/.makina/config.toml`) — already handled by config.rs.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
