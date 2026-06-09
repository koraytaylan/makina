# Architecture — Plan 0011 (deltas)

> Named providers, per-role assignment, and dynamic ACP mode/model/effort
> discovery, wired from config through the ACP client to a TUI editor. Line
> numbers are hints; locate by symbol. Build in the order below — each layer
> compiles and tests before the next.

## Layer map

```
config.rs (providers + role assignment)            ─0037─┐
roles.rs / backend.rs / orchestrator (wiring)      ─0040─┤
makina-acp protocol.rs / client.rs (modes+options) ─0038/0039─┤
api.rs (capabilities view + events)                ─0040─┤
makina TUI app.rs / ui.rs (editor modal)           ─0041─┘
```

## 0037 — Provider config schema (`crates/makina-core/src/config.rs`)

Today: one `BackendConfig { command, args }` in `GlobalConfig` (config.rs:209),
plus a Planner-only `PlannerConfig { model, mechanism }` (config.rs:112). Roles
share the single backend.

New types:

```rust
pub struct ProviderConfig {            // a named ACP backend
    pub name: String,
    pub command: String,
    #[serde(default)] pub args: Vec<String>,
    #[serde(default)] pub env: BTreeMap<String, String>,
}

pub struct RoleAssignment {            // how a role is staffed
    pub provider: String,              // references ProviderConfig.name
    #[serde(default)] pub mode: Option<String>,    // default modeId
    #[serde(default)] pub model: Option<String>,   // default model option value
    #[serde(default)] pub effort: Option<String>,  // default thought_level value
}

pub struct RolesConfig {
    pub planner: Option<RoleAssignment>,
    pub developer: Option<RoleAssignment>,
    pub reviewer: Option<RoleAssignment>,
}
```

- `GlobalConfig` gains `providers: Vec<ProviderConfig>` and `roles: RolesConfig`.
- **Back-compat resolution** (config.rs:396 `resolve`): if `providers` is empty
  but a legacy `[backend]` exists, synthesise `ProviderConfig { name: "default",
  … }` and assign it to every unset role. Existing configs keep working unchanged.
- Project layer may override `roles` (and add project-local providers); merge is
  field-wise like the existing `caps` override.
- **Validation** (config.rs:447): every `RoleAssignment.provider` must name a
  declared provider; provider commands non-empty; names unique.

## 0038 — ACP mode discovery (`crates/makina-acp`)

`protocol.rs` mirrors a minimal ACP subset (PROTOCOL_VERSION = 1). Add:

```rust
pub struct SessionMode { pub id: String, pub name: String,
                         #[serde(default)] pub description: Option<String> }
pub struct SessionModeState { pub current_mode_id: String,
                              pub available_modes: Vec<SessionMode> }
```

- `NewSessionResult` (protocol.rs:330) gains
  `#[serde(default)] modes: Option<SessionModeState>`.
- New request params `SetModeParams { session_id, mode_id }` for
  `session/set_mode`.
- `SessionUpdate` (protocol.rs:432) gains `CurrentModeUpdate { current_mode_id:
  String }` before the `Other` arm.
- `client.rs`: capture `modes` from the `session/new` result (client.rs:388) onto
  the session handle; add `AcpSession::set_mode(mode_id)`; forward
  `current_mode_update` as a new client event.

## 0039 — ACP config-option discovery (model + effort)

ACP exposes a generic config-option surface; the model picker and reasoning
effort are categories within it. Mirror it generically so we are forward-compatible
with the RFD:

```rust
pub struct ConfigOptionChoice { pub value: String, pub name: String,
                                #[serde(default)] pub description: Option<String> }
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    pub category: String,            // "model" | "model_config" | "thought_level" | …
    #[serde(rename = "type")] pub kind: String,   // "select" | "boolean" | …
    pub current_value: Option<serde_json::Value>,
    #[serde(default)] pub options: Vec<ConfigOptionChoice>,
    #[serde(flatten)] pub extra: HashMap<String, serde_json::Value>,
}
```

- `NewSessionResult` gains `#[serde(default)] config_options: Vec<ConfigOption>`
  (tolerate absence — many agents send none).
- New request `SetConfigOptionParams { session_id, option_id, value }` for
  `session/set_config_option`.
- `client.rs`: capture `config_options`; add `AcpSession::set_config_option(id,
  value)`. The **model** is the option with `category == "model"`; **effort** is
  `category == "thought_level"`; both are just options to the protocol layer.
