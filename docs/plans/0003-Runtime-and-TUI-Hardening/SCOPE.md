# Scope — Plan 0003

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Driving the plan-0002 task list through the TUI against a real agent surfaced
ten findings. They cluster into four workstreams; this plan addresses all ten
plus two follow-ups the plan-0002 final review flagged.

## In scope

Exactly the tasks in [TASKS.md](TASKS.md) (sections 0012–0015):

- **0012 — Unified `.makina/` workspace + run identity.** Relocate config,
  task-graph, worktrees, and audit under `.makina/`; introduce a persistent run
  id; commit/ignore split.
- **0013 — Logging & diagnostics.** A `tracing` subscriber with per-run /
  per-task file logs; run metadata; async audit write; registry eviction.
- **0014 — Scheduler robustness.** Continue-independents on failure + a
  `Skipped` terminal state; a parallelism root-cause probe.
- **0015 — TUI presentation & views.** Error/log pane, ANSI/diff exchange
  rendering, mouse scroll, run label, `G`/`R` legend, dependency view.

## Findings → workstream mapping

| Dogfood finding | Addressed by |
|---|---|
| System errors dumped over the frame | `tui-error-pane-*` (0015) + `log-subscriber` (0013) |
| Exchange pane garbles ANSI/diff output | `tui-ansi-parser` + `tui-diff-coloring` + `tui-exchange-render` (0015) |
| Sidebar shows bare file stem ("TASKS") | `tui-sidebar-label` (0015) |
| Mouse scroll switches tasks instead of scrolling | `tui-scroll-state` + `tui-mouse-scroll` (0015) |
| `G`/`R` columns unexplained | `tui-gr-legend` (0015) |
| No visible parallelism | `sched-parallelism-instrument`/`-verify` (0014) + `tui-dep-timeline` (0015) |
| No dependency display | `tui-dep-list`/`-toggle`/`-tree`/`-timeline` (0015) |
| One task failure halts the whole run | `fsm-skipped-state` + `sched-skip-dependents` + `sched-continue-on-failure` + `sched-run-status-failed` (0014) |
| No run logs on disk | `log-run-dir` + `log-subscriber` + `log-per-task-files` + `log-run-metadata` (0013), on `mk-run-id` + `mk-paths-module` (0012) |
| Move `makina.toml` → `.makina/config.toml` | `mk-config-path` + `mk-gitignore` (0012) |
| *(0002 follow-up)* audit sink blocks the async reader thread | `audit-async-write` (0013) |
| *(0002 follow-up)* audit registry never evicts | `audit-registry-evict` (0013) |

VISION principles served: **"work state is a tracked artifact"** (a single,
legible `.makina/` home with a committed/transient split), **"failures are
scoped"** (continue-independents + `Skipped`), and **"maximalist core, thin
shell"** (logging/error surfacing belongs to core; the TUI only renders it).

## Locked decisions

Settled in brainstorming; detail in [ARCHITECTURE.md](ARCHITECTURE.md).

- **Unified `.makina/` home** with an internal `.gitignore`:
  - **committed:** `.makina/config.toml`, `.makina/tasks/{slug}.json`
  - **gitignored (transient):** `.makina/runs/{run-id}/…`, `.makina/worktrees/{task-id}/`
- **Run identity:** a persistent, sortable run id (ULID-style string) keys
  `.makina/runs/{run-id}/`. The existing `RunId(u64)` stays as the in-memory
  session handle; the string id is the on-disk identity.
- **Run slug (collision-free):** the slug that keys `.makina/tasks/{slug}.json`
  is derived from the task list's **plan folder + file stem**, lowercased and
  kebab-sanitized — e.g. `0003-Runtime-and-TUI-Hardening/TASKS.md` →
  `0003-runtime-and-tui-hardening-tasks` — **not** the bare file stem. Every
  task list is named `TASKS.md`, so the old stem-only slug made all plans
  collide on `TASKS` (and made a stale `.tasks/TASKS.json` shadow an unrelated
  plan's `.md`); the plan-scoped slug fixes that.
- **Config move is back-compatible:** if `.makina/config.toml` is absent but a
  legacy `./makina.toml` exists, load the legacy path with a one-time
  deprecation warning.
- **Audit ledger moves to `.makina/runs/{run-id}/audit.jsonl`** — i.e. it
  becomes **per-run and transient (gitignored)**, a change from plan-0002's
  committed, slug-keyed `.tasks/{slug}/audit.jsonl`. (Rationale: it's a run
  transcript, and it now lives beside the per-run logs. Flagged as a decision
  to confirm at review — if you want the audit ledger to stay committed/
  diff-reviewable, it can instead live at `.makina/tasks/{slug}/audit.jsonl`.)
- **Failure policy: continue-independents.** On any task failure (hard errors
  included), the scheduler keeps launching independent ready tasks; only the
  failed task's transitive dependents are blocked, and they reach a new
  **`Skipped`** terminal state so the run report shows *why* they didn't run.
  A genuine panic remains fatal to the run.
- **Dependency view:** a key cycles **list → tree → timeline**; the timeline is
  the parallelism observability tool.
- **Exchange rendering:** parse ANSI SGR into styles and special-case unified
  diffs; strip non-SGR control codes.

## Out of scope — deferred to [FUTURE.md](../0001-Initial/FUTURE.md)

Hard sandboxing, hang detection, cost accounting / budget caps, richer
reviewer-side rule kinds, agent-driven conflict reconciliation, multiple task
sources, a full policy engine, and auto-committing `.makina/tasks/{slug}.json`
into history (this plan writes it; committing stays manual/CI, as in 0002).

**Tolerant task-list ingestion → plan 0004.** Today the TUI hardwires the
deterministic `StructuredTextInterpreter`, so a task list must follow the
strict `## NNNN —` / `### id — title` convention or it errors/mis-counts.
Making ingestion robust for free-form, prompt-generated input — wiring the
existing `ModelInterpreter` into the TUI, model-normalize-to-convention with a
human review step, a validator/linter with clear errors, and the FUTURE
"Qualifier" entry-gate — is a distinct Planner / input-contract concern and gets
its own plan (**0004 — Planner & Ingestion Robustness**), not plan 0003.
