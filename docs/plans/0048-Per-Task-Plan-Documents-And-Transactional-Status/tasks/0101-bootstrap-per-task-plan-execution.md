---
id: bootstrap-per-task-plan-execution
title: Bootstrap Per-Task Plan Execution
workstream: "0001"
kind: chore
depends_on: []
gated: false
touches:
  - .claude/skills/implement-plan/**
  - .claude/workflows/implement-plan.js
  - .claude/workflows/hold-repository-lease.py
  - .claude/workflows/hold-repository-lease.test.mjs
  - .claude/workflows/plan-digest.py
  - .claude/workflows/fixtures/plan-digest-v1.json
  - .claude/workflows/implement-plan.per-task.test.mjs
status: done
merged_as: "7d432292aec7c6f8e7b61d29825ad155edb9cd24"
---
# Bootstrap Per-Task Plan Execution

Plan 0048 is intentionally not readable by Makina's current `TASKS.md` runtime
or by the current implement-plan workflow. This is the one bootstrap task: an
external coordinating agent lands it directly from this file, then the updated
workflow can execute the remaining DAG without inventing a temporary
`TASKS.md`. The bootstrap must not call Makina's current destructive merge/reset
path.

**Steps:**

1. Record and follow the bootstrap rule: only this task is launched by an
   external coordinating agent, and only after this entire Plan 0048 bundle and
   any authored root row are in the exact target-base commit. The row may be
   absent; R derives/inserts it, while a present row must satisfy the closed
   cardinality/parity rule. Working-tree-only bytes fail as `AwaitingCommit`.
   It creates a run-qualified private integration workspace
   beneath Makina's off-repository per-project state root without switching the
   operator checkout, acquires the Git-common-directory lease described below,
   validates the committed bundle, and first creates distinct Phase R on a
   create-only `plan/{slug}` ref with registration/source/executable-digest and
   validation-base trailers. It then commits the coordinator-owned
   `in-progress` transition in task/plan/root status, implements these workflow
   files there, verifies the candidate diff against this task's footprint, and creates Phase A with
   `Makina-Plan`, `Makina-Task`, and `Makina-Run` trailers plus expected-old ref
   compare-and-swap. It then invokes the newly installed executor in that
   workspace, still under the lease, to create Phase B and write this task's
   `done`/`merged_as` plus plan/root status before continuing to
   `define-task-document-schema`. No user manually edits status files.
2. Replace implement-plan discovery with `tasks/*.md` discovery. Parse the nine
   closed frontmatter keys used by this plan, filename/workstream/ID parity,
   dependency IDs/DAG, gates, footprints, authored status, and empty/full merge
   evidence. The bootstrap reader may be smaller than the later Rust validator,
   but it must fail closed on fields it cannot interpret and must not infer or
   write a monolithic task list.
   Compute R's SourceDigest and PlanDigest with the exact SHA-256 v1
   domain-separated, length-prefixed canonical framing in `ARCHITECTURE.md` via
   a standard-library helper; never hash ad hoc JSON or concatenated strings.
3. Make the workflow schedule plan-local IDs topologically and reserve the
   active task documents, plan `STATUS.md`, and root roll-up from worker edits.
   The workflow coordinator—not developer/reviewer agents—commits claim,
   blocker/requeue, and post-landing done/OID transitions in all three layers.
4. Add the bootstrap's minimal cross-process repository lease. Resolve and
   canonicalize `git rev-parse --git-common-dir`; use the standard-library
   Python holder to open without truncation and take an exclusive Unix
   `fcntl.flock`/`flock(2)` on the persistent, never-unlinked
   `<git-common-dir>/makina.repository.lock` from before
   the first claim/ref/worktree mutation through complete or another durable
   retained run state. Give the holder a run-qualified ready/release handshake,
   explicit child ownership, and reap-on-cancel behavior; kernel release, not
   PID-file deletion, recovers a crash. Never replace/unlink the lock inode.
   Contending invocations wait/fail visibly, all in-process mutations also share
   one mutex, and unsupported or unverifiable locking fails closed. Make the
   holder a constrained broker for coordinator Git mutations: each such child
   inherits a controlled duplicate of the same locked open-file description,
   so killing the holder cannot free the lease until an already-running Git
   child exits. The workflow must not bypass the broker for a ref/index/worktree
   mutation. Track every developer/reviewer/fixer lifetime with a holder-owned
   token; on workflow death, cancel/await it when possible or stay visibly
   recovery-blocked with the lease held. A per-worker sentinel retains an
   inherited lease duplicate across hard holder death until explicit verified
   recovery, so an unquiesced agent cannot overlap another run. The external
   coordinator uses this same holder for the bootstrap task; task
   `serialize-plan-runs-per-repository` implements the matching product lease
   and task `port-plan-authoring-workflows` activates its long-lived Rust
   session.
5. Replace force-delete/reset setup prompts with create-only, run-qualified task
   branches and worktrees plus a private integration worktree. An existing,
   dirty, divergent, or ambiguous path/ref is recovery evidence and blocks; it
   is never force-removed. Do not switch, reset, clean, stage, or merge in the
   operator checkout. Resolve paths from the existing off-repository state-root
   convention rather than repository-local `.makina` or `.worktrees` literals.
6. Implement exact changed-path enforcement, not just collision scheduling.
   Match the authored portable grammar (literal, single-segment `*`, terminal
   `/**`) against every path and both sides of rename/copy records in a NUL-safe
   candidate diff. The reviewer checks the task-base-to-candidate diff, and the
   coordinator repeats the check immediately before Phase A. Reject any path
   outside `touches` and always reject task/frontmatter, plan STATUS, root
   STATUS, `.git`, and ordinary `.makina` paths owned by the coordinator. Carry
   typed exact-path markers for tracked `.makina/config.toml` modification and
   tracked-`.makina` chore deletion: accept only NUL-safe name-status `M` with
   an ordinary config result or `D` for an artifact, respectively; reject
   additions, wrong `M`/`D`, rename/copy, type, unmerged, and submodule changes.
   Never land an agent's
   claimed path list without deriving it from Git.
7. Capture the full task landing OID from the integration branch and write it to
   `merged_as` only after the landing exists. Keep the integration ref/worktree
   when bookkeeping fails and resume from Git evidence rather than repeating or
   deleting work. Advance every shared ref with an expected-old OID and fail
   closed on movement.
8. Add focused executable tests for this plan's per-task DAG, rejection of
   working-tree-only bootstrap source, exact committed-source R creation and
   response-loss reuse, Python digest golden vectors (empty/list/non-ASCII/
   length boundaries), malformed
   frontmatter/dependencies, gated tasks, footprint grammar plus out-of-scope,
   modify-only config/deletion-only `.makina`, and rename/copy/type/submodule diffs, reserved
   paths, same-repository lock contention and
   normal lock-holder crash release, hard-kill with a paused inherited-lock Git
   child, workflow death with a live worker sentinel, CAS races, safe existing-worktree failure, status
   ownership, and resume after a landed commit. The full schema,
   cross-language fixtures, final-mode semantics, and authoring workflow remain
   in `port-plan-authoring-workflows` after the Rust implementation exists.
9. Build the self-upgrade seam into this bootstrap executor now, because the
   running JavaScript process cannot hot-reload its later replacement. After
   saving a hashed exact copy of itself and its Python helpers in the run-owned
   external recovery directory before task 0502 is dispatched, and after
   `port-plan-authoring-workflows` reaches durable Phase B, stop dispatching and
   prove every workflow-owned developer/reviewer/fixer and Git child is
   quiescent or durably retained, persist the checkpoint, close and reap the
   Python holder, and launch the new
   long-lived Rust contract session for the same plan/run. Perform no mutation
   until that session reacquires the canonical lease, rereads/reconciles
   R/A/B/worktree/checkpoint evidence, and returns `Ready`. Then switch the
   already-running workflow into its prebuilt thin contract-client mode: all
   further DAG reads and coordinator mutations come from Rust, while JavaScript
   only dispatches/awaits agents and forwards typed requests. It does not try to
   hot-reload the newly committed workflow file. If a contender wins
   the close/reacquire gap, wait visibly. Keep that external bootstrap recovery
   copy plus the later exact-B handoff artifacts until verified C/stable Close
   (or throughout retained Stage/Manual); the final cutover only invokes/tests
   the post-C cleanup. Cover pre-release/post-release crash,
   contender, response-loss, and successful continuation into tasks 0503/0504.
10. Prebuild the current run's client half of the v1 cleanup protocol. Maintain
    the path-contained hashed retention manifest, authenticate and verify the
    exact-B server's C-bound `CleanupPermit`, reap that server before deleting
    its executable (including on Windows), remove only manifest-owned paths, and
    let the next bootstrap/client start finish an interrupted permitted cleanup.
    Never clean on initial Ready, ambiguous evidence, a live child/worker, or
    retained Stage/Manual. The 0502 server and future workflow client implement
    the matching generic protocol; 0504 merely invokes and tests it.

- **Done when:** from an exact committed Plan 0048 source, the one externally launched bootstrap creates distinct evidenced R, installs a non-destructive per-task executor, records its own A/B completion through that executor, and the remaining tasks can be selected from their frontmatter DAG under a tested cross-process lease, expected-old CAS, and exact review/Phase-A footprint enforcement; its preinstalled self-upgrade seam can hand the live run to the later Rust session without mutating during the lease gap or deleting bootstrap recovery code early; no `TASKS.md` or manual status edits are required; and the bootstrap Python/JavaScript test suites pass.
