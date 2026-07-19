---
id: isolate-integration-git-mutations
title: Isolate Integration Git Mutations
workstream: "0004"
kind: task
depends_on: [serialize-plan-runs-per-repository]
gated: false
touches:
  - crates/makina-core/src/paths.rs
  - crates/makina-core/src/worktree.rs
  - crates/makina-core/src/merge.rs
  - crates/makina-core/src/orchestrator.rs
  - crates/makina-core/src/actors/developer.rs
  - crates/makina-core/src/actors/supervisor.rs
  - crates/makina-core/tests/integration_worktree.rs
  - crates/makina-core/tests/worktree_recovery.rs
  - crates/makina-core/tests/plan_branch.rs
  - crates/makina-core/tests/worktree.rs
  - crates/makina-core/tests/squash_merge.rs
  - crates/makina-core/tests/supervisor_write_path.rs
  - crates/makina/src/app.rs
status: planned
merged_as: ""
---
# Isolate Integration Git Mutations

Makina currently checks the plan branch out in the operator repository and uses
destructive conflict cleanup there. No task/status transaction can be called
safe on top of that behavior. This task moves every coordinator Git mutation to
a Makina-owned integration worktree before landing evidence or status commits
are added.

**Steps:**

1. Add an `IntegrationWorkspace` rooted exactly through
   the now-fallible
   `paths::run_dir(repo_root, run_uid)?.join("integration")`, with
   registration created detached at the expected base. Build and validate R in
   that detached workspace, publish `plan/{slug}` only through one
   `git update-ref --stdin` transaction that verifies the expected base ref and
   creates the plan ref from zero, and attach the workspace only after the
   transaction succeeds; a resumed workspace
   may attach only after verifying an existing R/lineage. Reuse the existing
   per-project state-root namespace and add missing-user-state and path-
   containment tests; fail before mutation if it is unavailable, and do not
   introduce a repository-local `.makina/runs` or `.worktrees` literal. The
   operator checkout is read-only context: claim, task merge, status writes, final
   Squash/MergeCommit/Stage, conflict handling, and completion bookkeeping must
   never switch its branch or modify its worktree/index.
2. Make workspace creation create-only and recoverable. If the path, worktree
   registration, branch, lock file, or recovery ref already exists, inspect and
   reconcile it; never `worktree remove --force`, `branch -D`, `reset --hard`, or
   `clean -fd` over unknown/dirty/divergent state. A crash after detached R
   creation but before ref publication leaves an orphan candidate in the owned
   worktree/reflog; verify/reuse it on retry or retain it as recovery evidence,
   never publish an empty/non-R ref.
3. Route `SquashMerger` and supervisor integration operations through the
   private workspace. Capture expected plan/base OIDs before each mutation and
   advance shared refs with atomic compare-and-swap transactions, failing closed
   on movement of a ref that operation advances. R/R2 and P publication also
   verify the target-base ref in the same `update-ref --stdin` transaction; F/C
   CAS that base directly. Ordinary claim/B/blocker/disposition commits CAS only
   the plan ref against its recorded lineage, so unrelated mid-run base movement
   does not strand bookkeeping; P reconciles the latest base/root board.
4. Replace destructive conflict cleanup with merge abort/path-limited recovery
   inside the owned workspace. If clean recovery cannot be proven, preserve the
   workspace and refs and surface a blocked recovery state; never guess that
   files are disposable.
5. Give spawned Git children explicit lifetime ownership (`kill_on_drop` plus
   reap/await behavior or equivalent). Once R/claim/A/B/P/F/C mutation begins,
   defer cancellation until the child exits and the transaction reaches a
   documented reconciliable boundary; do not release repository/integration
   guards while a Git subprocess can still mutate state. Route every
   coordinator mutation spawn through task 0401's inherited lease-child token,
   closing it in the child only on exit, so parent SIGKILL cannot create a
   lock-free orphan mutator; never pass that token to worker agents or
   read-only commands.
6. Implement `Stage` only in the durable integration workspace, never the
   operator index. `Manual` retains a clean plan ref/workspace and requires an
   explicit later finalize/reconcile action. Release the repository lease only
   after these retained states and their status evidence are durable.
7. Prepare P in a detached Makina workspace without advancing the base. Before
   F/C, enumerate registered worktrees and refuse to advance a target base
   branch checked out in any non-Makina worktree. A Makina finalization
   worktree is allowed only when the branch is otherwise free and verified
   clean; never update a ref behind the operator index.
8. Before removing any task/integration worktree, verify it is clean, contains
   no untracked data, and its branch/tree is fully accounted for by recorded
   landing/recovery evidence. Preserve or archive dirty/divergent work instead
   of force-removing it. Expose this verifier to lease-bound
   `Command::PurgeWorktrees`; purge follows the identical ownership and
   preservation rules rather than keeping a forceful alternate path.
9. Add poison-checkout tests with tracked edits, staged edits, and untracked
   files in the operator checkout; inject conflicts, cancellation, process
   failure, ref races (including base movement at the final multi-ref
   transaction boundary), stale worktrees, crash before R-ref CAS, and a reusable
   orphan detached-R candidate; assert byte-for-byte operator/index preservation
   and deterministic recovery evidence. Deliberately pause a mutation Git child,
   SIGKILL its coordinator, and prove a contender cannot acquire the repository
   lease until the child exits. Include purge of clean-owned versus
   active/dirty/untracked/divergent/ambiguous worktrees, plus clean and dirty
   operator checkouts with the base branch checked out; both must block ref
   advancement without changing HEAD/index/worktree bytes.

- **Done when:** every coordinator Git mutation runs in a private recoverable integration workspace; registration cannot expose `plan/{slug}` before a fully validated R exists; operator tracked/staged/untracked state survives all success/conflict/cancel/failpoint paths unchanged; normal cleanup and PurgeWorktrees share the same non-destructive evidence checks; shared refs advance only by expected-old compare-and-swap; and cargo fmt/clippy/test are green.
