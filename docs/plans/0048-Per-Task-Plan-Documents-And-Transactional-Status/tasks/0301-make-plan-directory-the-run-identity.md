---
id: make-plan-directory-the-run-identity
title: Make The Plan Directory The Application Identity
workstream: "0003"
kind: task
depends_on: [enforce-authored-gates-and-footprints]
gated: false
touches:
  - crates/makina-core/src/api.rs
  - crates/makina-core/src/orchestrator.rs
  - crates/makina-core/src/run_metadata.rs
  - crates/makina-core/src/task.rs
  - crates/makina-core/src/persist.rs
  - crates/makina-core/src/actors/**
  - crates/makina-core/tests/**
  - crates/makina/src/project_api.rs
  - crates/makina/src/event.rs
  - crates/makina/src/app.rs
  - crates/makina/src/ui.rs
  - crates/makina/src/placeholder.rs
  - crates/makina/src/browser.rs
  - crates/makina/src/main.rs
  - crates/makina/src/log.rs
  - crates/makina/tests/**
  - crates/makina-acp/tests/**
status: planned
merged_as: ""
---
# Make The Plan Directory The Application Identity

Even with typed source, Makina still exposes `TASKS.md` through core commands,
binary routing, views, discovery, runtime metadata, mocks, tests, and replay.
Because the public enum/field rename cannot leave half the workspace compiling,
this task performs one atomic core+binary cutover: one canonical `PlanKey`
travels through every layer and file-derived identity fallbacks disappear.

**Steps:**

1. Replace `Command::OpenRun { task_list_path }` with the plan-directory form
   (`OpenPlan { plan_dir }`, or an equivalently clear name) and rename
   `RunView`, `RunEntry`, `RunOpened`, reset, replay, and run-metadata fields to
   `plan_dir`/`PlanKey`. Update persisted schemas directly; there is no legacy
   compatibility reader in this unreleased project.
2. Derive plan slug from the complete plan folder name and keep `run_uid` as the
   attempt identity. Ensure runtime/checkpoint, branch, worktree, and log names
   combine plan identity with task/run identity so equal task IDs in different
   plans cannot collide.
3. Replace recursive `TASKS.md` discovery and `is_plan_tasks_path` with one
   plan scanner using the candidate classifier and `load_plan`. Take the union
   of numbered working/base candidates and `plan/*` refs; derive PlanKey from
   verified R trailers/tree for ref-only generated plans. Return typed
   AwaitingCommit/unregistered/ready/active entries and visible invalid
   candidates with shared diagnostics. Ignore historical directories without
   `tasks/` and no R. Count every numbered directory and R when reserving a plan
   number so a new candidate cannot collide with an inert historical directory
   or ref-only plan. For an active ref, treat R's digests as the chain root and
   accept a different current SourceDigest/PlanDigest only after folding
   verified first-parent task-disposition commits; unexplained source drift
   remains invalid.
4. Route direct open, reset, resume, historical reconstruction, and generated
   output through the same scanner/loader contract. Remove synthetic
   `tasks/<id>.md` and `TASKS.md` source paths from metadata.
5. Introduce the shared `PlanFileSource` boundary: a contained working-tree
   implementation and a read-only exact-Git-tree implementation that reads
   blobs without checking a ref out. Require ordinary files/directories and
   reject symlink/submodule entries. Implement
   `is_tracked_ordinary_file(path)` from the plan's immutable
   `validation_base_oid` for both Git and filesystem sources, even when the
   current source tree no longer contains a deliberately deleted artifact;
   never query the operator's current tree/index for this exception. Use it for history now; reconciliation
   will select active plan/base refs after durable integration state exists.
6. In the same commit, retarget `project_api.rs`, application events/state,
   placeholder/browser adapters, startup wiring, logging text, and the TUI to
   `PlanKey`. Preserve multi-folder containment/symlink rejection and remove
   every independent task-list preview parser.
7. Render typed plan/task data: validation/registration state, title/status/
   progress, workstream, kind, authored status, gate, dependencies, collision
   edges, footprint, body, and abbreviated repository-format `merged_as`.
   Active-ref switching and committed-status refresh land with reconciliation,
   after those ref/evidence types exist.
8. Define deterministic handling for a moved/renamed plan: it is a new
   `PlanKey`; an old active checkpoint is not silently attached. Preserve
   actionable recovery metadata for the old run.
9. Update every core/binary/ACP consumer, mock, test, and serialization fixture
   in the same task so workspace gates never observe a half-renamed public API.
   Cover valid, AwaitingCommit, unregistered, invalid, duplicate-number,
   historical, renamed, multi-folder, and same-task-ID/different-plan cases;
   restart discovery of an R-only generated plan with no working/base path; and
   an active plan whose current tip is claim/A/B/P while exactly one verified R
   remains on its expected first-parent lineage, including valid versus forged
   post-R ungate digest chains.

- **Done when:** one canonical repository-relative plan directory identifies discovery, open, execution, reset, resume, history, project routing, and TUI state; typed details render without reparsing Markdown; historical directories are inert; no production/test API uses `task_list_path`; and the complete workspace cargo gates are green in this single cutover commit.
