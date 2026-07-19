---
id: define-task-document-schema
title: Define The Typed Task Document Schema
workstream: "0001"
kind: task
depends_on: [bootstrap-per-task-plan-execution]
gated: false
touches:
  - Cargo.toml
  - Cargo.lock
  - crates/makina-core/Cargo.toml
  - crates/makina-core/src/lib.rs
  - crates/makina-core/src/plan.rs
  - crates/makina-core/tests/plan_task_document.rs
  - crates/makina-core/tests/fixtures/plan-documents/**
status: planned
merged_as: ""
---
# Define The Typed Task Document Schema

Makina currently extracts task meaning from Markdown conventions in multiple
modules. This task creates the single typed boundary for one task document and
its canonical renderer. It deliberately does not discover whole plans or build
the dependency DAG; the next task composes validated task documents into a
bundle.

**Steps:**

1. Add `serde-saphyr = "0.0.29"` and `sha2 = "0.10.9"` as explicit workspace
   dependencies and consume them from `makina-core`; SHA-256 is not available
   in the Rust standard library and the following bundle task must not invent a
   subprocess or ad hoc implementation. Confirm the resolved crates build with
   the workspace's `rust-version = "1.85"`; do not raise the MSRV or add a
   deprecated YAML implementation to work around a failure.
2. Create `crates/makina-core/src/plan.rs` and export it from `lib.rs`. Define
   validated newtypes and enums for `TaskId`, `WorkstreamId`, `TaskSequence`,
   `RepoPattern`, `GitObjectId`, `TaskKind`, and `AuthoredTaskStatus`, plus
   `TaskFrontmatter` and `TaskDocument` as specified in `ARCHITECTURE.md`.
   Represent the two tracked-`.makina` exceptions as explicit pattern variants,
   not ordinary writable paths: exact `.makina/config.toml` is modify-only
   `TrackedMakinaConfig`, while any other exact path is deletion-only
   `TrackedMakinaDeletion` and requires `kind: chore`. In authoring mode with
   blank provenance, retain inert candidate forms; only exact-base validation
   may resolve them to executable variants.
3. Implement task parsing over a `PlanFileSource` boundary, with a contained
   filesystem implementation first and room for an exact Git-tree source, using a
   constrained YAML boundary: one frontmatter document at byte zero; exact
   required fields; duplicate/unknown keys rejected; aliases, anchors, tags,
   merge keys, directives, and multiple documents rejected; bounded input,
   nesting, scalar, and collection sizes; UTF-8 and ordinary-file checks. Put
   `is_tracked_ordinary_file(path)` on this boundary as well. The filesystem
   source receives an optional immutable validation-base commit provenance;
   generated/unregistered sources may receive none. Resolve tracked ordinary blobs
   from that exact commit, not from the source's current tree or whichever
   index happens to be active in the operator checkout. This provenance remains
   usable after the task deletes the named artifact. Missing provenance permits
   only the authoring candidate above, never execution.
4. Validate field-local invariants: kebab-case IDs; workstreams `0001..0099`;
   task sequences `01..99`; filename `{WS-prefix}{sequence}-{id}.md` with `NN`
   mapping exactly to workstream `00NN`; non-empty title; unique dependency
   entries; and safe repository-relative `touches`. Add an explicit serde codec
   mapping authored `merged_as: ""` to `None`; define `touches` grammar as
   literal paths, single-segment `*`, and terminal `/**` only; validate a
   non-empty merge value as a full 40- or 64-hex OID matching the repository's
   reported SHA-1/SHA-256 object format. Reject `.git`, ordinary `.makina`,
   absolute paths, parent traversal, NULs, and active plan/root status paths.
   Permit exact non-glob `.makina/config.toml` as the config candidate and any
   other exact non-glob `.makina` path only for `kind: chore` as a deletion
   candidate, in both cases only when
   `PlanFileSource::is_tracked_ordinary_file` proves that exact artifact in the
   plan's coordinator-bound validation-base commit.
5. Enforce status coherence: `done` requires `merged_as`; every other authored
   status requires it to be empty. Keep Git reachability checks out of this
   field-local parser—the bundle/resume layers own repository evidence.
6. Validate the Markdown body without rewriting it: the H1 equals `title`, an
   ordered `**Steps:**` section exists, and the document's final semantic block
   is a non-empty `- **Done when:**` criterion.
7. Implement a canonical frontmatter renderer that preserves the body bytes
   exactly. Add a narrow mutation API for coordinator-owned `status` and
   `merged_as` updates rather than exposing arbitrary YAML edits.
8. Add fixtures and table-driven tests for a valid document, every enum, quoted
   workstream, empty-string↔`None`, SHA-1/SHA-256 merge evidence and wrong-format
   OIDs, exact tracked-`.makina/config.toml` modification and chore-deletion
   exceptions, duplicate and
   unknown keys,
   forbidden YAML features, malformed delimiters, resource limits, unsafe
   paths, filename/body mismatches, current-index/current-tree/base-tree
   disagreement, post-deletion revalidation from the retained validation base,
   blank→candidate→exact-base config/deletion resolution, untracked/wrong-base
   rejection, config status exactly `M`, config deletion/type/submodule
   rejection, preservation of both typed markers, and
   parse→render→parse equivalence.

- **Done when:** one public parser/renderer round-trips the task format used by this plan, including the explicit empty merge codec and repository-format OIDs; all malformed and unsafe fixtures fail with field-specific diagnostics; body preservation is byte-exact; the YAML and SHA-256 dependencies respect Rust 1.85; and cargo fmt/clippy/test are green.
