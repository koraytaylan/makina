---
id: retire-live-tasks-md-contract
title: Retire The Live TASKS.md Contract
workstream: "0005"
kind: chore
depends_on: [replace-guides-and-scaffold-templates]
gated: false
touches:
  - crates/makina-core/src/**
  - crates/makina-core/tests/**
  - crates/makina/src/**
  - crates/makina/tests/**
  - crates/makina-acp/tests/**
  - .claude/**
  - .makina/.gitignore
  - .makina/config.toml
  - .makina/tasks/0005-tui-ingestion-responsiveness-tasks.json
  - .makina/tasks/0008-gate-sandboxing-tasks.json
  - docs/spec/**
  - docs/plan-discovery.md
  - docs/demo/README.md
  - docs/plans/README.md
  - README.md
status: planned
merged_as: ""
---
# Retire The Live TASKS.md Contract

The cutover is complete only when an old parser cannot be reached accidentally
and a new producer cannot recreate the old format. This final chore removes the
obsolete surface, runs a narrow allowlisted repository search, and exercises
the whole lifecycle through final integration.

**Steps:**

1. Remove remaining live symbols and code paths built around a task-list file,
   including `task_list_path`, `is_plan_tasks_path`, `parse_plan_tasks`,
   `write_tasks_md`, `render_tasks_md`, obsolete normalizer/interpreter branches,
   synthetic task-entry paths, and preview-specific Markdown parsing.
2. Delete or rewrite old fixtures/tests/templates that create or open
   `TASKS.md`. Keep negative fixtures proving historical-only directories are
   ignored, completed historical plan content, and dated `docs/reviews/**`
   records as inert repository evidence. The clean-removal allowlist must label
   those review files as historical rather than treating their observations as
   normative instructions.
3. Delete the two explicitly listed tracked legacy `.makina/tasks/*.json`
   artifacts and the comments-only `.makina/.gitignore` through the schema's
   exact tracked-`kind: chore` deletion exception,
   resolving their ordinary-blob provenance from Plan 0048's immutable
   validation-base tree before the diff and retaining that source after the
   files disappear. Require name-status exactly `D`, reload/validate the
   completed plan from Git after deletion, and prove the exception does not
   degrade into a writable `.makina` glob. Through the separate exact tracked-
   config variant, update `.makina/config.toml` comments to describe plan-ref
   task landings plus P/F/C rather than direct task-branch→base squash, and
   require status exactly `M`; keep the config file itself. Never glob/delete
   live or untracked runtime state.
4. Add an allowlisted search test/script for `TASKS.md`. Matches are permitted
   only under completed historical plans, explicit cutover prose saying the
   format is inert, dated historical review evidence, and negative cutover
   tests. Fail on matches in Rust/JavaScript code, active prompts/workflows,
   scaffolds, help, or normative examples.
5. Add a full temporary-repository acceptance test: create/generate a bundle;
   discover/open it by plan directory; schedule footprint-safe parallel tasks;
   observe claim status commits; land one task with a full OID and bookkeeping
   commit; interrupt/reconcile another landing; finalize; and verify task files,
   plan status, root roll-up, runtime checkpoint, retained plan ref, and final
   provenance all agree. Assert the checkpoint and every mutable run artifact
   live below the external state root, poison repository-local `.makina` with a
   plausible checkpoint that must be ignored, and prove missing/unusable user
   state fails before mutation instead of falling back to the repository.
6. Run historical-plan discovery against this repository and assert old
   `TASKS.md` directories neither execute nor flood discovery with invalid
   errors, while Plan 0048 loads as the first executable new-format example.
7. Invoke and acceptance-test the post-finalization cleanup capability already
   compiled into the exact 0502-B server/client. Retain the bootstrap recovery
   copy, exact-B Rust binary, artifact
   hash/build manifest, authentication/control endpoint metadata, and sentinels
   through every crash before C and throughout stable Stage/Manual retention.
   Only after C is verified on base and `Close` has no live worker/Git child may
   it emit the manifest-bound cleanup permit, exit, and let the reaping client
   remove those exact owned paths; make response loss idempotent and preserve
   everything on an ambiguous session/evidence mismatch.
   Test server death after initial Ready and during 0503/0504, restart from the
   retained exact-B binary, retained Stage/Manual, crash before/after C, and
   cleanup response loss. This is a post-C runtime action, not a conditional
   repository source commit.
8. Run formatting and lint cleanup after deletion. Confirm there are no dead
   compatibility types, serde aliases, deprecated docs, or task-list terminology
   in production/test APIs, including stale comments in audit/persistence,
   browser/logging, placeholder, and startup code.
9. Run every gate: `cargo fmt --all -- --check`,
   `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
   `cargo test --workspace --all-features`, scaffold integration tests, and all
   `.claude/workflows/*.test.mjs` tests.

- **Done when:** no live producer or consumer can create, discover, parse, preview, execute, persist, or resume `TASKS.md`; historical occurrences are inert and narrowly allowlisted; Plan 0048 still validates after its exact tracked legacy artifacts/obsolete `.gitignore` are deleted and `.makina/config.toml` is safely updated in place; no runtime state is read from or written inside the repository; all handoff artifacts survive Ready-to-C and retained-finalization recovery and are cleaned idempotently only after verified C/stable close; the end-to-end per-task/status/provenance lifecycle passes; and every Rust and JavaScript gate is green.
