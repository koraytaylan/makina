# Scope — Plan 0027

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Makina already standardises on a `docs/plans/NNNN-*/` layout — every plan in
this very repo is a directory holding `SCOPE.md`, `ARCHITECTURE.md`, and
`TASKS.md`, and the orchestrator already keys its run/plan slugs off that shape
(`run_slug` / `plan_slug` derive identity from the **parent directory name** of
the `TASKS.md` file, `orchestrator.rs:120,163`). Yet the only way to *start* a
run from the TUI is the bare file browser: press `o`, get a `read_dir` of the
process CWD, and hand-walk the tree to a `TASKS.md` file
(`event.rs:238` `OpenBrowser` → `read_dir_event`, `event.rs:248`
`BrowserActivate` → `execute(OpenRun{ task_list_path })`).

Two concrete problems:

1. **The recommended structure is invisible.** A user who has organised their
   work under `docs/plans/NNNN-*/` gets no help from Makina: the file browser
   opens at the CWD and treats `docs/plans/0027-Plan-Auto-Discovery/TASKS.md`
   like any other `.md` file buried three levels down. The convention the whole
   product is built around is never *surfaced* as the default.
2. **A plan dir without a `TASKS.md` is a dead end.** A directory that has
   `SCOPE.md`/`ARCHITECTURE.md` but no task graph yet cannot be opened at all —
   `OpenRun` needs a task-list file — so the in-flight authoring workflow (write
   the scope, let the planner generate the graph) has nowhere to land in the UI.

This plan adds a **`makina-core` plan-discovery helper** that scans
`docs/plans/*/` for the convention, and **surfaces the discovered plans on the
existing sidebar tree** (plan 0016's `visible_tree_nodes` "Runs & Tasks" surface)
as the recommended default instead of a raw file browse. A dir **without**
`TASKS.md` is still listed (flagged `has_tasks=false`) so it can route to the
planner-generate path rather than vanishing.

## In scope

Work items in [TASKS.md](TASKS.md) (workstream 0078):

- **0078 — Discover plans under `docs/plans`.** Add a pure, IO-light
  `discover_plans(repo_root) -> Vec<PlanEntry { dir, slug, has_tasks }>` helper in
  `makina-core` that scans `docs/plans/*/` for the `SCOPE.md`/`ARCHITECTURE.md`/
  `TASKS.md` convention, and surface the result in the TUI open flow: a "Plans"
  section on the plan-0016 sidebar tree (and as the default starting point of the
  file browser) presents discovered plans as the recommended default. A dir
  without `TASKS.md` is listed with `has_tasks=false` and routes to plan 0028's
  planner-generate instead of `OpenRun`.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| The `docs/plans/NNNN-*/` convention is never surfaced as the open default | `0078` |
| A plan dir without `TASKS.md` cannot be opened or routed anywhere | `0078` |
| The file browser opens at a bare CWD with no plan awareness | `0078` |

## Locked decisions

- **Discovery lives in `makina-core`, not the TUI.** `discover_plans` is a
  pure-ish scan helper next to `run_slug`/`plan_slug` (same module, same slug
  derivation) so it has one source of truth for *what a plan dir is* and any
  consumer (TUI, future CLI, tests) shares it. The only IO is `read_dir` +
  existence checks; no parsing of `SCOPE`/`ARCHITECTURE` content.
- **Convention = directory under `docs/plans/` that has `SCOPE.md` AND
  `ARCHITECTURE.md`.** `TASKS.md` is *not* required to be listed — it is recorded
  as the `has_tasks` flag. Dirs missing the SCOPE/ARCHITECTURE pair (e.g. a bare
  `assets/` folder) are **ignored**, so non-plan directories never pollute the
  list.
- **The slug is `plan_slug`'s slug.** `PlanEntry::slug` is exactly what
  `orchestrator::plan_slug` would derive for that dir's `TASKS.md`
  (`0027-plan-auto-discovery`), so a discovered entry and the run it opens share
  one identity. Reuse the existing fn; do not re-implement kebab sanitisation.
- **Surface on the existing 0016 tree, do not replace the browser.** Discovered
  plans appear as a top "Plans" affordance on the sidebar (the `visible_tree_nodes`
  "Runs & Tasks" surface already shipped) and as the file browser's default start
  dir; the raw `o`/`BrowserActivate` browse path is preserved as the escape hatch.
- **`has_tasks=false` routes to planner-generate, not `OpenRun`.** Activating a
  plan that has a `TASKS.md` issues `OpenRun{ task_list_path: dir/TASKS.md }`
  exactly as today. Activating one **without** `TASKS.md` routes to plan 0028's
  planner-generate entry (auto-generates the graph from `SCOPE`/`ARCHITECTURE`);
  this plan only flags the entry and wires the branch — it does not implement the
  planner itself.
- **No persistence, no caching.** `discover_plans` is cheap and re-runs on open;
  nothing about the discovery is stamped or stored by *this* plan (the
  `[discovery]` stamp for LLM-driven project discovery is a sibling plan, out of
  scope here).

## Out of scope

- LLM-driven *project* discovery (gate/role-constraint inference, the
  `[discovery]` config stamp, the Ctrl+P re-run) — that is a sibling plan; this
  plan only finds **plan directories**, not project settings.
- Implementing the planner-generate path itself (plan 0028); 0078 only flags
  `has_tasks=false` entries and routes them at the seam.
- Parsing or validating `SCOPE.md` / `ARCHITECTURE.md` content (we check
  existence only).
- Recursing below `docs/plans/*/` or supporting an alternate plans root.
- Merging discovered gates into `[[gates]]`, per-role `system_prompt`, or run
  metrics — unrelated sibling work.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
