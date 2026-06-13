# Scope — Plan 0026

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Makina's wedge is **deterministic governance**: once a repo's quality gates are
configured, every task must pass them — in the worktree, exit-code-zero — before
a Reviewer ever sees the work, and a failing gate loops the work back to the
Developer until it passes or a cap fires. That contract already runs today for
gates a human typed into `[[gates]]` (`develop_until_gates_pass` →
`ctx.gate_runner.run_gates(&ctx.config.gates, …)`).

Plans 0024/0025 add **LLM-driven project discovery**: a discovery agent inspects
the repo and proposes gate commands, and plan 0025 **merges those discovered
gates into the single `[[gates]]` list** with a `source="discovered"` marker. But
discovery only *writes config*; nothing yet asserts that the merged list **runs
in the right place with the right loop-back semantics**. A discovered gate is
worthless if it silently sits after the reviewer, or never blocks, or doesn't
feed its output back to the Developer.

Two concrete gaps:

1. **Execution ordering is unverified for discovered gates.** The discovered
   commands must run **after the Developer and before the Reviewer**, in the
   merged order (configured gates first, then `source="discovered"`), so a
   discovered gate can truly block the path to review — not just decorate config.
2. **The contract is undocumented.** There is no single doc a user (or a future
   contributor) can read to understand discovery → gate commands in config →
   run-after-develop/before-review → loop-back-to-developer → `GateCap` on
   exhaustion, and how configured vs discovered gates compose and how to edit or
   override them. The product owner explicitly asked for this.

This plan **closes the loop on execution**: it confirms (and fills any missing
wiring so) the merged gate list runs in the governance loop with loop-back
semantics, the shared gate-iteration cap, and gate output as Developer feedback —
then **documents the deterministic-governance contract** end to end.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0076–0077):

- **0076 — Run merged gates with loop-back.** Ensure the existing gate loop
  (`develop_until_gates_pass`) runs the **full merged `[[gates]]` list**
  (configured then `source="discovered"`) after the Developer and before the
  Reviewer; a failing gate feeds its combined output back to the Developer as
  revision feedback and increments the shared `gate_iterations`; exhaustion hits
  `caps.gate_iterations` → `GateCapReached` → `FailureKind::GateCap`. Most of the
  machinery already exists (`GateRunner`); this verifies ordering and that
  discovered gates participate **identically**, and adds any missing wiring.
- **0077 — Governance documentation.** Write `docs/spec/deterministic-governance.md`
  describing the full contract: discovery → gate commands in `config.toml` → run
  after Developer / before Reviewer → failure loops back to the Developer under
  the shared cap → `GateCap` on exhaustion; how configured vs discovered gates
  compose; and how to edit/override gates. Link it from the README's "How it
  works" governance section.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Merged discovered gates must run after develop / before review, in order | `0076` |
| A failing (discovered or configured) gate must loop back to the Developer | `0076` |
| Cap exhaustion must classify `FailureKind::GateCap`, not a generic failure | `0076` |
| No single doc describes the deterministic-governance contract | `0077` |

## Locked decisions

- **One execution path; no special-casing discovered gates.** Discovered gates
  are already merged into the single `config.gates` (`Vec<GateConfig>`) by plan
  0025 (which adds `#[serde(default)] pub source: Option<String>` to `GateConfig`,
  set to `Some("discovered")` for discovered gates); the loop runs that one list
  through `GateRunner::run_gates`. The `source` field is **provenance metadata
  only** — execution treats every `GateConfig` identically, so a discovered gate
  blocks exactly like a hand-written one.
- **Ordering is configured-first, then discovered.** Plan 0025 owns the merge
  *order* (it appends discovered gates after the configured ones). This plan
  asserts that order is preserved at execution time; it does not re-sort.
- **Loop-back is the existing self-loop.** On `GateOutcome::Failed`, the loop
  already applies `TaskEvent::GateFailed` (InProgress self-loop), calls
  `increment_gate_iterations_locked`, and rebuilds `feedback` from the failing
  gate's name + output. This plan reuses that path verbatim for discovered gates;
  no second feedback mechanism.
- **Shared cap, single classifier.** All gate iterations — configured or
  discovered — count against the **one** `caps.gate_iterations` counter. On
  exhaustion the loop emits `GateCapReached` and `set_failure_reason_locked(…,
  FailureKind::GateCap, …)` exactly as today. (Plan 0017 lands first and owns the
  retry surface that re-runs a `GateCap`-failed task; this plan does not add
  retry.)
- **Documentation is normative and code-accurate.** `docs/spec/deterministic-
  governance.md` follows the existing `docs/spec/*.md` convention (title, status,
  rationale, mechanism) and describes the *implemented* flow — no aspirational
  behaviour.

## Out of scope

- **Project discovery itself** — inspecting the repo and proposing gate commands
  / role constraints (plans 0024/0025). This plan consumes the merged list; it
  does not produce it.
- **The `source="discovered"` marker / merge** on `GateConfig` (plan 0025
  introduces the field and the merge ordering). This plan only relies on the
  merged list and asserts ordering at execution.
- **Retrying a `GateCap`-failed task** (plan 0017's retry surface). This plan
  ends the task at `Failed`/`GateCap`; re-dispatch is 0017.
- **Per-gate caps, parallel gate execution, or changing first-failure-stops
  semantics** — `GateRunner` stops at the first failing gate this pass and
  re-runs all gates next pass; unchanged here.
- **Sandboxing / Docker image execution** of gates (plan 0008; `GateConfig.image`
  already drives it and is untouched).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
