---
id: port-plan-authoring-workflows
title: Port The Plan Authoring And Execution Workflows
workstream: "0005"
kind: task
depends_on: [generate-task-bundles-atomically]
gated: false
touches:
  - Cargo.toml
  - Cargo.lock
  - crates/makina-core/Cargo.toml
  - crates/makina/Cargo.toml
  - .claude/agents/developer.md
  - .claude/agents/reviewer.md
  - .claude/skills/create-plan/**
  - .claude/skills/implement-plan/**
  - .claude/workflows/create-plan.js
  - .claude/workflows/implement-plan.js
  - .claude/workflows/hold-repository-lease.py
  - .claude/workflows/hold-repository-lease.test.mjs
  - .claude/workflows/implement-plan.per-task.test.mjs
  - .claude/workflows/create-plan.graph-gate.test.mjs
  - .claude/workflows/fixtures/**
  - crates/makina-core/src/lib.rs
  - crates/makina-core/src/plan_contract.rs
  - crates/makina-core/src/api.rs
  - crates/makina-core/src/orchestrator.rs
  - crates/makina-core/tests/workflow_plan_fixtures.rs
  - crates/makina-core/tests/plan_contract.rs
  - crates/makina-core/tests/repository_run_lease.rs
  - crates/makina-core/tests/fixtures/repository-lease/**
  - crates/makina/src/**
  - crates/makina/tests/**
status: planned
merged_as: ""
---
# Port The Plan Authoring And Execution Workflows

The repository's agent-facing skills and JavaScript workflows are first-class
producers and consumers of plans. If they keep teaching `TASKS.md`, the Rust
cutover will immediately generate invalid work. This task makes them conform to
the same bundle, DAG, ownership, and Git-evidence rules.

**Steps:**

1. Rewrite `create-plan` instructions to create `SCOPE.md`,
   `ARCHITECTURE.md`, thin `STATUS.md`, and one canonical `tasks/*.md` file per
   task. Document exact frontmatter fields, plan-local IDs, filename/workstream
   mapping, dependency IDs, gate/footprint semantics, and final Done-when form.
2. Add a versioned, long-lived JSON-lines `plan-contract` server backed
   directly by the Rust loader/coordinator and export it through `lib.rs` plus
   the binary. Cover inspect/render, candidate diff checking, registration,
   claim, Phase A/B landing, blocker/retry/disposition, resume, and P/F/C
   finalization. Serve an authenticated, run-owned local endpoint below the
   external state root (Unix-domain socket or Windows named pipe), so a new
   workflow client can reconnect after its predecessor dies; do not claim a
   closed stdio pipe is reconnectable. Enable Tokio's locked `net` feature in
   the workspace/owning crates; use a mode-0600 socket inside a mode-0700
   run directory on Unix and an authenticated unpredictable pipe name with
   same-user access controls on Windows. `StartSession` acquires the canonical repository lease, rereads
   evidence, and returns an opaque session token/current plan OID only after it
   is ready; every mutation carries that token, monotonic request ID, and
   expected source/ref OIDs. Keep the lease through agent work until stable
   `Close`/cancel. Use closed tagged request/response enums, bounded input,
   machine-readable diagnostics, stale/replayed/cross-session rejection, and
   nonzero failure exits; do not expose a raw arbitrary-file/status mutation
   escape hatch.
3. Port `create-plan.js` to produce blueprint data and delegate canonical
   bundle rendering/validation to the Rust contract. Accumulate returned
   diagnostics and reject mixed/old formats. Define the two authoring outcomes:
   `commit: false` writes a create-only working candidate and returns
   `AwaitingCommit`; `commit: true` requires an explicit clean/exact target-base
   authoring context, commits only the closed bundle (plus its derived root row)
   without absorbing unrelated staged/worktree bytes, then invokes the shared
   `RegisterPlan` transaction and returns the exact reusable R. Response loss
   reopens/verifies the authored commit and R rather than duplicating either;
   uncommitted source is never executed. Keep numbering based on all plan
   directories and verified R refs so historical and ref-only plan numbers are
   not reused.
4. Rewrite `implement-plan` instructions/workflow to obtain its typed plan/DAG
   from the Rust contract,
   topologically schedule plan-local IDs, honor `gated`, `dropped`, statuses,
   and the exact footprint glob/diff rules, and use plan/run-qualified create-
   only task/private-integration worktrees. Remove every force-delete/reset/
   clean prompt inherited by the bootstrap or old workflow. Spawn exactly one
   contract session for a run rather than independent one-shot CLI calls.
5. Make status ownership unambiguous in every worker role: developer/reviewer/
   fixer agents may not edit task frontmatter or either status layer. The
   serialized coordinator commits claim, blocker/requeue, done/OID, progress,
   and finalization transitions only after the corresponding Git outcome.
6. Delegate every shared-lock/CAS/status/Git phase to the Rust contract and
   record repository-format Phase A OIDs plus R/A/B/P/F/C linkage trailers. The
   Rust/Python lock namespace is already proven in task 0401; remove the
   bootstrap workflow's direct lock/Git implementation rather than retaining a
   second coordinator. Move only a minimal no-Git Python `flock` holder into a
   clearly inert Rust test fixture and retarget `repository_run_lease.rs` to it,
   preserving cross-language conformance without a live second lock path. Never accept a manually typed,
   short, task-branch, bookkeeping, or final-base OID as `merged_as`. Align
   interrupted landing, reset/archive, Stage/Manual delayed finalization, and
   resume with the Rust coordinator instead of retaining bootstrap shortcuts.
   Wrap every host-level `agent()` Promise in typed `BeginWorker`/`EndWorker`
   requests and send End only after that exact call terminates. The Rust server
   does not claim direct cancellation authority it lacks: on client death it
   remains reconnectable and recovery-blocked with the lease held until the
   original client reports termination or a recovery client supplies verifiable
   host cancellation/termination evidence. Back live workers with inherited-
   lock sentinels so server SIGKILL cannot admit a second run while they may
   still mutate task branches.
7. Implement and exercise Plan 0048's live self-upgrade boundary after this
   task reaches Phase B. The bootstrap scheduler must have no other ready/live
   task (the scaffold task depends on this one), and every workflow-owned agent
   and Git child must be quiescent or durably retained. Before dispatching this
   task, preserve an exact hashed copy of the running bootstrap scripts in the
   run-owned external recovery directory; Phase A may replace the repository
   files, but that copy survives until the Rust handoff is proven. From the exact 0502-B
   plan-ref/integration tree, build the binary into a run-owned external target
   directory with B embedded as its build-source OID, record its artifact hash,
   and select it by absolute path—never PATH/current-base. Persist/halt the old
   coordinator, close/reap the Python holder, launch the new binary, verify its
   `Hello` protocol/build OID, let it reacquire the lock, and require a full
   R/A/B/checkpoint/worktree reconcile plus `Ready` before allowing cutover or
   continuing to task 0503. Retain the recovery copy, exact-B binary,
   artifact/build evidence, and endpoint metadata through verified C/stable
   Close (and throughout retained Stage/Manual), not merely initial Ready. A
   contender winning the gap produces a visible wait and no mutation.
   Implement the cleanup capability in this exact-B server and the future
   workflow client, matching the v1 client/janitor already prebuilt into task
   0101 for Plan 0048's live process: keep a hashed, path-contained retention
   manifest; only verified C plus no live
   worker/Git child lets `Close` emit a manifest-bound `CleanupPermit` and exit;
   the client reaps the server before deleting the exact owned paths (important
   for the running binary on Windows). If the client dies after the permit,
   next startup verifies and completes the same idempotent cleanup. Pending
   Stage/Manual or ambiguous evidence never emits a permit.
8. Expand JavaScript plus Rust contract tests for filename/frontmatter validation, workstream/DAG
   errors, gates, dropped tasks, modify-only tracked `.makina/config.toml` and
   deletion-only tracked-`.makina` enforcement at both diff checkpoints, reserved
   status paths, private-workspace safety, phase evidence/CAS, typed integration
   transitions, and interrupted landings/finalization. Check generated fixtures
   into `.claude/workflows/fixtures/` and add a Rust integration test that loads
   them through `PlanDocument` for cross-language conformance. Retain the v1
   Python↔Rust digest vectors and inert Python flock fixture as bootstrap-
   upgrade evidence even after live JS delegates hashing/locking to Rust. Cover
   Unix endpoint permissions/authentication,
   Windows transport compilation/auth rejection, and
   reconnect, an idle session blocking a second process; close/cancel/client
   disconnect; client and server hard death with live Git/worker guards;
   stale tokens/request IDs; `commit: false` AwaitingCommit;
   `commit: true` commit+R response loss/base races; exact worker quiescence at
   handoff; an intentionally old PATH binary; both sides of lock release; and a
   contender winning the upgrade gap. Exercise crash after Ready/before C,
   retained Stage/Manual, cleanup-permit response loss, server reap before
   binary deletion, and next-start cleanup recovery.
9. Remove the bootstrap/old workflow's YAML/Markdown parsing, frontmatter/status
   rendering, footprint matching, lock, and direct Git-phase functions, plus
   code whose only purpose is monolithic task lists from the repository Phase-A
   result. The already-running process uses the external recovery copy across
   Phase B/handoff; the final cutover invokes/tests post-C/stable-close cleanup, so
   none of the handoff artifacts disappear during 0503/0504 or retained
   finalization. Preserve historical references only in
   explicit negative fixtures. Add a search gate preventing those semantic
   implementations from reappearing in JavaScript.

- **Done when:** both agent skills/workflows orchestrate only one lease-owning Rust-contract session per run; no live JavaScript parser/renderer/footprint matcher/status, lock, or Git coordinator remains; create-only versus commit-and-register authoring is explicit and idempotent; workers cannot race status documents or outlive the repository exclusion barrier unaccounted; Plan 0048 demonstrably resumes from the exact 0502-B-built Rust binary while all handoff recovery artifacts remain available through C or retained finalization; R/A/B/P/F/C transitions and OIDs use the Rust lifecycle; versioned integration fixtures pass; and all JS plus cargo gates are green.
