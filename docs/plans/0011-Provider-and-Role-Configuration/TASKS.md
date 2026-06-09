# Makina Plan 0011 — Provider & Role Configuration

Introduce named ACP providers, assign them per role (Planner / Developer /
Reviewer), discover session **modes** and **model/effort config options** from
the live agent over ACP, apply selections, and add a TUI editor to manage it all.

See [SCOPE.md](SCOPE.md) for boundaries and what ACP actually exposes, and
[ARCHITECTURE.md](ARCHITECTURE.md) for the type deltas and build order. Build the
workstreams in numeric order — each compiles and tests before the next.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).
- ACP grounding: **modes** are stable v1 (`session/new` → `modes`,
  `session/set_mode`, `current_mode_update`); **model/effort** ride the generic
  config-option surface (`session/set_config_option`, categories `model` /
  `model_config` / `thought_level`) and may be absent — degrade gracefully.

---

## 0037 — Provider & role config schema

### add-provider-and-role-config — Named providers and per-role assignment

Today `GlobalConfig` has a single `BackendConfig { command, args }` shared by all
roles. Add named providers and a role→provider mapping with default
mode/model/effort, preserving back-compat.

**Steps:**

1. In `crates/makina-core/src/config.rs`, add the new types:

   ```rust
   #[derive(Debug, Clone, Serialize, Deserialize)]
   pub struct ProviderConfig {
       pub name: String,
       pub command: String,
       #[serde(default)] pub args: Vec<String>,
       #[serde(default)] pub env: std::collections::BTreeMap<String, String>,
   }

   #[derive(Debug, Clone, Default, Serialize, Deserialize)]
   pub struct RoleAssignment {
       pub provider: String,
       #[serde(default)] pub mode: Option<String>,
       #[serde(default)] pub model: Option<String>,
       #[serde(default)] pub effort: Option<String>,
   }

   #[derive(Debug, Clone, Default, Serialize, Deserialize)]
   pub struct RolesConfig {
       #[serde(default)] pub planner: Option<RoleAssignment>,
       #[serde(default)] pub developer: Option<RoleAssignment>,
       #[serde(default)] pub reviewer: Option<RoleAssignment>,
   }
   ```

2. Add to `GlobalConfig`: `#[serde(default)] pub providers: Vec<ProviderConfig>`
   and `#[serde(default)] pub roles: RolesConfig`.

3. In `Config::resolve` (the merge), add back-compat: if `providers` is empty but
   a legacy `[backend]` exists, synthesise
   `ProviderConfig { name: "default".into(), command: backend.command, args: backend.args, env: default }`
   and assign `RoleAssignment { provider: "default", .. }` to every role left
   `None`.

4. In `Config::validate`, reject a `RoleAssignment.provider` that names no
   declared provider; require unique provider names and non-empty commands.

5. Add tests:

   ```rust
   #[test] fn legacy_backend_becomes_default_provider() { /* config with only [backend] → resolve → provider "default" assigned to all roles */ }
   #[test] fn role_assignment_resolves_provider() { /* two providers + roles.developer.provider = "b" → resolve maps Developer → "b" */ }
   #[test] fn unknown_provider_rejected() { /* roles.reviewer.provider = "nope" → validate() Err */ }
   ```

- **Depends on:** —
- **Done when:** the three tests pass; `grep -n 'struct ProviderConfig\|struct RoleAssignment' crates/makina-core/src/config.rs` matches; existing config tests (single `[backend]`) still pass; cargo test/clippy/fmt green.

---

## 0038 — ACP session-mode discovery

### acp-discover-and-set-session-mode — Mirror modes; add set_mode; handle updates

**Steps:**

1. In `crates/makina-acp/src/protocol.rs`, add (near the other session structs):

   ```rust
   #[derive(Debug, Clone, Deserialize)]
   pub struct SessionMode { pub id: String, pub name: String,
       #[serde(default)] pub description: Option<String> }

   #[derive(Debug, Clone, Deserialize)]
   #[serde(rename_all = "camelCase")]
   pub struct SessionModeState { pub current_mode_id: String,
       pub available_modes: Vec<SessionMode> }
   ```

2. Add `#[serde(default)] pub modes: Option<SessionModeState>` to
   `NewSessionResult`.

