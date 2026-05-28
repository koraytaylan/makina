# Makina MVP — Build Task List

Structured-text task list for building the Makina MVP, derived from
[`docs/plans/0001-Initial`](docs/plans/0001-Initial/). Sections map 1:1 to
the roadmap slices (0002–0008). This file is also the intended first
**dogfood input**: once the Planner (0004) can interpret it, Makina
should be able to build itself from this list.

**Conventions**
- Each task has a stable kebab-case **id** (also used for its branch
  `task/{id}` and worktree `.worktrees/{id}/`).
- **Depends on** lists *direct* structural prerequisites only. Tasks in
  a later slice generally assume earlier slices exist.
- The Planner adds further dependency edges automatically for tasks
  that touch the same files/areas — so "Depends on" need not be
  exhaustive about file overlap.
- **Done when** is the acceptance check (kept verifiable so gates and
  the Reviewer have something concrete to confirm).

---

## 0002 — Architecture & Workspace

### workspace-scaffold — Cargo workspace with three crates
Create the workspace: `makina-core` (lib), `makina-acp` (lib),
`makina` (bin). Pin `tokio`, `kameo`, `ratatui`, `serde`, `toml`.
- **Depends on:** —
- **Done when:** `cargo build` builds all three crates and `makina`
  runs a placeholder binary.

### agent-backend-trait — Define the agent-backend trait
Define `AgentBackend` in `makina-core` (spawn an agent, send a prompt,
stream responses, terminate). This is the seam `makina-acp` implements.
- **Depends on:** workspace-scaffold
- **Done when:** the trait compiles with documented method contracts a
  stub can implement.

### core-api-surface — Define `makina-core::api`
The query/command/event surface the TUI consumes: commands it issues
(open a Run, control it), queries it reads, and the event stream it
subscribes to.
- **Depends on:** workspace-scaffold
- **Done when:** the `api` module compiles with command/query/event
  types defined.

### supervision-tree — kameo supervision tree skeleton
Root supervisor plus supervised child placeholders and a restart
strategy.
- **Depends on:** workspace-scaffold
- **Done when:** a deliberately-crashing test actor is restarted by its
  supervisor (covered by a test).

---

## 0003 — Core Skeleton

### task-model — Task and task-graph types
Data structures for tasks and the task graph, with `serde` for
`.tasks/{slug}.json`.
- **Depends on:** workspace-scaffold
- **Done when:** a sample task graph round-trips through
  serialize/deserialize.

### task-state-machine — Lifecycle FSM
States `new`, `ready`, `in-progress`, `in-review`, `done`, `failed`,
with the Developer gate self-loop and the reviewer-reject loop;
transition validation.
- **Depends on:** task-model
- **Done when:** unit tests cover every legal transition and reject
  illegal ones.

### actor-traits — Supervisor / Planner / Developer / Reviewer
Actor skeletons and their kameo message types, wired into the
supervision tree.
- **Depends on:** supervision-tree, task-model
- **Done when:** each actor spawns and accepts its message types.

### noop-backend — Test agent backend
A `NoopBackend` implementing `AgentBackend` with deterministic canned
responses, so orchestration is testable without a real agent CLI.
- **Depends on:** agent-backend-trait
- **Done when:** a test drives an actor through the backend without a
  real CLI.

### config-loading — Two-layer TOML config
Load `~/.makina/config.toml` (global) and `makina.toml` (project),
merge with project winning, validate.
- **Depends on:** workspace-scaffold
- **Done when:** sample configs merge correctly and invalid config
  fails with a clear error.

### testing-harness — Test strategy + integration harness
Establish unit-vs-integration boundaries and an integration harness
built on `noop-backend`.
- **Depends on:** noop-backend, actor-traits
- **Done when:** an integration test drives a task through the FSM end
  to end with the noop backend.

---

## 0004 — Planner

### structured-text-convention — Task list format
Define the structured-text convention (markdown rules) the Planner
parses, with at least one worked example.
- **Depends on:** —
- **Done when:** a written spec exists and this `TASKS.md` conforms to
  it.

### runtime-artifact-schema — `.tasks/{slug}.json` schema
Finalize the runtime artifact: tasks, dependency edges, state,
timestamps, iteration counts.
- **Depends on:** task-model
- **Done when:** the schema is documented and a sample artifact
  validates.

### planner-actor — Interpret task list → task graph
The Planner reads structured text, makes a model call, produces the
task graph, and hands it to the Supervisor (the only `.tasks/` writer).
- **Depends on:** actor-traits, runtime-artifact-schema,
  structured-text-convention
- **Done when:** a sample task list yields a valid task graph.

### dependency-detection — Cross-cutting dependency inference
The Planner flags tasks that touch the same files/areas and encodes
them as dependency edges so they serialize.
- **Depends on:** planner-actor
- **Done when:** two tasks over overlapping areas come out linked by a
  dependency edge.

### planner-model-mechanism — Model call + auth path
Decide and implement the Planner's model-call mechanism (direct API
vs. one-shot agent backend) and wire its credential path.
- **Depends on:** planner-actor
- **Done when:** the Planner makes a real model call and the auth path
  is documented.

