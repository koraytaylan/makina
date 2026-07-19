---
id: commit-status-with-task-landing
title: Commit Status With Each Task Landing
workstream: "0004"
kind: task
depends_on: [capture-task-landing-evidence]
gated: false
touches:
  - crates/makina-core/src/lib.rs
  - crates/makina-core/src/plan.rs
  - crates/makina-core/src/plan_status.rs
  - crates/makina-core/src/landing.rs
  - crates/makina-core/src/api.rs
  - crates/makina-core/src/orchestrator.rs
  - crates/makina-core/src/roles.rs
  - crates/makina-core/src/actors/supervisor.rs
  - crates/makina-core/tests/status_landing.rs
  - crates/makina-core/tests/registration_status.rs
  - crates/makina-core/tests/status_lifecycle.rs
  - crates/makina-core/tests/role_contracts.rs
  - crates/makina/src/project_api.rs
  - crates/makina/src/event.rs
  - crates/makina/src/app.rs
  - crates/makina/src/ui.rs
  - crates/makina/src/placeholder.rs
  - crates/makina/tests/**
status: planned
merged_as: ""
---
# Commit Status With Each Task Landing

Status must move automatically with landed work, but parallel developer
branches are the wrong place to edit shared plan documents. This task assigns all durable
transitions to the serialized integration coordinator and makes the bookkeeping
commit—not a hopeful agent edit—the logical completion point.

**Steps:**

1. Add `plan_status.rs` with a typed parser/renderer for the fixed heading,
   `Status`, `Goal`, `Root cause`, `Approach`, `Progress`, `Integration`,
   `Exceptions`, `Outcome`, and last-updated anchors. Parse typed plan-level
   integration state plus current run, expected base, immutable validation
   base, selected final mode, and F evidence; preserve authored narrative and
   deterministically derive task counts without pretending final state is a
   task-only aggregate. Keep bounded single-line blocked/dropped exception
   records append-only within plan history; retry marks resolution, never erases.
   Reject any attempted validation-base rewrite after registration.
2. Add a root roll-up editor that locates exactly one row by plan number and
   regenerates title, display status, `done/total` or explicit
   `done + dropped / total`, exact outcome, and relative status link. Its
   registration operation inserts one absent row; transition operations require
   exactly one. For registration, zero matching-number rows inserts the derived
   row, one exact semantic row is reused, one mismatched row returns an
   actionable diff without rewriting base history, and more than one fails
   ambiguous. Registration builds from the current expected base board. Mid-run
   claim/B/blocker/retry/disposition transitions edit only this plan's row in
   the recorded board snapshot carried by the plan lineage and CAS only that
   ref, so unrelated base movement cannot strand bookkeeping; P later rereads
   the latest base board and overlays only this row. Test all four registration
   cardinality cases plus mid-run base movement.
3. Add and route
   `Command::RegisterPlan { plan_dir, expected_base_oid, expected_source_digest }`
   as the explicit recovery action for committed-but-unregistered source. A working-tree-only candidate
   returns `AwaitingCommit`; accept only the exact bundle present in the current
   target-base commit plus its expected base/source digest, or an exact
   already-registered R after response loss. Under the repository lease, reread
   the base through the Git-tree loader, all numbered directories/registration
   refs, plan, and root board; bind/verify validation provenance in the private
   workspace; derive/overlay only its row; reload; build/validate Phase R while
   detached; then publish it with one `git update-ref --stdin` transaction that
   verifies `expected_base_oid` at the target base ref and creates
   `refs/heads/plan/{slug}` from zero; only afterward attach the workspace.
   Expose the same internal transaction over a
   closed generated-bundle source for later direct-R generation. R carries
   `Makina-Phase: plan-registration`, plan, full-source/executable-digest, and
   validation-base plus `Makina-Source-Origin: base|generated` trailers. Zero evidence permits creation, one exact R is
   reused, and divergent/non-R/off-lineage evidence blocks. Before any claim,
   an otherwise valid stale R may be revised only with expected-old-R CAS:
   archive the old R under a deterministic recovery ref, create R2 from the
   current committed base/source with `Makina-Previous-Registration: <R>`, and
   move the plan ref. For generated origin, replay the exact verified closed
   subtree from R onto the new base, recheck global identity and tracked-`.makina`
   provenance, and regenerate only its row; do not require operator/base source
   bytes. Reject refresh after any claim/A/run/disposition evidence. The Git tree is
   the atomic multi-file boundary; never rewrite or stage operator files.
   Authoring-mode tracked-`.makina` candidates must resolve to ordinary blobs in
   this exact base and become typed modify-only config or deletion-only artifact
   patterns before R; reject untracked, wrong-base, or still-unresolved candidates.
   Make R2's base verification, create-or-verify deterministic archive ref, and
   expected-old-R plan-ref move one idempotent ref transaction.
4. Add `landing.rs` as the coordinator transaction boundary. For each authored
   transition in the private integration workspace, capture original bytes/
   index entries, render task frontmatter plus plan/root status to temporary
   files, atomically replace only those owned files, reload the whole plan,
   validate all three layers, and create one bookkeeping commit using
   expected-old ref compare-and-swap.
5. At claim under the integration lock, transition `planned -> in-progress`,
   set integration state `assembling` plus the current run UID, regenerate both status layers, commit,
   and only then cut the worker
   branch/worktree. On cancel/requeue without a durable blocker, transition back
   to `planned`; on terminal failure, transition to `blocked` with a concise
   coordinator-owned Exceptions record. A dropped task likewise requires a
   reason; workers never rewrite instructions to carry bookkeeping prose.
6. After Phase A, set the landed task to `done`, write its exact full
   implementation OID to `merged_as`, recompute plan/root progress, validate,
   and create Phase B with `Makina-Phase: task-status` and
   `Makina-Landing: <A>` trailers. Search/reuse exact B evidence before creating
   it. Only after Phase B succeeds may the supervisor persist
   runtime `Done`, emit completion, unblock dependents, or remove the worktree.
7. If a handled rendering/replacement/validation/commit-B error occurs, restore
   only coordinator-owned integration-workspace bytes/index entries to Phase A.
   On process death or ambiguous index state, preserve the entire workspace.
   Leave A on the plan branch, retain branch/worktree and landing-pending state,
   pause further claims/landings, and return recovery evidence. Never touch the
   operator checkout or re-squash automatically.
8. Reserve the active plan's `tasks/*.md`, its `STATUS.md`, and root
   `docs/plans/STATUS.md` in developer/reviewer/fixer role prompts and changed-
   path validation. Agents report outcomes; they do not author status commits.
9. Route cancel/requeue and retry through `orchestrator.rs` before their runtime
   graph mutations: under the lease, write `planned` plus resolved Exception
   evidence first, then redispatch/rebuild volatile state. Cancellation with a
   durable blocker preserves `blocked`; cancellation without one writes
   `planned`. Cover single-task retry, retry-all, cancel races, persistence
   failure, and restart between source transition and runtime mutation.
10. Add a lease-bound
   `Command::SetTaskDisposition { run, task, expected_plan_oid, action }`
   coordinator path for `Ungate` and `Drop { reason }`. Allow ungating only a
   planned gated task and dropping only work with no live driver or unlanded
   evidence; require bounded reason/Exceptions for drop. Commit the validated
   task/plan/root change on the plan ref with expected plan OID plus previous/new
   `SourceDigest` and `PlanDigest` trailers. Ungate permits exactly
   `gated: true -> false` plus derived coordinator fields; Drop permits only
   status/Exceptions/roll-up fields and therefore has equal old/new digests.
   Reconciliation/discovery accepts digest movement only through this verified
   disposition chain from R. Invalidate and rebuild the checkpoint before
   dispatch when the executable digest changes. This disposition is execution
   evidence and freezes the current registered source; a task depending on a
   drop remains blocked and must itself be dropped or moved to a newly authored
   plan. Reject races, done/in-progress
   mutation, any other source diff, and runtime-only disposition.
11. Add failpoint/ref-race tests for detached registration before/after R creation
   and ref CAS, including target-base movement at the final multi-ref publish
   boundary, claim rendering/commit, every Phase B boundary,
   partial atomic replacement, validation drift, Git commit failure, runtime
   persistence failure after B, and duplicate retry. Assert no unrelated path
   changes, no premature `Done`, correct OID, and exact plan/root synchronization.
   Cover committed-base registration, working-source `AwaitingCommit`, blank
   provenance tracked-`.makina` config/deletion candidate→R success and
   untracked/wrong-base
   rejection, direct
   generated-source registration, edit-between-open/register/start, response
   loss after R, stale-base/source pre-freeze R2+archive, forbidden post-freeze
   refresh, generated R→unrelated base advance→R2/start, orphan-R recovery,
   exhaustive command routing, ungate/drop/dependent behavior, disposition CAS
   races, old/new digest-chain verification, rejection of an ungate commit with
   any extra source edit, checkpoint invalidation, and restart between
   disposition commit and graph rebuild.

- **Done when:** `RegisterPlan` imports an exact working/base source and atomically publishes a fully built evidenced R from a detached workspace without touching operator bytes or exposing a premature ref; claim, success, blocker, disposition, retry, and cancel transitions automatically update task frontmatter plus typed integration/Exceptions/root status under the coordinator lock; `Done` requires an evidenced B commit; authorized ungating forms a verified SourceDigest/PlanDigest chain and every other post-R source edit fails closed; stale digests/root rows/ref races fail closed; Phase B is recoverable in the private workspace; worker agents cannot modify status paths; and cargo fmt/clippy/test are green.
