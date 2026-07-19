---
id: replace-guides-and-scaffold-templates
title: Replace Plan Guides And Scaffold Templates
workstream: "0005"
kind: task
depends_on: [port-plan-authoring-workflows]
gated: false
touches:
  - docs/plans/README.md
  - docs/plan-discovery.md
  - docs/demo/README.md
  - docs/spec/structured-text-convention.md
  - docs/spec/planner-model-mechanism.md
  - docs/spec/project-discovery.md
  - docs/spec/runtime-artifact-schema.md
  - docs/spec/examples/sample-run.tasks.json
  - README.md
  - crates/makina/src/templates/plans_readme.md
  - crates/makina/src/templates/todo/plan_scope_md
  - crates/makina/src/templates/todo/plan_architecture_md
  - crates/makina/src/templates/todo/plan_status_md
  - crates/makina/src/templates/todo/plan_tasks_md
  - crates/makina/src/templates/todo/plan_task_md
  - crates/makina/src/templates/todo/plans_status_md
  - crates/makina/src/scaffold.rs
  - crates/makina/src/folder_init.rs
  - crates/makina/tests/scaffold_integration_test.rs
status: planned
merged_as: ""
---
# Replace Plan Guides And Scaffold Templates

Project creation and normative documentation still present one monolithic task
file as Makina's plan law. This task publishes the new law, points to Plan 0048
as the full example, and makes every newly scaffolded project valid on first
open.

**Steps:**

1. Create `docs/plans/README.md` with the concise normative contract: folder
   layout; exact task frontmatter; filename/workstream rules; body form;
   dependency/gate/footprint/status semantics; root/plan status ownership; and
   the validation commands. Link this plan as the complete executable example.
2. Replace the live structured-text spec with the typed bundle contract or
   rename its subject clearly while updating inbound links. Explain that
   historical `TASKS.md` files are pre-cutover records and are not executable;
   do not document a migration/fallback path.
3. Update README/help copy and `templates/plans_readme.md` to describe plan
   directories and per-task files. Include the distinction between authored
   status, off-repository runtime checkpoints, coordinator-owned Git evidence,
   immutable validation-base provenance, R registration, and the
   AwaitingCommit→Unregistered→Ready states in the smallest useful form.
4. Replace the todo scaffold's monolithic template with a complete valid plan:
   scope, architecture, planned status with correct count, and one task file in
   `tasks/` whose frontmatter/body/footprint match the generated project, plus a
   root `docs/plans/STATUS.md` row with exact outcome/progress/link parity.
   Remove the old `plan_tasks_md` include once the new templates are wired.
5. Update `scaffold.rs` and `folder_init.rs` to create directories/files safely
   through create-only logic, preserve existing destinations, and never leave a
   partial advertised plan after failure. After the initial closed bundle is
   committed on `develop`, call the shared `RegisterPlan` transaction so the
   sample has exact Phase R and is `Ready`, then create/check out a create-only
   user branch (for example `workspace`) at that same initial commit so
   `develop` is not checked out and automatic F/C can later advance it safely.
   Report both branch roles clearly. Registration response loss reuses R; any
   pre-publication failure cleans only the newly owned scaffold destination.
   Keep generated relative links valid.
6. Extend scaffold integration tests to run the shared Rust plan loader over the
   output, verify the exact R makes the task `Ready`, verify no `TASKS.md`
   exists, and cover existing destination, nested target, registration response
   loss, and failure cleanup cases. Run the sample plan through claim/A/B/P/F/C
   in a temporary repository and prove it completes onto `develop` while the
   checked-out `workspace` HEAD/index/worktree stay byte-for-byte unchanged.
7. Update `docs/plan-discovery.md`, model/project/runtime specs, sample runtime
   JSON, and demo documentation so candidate=`tasks/`, Unregistered, PlanKey,
   checkpoint-only JSON, and typed status/evidence all agree.
8. Search normative/live documentation for stale commands, screenshots, and
   terminology. Leave completed historical plan content unchanged and add only
   a top-level cutover note where readers would otherwise mistake it for current
   authoring law.

- **Done when:** one authoritative guide describes the new format and external runtime/provenance model; a fresh scaffold contains a committed, exactly registered, loader-valid and end-to-end-finalizable per-task plan, leaves `develop` free behind a user workspace branch, and contains no `TASKS.md`; live README/spec/template links agree; historical records remain untouched; and cargo fmt/clippy/test are green.
