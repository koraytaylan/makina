# Scope — Plan 0004

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Plan 0003 (Runtime & TUI Hardening) explicitly deferred one workstream to a
plan of its own: making task-list **ingestion robust**. Today the TUI hardwires
the deterministic `StructuredTextInterpreter` (wrapped in `EdgeInferrer`), so a
task list must follow the strict `## NNNN —` / `### id — title` convention or it
errors / mis-counts — and a perfectly-structured list with a vague,
non-actionable task still burns a full agent run before anyone notices. The
existing `ModelInterpreter` (model → JSON `TaskGraph`) is fully implemented and
unit-tested but **unreachable from the production TUI binary** (`main.rs` builds
`EdgeInferrer(StructuredTextInterpreter)` and never instantiates the model path).

This plan wires model-backed ingestion into the TUI, puts a human-review gate in
front of every run, and adds two deterministic pre-run checkers — a structural
**validator/linter** and a semantic **Qualifier** — so a run only starts on a
graph a human has seen and the machine has vetted.

## In scope

Exactly the tasks in [TASKS.md](TASKS.md) (sections 0017–0020):

- **0017 — Model-backed ingestion.** Make the `ModelInterpreter` the default
  `OpenRun` interpreter in the TUI (via `build_planner_interpreter` over the ACP
  backend), with the deterministic `StructuredTextInterpreter` as the offline /
  no-backend fallback; keep `EdgeInferrer` composed on top; reuse the persisted
  `.makina/tasks/{slug}.json` artifact so the model runs once per source, not on
  every re-open.
- **0018 — Ingestion validator/linter.** A pure
  `validate(&TaskGraph) -> Vec<IngestionIssue>` that collects **all** structural
  problems (dangling deps, duplicate ids, empty `done_when`, cycles, …) with
  actionable messages, replacing the single fail-fast `graph.validate()` error;
  plus richer multi-error diagnostics on the deterministic offline parse path.
- **0019 — Qualifier entry-gate.** A pure
  `qualify(&TaskGraph) -> Vec<IngestionIssue>` of deterministic actionability
  heuristics (a non-empty, verifiable `done_when`; no placeholder / `TBD` text;
  an actionable title; substantive description; resolvable deps). Flagged tasks
  bounce back *before* any agent run.
- **0020 — Review & approval gate.** The `IngestionReport` type threaded onto the
  Pending run + `RunView`; a TUI issues panel; a `StartRun` guard that refuses
  while any **blocking** issue exists; and a **re-interpret** action that re-runs
  the interpreter (bypassing the persisted artifact). Ties 0017/0018/0019
  together.

VISION principles served: **"no guessing on ambiguity"** (the Qualifier makes
non-actionability an enforced, pre-delegation property), **"deterministic
governance is the wedge"** (validator + qualifier are deterministic gates a human
approves, not prompt-and-hope), and **"maximalist core, thin shell"** (interpret
/ validate / qualify all live in `makina-core`; the TUI only renders the report
and gates the start key).

## Origin → workstream mapping

| Source | Addressed by |
|---|---|
| 0003 SCOPE deferral: "wire the existing `ModelInterpreter` into the TUI" | `0017` model-backed ingestion |
| 0003 SCOPE deferral: "a validator/linter with clear errors" | `0018` validator/linter |
| 0003 SCOPE deferral: "the FUTURE *Qualifier* entry-gate" ([FUTURE.md](../0001-Initial/FUTURE.md) §"Qualifier — entry-gate quality checks") | `0019` qualifier |
| 0003 SCOPE deferral: "model-normalize-to-convention with a human review step" | `0020` review & approval gate |
| 0001 ROADMAP slot "0004 — Planner" (interpret structured text → JSON `TaskGraph`) | folded in: the `ModelInterpreter` mechanism already landed; this plan *wires + hardens* it |

## Locked decisions

Settled in brainstorming; detail in [ARCHITECTURE.md](ARCHITECTURE.md).

- **Model produces `TaskGraph` JSON directly** (reuse the existing
  `ModelInterpreter`); the human reviews the *rendered* tasks (titles / deps /
  done-when) in the TUI, **not** an intermediate normalized Markdown file. The
  "normalize-to-convention Markdown then re-parse deterministically" alternative
  was considered and rejected as more machinery for this plan; the model output
  is the source of truth, vetted by the validator + qualifier + human gate.
- **The model is the default interpreter.** Every fresh `OpenRun` interprets via
  the `ModelInterpreter` when a backend is present; the deterministic
  `StructuredTextInterpreter` is the offline / no-backend fallback only. Cost and
  latency are bounded by **artifact-first reuse** — the persisted
  `.makina/tasks/{slug}.json` is loaded on re-open, so the model runs *once per
  source*, and the human-review gate is the safety net against model drift.
- **The Qualifier blocks at ingestion; no new FSM states.** Flagged tasks are
  surfaced in the review gate and the run cannot start until they are resolved
  (human edits the source + re-interprets, or removes them). This matches
  FUTURE's "bounce back to the source unmodified rather than wasting an agent
  run" framing and deliberately leaves the task FSM — just re-proven total in
  plan 0003 (the `Skipped` addition) — untouched. The FUTURE state-machine
  extensions (`non-actionable` / `requires-human`) stay a later concern.
- **The review gate reuses the existing `Pending → StartRun` boundary.** The
  human already sees a Pending run's tasks and presses a key to start; this plan
  adds an issues panel, a `StartRun` guard, and a re-interpret action rather than
  introducing a new `RunStatus::AwaitingReview`.
- **One issue type for all three sources.** Interpreter failures, validator
  findings, and qualifier findings are all `IngestionIssue`s with a `severity`
  (`Blocking` / `Warning`) and a stable kebab `code`, collected into one
  `IngestionReport` on the run. Only `Blocking` gates `StartRun`.
- **The Qualifier is deterministic.** Heuristics over the parsed `TaskGraph`
  (string / structural predicates), not a second LLM call. An LLM-based qualifier
  is out of scope.

## Out of scope — deferred to [FUTURE.md](../0001-Initial/FUTURE.md) or a later plan

- **Multiple task sources** (GitHub Issues, JIRA, CSV/Excel) — FUTURE; this plan
  stays single-source file-driven.
- **In-TUI graph editing.** The human approves / rejects / re-interprets; to
  change task *content* they edit the source file and re-interpret. No TaskGraph
  editor in the TUI.
- **New FSM task states** (`non-actionable` / `requires-human`) — deferred per the
  block-at-ingestion decision above.
- **Cost-tiered routing / multiple backends / `UsageReport`** — FUTURE; the model
  interpret uses the single configured ACP backend.
- **Normalize-to-convention Markdown intermediate artifact** — rejected in favor
  of direct-to-JSON (see locked decisions).
- **LLM-based qualification** — the Qualifier stays deterministic.
- **Auto-committing `.makina/tasks/{slug}.json`** — unchanged from 0002/0003:
  this plan writes the artifact; committing stays manual / CI.