3. Add request params and a `SessionUpdate` variant (before the `#[serde(other)]
   Other` arm):

   ```rust
   #[derive(Debug, Clone, Serialize)]
   #[serde(rename_all = "camelCase")]
   pub struct SetModeParams { pub session_id: String, pub mode_id: String }

   // in enum SessionUpdate:
   #[serde(rename = "current_mode_update")]
   CurrentModeUpdate {
       #[serde(rename = "currentModeId")] current_mode_id: String,
   },
   ```

4. In `crates/makina-acp/src/client.rs`, capture `modes` from the `session/new`
   result onto the session handle; add `AcpSession::set_mode(&self, mode_id: &str)`
   that sends a `session/set_mode` request with `SetModeParams`; surface
   `current_mode_update` as a new client event (alongside thoughts/tools).

5. Tests in the protocol/client modules:

   ```rust
   #[test] fn session_new_parses_modes() { /* a session/new result with modes → NewSessionResult.modes is Some with 2 available_modes */ }
   #[test] fn set_mode_serializes() { /* SetModeParams → {"sessionId":…,"modeId":…} */ }
   #[test] fn current_mode_update_is_not_other() { /* a current_mode_update notification parses to CurrentModeUpdate, not Other */ }
   ```

- **Depends on:** —
- **Done when:** the three tests pass; `grep -n 'SessionModeState\|set_mode' crates/makina-acp/src` matches; existing protocol tests (unknown update → Other) still pass; cargo test/clippy/fmt green.

---

## 0039 — ACP config-option (model + effort) discovery

### acp-discover-and-set-config-options — Mirror config options; add set_config_option

Model and reasoning effort are config-option categories on the session. Mirror
the surface generically so unknown categories are preserved.

**Steps:**

1. In `crates/makina-acp/src/protocol.rs`, add:

   ```rust
   #[derive(Debug, Clone, Deserialize)]
   pub struct ConfigOptionChoice { pub value: String, pub name: String,
       #[serde(default)] pub description: Option<String> }

   #[derive(Debug, Clone, Deserialize)]
   #[serde(rename_all = "camelCase")]
   pub struct ConfigOption {
       pub id: String,
       pub name: String,
       pub category: String,                 // "model" | "model_config" | "thought_level" | …
       #[serde(rename = "type")] pub kind: String,
       #[serde(default)] pub current_value: Option<serde_json::Value>,
       #[serde(default)] pub options: Vec<ConfigOptionChoice>,
       #[serde(flatten)] pub extra: std::collections::HashMap<String, serde_json::Value>,
   }
   ```

2. Add `#[serde(default)] pub config_options: Vec<ConfigOption>` to
   `NewSessionResult` (tolerate absence — most agents send none).

3. Add request params:

   ```rust
   #[derive(Debug, Clone, Serialize)]
   #[serde(rename_all = "camelCase")]
   pub struct SetConfigOptionParams { pub session_id: String, pub option_id: String,
       pub value: serde_json::Value }
   ```

4. In `client.rs`, capture `config_options` onto the session handle; add
   `AcpSession::set_config_option(&self, option_id: &str, value: serde_json::Value)`
   sending `session/set_config_option`. Convenience: `set_model` /
   `set_effort` locate the option whose `category` is `"model"` /
   `"thought_level"` and call `set_config_option`.

5. Tests:

   ```rust
   #[test] fn session_new_parses_config_options_and_preserves_unknown() { /* options incl. an unknown category → parsed; extra retains unknown fields */ }
   #[test] fn set_config_option_serializes() { /* → {"sessionId":…,"optionId":…,"value":…} */ }
   ```

- **Depends on:** acp-discover-and-set-session-mode
- **Done when:** both tests pass; `grep -n 'ConfigOption\|set_config_option' crates/makina-acp/src` matches; a `session/new` result with no config options still parses; cargo test/clippy/fmt green.

---

## 0040 — Wire providers and selections through the orchestrator

### wire-per-role-providers-and-selections — One backend per provider; apply selections per role

**Steps:**

1. Where the single backend is built today (in `main.rs` / the orchestrator
   bootstrap), build a map `name → Arc<dyn AgentBackend>` from `config.providers`
   via `AcpBackend::new(command, args, env)` (cache; build lazily per provider).

2. Hand each role the backend named by its `RoleAssignment.provider`:
   `DeveloperArgs.backend` / `ReviewerArgs.backend` (in
   `crates/makina-core/src/actors/{developer,reviewer}.rs`) receive the resolved
   `Arc`. The Planner `one-shot-agent` mechanism (`actors/planner.rs`) likewise
   uses its assignment's provider.

