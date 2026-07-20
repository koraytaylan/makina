---
id: validate-plan-bundle-and-dag
title: Validate The Complete Plan Bundle And DAG
workstream: "0001"
kind: task
depends_on: [define-task-document-schema]
gated: false
touches:
  - crates/makina-core/src/plan.rs
  - crates/makina-core/src/ingestion.rs
  - crates/makina-core/tests/plan_bundle.rs
  - crates/makina-core/tests/fixtures/plan-bundles/**
  - .claude/workflows/fixtures/plan-digest-v1.json
status: done
merged_as: "f31b531bd20a1e12b9a251a373f627a05ec5bcbd"
---
# Validate The Complete Plan Bundle And DAG

A valid task file is not yet a valid bundle: four documents must identify the
same plan, workstreams must agree, dependency IDs must resolve, and the root
roll-up must reflect task frontmatter. This task introduces `load_plan` and one
deterministic, multi-error validation report for every later consumer.

**Steps:**

1. Add `PlanKey`, `PlanDocument`, `PlanStatusDocument`, `MarkdownDocument`,
   `SourceDigest`, `PlanDigest`, `PlanValidationDiagnostic`, and `PlanValidationReport` to the
   typed plan module. Diagnostics contain a stable code, repository-relative
   path, optional field, and actionable message and sort deterministically.
2. Implement new-format candidate classification using `tasks/` as the sole
   discriminator. A directory containing `tasks/` must also have `SCOPE.md`,
   `ARCHITECTURE.md`, and `STATUS.md`, must reject a sibling `TASKS.md` as a
   mixed-format diagnostic, and must have non-empty, flat, ordinary task files.
   Any directory without `tasks/`—including a historical
   SCOPE/ARCHITECTURE/STATUS/`TASKS.md` quartet—returns `NotCandidate`; once
   `tasks/` exists, `TASKS.md` is an error rather than a fallback.
3. Implement `load_plan(source, plan_key)` with filesystem containment/symlink
   rejection at this stage, deterministic task ordering, and accumulated errors;
   keep parser logic independent of storage so the identity task can add a
   Git-tree source without a second parser. Define repository validation over a
   supplied reservation set whose canonical members are numbered directories
   and verified Phase-R registrations; the filesystem implementation initially
   supplies every configured-root directory, and the identity/ref task later
   supplies R-only registrations through the same API. A new-format candidate
   matching any new, historical, or registered reservation is invalid, while
   collisions solely among inert historical directories remain untouched.
4. Parse the plan folder prefix/title and the H1s in `SCOPE.md`,
   `ARCHITECTURE.md`, and `STATUS.md`. Require the basename to be exactly
   `NNNN-<slug>`, with ASCII slug
   `[A-Za-z0-9]+(?:-[A-Za-z0-9]+)*`, complete basename length at most 200 bytes,
   and the reversible derived `refs/heads/plan/<basename>` accepted by
   `git check-ref-format`; never sanitize a malformed folder into a different
   ref. Extract `- **NNNN — ...**` declarations
   from Scope's In-scope section and `## NNNN — ...` sections from Architecture,
   then require a one-to-one match. Require every task's workstream and filename
   prefix to select one declared workstream.
5. Validate unique task IDs and numeric prefixes, plan-local dependency
   resolution, no self/duplicate edges, and an acyclic graph. Report a stable
   cycle path rather than a generic topological-sort failure.
6. Parse fixed plan-status anchors (`Status`, `Goal`, `Root cause`, `Approach`,
   `Progress`, `Integration`, `Exceptions`, `Outcome`, last-updated). Validate
   counts, typed integration state/run/base/immutable-validation-base/mode/F
   evidence, and blocked/dropped reason
   coverage. Keep root roll-up validation as a separate repository-level check
   so a temporary/unregistered bundle can pass `load_plan`; validate a proposed
   or registered row's number/title/display status, explicit dropped progress,
   link, and exact Outcome while leaving historical rows untouched.
7. Compute the executable digest from plan identity and normalized immutable
   task fields/body. Prove in tests that dependency, gate, footprint, or body
   changes alter it while `status`, `merged_as`, plan-status progress, root row,
   and harmless canonical whitespace changes do not.
   Compute a second source digest over the normalized closed authored bundle,
   excluding coordinator-owned status fields/root row, so SCOPE/Architecture/
   instruction edits invalidate registration while status/provenance rebinding
   does not.
   Implement the exact SHA-256 v1 domain-separated, length-prefixed framing in
   `ARCHITECTURE.md` and consume the bootstrap golden vectors; include
   empty/list/non-ASCII/boundary-length cases and assert Python↔Rust parity.
8. Replace `ingestion.rs` graph-only checks with calls into the shared report
   where appropriate; do not keep a second set of task ID/cycle rules.
9. Add complete and malformed bundle fixtures covering missing files, empty or
   nested task directories, historical quartets without `tasks/`, a mixed
   `tasks/` + `TASKS.md` candidate, plan/workstream mismatch, new↔new and
   new↔historical and directory↔verified-R duplicate plan numbers (without
   historical↔historical noise),
   bad filename prefixes, unknown dependencies, stable cycle reporting,
   ref-unsafe/overlong plan basenames including `.lock`, trailing dot, `@{`,
   spaces, controls, and Git ref metacharacters,
   symlink/containment attacks, status/integration/count drift, registered and
   `Unregistered` roll-up states, exact outcome parity, and multiple simultaneous
   failures. Include this plan itself as a real-world integration-test input.

- **Done when:** `load_plan` returns one typed digest-bearing bundle with a reversible Git-ref-safe PlanKey or one stable multi-error report independent of root registration; repository validation distinguishes registered/unregistered plans and reserves both directory and supplied verified-R identities; every directory without `tasks/` is inert while mixed-format or number-colliding new candidates are rejected; immutable-validation-base, cross-file, DAG, containment, integration-status, outcome, and digest tests pass; and cargo fmt/clippy/test are green.
