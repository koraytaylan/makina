# Plan authoring contract

Makina executes validated plan directories. The complete current example is [Plan 0048](0048-Per-Task-Plan-Documents-And-Transactional-Status/).

## Layout

```text
docs/plans/
├── STATUS.md
└── NNNN-ASCII-Slug/
    ├── SCOPE.md
    ├── ARCHITECTURE.md
    ├── STATUS.md
    └── tasks/
        └── WWSS-task-id.md
```

The directory name is the plan identity (`PlanKey`); its slug consists of non-empty ASCII-alphanumeric tokens separated by single hyphens. `NNNN` is unique across all numbered plan directories and registration refs. A directory becomes a new-format candidate when it contains `tasks/`; it is executable only when all three plan documents and at least one ordinary `tasks/*.md` file validate. Historical directories containing the former monolithic task-list file are pre-cutover records and are inert. There is no fallback or mixed format.

## Task documents

Each filename is `WWSS-<id>.md`: `WW` maps to workstream `00WW`, `SS` is `01..99`, and the lowercase kebab ID must equal frontmatter `id`. Frontmatter is closed and canonical:

```yaml
---
id: add-cache
title: Add Cache
workstream: "0001"
kind: task
depends_on: []
gated: false
touches:
  - src/cache.rs
status: planned
merged_as: ""
---
```

Allowed status values are `planned`, `in-progress`, `blocked`, `done`, and `dropped`. `done` requires the coordinator-recorded landing OID in `merged_as`; all other authored states keep it empty. Dependencies name plan-local IDs, contain no duplicates, and form a DAG. A gated task remains visible but is not automatically dispatched.

The body starts with `# <title>`, contains an ordered `**Steps:**` section, and ends with exactly one falsifiable `- **Done when:**` criterion. `touches` is the task's enforced mutation boundary: normalized repository-relative literal paths, `*` within one segment, or a terminal `/**`. Absolute paths, parent traversal, `.git`, runtime-state paths, and coordinator-owned status files are forbidden.

## Plan documents and status

`SCOPE.md` declares numbered workstreams under “In scope”; `ARCHITECTURE.md` contains exactly the matching `## NNNN — Name` sections. Plan `STATUS.md` contains the fixed goal, root-cause, approach, progress, integration, exceptions, outcome, and last-updated anchors. Root `docs/plans/STATUS.md` has one derived roll-up row per plan.

Authors own scope, architecture, task instructions, dependencies, gates, footprints, and narrative. Makina's lease-bound coordinator alone writes task lifecycle fields, landing OIDs, plan progress/integration/evidence, and the root row. Runtime checkpoints live outside the repository and contain only volatile scheduler/recovery state; they never override source documents or Git evidence.

New committed bundles first appear as `Unregistered`. Registration records exact Phase R and immutable validation-base provenance; only then are valid tasks `Ready`. Working-tree-only bundles are `AwaitingCommit`. Finalization uses coordinator-owned prepare/integrate/complete evidence and never advances a checked-out base branch.

## Validate

Use Makina's plan inspection/start surface against the plan directory, then run the repository gates:

```sh
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

Do not hand-edit coordinator-owned status after registration or use runtime JSON as completion evidence.
