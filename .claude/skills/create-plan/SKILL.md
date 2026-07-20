---
name: create-plan
description: Author a canonical per-task plan bundle through Makina's Rust plan-contract. Use for create-plan or requests to author numbered plans; not for implementing a plan.
---

# create-plan

Launch the repository workflow; do not reproduce plan parsing, rendering, validation, numbering, Git, registration, or status logic in prompts.

- Preferred: `Workflow({ name: "create-plan", args: { brief: "<topic>", ... } })`
- Fallback: `Workflow({ scriptPath: ".claude/workflows/create-plan.js", args: { ... } })`

Arguments are `brief`, `count` (default `1`, maximum `5`), `dryRun`, `commit` (default `false`), and optional author/critic model overrides. A bare string is the brief.

The workflow asks agents only for structured blueprint content. The Rust contract assigns collision-free numbers and canonically renders and validates `SCOPE.md`, `ARCHITECTURE.md`, thin `STATUS.md`, and `tasks/NNSS-id.md` documents. Task frontmatter has exactly `id`, `title`, `workstream`, `kind`, `depends_on`, `gated`, `touches`, `status`, and `merged_as`; IDs are plan-local, dependencies form a DAG, footprints are portable repository globs, and every body ends in a falsifiable **Done when** criterion.

`commit:false` creates a validated working candidate and returns `AwaitingCommit`; it is not executable. `commit:true` requires an exact clean target-base context, commits only the closed bundle plus derived root row, and registers the same source transactionally. Response-loss retries verify and reuse the exact commit and registration.

Report the returned plan directory, task count, diagnostics, source/registration OIDs, and whether the outcome is `AwaitingCommit` or `Ready`. The workflow never pushes.
