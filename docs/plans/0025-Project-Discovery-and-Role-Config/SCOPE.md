# Scope — Plan 0025

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Makina's runtime is deterministic by design: the orchestrator runs the gates
listed in `config.gates`, in order, after the Developer and before the Reviewer
(`supervisor.rs:2031` `run_gates(&ctx.config.gates, …)`), and each role's system
prompt is a fixed constant (`DEVELOPER_SYSTEM_PROMPT` / `REVIEWER_SYSTEM_PROMPT`
in `roles.rs:98,133`, `PLANNER_SYSTEM_PROMPT` in `interpreter.rs:604`). That
determinism is the product wedge. But it has a sharp onboarding edge: **someone
has to hand-author the gates and the role guidance for every repo.** A fresh
clone has no `[[gates]]`, so the gate turn is a no-op (`run_gates` returns
`Passed` on an empty list, `gate.rs:228`), and the three role prompts know
nothing about *this* project's conventions (its lint command, its "never touch
`vendor/`" rule, its test runner).

Two concrete gaps:

1. **Gates are unconfigured on first open.** A repo with no `[[gates]]` ships
   work to the Reviewer with zero mechanical checks; the user must read the docs,
   work out `cargo clippy` vs `npm test`, and write the `[[gates]]` block by hand
   before Makina does anything useful.
2. **Role prompts are project-blind and unconfigurable.** There is no
   `system_prompt` field on a `RoleAssignment` (`config.rs:157`), so a project's
   prose conventions ("use the workspace lints", "do not edit generated files")
   cannot reach the Developer/Reviewer/Planner at all.

This plan closes both gaps with an **LLM-driven discovery pass**: a discovery
agent inspects the repo (manifests + `README`/`CONTRIBUTING`/`AGENTS`) and
proposes (a) gate commands and (b) per-role constraint instructions. The proposal
is written into `config.toml` — discovered gates merged into the single existing
`[[gates]]` list with `source = "discovered"`, role constraints appended to each
role's `system_prompt`, and a `[discovery]` stamp recording the run. **Only
discovery is model-driven; execution stays deterministic** — the written
`config.toml` is the auditable, editable record, and from then on Makina runs
exactly the gates and prompts that file describes.

This is also the **config write path keystone**: it generalises the 0011 config
writer (`commit_provider_config`, `event.rs:323`) from "providers + roles only"
into a merge-preserving writer that can persist gates, the `[discovery]` stamp,
and folded role constraints without losing the other sections — which plans 0026
and 0028 then build on.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0073–0075):

- **0073 — Per-role system prompt config.** Add `system_prompt: Option<String>`
  and `system_prompt_mode: Option<String>` (`"append"` | `"replace"`, default
  append) to `RoleAssignment`. Add a `system_prompt_for(role, assignment) ->
  String` that returns the built-in constant, the constant + `"\n\n"` + custom
  (append), or the custom prompt alone (replace), and wire it into
  `session_config_for` and the planner prompt assembly so the effective system
  prompt honours config — **append-by-default preserves each role's invariant
  contract** (esp. the Reviewer's JSON-verdict protocol).
- **0074 — LLM project-discovery agent.** A `makina-core` discovery module: a
  `DiscoveryResult { gates, role_constraints }` produced by an LLM pass over the
  repo (manifests + `README`/`CONTRIBUTING`/`AGENTS`). Build a discovery system
  prompt; run it through an `AgentBackend` session (the same backend abstraction
  the planner uses, so it is testable with a stub backend); parse the model's
  JSON into `DiscoveryResult` deterministically.
- **0075 — Discovery trigger + persistence.** Generalise the 0011 config writer
  to persist discovered gates (`source = "discovered"`), a `[discovery]` table
  stamp (`last_run` RFC3339 + scanned files), and the role constraints folded into
  each role's `system_prompt`. On the **first `OpenRun`** for a repo whose config
  has no `[discovery]` stamp, run discovery and persist automatically; subsequent
  opens skip (stamp present). Expose a re-runnable "Discover project" action that
  force-re-runs.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Role prompts are fixed constants; no per-project guidance reaches a role | `0073` |
| No way to detect a repo's gate commands or prose conventions | `0074` |
| Gates are unconfigured on first open; the gate turn is a no-op | `0074`, `0075` |
| No config *write* path beyond providers/roles (0011) | `0075` |
| Discovery is not triggered anywhere, nor re-runnable | `0075` |

## Locked decisions

- **Discovery is model-driven; execution is deterministic.** The discovery agent
  is the *only* model-driven part. Its output is written to `config.toml`; from
  there everything runs deterministically off that file (gates via
  `run_gates(&ctx.config.gates, …)`, prompts via `system_prompt_for`). The
  written config is the auditable record and is editable in the existing settings
  screen (the 0011 `ProviderEditor`).
- **Auto on first open, idempotent via a `[discovery]` stamp, re-runnable.**
  Discovery triggers automatically on the first `OpenRun` for a repo whose
  `config.toml` has no `[discovery]` stamp; the stamp makes it idempotent
  (subsequent opens skip). A "Discover project" action force-re-runs (overwriting
  the `source = "discovered"` gates and re-stamping). **No blocking confirmation
  step** — the written `config.toml` *is* the confirmation surface.
- **Discovered gates MERGE into the single `[[gates]]` list.** There is one gate
  list and one execution path. Discovered gates are appended to the existing
  `[[gates]]` with a `source = "discovered"` marker (hand-written gates have no
  marker / `source = "manual"`), and they run after the Developer and before the
  Reviewer exactly like every other gate (`supervisor.rs:2031`). Any gate failure
  loops back to the Developer under the existing shared gate-iteration cap
  (`GateCapReached` → `FailureKind::GateCap`, `supervisor.rs:2113,2118`). Gates can
  truly block.
- **`system_prompt` APPENDS by default.** A custom prompt is appended to the
  built-in role constant (`constant + "\n\n" + custom`) so each role keeps its
  invariant contract; `system_prompt_mode = "replace"` is the escape hatch for a
  fully custom prompt. Role constraints discovered in 0074 are folded in via the
  **append** path so they never clobber the Reviewer JSON protocol.
- **Reuse the planner's backend pattern for the discovery pass.** The discovery
  agent spawns a session via `AgentBackend::spawn`, sends one prompt, collects
  `ResponseEvent::TextChunk` to `TurnComplete`, and parses the outermost JSON via
  `crate::json::extract_json_object` — the exact one-shot shape `ModelInterpreter`
  uses (`interpreter.rs:724`), so it is unit-testable with a `StubBackend`.
- **No token estimation.** Out of scope for this plan; see plan 0024 for the
  duration/model metrics surface and the `usage: Option<UsageStats>` slot.

## Out of scope

- The plan-directory discovery / "Plans" sidebar surface (plan 0027 — that scans
  `docs/plans/*/` for the SCOPE/ARCHITECTURE convention; this plan inspects the
  *repo* for gates + conventions, a different artifact).
- Planner auto-generation of a task graph from `SCOPE`/`ARCHITECTURE` when
  `TASKS.md` is missing (plan 0028; this plan relies on 0073's planner prompt
  wiring but does not add the generate path).
- A full Ctrl+P command palette (a future UX plan); the "Discover project" action
  here is a single concrete entry-point that a palette will later host.
- Token/usage metrics and the duration/model header (plan 0024).
- Sandboxing or Docker-imaging the discovered gates beyond the existing optional
  `GateConfig::image` field (plan 0008).
- Any change to *how* gates execute or *how* the Reviewer verdict is parsed.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