3. Carry the role's mode/model/effort defaults to the session. Extend
   `SessionConfig` (`crates/makina-core/src/backend.rs`) with
   `#[serde(default)] pub mode: Option<String>`, `model`, `effort` (or carry them
   in the existing `extra`), and thread them through
   `roles::session_config_for(role, working_dir, assignment)`.

4. In the ACP backend (`crates/makina-acp/src/backend.rs`), after `session/new`,
   apply the selections by calling `set_mode` / `set_config_option` when the
   corresponding option/mode is advertised by the agent (skip silently if not).

5. Surface discovered capabilities up to the view layer: add an
   `api::SessionCapabilities { modes, options }` (mirroring the protocol structs
   at the api level) and an event that carries them when a session opens, plus a
   `current_mode_update` event, so the TUI (0041) can render what the live agent
   offers.

6. Tests:

   ```rust
   #[tokio::test] async fn roles_use_distinct_providers() { /* config with two providers → Developer and Reviewer get backends built from different commands */ }
   #[tokio::test] async fn selections_applied_after_session_new() { /* mock agent advertising a mode + a model option → set_mode/set_config_option are invoked with the role's defaults */ }
   ```

- **Depends on:** add-provider-and-role-config, acp-discover-and-set-config-options
- **Done when:** both tests pass; `grep -n 'SessionCapabilities' crates/makina-core/src/api.rs` matches; a single-`[backend]` config still runs (one provider for all roles); cargo test/clippy/fmt green.

---

## 0041 — TUI provider/role editor

### add-provider-role-editor-modal — Configure providers, roles, model + effort

Reuse the proven modal pattern (`Mode::FileBrowser` + `List`/`ListState`) to add
a settings editor.

**Steps:**

1. In `crates/makina/src/app.rs`, add `Mode::ProviderConfig` to the `Mode` enum
   and `pub provider_editor: Option<ProviderEditor>` to `App` (init `None` in
   `App::new`). Define `ProviderEditor` holding: the editable list of providers,
   the role→provider assignments, and per-role mode/model/effort selections. The
   selection lists are populated from `api::SessionCapabilities` when available,
   else from the declared config (free-text entry for first-time setup).

2. Add `AppEvent` variants for open/close/navigate/edit/commit (mirror the
   `Browser*` events). Open the editor with a hotkey that does **not** collide
   with plan 0012's `[v]` — use `g` (for "aGents/config"). Coordinate with plan
   0012, which owns the status-bar hint string; add `[g] config` there.

3. In `crates/makina/src/ui.rs`, render the modal as an overlay using
   `List` + `ListState` (like the file browser). Render each model choice as
   `name · effort` by joining the `model` option value with the selected
   `thought_level`, exactly as requested.

4. On commit, write the updated `providers` + `roles` back to the `config.toml`
   the run was loaded from (serialise via `toml`), best-effort; on IO/serialise
   error push an error-pane message rather than crashing.

5. Tests:

   ```rust
   #[test] fn provider_editor_opens_and_lists_providers() { /* press 'g' → Mode::ProviderConfig; render shows configured providers + role rows */ }
   #[test] fn provider_editor_commit_writes_config() { /* edit an assignment + commit → the temp config.toml round-trips the change */ }
   ```

- **Depends on:** wire-per-role-providers-and-selections
- **Done when:** both tests pass; `grep -n 'Mode::ProviderConfig' crates/makina/src` matches; the editor opens with `g`, lists providers/roles, shows model `name · effort`, and commit persists to config; cargo test/clippy/fmt green.

---

## Cross-cutting verification

### plan-0011-acceptance — Two providers, two roles, applied selections

Add one end-to-end test: a config declaring two providers with Developer → A and
Reviewer → B, each with a default mode and model/effort; drive a gated run with
mock agents that advertise those modes/options; assert each role's session was
opened on the correct backend and that `set_mode` / `set_config_option` were
invoked with the role's defaults.

- **Depends on:** wire-per-role-providers-and-selections
- **Done when:** the test (name containing `two_providers_two_roles`) passes and
  would have failed before 0037–0040; cargo test/clippy/fmt green.

---

**End of plan 0011 TASKS.** When every "Done when" bullet is green, Makina can run
different ACP providers per role and pick each role's model + effort from what the
live agent advertises, all from a TUI editor — with single-`[backend]` configs
still working unchanged.
