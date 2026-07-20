---
id: capture-task-landing-evidence
title: Capture Verifiable Task Landing Evidence
workstream: "0004"
kind: task
depends_on: [isolate-integration-git-mutations]
gated: false
touches:
  - crates/makina-core/src/merge.rs
  - crates/makina-core/src/actors/supervisor.rs
  - crates/makina-core/src/task.rs
  - crates/makina-core/tests/squash_merge.rs
  - crates/makina-core/tests/task_landing_evidence.rs
status: done
merged_as: "a997f4b09ebe89be9cffdd067f26be79f2045d9c"
---
# Capture Verifiable Task Landing Evidence

`MergeOutcome::Merged` currently says only that a squash command succeeded. A
status writer cannot safely fill `merged_as` without the exact commit created on
the plan branch, and later resume cannot distinguish that commit from a guessed
task-branch SHA. This task makes landing identity explicit and self-describing.

**Steps:**

1. Change successful task-merge results to carry a validated full
   `GitObjectId` matching the repository's SHA-1/SHA-256 object format for the new plan-branch implementation commit. Resolve it from
   Git after the commit succeeds (`rev-parse`/equivalent), never from abbreviated
   command output or the task branch tip.
2. Build task landing commit messages with stable trailers:
   `Makina-Plan: <plan-folder-slug>`, `Makina-Task: <task-id>`, and
   `Makina-Run: <run_uid>`. Use Git's trailer-safe formatting and reject values
   containing newlines or ambiguous identities.
3. Under the integration lock and immediately before a squash, search the
   expected first-parent plan lineage for the exact plan/task/run trailer tuple:
   zero matches permits A, one is verified/reused, multiple or off-lineage
   matches block as ambiguous. Re-run declared-footprint diff validation first.
4. Return the evidence through the supervisor without marking runtime `Done` or
   deleting the task worktree. Introduce a landing-pending/InReview payload that
   can hold the implementation OID until the bookkeeping task commits it.
5. Add a verifier that resolves a full OID on the retained plan ref, reads the
   commit trailers, and checks exact plan/task/first-parent lineage. Compare run
   identity when durable run metadata exists; after checkpoint loss recover it
   from the single verified commit rather than comparing to a newly minted run.
   Distinguish missing, unreachable, mismatched, off-lineage, and ambiguous evidence.
6. Preserve conflict handling and reviewer/fixer retry behavior. A failed or
   conflicted squash returns no implementation OID and cannot reach the
   bookkeeping phase.
7. Add SHA-1/SHA-256 temporary-repository tests proving the returned OID is the plan-branch
   squash commit, not the branch tip; trailers round-trip exactly; short or
   malformed OIDs fail; wrong-plan/task/run evidence fails; conflicts create no
   evidence; crash-before-checkpoint reuses exactly one A; multiple/off-lineage
   evidence blocks; and runtime/worktree cleanup has not happened at Phase A.

- **Done when:** every successful task landing yields one full, trailer-verified implementation OID on the plan ref, no non-success can fabricate evidence, the supervisor retains it in a non-done phase for bookkeeping, and cargo fmt/clippy/test are green.
