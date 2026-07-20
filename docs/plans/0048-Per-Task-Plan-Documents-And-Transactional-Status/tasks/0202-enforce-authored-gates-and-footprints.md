---
id: enforce-authored-gates-and-footprints
title: Enforce Authored Gates Statuses And Footprints
workstream: "0002"
kind: task
depends_on: [project-plan-documents-into-runtime]
gated: false
touches:
  - crates/makina-core/src/task.rs
  - crates/makina-core/src/dependency.rs
  - crates/makina-core/src/actors/supervisor.rs
  - crates/makina-core/tests/authored_scheduling.rs
  - crates/makina-core/tests/edge_inference.rs
status: done
merged_as: "1d8ee6545badfe9bd7d29578d219c252c7a4d2d6"
---
# Enforce Authored Gates Statuses And Footprints

The runtime projection carries plan metadata only if scheduling actually honors
it. This task defines how authored statuses and gates seed a run and makes the
explicit `touches` footprint the primary source of collision ordering.

**Steps:**

1. Add a source-to-runtime seeding function with exhaustive handling for
   `planned`, `in-progress`, `done`, `blocked`, and `dropped`. Keep reconciliation
   hooks for Git/live-driver evidence instead of treating enum names as runtime
   state by coincidence.
2. Prevent automatic dispatch of `gated: true` and `dropped` tasks. A gated task
   stays visible and non-terminal; a dropped task is terminal and remains in
   plan counts but does not satisfy `depends_on`. Keep a dependent blocked until
   it is explicitly dropped too. Once claim/run/disposition evidence freezes
   the registered source, moving that work requires a newly authored plan rather
   than an in-place edge edit; before the freeze, a committed edit must go
   through verified R2.
3. Seed `done` only through the evidence checker supplied by the landing
   workstream. Until that checker exists, use an injected/testable verifier and
   fail closed. Do not allow a runtime checkpoint to satisfy the verifier.
4. Refactor `EdgeInferrer` so normalized `touches` paths/globs create collision
   edges for exact, parent/child, and intersecting-pattern overlap. Preserve the
   distinction between authored dependency edges and inferred collision edges.
5. Implement the portable footprint grammar exactly: literal paths,
   single-segment `*`, and terminal recursive `/**`; reject `?`, classes,
   braces, extglobs, negation, backslashes, and ambiguous patterns.
6. Before review acceptance and immediately before Phase A, diff the task tip
   from its recorded base (including rename source/destination and submodule
   entries) and require every changed path to match `touches`. Reserved status
   paths fail regardless of footprint. Return undeclared changes for correction
   on the task branch. Once a claim exists no footprint/source edit is legal, so
   broader work stops and moves to a newly authored plan; before the first claim
   an author may instead commit the edit and use the explicit R2 refresh path.
   Never land it silently. Parse
   name/status records NUL-safely with rename/copy detection. Typed
   `TrackedMakinaConfig` accepts only exact `.makina/config.toml`, status `M`,
   and an ordinary-file result; `TrackedMakinaDeletion` accepts only status `D`
   for its one exact path. Reject additions, wrong `M`/`D`, config deletion,
   artifact modification, `R*`, `C*`, `T`, unmerged, and submodule changes even
   when the pathname matches.
7. Retain textual path inference only as a supplemental warning/fallback for an
   empty or suspicious footprint. It may add conservative ordering, but it may
   not remove or weaken an explicit collision.
8. Make ordering deterministic and cycle-safe: compute one topological linear
   extension of the authored DAG with Kahn-ready ties broken by task numeric/
   source order, then orient every collision between incomparable tasks from
   earlier to later in that fixed order. Assert the augmented graph remains
   acyclic and explain the chosen edge/order in diagnostics and TUI metadata;
   never choose directions independently per pair.
9. Add scheduler/graph/diff tests for each status, gated tasks, dropped dependencies,
   blocked downstream work, exact and glob collisions, disjoint footprints,
   authored ordering precedence, textual fallback, supported/forbidden glob
   forms, undeclared edits, renames, copies, type changes, submodules, every
   rejected tracked-`.makina` statuses plus accepted config modification and
   accepted deletion at both diff checkpoints, reserved paths, and deterministic
   graph output under shuffled input. Include an adversarial three-task overlap
   where an authored edge opposes numeric order and prove inferred edges cannot
   close a cycle.

- **Done when:** no gated or dropped task can be dispatched, authored status seeds runtime through explicit rules, the documented `touches` grammar deterministically orders overlaps and rejects every undeclared landed path, collision/dependency edges remain distinguishable, and cargo fmt/clippy/test are green.
