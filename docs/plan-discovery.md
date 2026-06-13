# Plan Auto-Discovery

Makina includes automatic discovery of plan directories, making it easy to organize and manage your work using a recommended directory structure.

## Convention

Makina recommends organizing plans under a `docs/plans/NNNN-*/` layout, where each plan directory contains three metadata files:

- **`SCOPE.md`** — the plan's scope, rationale, and decisions
- **`ARCHITECTURE.md`** — the concrete deltas and design
- **`TASKS.md`** — the executable task list with verifiable acceptance criteria

Example structure:

```
docs/plans/
├── 0001-Initial/
│   ├── SCOPE.md
│   ├── ARCHITECTURE.md
│   └── TASKS.md
└── 0027-Plan-Auto-Discovery/
    ├── SCOPE.md
    ├── ARCHITECTURE.md
    └── TASKS.md
```

## Open Default

When you press `o` in Makina to open a task list, Makina performs **plan auto-discovery**:

1. **Scan** — Makina scans `repo_root/docs/plans/*/` for directories containing **both** `SCOPE.md` and `ARCHITECTURE.md`.
2. **Discover** — Each matching directory is discovered as a plan and displayed in a sortable list (sorted by directory name, so `0001-…` precedes `0027-…`).
3. **Activate** — Press `↑`/`↓` (or `j`/`k`) to navigate the list and `Enter` to open a plan.

If Makina discovers plans under `docs/plans/`, those discovered plans become the **default open target** instead of the bare file browser. This surfaces the recommended structure as the primary workflow.

### Routing by `has_tasks`

Each discovered plan is marked with a `has_tasks` flag based on the presence of a `TASKS.md` file:

- **`has_tasks=true`** — The plan contains a `TASKS.md` file. Pressing `Enter` opens the plan via `OpenRun`, reading and interpreting the task list normally. Status: `Interpreting {slug}…`.
- **`has_tasks=false`** — The plan **lacks** a `TASKS.md` file (e.g., authoring in progress). Pressing `Enter` routes to the planner-generate path instead of `OpenRun`, allowing you to generate the task graph from the `SCOPE.md` and `ARCHITECTURE.md` alone. Status: `{slug}: no TASKS.md — planner will generate the graph`.

This allows in-flight authoring workflows: write your `SCOPE.md` and `ARCHITECTURE.md`, then have Makina generate the task graph before you manually author it.

### Browser Fallback

If your repository **does not** have a `docs/plans/` directory or has one with no plan directories inside it, Makina falls back to the traditional file browser behavior: press `o` and browse the current working directory to select a task-list file. This ensures compatibility with non-convention repositories.

## Implementation

Plan auto-discovery is implemented via:

- **`discover_plans(repo_root)`** — A pure scan helper in `makina_core::orchestrator` that returns `Vec<PlanEntry>`. See the implementation for details.
- **`PlanEntry`** — A struct in `makina_core::orchestrator` holding:
  - `dir: PathBuf` — the plan directory path
  - `slug: String` — the plan slug (derived from `plan_slug`)
  - `has_tasks: bool` — whether `dir/TASKS.md` exists

Both symbols are public exports from `makina-core` and can be reused by other consumers (CLI, tests, etc.) that need to discover plans programmatically.

## Cross-references

- **Plan 0027** — ["Plan Auto-Discovery"](../plans/0027-Plan-Auto-Discovery/) covers the full scope, architecture, and task graph for this feature.
- **Slug derivation** — See `run_slug` and `plan_slug` in `crates/makina-core/src/orchestrator.rs` for how plan identity is derived from directory names.
- **Structured text convention** — See [`docs/spec/structured-text-convention.md`](spec/structured-text-convention.md) for the full grammar of task lists.