- Unknown categories are preserved (via `extra`) and surfaced verbatim so the UI
  can render them with graceful degradation.

## 0040 — Wiring (core ↔ acp ↔ api)

- **Backend per provider.** Where `main.rs`/orchestrator builds the single
  backend today, build a map `name → Arc<dyn AgentBackend>` from
  `config.providers` (lazily / cached). `AcpBackend::new` takes the provider's
  `command/args/env`.
- **Role → backend.** `DeveloperArgs`/`ReviewerArgs` (developer.rs:100,
  reviewer.rs:94) already take `backend: Arc<dyn AgentBackend>`; pass the backend
  named by the role's assignment. Planner's `one-shot-agent` path gets the same
  treatment.
- **Apply selections.** Extend `SessionConfig` (backend.rs:74) with optional
  `mode`/`model`/`effort` (or carry them in `extra`); after `session/new`, the
  ACP backend calls `set_mode` / `set_config_option` to apply the role's defaults
  (or a TUI-chosen override). `roles::session_config_for` (roles.rs:184) gains
  these params.
- **Surface capabilities up.** Add an `api` view (e.g. `SessionCapabilities {
  modes, options, current_* }`) and an event (`Event::SessionCapabilities` or fold
  into the existing exchange/session events) so the TUI can render what the live
  agent advertised, and a `current_mode_update` event to reflect autonomous
  changes.

## 0041 — TUI provider/role editor (`crates/makina/src`)

Reuse the proven modal pattern (`Mode::FileBrowser`, app.rs:320; `List` +
`ListState`, ui.rs):

- `Mode::ProviderConfig` + `provider_editor: Option<ProviderEditor>` on `App`.
- `ProviderEditor` state: list of providers (add/edit/remove), role→provider
  assignment, and per-role mode/model/effort selection. Selection lists are
  populated from **discovered** `SessionCapabilities` when available, else from
  declared config (and free-text entry for first-time setup).
- The model selector renders each choice as `name · effort` (model option joined
  with the `thought_level` option) exactly as requested.
- `AppEvent` variants for navigation/edit/commit; on commit, write the updated
  providers + role assignments back to the `config.toml` the run was loaded from
  (best-effort, with an error surfaced in the pane on failure).
- Status-bar hint (a `[g]`/`[,]`-style key) opens the editor; coordinate the key
  with plan 0012's status-bar work to avoid collisions.

## Data flow (a Developer turn after this plan)

```
config.toml ─ providers + roles ─► orchestrator builds backend("grok") for Developer
   ► session/new (cwd) ─► result.modes + result.config_options captured
   ► set_mode("code"); set_config_option("model","grok-…"); set_config_option("thought_level","high")
   ► prompt turn … (current_mode_update if the agent switches)
TUI editor ◄─ SessionCapabilities (modes/options) ──► user overrides ──► same set_* calls
```

## Phased build / test strategy

1. **0037** config types + merge + back-compat + validation — unit tests:
   `legacy_backend_becomes_default_provider`, `role_assignment_resolves_provider`,
   `unknown_provider_rejected`.
2. **0038/0039** protocol round-trip tests: `session_new_parses_modes`,
   `session_new_parses_config_options_and_preserves_unknown`,
   `set_mode_and_set_config_option_serialize`; mock agent advertising
   modes+options; `current_mode_update` lands as the new variant (not `Other`).
3. **0040** wiring: a two-provider config drives Developer and Reviewer to
   different backends (`roles_use_distinct_providers`); selected mode/model/effort
   reach the (mock) `set_*` calls.
4. **0041** modal: render/selection tests for the editor; commit writes config
   and re-reads it.

`cargo test`, `clippy --all-targets -D warnings`, `fmt --check` stay green at each
phase.

## Interaction with prior plans / FUTURE

- Independent of 0009/0010/0012; can proceed in parallel (coordinate the editor
  hotkey with 0012's status bar).
- Lays the backend-per-provider groundwork the FUTURE "Cost-tiered routing across
  multiple backends" direction builds on, without committing to auto-routing here.

## Future work (not in this plan)

- Automatic cost/complexity routing; `UsageReport` aggregation.
- Direct-API (non-ACP) backends; custom-llm-endpoint configuration.
- Per-task overrides; auth-method selection UI.
