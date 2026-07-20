---
id: project-plan-documents-into-runtime
title: Project Plan Documents Into Runtime State
workstream: "0002"
kind: task
depends_on: [validate-plan-bundle-and-dag]
gated: false
touches:
  - crates/makina-core/src/**
  - crates/makina-core/tests/**
  - crates/makina/src/**
  - crates/makina/tests/**
  - crates/makina-acp/tests/provider_and_role_wiring.rs
  - docs/spec/runtime-artifact-schema.md
status: done
merged_as: "37c2b73794b990792d799dc8c73573f5f7fe554f"
---
# Project Plan Documents Into Runtime State

`CoreApi::open_run` currently returns early when a runtime JSON artifact exists,
so an old cache can bypass the current source plan. This task makes validated
documents the mandatory input and limits JSON to compatible, volatile scheduler
state.

**Steps:**

1. Extend `Task`/`TaskGraph` with the immutable authored metadata required by
   execution and presentation: source task path, workstream, kind, gate,
   normalized footprint, authored status, merge evidence, and plan executable
   digest. Keep authored status distinct from the existing runtime lifecycle.
2. Implement a deterministic `PlanDocument -> TaskGraph` projection. Preserve
   authored dependencies and source ordering, and retain enough source identity
   to map every runtime transition back to exactly one task document.
3. Refactor the open path so it always loads and validates plan source before
   reading a checkpoint. During open, inspect source, Git, worktrees, and
   checkpoint into a read-only reconciliation plan; do not repair or persist.
   Define the start hook that will reread/apply after the repository lease
   exists. Persist `PlanKey`, executable digest, task IDs, and source paths only
   in that apply path and overlay volatile fields when identifiers agree.
4. Make `paths::state_root` and its derived mutable-runtime helpers fallible,
   remove the HOME-less `repo_root/.makina` fallback, and reject any resolved
   state root inside the repository. Update all workspace callers. Read-only
   inspection reports an unavailable external state location; execution fails
   before refs, worktrees, status, or agents mutate anything. Add containment,
   missing-HOME/user-state, unwritable-root, and symlink-alias tests.
5. Move checkpoint load/persist/archive behavior in `persist.rs` behind this
   source-aware contract and relocate it to a plan-qualified, safely encoded
   path below external `state_root/checkpoints`. Do not read or migrate old
   repository-local `.makina/tasks/*.json`. A clean stale checkpoint is planned
   for archive/replacement; one containing active worktree/branch references is
   preserved with recovery instructions. No mutation happens until lease-bound
   start, and `Done` is never restored solely from JSON.
6. Remove the deterministic `StructuredTextInterpreter` and its monolithic
   source linter from the execution path. Leave model-assisted generation
   callable only through the atomic bundle task; it must not emit a graph
   directly.
7. Update run metadata and runtime-artifact documentation to describe plan
   directories as source identity and JSON as a checkpoint. Do not introduce
   compatibility aliases for `task_list_path`; the plan-directory API cutover
   task will rename the outer surfaces.
8. Add tests for read-only open, lease-bound reread/apply, compatible resume, executable-field edit,
   bookkeeping-only edit, task addition/removal, plan move/rename, malformed
   source with a valid cache, JSON-only done state, and active-work mismatch.

- **Done when:** every open/resume parses current plan documents before accepting a checkpoint, only a matching digest restores volatile runtime fields, every checkpoint/runtime helper fails closed instead of writing inside the repository, unchecked JSON can never override source or Git truth, the old structured-text execution parser is unused, and cargo fmt/clippy/test are green.
