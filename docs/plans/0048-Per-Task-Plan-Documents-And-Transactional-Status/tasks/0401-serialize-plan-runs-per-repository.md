---
id: serialize-plan-runs-per-repository
title: Serialize Executing Plans Per Repository
workstream: "0004"
kind: task
depends_on: [make-plan-directory-the-run-identity]
gated: false
touches:
  - Cargo.toml
  - Cargo.lock
  - crates/makina-core/Cargo.toml
  - crates/makina-core/src/lib.rs
  - crates/makina-core/src/repository_lease.rs
  - crates/makina-core/src/api.rs
  - crates/makina-core/src/orchestrator.rs
  - crates/makina-core/src/actors/supervisor.rs
  - crates/makina-core/tests/repository_run_lease.rs
  - .claude/workflows/hold-repository-lease.py
  - crates/makina/src/main.rs
  - crates/makina/src/project_api.rs
  - crates/makina/src/event.rs
  - crates/makina/src/app.rs
  - crates/makina/src/ui.rs
  - crates/makina/tests/repository_run_lease.rs
status: done
merged_as: "1cd966135ee50d4984d8c355342109ddbe5868be"
---
# Serialize Executing Plans Per Repository

The supervisor's merge lock is scoped to one run. Two runs can therefore mutate
the same integration branch namespace and root status board concurrently. This
task adds one injected in-process registry plus a Git-common-directory advisory
lock for the complete executing run, while preserving read-only inspection and
cross-repository execution.

**Steps:**

1. Add a repository lease registry keyed by the canonical Git common directory,
   not by an operator checkout/worktree path. Inject one registry from the
   binary/project-router composition root into every `CoreApi`; tests inject
   isolated registries rather than using an unscoped global singleton.
2. Add a maintained Rust-1.85-compatible advisory file-lock dependency and pair
   each local lease with the bootstrap's exact canonical protocol:
   `<git-common-dir>/makina.repository.lock`, opened without truncation, never
   unlinked, and held on Unix with an exclusive whole-file `flock(2)` compatible
   with Python `fcntl.flock`. A second Makina process must wait/fail visibly,
   and kernel lock ownership must release on process death without unsafe
   stale-PID deletion or inode replacement. Expose a narrow Unix child token
   that duplicates the already-locked open-file description for inheritance by
   coordinator mutation children; do not reopen by pathname or expose the
   descriptor to ordinary agents/non-mutating children. A parent SIGKILL must
   not release the kernel lock until every inherited child copy closes.
3. Keep discovery/open/preview and reconciliation inspection read-only. On
   start, acquire both lease layers before emitting `Running`, creating/checking
   out refs/workspaces, or mutating status; then reread source, Git, worktrees,
   and checkpoint before applying the reconciliation plan.
4. Preserve the per-run integration lock inside the repository lease. The
   transaction coordinator owns both guards; short mutations serialize while
   developer/reviewer/fixer agents work concurrently outside the inner lock.
5. Expose a typed waiting state/event containing the owning plan/run identity
   rather than blocking the event loop invisibly. Cancellation while waiting
   removes the waiter without acquisition. Wire the state through core API,
   project routing, events/application state, and concise TUI copy in this same
   compile-safe task.
6. Release guards across success, stable Stage/Manual retention, automatic
   Squash/MergeCommit retained at durable P because the base is unavailable,
   cancellation, terminal error, panic/join failure, and dropped handles. Never
   hold the old session's lease while a later `FinalizePlan` waits to reacquire
   it. Require the mutation
   subsystem to report a quiescent/reconciliable boundary before release; the
   dependent integration-workspace task adds and tests concrete Git-child
   termination/reaping ownership.
7. Route `Command::PurgeWorktrees` through the same repository lease before it
   prunes registrations or removes paths. If a run owns the lease, return the
   typed visible waiting/busy state; never let maintenance race the session.
   Replace forceful purge semantics with classification that preserves active,
   dirty, untracked, divergent, unreachable, or ambiguously owned worktrees and
   reports their recovery paths. The next task supplies the shared concrete
   clean/reachability verifier used by normal cleanup and purge.
8. Add deterministic async and subprocess tests proving all CoreApi instances
   share the injected registry; worktree aliases and a second process serialize;
   different repositories run concurrently; FIFO/cancellation is stable; open
   performs no mutation; start rereads after acquisition; and every stable exit
   path releases both lock layers. Include purge-vs-active-run contention and
   no-mutation busy behavior. Pause an inherited-token child, SIGKILL the Rust
   owner, and prove a contender stays blocked until the child exits; also prove
   normal children receive no descriptor. Add a cross-language subprocess test in
   which the committed Python holder blocks the Rust contender and forced
   Python-holder death releases it, proving the bootstrap/product handoff uses
   one lock namespace.

- **Done when:** at most one plan run or mutating worktree purge operates against a Git common directory across CoreApi instances, Rust processes, and the bootstrap Python holder; an already-running mutation child keeps the same lock alive across coordinator SIGKILL; other repositories/read-only inspection remain concurrent; start rereads evidence under the lease before `Running`; waiting/cancellation and stable release are observable; purge cannot race a run or force-remove recovery evidence; and workspace cargo gates are green.