---

## 0005 — Agent Backend (makina-acp)

### acp-client — ACP client over stdio
Spawn an ACP-compatible CLI subprocess and speak JSON-RPC over stdio
with lifecycle handling.
- **Depends on:** agent-backend-trait
- **Done when:** the client exchanges a prompt/response with a real ACP
  CLI.

### acp-backend-impl — Implement AgentBackend for ACP
Map the trait methods onto the ACP protocol, including response
streaming.
- **Depends on:** acp-client
- **Done when:** a prompt round-trips through `makina-acp` behind the
  trait.

### role-prompts — Developer and Reviewer prompt configs
Configure the same backend to serve the Developer role and the
Reviewer role via different prompts.
- **Depends on:** acp-backend-impl
- **Done when:** the backend produces dev output and review verdicts
  from the respective prompts.

### acp-auth-verify — Confirm Zed-model auth inheritance
Verify Makina spawns an already-signed-in ACP CLI and inherits its
session without handling credentials.
- **Depends on:** acp-client
- **Done when:** a run works against a pre-authenticated CLI with no
  credential handling in Makina.

---

## 0006 — Orchestration + Worktrees

### worktree-manager — Worktree + branch lifecycle
Supervisor creates `task/{id}` off `develop` and a worktree at
`.worktrees/{id}/` on dispatch; tears both down on completion.
- **Depends on:** actor-traits, config-loading
- **Done when:** dispatching a task creates the worktree/branch and
  completion removes them.

### develop-review-loop — The core orchestration cycle
Supervisor drives dispatch → Developer → hand-back → Reviewer →
approve/reject, relaying feedback on reject.
- **Depends on:** worktree-manager, task-state-machine, noop-backend
- **Done when:** a task runs end-to-end through the loop with the noop
  backend.

### gate-runner — Developer-side gate iteration
Run the configured gates, feed failures back to the agent, re-run all
gates until they pass or the cap is hit.
- **Depends on:** config-loading, develop-review-loop
- **Done when:** gate failure loops, passing gates advance to review,
  and the cap moves the task to `failed`.

### squash-merge — Merge approved work into develop
On approval the Supervisor squash-merges `task/{id}` into `develop`;
straggler conflicts reconciled rather than hard-failed.
- **Depends on:** worktree-manager, develop-review-loop
- **Done when:** an approved task lands on `develop` as one squashed
  commit and its worktree is torn down.

### concurrency — Parallel tasks
Run multiple Developers across independent tasks up to a configured
max; a single Developer per task.
- **Depends on:** develop-review-loop
- **Done when:** independent tasks run in parallel up to the limit.

### termination-caps — Enforce the caps
Enforce gate-iteration, reviewer-iteration, and wall-clock caps,
moving the task to terminal `failed`.
- **Depends on:** develop-review-loop
- **Done when:** each cap independently drives a task to `failed`.

---

## 0007 — TUI

### tui-scaffold — ratatui app skeleton
Event loop, navigation, and clean shutdown; consumes
`makina-core::api`.
- **Depends on:** core-api-surface
- **Done when:** the TUI launches, renders, and quits cleanly while
  talking to core through the api.

### file-browser — Open a task list
A file browser to pick a task list; opening one starts a Run.
- **Depends on:** tui-scaffold, planner-actor
- **Done when:** selecting a task list triggers interpretation and
  creates a Run.

### runs-sidebar — Left sidebar of Runs
List open Runs with their aggregate status.
- **Depends on:** tui-scaffold
- **Done when:** opened Runs appear in the sidebar with live status.

### task-status-view — Per-task live status
A panel showing each task's state and iteration counts, updating live.
- **Depends on:** runs-sidebar
- **Done when:** task states update in the TUI as the loop progresses.

### prompt-answer-stream — Live ACP exchange
Stream the prompts and answers for the focused agent in real time.
- **Depends on:** task-status-view, acp-backend-impl
- **Done when:** the focused agent's exchange streams into the TUI live.

### run-control — Control a Run
Start, pause, and cancel a Run from the TUI.
- **Depends on:** runs-sidebar, develop-review-loop
- **Done when:** control actions affect the underlying Run.

---

## 0008 — End-to-End Trial

### dogfood-task-list — Author a trial task list
Write a small structured task list for a concrete, contained coding
task in this repo.
- **Depends on:** structured-text-convention
- **Done when:** a valid task list exists that exercises the full loop.

### e2e-run — Run the full loop
Run the trial list end to end through the TUI: Planner → Supervisor →
Developer + gates → Reviewer → squash-merge.
- **Depends on:** dogfood-task-list, planner-model-mechanism,
  acp-backend-impl, squash-merge, run-control
- **Done when:** at least one task reaches `done` and lands on
  `develop`, driven from the TUI.

### trial-findings — Capture findings
Record what worked, what broke, and which `FUTURE.md` directions the
test-drive makes most urgent.
- **Depends on:** e2e-run
- **Done when:** a written findings note exists to inform prioritization.
