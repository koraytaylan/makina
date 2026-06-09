# Scope — Plan 0011

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Makina drives ACP coding agents in distinct **roles** — Planner, Developer,
Reviewer — but today every role is forced through a single global `[backend]`
command (`crates/makina-core/src/config.rs`). You cannot run a cheaper agent as
Reviewer and a stronger one as Developer; you cannot pick a model; and there is
no notion of reasoning **effort**. Meanwhile the ACP agents Makina spawns already
advertise **session modes** (and, increasingly, **model and effort** via session
config options) in their `session/new` response — and Makina drops all of it
into the protocol's `Other` bucket.

This plan introduces:

1. **Named providers** — a provider is one ACP backend `{command, args, env}`
   (e.g. `grok agent --always-approve stdio`, `gemini --acp`).
2. **Per-role assignment** — Planner / Developer / Reviewer each choose a
   provider plus a default mode / model / effort.
3. **Dynamic discovery** — read the agent's advertised modes and config options
   from the live session and let the user pick from what the agent actually
   supports, applying the choice over ACP.
4. **A TUI editor** — manage providers, assign roles, and pick the model **with
   its effort level shown alongside its name**, as requested.

## What ACP actually exposes (grounding the dynamic part)

Confirmed against the v1 schema (`agentclientprotocol.com/protocol/v1`):

- **Modes — stable v1.** `NewSessionResponse.modes: SessionModeState |
  null`, with `currentModeId` and `availableModes: [{ id, name, description? }]`.
  Change via `session/set_mode { sessionId, modeId }`. The agent may switch
  autonomously and notify via a `session/update` `current_mode_update`.
- **Model + effort — config options.** v1 has a generic `session/set_config_option`
  method; an April-2026 RFD models the **model** picker and reasoning **effort**
  as config-option *categories* (`model`, `model_config`, and the existing
  `thought_level` for effort). So "model name + effort" rides the config-option
  surface — partly standardised (the method + `thought_level`), partly proposed
  (the `model` category) — and **depends on each agent advertising it**.
- **There is no standalone `availableModels` / `set_model`.** We discover what
  the agent offers and degrade gracefully to declared static defaults when it
  offers nothing.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0037–0041):

- **0037 — Provider config schema.** Named `providers` table; per-role assignment
  with default mode/model/effort; two-layer (global + project) merge; validation;
  back-compat so a lone `[backend]` becomes one default provider used by all
  roles.
- **0038 — ACP mode discovery.** Mirror `SessionModeState`; capture it from
  `session/new`; add `session/set_mode`; handle `current_mode_update`.
- **0039 — ACP config-option (model/effort) discovery.** Mirror the generic
  `configOptions` surface (categories `model`, `model_config`, `thought_level`);
  capture from `session/new` + updates; add `session/set_config_option`; degrade
  gracefully when absent.
- **0040 — Orchestrator/role wiring.** Build (and cache) a backend per provider;
  hand each role the backend its assignment names; apply the selected
  mode/model/effort to that role's session; surface discovered capabilities to
  the API layer.
- **0041 — TUI provider/role editor.** A modal (the `Mode::FileBrowser` +
  `List`/`ListState` pattern) to add/edit providers, assign them to roles, and
  pick mode + model + effort from discovered options (model listed as
  `name · effort`), with declared static fallback; write back to config.

## Origin → workstream mapping

| Request | Addressed by |
|---|---|
| Configure available ACP providers in the UI | `0037`, `0041` |
| Select different providers for different roles | `0037`, `0040`, `0041` |
| Model selection lists model **and** effort level | `0039`, `0041` |
| Apply mode/model/effort to the live agent | `0038`, `0039`, `0040` |

## Locked decisions

- **Two layers, cleanly separated.** A *provider* (which binary to spawn) is a
  Makina concept and lives in config. *Session config* (mode / model / effort)
  is an ACP concept discovered per session and applied over the wire. The TUI
  picker shows discovered options when a session exists and declared defaults
  otherwise.
- **Modes always; model/effort where advertised.** Mode discovery is solid on
  stable v1. Model/effort uses `configOptions` + `set_config_option` and degrades
  to declared static values for agents that advertise nothing — never an error.
- **Back-compat is mandatory.** Existing configs with a single `[backend]` keep
  working: resolution synthesises a `default` provider assigned to every role.
- **Planner included where it spawns an agent.** The `one-shot-agent` planner
  mechanism gets a provider/role assignment like Developer/Reviewer; the
  deferred `direct-api` mechanism is unchanged and out of scope.
- **Config is the persistence layer.** Role assignments and declared defaults are
  written to the project (or global) `config.toml`; the editor edits that file.
  No new database.

## Out of scope — deferred to FUTURE or a later plan

- **Cost-tiered automatic routing** across providers by task complexity (FUTURE
  "Cost-tiered routing"). This plan is manual per-role selection only.
- **Direct-API (non-ACP) backends** (FUTURE "Direct API agent backends").
- The **custom-llm-endpoint RFD** (configuring an LLM endpoint *inside* one
  agent) — distinct from choosing which agent binary to run.
- **Per-task** provider/model overrides — assignment is per role, not per task.
- MCP-over-ACP, auth-method selection UI, or any non-model session capability.
- Usage/cost accounting from `model_config` (display only; no billing math).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the type deltas across config, ACP,
core, and the TUI, and the phased build order. See [TASKS.md](TASKS.md) for the
executable task list.
