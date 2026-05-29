# Plan 0002 — Governance & Persistence

The first post-MVP plan. Plan 0001 shipped the orchestration backbone and was
test-driven end-to-end against a real `gemini --acp` agent. The trial
([`docs/trial/trial-findings.md`](../../trial/trial-findings.md)) surfaced two
correctness gaps that this plan closes, plus two cheap debts it pays down.

This plan deliberately stays narrow: **minimal viable governance** (the action
gateway at its smallest useful slice) and **task-graph persistence** (making
`.tasks/{slug}.json` a live runtime artifact). The heavier directions the trial
also surfaced — hard sandboxing, hang detection, cost accounting, richer
reviewer rules, conflict reconciliation, multiple task sources — remain in
[`../0001-Initial/FUTURE.md`](../0001-Initial/FUTURE.md).

## Sections

- [SCOPE.md](SCOPE.md) — what's in and out, the locked decisions, and how each
  workstream maps to the trial findings, FUTURE directions, and VISION principles
- [ARCHITECTURE.md](ARCHITECTURE.md) — the concrete design deltas to
  [`0001-Initial/ARCHITECTURE.md`](../0001-Initial/ARCHITECTURE.md), with
  file-level seams and the decisions/open-questions record
- [TASKS.md](TASKS.md) — the structured-text task list (sections 0009–0011),
  the executable spec for the increment; also a dogfood input Makina can build

## Relationship to plan 0001

Plan 0001 (`../0001-Initial/`) holds the stable VISION, ARCHITECTURE, ROADMAP,
and FUTURE for the project. This plan does not restate them; it records only
the **deltas** needed for this increment and defers everything else to 0001's
FUTURE.md.

## Status

Draft. Authored from the trial findings; the design is settled except for one
empirical unknown (the ACP permission trigger), which the first task resolves.
