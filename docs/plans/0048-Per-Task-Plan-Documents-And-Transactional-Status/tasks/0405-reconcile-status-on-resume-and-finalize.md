---
id: reconcile-status-on-resume-and-finalize
title: Reconcile Status On Resume And Finalization
workstream: "0004"
kind: task
depends_on: [commit-status-with-task-landing]
gated: false
touches:
  - crates/makina-core/src/plan.rs
  - crates/makina-core/src/landing.rs
  - crates/makina-core/src/plan_status.rs
  - crates/makina-core/src/merge.rs
  - crates/makina-core/src/worktree.rs
  - crates/makina-core/src/api.rs
  - crates/makina-core/src/orchestrator.rs
  - crates/makina-core/src/run_metadata.rs
  - crates/makina-core/src/actors/supervisor.rs
  - crates/makina/src/**
  - crates/makina/tests/**
  - crates/makina-core/tests/status_reconciliation.rs
  - crates/makina-core/tests/final_merge_status.rs
  - crates/makina-core/tests/plan_branch.rs
status: done
merged_as: "dde3fef39d0ce694e847a10ccb46380190e1db86"
---
# Reconcile Status On Resume And Finalization

A recoverable landing is safe only if interruption at any boundary converges on
resume, and task evidence remains meaningful after every final merge mode. This
task reconciles plan documents, Git trailers, and checkpoints and defines when
the plan—not merely its last task—is complete.

**Steps:**

1. Split reconciliation into read-only inspection during open and mutation only
   after start/resume reacquires the repository lease and rereads status/task
   documents, base/plan refs, private/task worktrees, phase trailers, and then a
   digest-matching checkpoint. Emit an auditable, idempotent action per task/
   plan; never apply a stale inspection result.
2. Apply exact evidence rules: valid `done` + reachable/matching repository-
   format `merged_as` + first-parent plan/task evidence stays done (run checked
   when durable metadata exists); A + `in-progress` completes/reuses B; JSON-only
   done is rejected; source done with bad evidence blocks. Stale `in-progress`
   returns to planned only after proving no dirty/untracked/divergent/unlanded
   task branch/worktree evidence; otherwise retain it as a recovery blocker.
   Fold verified task-disposition commits from R to the inspected plan tip,
   checking exact allowed diffs plus every previous/new SourceDigest and
   PlanDigest link; reject unexplained source drift, and invalidate/rebuild a
   checkpoint whose digest predates an authorized Ungate.
3. Recover a runtime-persistence failure after committed Phase B by rebuilding
   `Done` from source + Git. Remove a retained worktree only after it is clean,
   has no untracked data, and branch/tree content is accounted by A/recovery
   evidence; otherwise preserve it. Recover pre-B by finishing B without
   re-squashing A.
4. Define final integration transitions. Every non-dropped task must be exactly
   `done`; no planned/in-progress/blocked/non-dropped gated work may remain; each
   drop needs an Exceptions reason. Then set `awaiting-integration`. For every
   configured mode, verify final gates/base/ref OIDs and construct/reuse Phase P
   as a new child of the retained plan tip whose tree is the conflict-checked
   integration of that immutable history with the current base/root board;
   never rebase/rewrite R/A/B/disposition commits or `merged_as`. P writes
   `finalization-pending`, run/base/mode, a blank F field, and this plan's
   Finalizing root row, and its plan-ref transaction verifies the base ref.
   `Stage` also materializes/records the exact staged index tree in a private
   detached finalization workspace; `Manual` exposes P's exact landing recipe.
   A checked-out base does not block P but does block F/C, leaving the durable
   pending state. After every child is quiescent, this pending boundary is a
   stable retained Close/release point for automatic Squash/MergeCommit too;
   the later `FinalizePlan::Automatic` reacquires rather than deadlocking behind
   the original lease. A preparation conflict/error sets plan-level
   `integration-blocked` without falsifying task status.
5. For `MergeCommit`, verify task implementation OIDs remain ancestors. For
   P add `Makina-Phase: finalization-prepared`, `Makina-Final-Mode`,
   `Makina-Plan`, `Makina-Run`, and `Makina-Expected-Base`. Phase F integrates
   that exact P tip and adds `Makina-Phase: final-integration`,
   `Makina-Final-Mode`, `Makina-Plan`, `Makina-Run`, and
   `Makina-Plan-Tip: <P>`; Squash and approved Stage also add
   deterministic `Makina-Task: <id> <implementation-oid>` trailers. Phase C adds
   `Makina-Phase: completion`, `Makina-Plan`, `Makina-Run`, and
   `Makina-Final-Commit: <F>`, and writes `complete`, the known F OID, and the
   complete root row. Search/reuse exact P/F/C evidence and CAS every expected
   base/ref. STATUS never stores C's own OID; persist C to run metadata only
   after its commit succeeds, and recover it after checkpoint loss from exact
   completion trailers plus first-parent lineage. Retain plan/recovery refs and
   never replace task evidence. Approved Stage verifies the retained index/tree
   is unchanged and creates a single-parent squash-shaped F. Manual accepts only
   a supplied full OID already at the base tip whose first parent is P's
   expected base, whose tree/trailers exactly match P's recipe, and whose only
   optional second parent is P; then C attests that F-equivalent landing.
6. Implement durable delayed finalization. Stage keeps its diff/index only in
   the private integration workspace. Add and exhaustively route
   `Command::FinalizePlan { plan_dir, run_uid, expected_plan_oid, input }`, with
   `FinalizeInput::{Automatic, PreparedStage, ManualCommit(GitObjectId)}`.
   Require the configured mode to match its input; `Automatic` retries
   Squash/MergeCommit, `PreparedStage` explicitly approves the inspected private
   index, and Manual never guesses from arbitrary base history. On re-entry
   reacquire the lease, reread all evidence, rerun gates, and block on
   worktree/ref/tree mismatch before F/C. If base advanced, preserve the old
   P/workspace and route
   `Command::ReprepareFinalization { plan_dir, run_uid, expected_plan_oid }` to
   archive the inspected workspace and construct a new P child/tree from the
   current base/root row without rewriting earlier commits; every prior
   `merged_as` stays identical and reachable.
7. Redefine `ResetRun`: if no durable/dirty evidence exists, remove only
   verified-clean ephemeral state; otherwise refuse with recovery paths or CAS-
   archive plan/task refs under `refs/makina/recovery/<plan>/<run_uid>/...`
   before a new attempt. Never force-remove dirty worktrees or delete the only
   ref retaining R/A/B/P/F/C/`merged_as` evidence. Reject reset of `complete`; new
   work after completion requires a new plan rather than duplicate integration.
8. Keep active/blocked/staged/manual and pre-F prepared status authoritative on
   the plan ref and emit a committed-source refresh event for the TUI. After
   Phase F, read the pending/completion state from base. If C is interrupted, resume it without
   repeating F; if F is interrupted after P, resume it from that exact tip.
   After C, verify root and plan status together, then record C in runtime
   metadata before releasing the repository lease.
9. Add interruption tests at every R/claim/A/B/P/F/C/runtime/Git-child boundary and
   final-mode tests for squash, merge commit, private Stage, explicit Manual,
   all three `FinalizeInput` variants plus Reprepare command-routing branches,
   prepare→user landing→Manual finalize, exact Stage index approval, delayed
   stale-base/root reconciliation/reprepare (including automatic P→P2→F/C),
   reset/archive, dirty worktrees, and
   conflicts. Update existing final-mode/plan-branch assertions so successful
   automated finalization ends with C as base HEAD while STATUS records F.
   Assert no duplicate phases, exact task/drop counts, correct typed integration
   state, trailers/CAS/reachability, ref retention, operator-checkout
   preservation, unchanged/reachable R/A/B/disposition/`merged_as` evidence
   across P2, TUI view selection, and stable repeated resume.

- **Done when:** lease-bound resume/reset/delayed-finalize deterministically converges source, Git, worktrees, and runtime without losing or duplicating evidence; P/F/C produce realizable typed status and CAS-linked provenance with no self-referential OID; `complete` requires C; dirty/recovery refs survive; active/prepared state reads from plan ref and post-F state from base; and cargo fmt/clippy/test are green.
