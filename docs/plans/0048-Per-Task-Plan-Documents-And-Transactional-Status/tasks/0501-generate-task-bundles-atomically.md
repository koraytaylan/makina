---
id: generate-task-bundles-atomically
title: Generate Complete Task Bundles Atomically
workstream: "0005"
kind: task
depends_on: [reconcile-status-on-resume-and-finalize]
gated: false
touches:
  - Cargo.toml
  - Cargo.lock
  - crates/makina-core/Cargo.toml
  - crates/makina-core/src/plan.rs
  - crates/makina-core/src/api.rs
  - crates/makina-core/src/constants.rs
  - crates/makina-core/src/interpreter.rs
  - crates/makina-core/src/normalizer.rs
  - crates/makina-core/src/orchestrator.rs
  - crates/makina-core/src/dependency.rs
  - crates/makina-core/src/discovery.rs
  - crates/makina-core/src/actors/planner.rs
  - crates/makina-core/src/actors/mod.rs
  - crates/makina-core/src/actors/reviewer.rs
  - crates/makina-core/tests/**
  - crates/makina/src/**
  - crates/makina/tests/**
  - crates/makina-acp/tests/**
  - crates/makina-core/tests/fixtures/generated-plans/**
status: planned
merged_as: ""
---
# Generate Complete Task Bundles Atomically

Makina's normalizer and model path currently converge on a generated
`TASKS.md`, sometimes after a partial file already exists. The new format needs
one safe publication operation whose output is indistinguishable from a
hand-authored valid plan.

**Steps:**

1. Define a `GeneratedPlanBundle` intermediate representation containing plan
   identity/title, scope and architecture sections, initial status narrative,
   workstreams, and typed task documents. It is an authoring representation,
   not an alternative executable graph.
2. Replace `write_tasks_md`/`render_tasks_md` and monolithic normalizer output
   with canonical bundle rendering through `plan.rs`. Adapt model generation to
   return structured bundle data and reject any response that cannot be mapped
   without inventing required fields.
3. Render through an exclusively created, run-qualified temporary directory
   below Makina's fallible external state root. Write all four plan layers with
   unbound validation provenance and run structural `load_plan` validation
   outside the repository lease. Never publish untracked plan paths in the
   operator checkout.
4. Acquire the repository lease before publication. Reread the exact base,
   every numbered directory/registered plan ref, and root board; recheck the
   global number/ref namespace; then copy only the closed validated bundle into
   the private integration workspace, bind validation provenance, derive its
   row over the exact base board, and validate the resulting Git tree.
5. Publish atomically by creating Phase R and its `plan/{slug}` ref with an
   atomic `git update-ref --stdin` transaction that verifies the target base at
   its expected OID and creates the ref from zero. Return `Registered` on
   success and immediately refresh committed-ref discovery/UI state. If R exists after
   response loss, verify exact source/executable digest, validation base,
   trailers, and tree and return success rather than duplicating it.
6. Make publication create-only. Reject an existing valid, malformed, or
   divergent plan ref and any numeric prefix already used by a new/historical
   directory or registered ref; never merge generated tasks into hand-authored
   content. On pre-R failure remove only the owned external temporary. After R,
   retain the ref as the sole durable generated source.
7. Return the shared validation report through `CoreApi` and preserve model/raw
   output only in run diagnostics, never as an executable fallback. Replace the
   existing generation/seed entry point with an explicit
   `GeneratePlanBundle` command/result and route its TUI/project API inputs and
   committed-ref discovery refresh plus open/start action in the same compile-
   safe task. A direct-R success never asks for registration again. If its
   validation base later becomes stale before any claim, discovery returns a
   non-runnable `RefreshRegistration` state/action that performs the exact
   generated-origin R2 transaction; it never starts from stale R. A failed
   generation must leave no discoverable partial plan; this command never opens
   or starts a run as a side effect.
8. Keep generation deterministic after structured data exists: canonical file
   names/order, frontmatter order, dependency order, status `planned`, empty
   `merged_as`, and root roll-up `0/N`. The model may author prose, but the
   renderer owns structure.
9. Add tests for successful direct-R generation; failure/crash before R, during
   provenance/tree construction, after R, and after response loss;
   new↔historical and same-number/different-slug races with exactly one R winner;
   target-base movement at the final multi-ref publish boundary;
   invalid model output; cross-file/DAG failure; existing/divergent refs;
   symlink/temporary-state attacks; zero operator tracked/staged/untracked
   changes; checkout/finalization without shadowing untracked plan paths;
   cleanup limited to pre-R owned temporaries; immediate post-CAS discovery/UI
   refresh, restart discovery of R-only source, generated-R/base-advance→typed
   RefreshRegistration→R2→Ready, exhaustive binary command routing; and
   parse→render reproducibility.

- **Done when:** `GeneratePlanBundle` reserves the global number/ref namespace under the repository lease and publishes exactly one complete loader-valid Phase R without writing operator plan paths; response loss reuses R; failure loses no post-R source or unrelated bytes; it never starts a run, overwrites an identity, or emits executable `TASKS.md`; and cargo fmt/clippy/test are green.
