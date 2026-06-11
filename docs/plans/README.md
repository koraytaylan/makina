# Plans — index & status

Each plan is a numbered directory (`NNNN-Title/`) holding `SCOPE.md`,
`ARCHITECTURE.md`, and `TASKS.md` (plan 0001, the MVP plan, predates that shape
and carries its own `VISION`/`ROADMAP`/`FUTURE` set). `TASKS.md` files follow the
structured-text convention (`docs/spec/structured-text-convention.md`), so a plan
can be executed by pointing Makina at its own task list.

**This table is the single source of truth for plan status.** Do not duplicate
status markers inside the plan documents. A status flip is a one-line diff here,
made in the same change that changes the plan's reality:

- `planned` — the plan documents exist; no implementation has landed.
- `in progress` — implementation work has started (a Makina run is open on the
  plan's `TASKS.md`, or manual implementation commits have begun).
- `done` — every task's "Done when" is satisfied and the work is merged. For
  Makina-driven plans the runtime evidence is the committed
  `.makina/tasks/{slug}.json` artifact; flip the row in the commit that lands
  the last task.
- `superseded` — overtaken by a later plan before completion (name it in the
  Notes column).

| Plan | Theme | Status |
|------|-------|--------|
| [0001 — Initial](0001-Initial/) | MVP: vision, architecture, the full develop → gate → review → merge loop | done |
| [0002 — Governance and Persistence](0002-Governance-and-Persistence/) | Permission gateway, audit ledger, task-graph persistence | done |
| [0003 — Runtime and TUI Hardening](0003-Runtime-and-TUI-Hardening/) | Exit-path reaping, panic hooks, per-run log routing | done |
| [0004 — Planner and Ingestion Robustness](0004-Planner-and-Ingestion-Robustness/) | Ingestion validator, lint codes, qualifier | done |
| [0005 — TUI Ingestion Responsiveness](0005-TUI-Ingestion-Responsiveness/) | Non-blocking interpret, ingestion panel | done |
| [0006 — Exchange Thoughts and Tools](0006-Exchange-Thoughts-and-Tools/) | Agent thoughts + tool calls in the Exchange pane | done |
| [0007 — Ingestion Gate Hardening](0007-Ingestion-Gate-Hardening/) | Blocking-issue gates on run start, reinterpret flow | done |
| [0008 — Gate Sandboxing](0008-Gate-Sandboxing/) | Docker-image gate execution | done |
| [0009 — Exchange Pane Fidelity](0009-Exchange-Pane-Fidelity/) | Markdown rendering, ANSI handling, entry structure | done |
| [0010 — Exchange Persistence and Replay](0010-Exchange-Persistence-and-Replay/) | JSONL transcripts, replay on reopen | done |
| [0011 — Provider and Role Configuration](0011-Provider-and-Role-Configuration/) | Named providers, per-role assignment, ACP discovery | done |
| [0012 — Task-List Polish](0012-Task-List-Polish/) | Task table cleanup, dependency views | done |
| [0013 — Preflight Doctor and First-Run Guidance](0013-Preflight-Doctor-and-First-Run-Guidance/) | Actionable config errors, binary preflight, doctor view | planned |
| [0014 — Failure Reasons and Log Access](0014-Failure-Reasons-and-Log-Access/) | Surface why a task failed; open per-task logs | planned |
| [0015 — Idle Hang Detection and Live Activity](0015-Idle-Hang-Detection-and-Live-Activity/) | Idle watchdog below the wall-clock cap, live activity | planned |
| [0016 — CI and Test Hermeticity](0016-CI-and-Test-Hermeticity/) | CI workflow, toolchain pin, hermetic git helpers, README refresh | planned |
| [0017 — Provider Editor Config Safety](0017-Provider-Editor-Config-Safety/) | Lossless config writes to the right layer; seeded editor | planned |
| [0018 — Merge Isolation and Data Safety](0018-Merge-Isolation-and-Data-Safety/) | Staging-worktree merges; never mutate the operator checkout | planned |
| [0019 — Slug Safety and Run Exclusivity](0019-Slug-Safety-and-Run-Exclusivity/) | Validated `Slug` newtype; one live run per slug | planned |
| [0020 — Run Lifecycle Correctness](0020-Run-Lifecycle-Correctness/) | Run epochs, enforced preconditions, honest terminal states | planned |
| [0021 — ACP Timeouts and Turn Hygiene](0021-ACP-Timeouts-and-Turn-Hygiene/) | Transport timeouts, `session/cancel`, teardown correctness | planned |
| [0022 — Exchange Pane Performance and Fidelity](0022-Exchange-Pane-Performance-and-Fidelity/) | Frame-budget rendering, render cache, replay backfill | planned |
| [0023 — Structured Terminal Outcomes](0023-Structured-Terminal-Outcomes/) | Typed `TerminalOutcome` replacing counter-inferred reasons | planned |
| [0024 — Permission Policy and Sandbox Teeth](0024-Permission-Policy-and-Sandbox-Teeth/) | Location-aware policy, audit correlation, Docker isolation | planned |
| [0025 — Engine Shape Consolidation](0025-Engine-Shape-Consolidation/) | One engine shape, single persistence writer, shared infra | planned |

Suggested sequencing for 0016–0025 (from the 2026-06-11 review that produced
them): 0016/0017 first (CI gates everything that follows), the safety set
0018–0020 before wider use, 0021–0024 next, and 0025 last — it refactors files
that 0018, 0020, and 0023 touch.
