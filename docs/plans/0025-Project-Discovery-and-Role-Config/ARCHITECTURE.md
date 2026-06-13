# Architecture — Plan 0025

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). This plan touches `makina-core` (config, roles, interpreter, a
> new `discovery` module, the api `Event` enum) and the `makina` TUI (the
> generalised config writer + the trigger/action wiring).

## Current shape (what exists)

- **Role config** (`crates/makina-core/src/config.rs`): `RoleAssignment` (`:157`)
  carries `provider` + optional `mode` / `model` / `effort`. `RolesConfig`
  (`:181`) holds `Option<RoleAssignment>` for `planner` / `developer` /
  `reviewer`. `load_defaults_with_paths` (`:864`) is **load-only** — there is no
  save in `makina-core`.
- **Gates** (`config.rs`): `GateConfig { name, command, image }` (`:395`) derives
  **only `Deserialize`**. Gates live on the **project** layer (`ProjectConfig
  .gates`, `:446`) and resolve into `Config.gates` (`:510`); they are *not* a
  `GlobalConfig` field (`:312`). The `[[gates]]` TOML shape is documented at
  `config.rs:386`.
- **Role prompts** (`crates/makina-core/src/roles.rs`): `DEVELOPER_SYSTEM_PROMPT`
  (`:98`), `REVIEWER_SYSTEM_PROMPT` (`:133`), `system_prompt_for(Role) ->
  &'static str` (`:166`), and `session_config_for(Role, working_dir,
  Option<RoleAssignment>) -> SessionConfig` (`:194`) which sets
  `system_prompt: system_prompt_for(role).to_string()`. **Note:** `Role` (`:80`)
  has only `Developer` and `Reviewer`; the Planner's prompt is the separate
  `PLANNER_SYSTEM_PROMPT` constant in `interpreter.rs:604`, applied via
  `ModelInterpreter::with_system_prompt` (`:695`).
- **One-shot model pass** (`crates/makina-core/src/interpreter.rs`):
  `ModelInterpreter::interpret` (`:724`) spawns an `AgentBackend` session, sends
  one `Prompt`, drains `ResponseEvent::TextChunk` to `TurnComplete`, then
  `parse_model_response` (`:781`) calls `crate::json::extract_json_object` (`json
  .rs:12`) + `serde_json`. This is the template for the discovery pass.
- **Backend abstraction** (`crates/makina-core/src/backend.rs`): `AgentBackend`
  (`:258`) `spawn(SessionConfig) -> Box<dyn AgentSession>`; `AgentSession` (`:291`)
  `prompt(Prompt) -> ResponseStream`; `ResponseEvent` (`:140`); the in-crate
  `StubBackend` test pattern (`backend.rs:368`).
- **Gate execution** (`crates/makina-core/src/actors/supervisor.rs`): after the
  Develop turn, `run_gates(&ctx.config.gates, worktree_path)` (`:2034`) runs the
  single gate list; on exhaustion of the cap it applies `TaskEvent::GateCapReached`
  (`:2113`) and fails with `api::FailureKind::GateCap` (`:2118`). On pass it
  advances to `InReview` (`:2049`).
- **The 0011 config writer** (`crates/makina/src/event.rs`):
  `commit_provider_config` (`:323`) reads `config_file(repo_root)` as a
  `GlobalConfig`, replaces `providers` + `roles`, and re-serialises with
  `..existing_global`. It is **lossy for gates**: it parses the project file as
  `GlobalConfig`, which has no `gates` field, so any `[[gates]]` already on disk
  would be dropped on save. Generalising this is the heart of 0075.
- **App + open flow** (`crates/makina/src/app.rs`, `event.rs`): `App` stores
  `providers` (`:649`), `roles` (`:652`), `config_paths` (`:788`), and
  `repo_root`; `App::with_config` (`app.rs:987`) seeds them at startup
  (`main.rs:282`). The `OpenRun` command originates in the TUI IO layer
  (`event.rs:265` `execute(Command::OpenRun { task_list_path })`). `main.rs:51`
  `Config::load_defaults_with_paths` is the only config-touch site at startup;
  there is no per-project init/discovery hook.
- **Failure surface** (`crates/makina-core/src/api.rs`): `FailureKind` (`:180`)
  already includes `GateCap`; `Event` (`:629`) is the TUI broadcast enum.

## 0073 — Per-role system prompt config

Edits in `config.rs`, `roles.rs`, `interpreter.rs`.

- **Config fields.** Extend `RoleAssignment` (`config.rs:157`) with two optional
  fields (serde-default `None`, so existing configs parse unchanged) and add
  `Serialize` to its derive (needed by the 0075 writer):

  ```rust
  /// Project-specific instructions appended to (or, with `replace`, substituted
  /// for) the role's built-in system prompt.
  #[serde(default)]
  pub system_prompt: Option<String>,

  /// How `system_prompt` combines with the built-in constant:
  /// `"append"` (default) or `"replace"`.
  #[serde(default)]
  pub system_prompt_mode: Option<String>,
  ```

- **Effective-prompt helper.** Add to `roles.rs` (keep the existing
  `system_prompt_for(Role) -> &'static str` as the built-in lookup; add an
  assignment-aware variant — name it `effective_system_prompt` to avoid the
  signature clash, or shadow with an overload-free new name):

  ```rust
  /// Combine the built-in role constant with the role's configured
  /// `system_prompt` per `system_prompt_mode` (default "append").
  pub fn effective_system_prompt(role: Role, assignment: Option<&RoleAssignment>) -> String {
      let builtin = system_prompt_for(role); // &'static str
      match assignment.and_then(|a| a.system_prompt.as_deref()) {
          None => builtin.to_string(),
          Some(custom) => match assignment.and_then(|a| a.system_prompt_mode.as_deref()) {
              Some("replace") => custom.to_string(),
              _ => format!("{builtin}\n\n{custom}"), // append (default)
          },
      }
  }
  ```

  Wire it into `session_config_for` (`roles.rs:194`): replace
  `system_prompt: system_prompt_for(role).to_string()` with
  `system_prompt: effective_system_prompt(role, assignment.as_ref())` — and read
  `mode/model/effort` from `assignment` *before* moving it (the current code
  destructures the `Option` first, so keep a borrow for the prompt).

- **Planner.** The Planner is not a `roles::Role`. Apply the same combine logic
  for `RolesConfig::planner`: where the orchestrator builds the
  `ModelInterpreter`, call `.with_system_prompt(combine(PLANNER_SYSTEM_PROMPT,
  planner_assignment))` using a small free fn that mirrors
  `effective_system_prompt` but takes the planner constant. (Factor the
  combine-string logic into one private `fn combine_prompt(builtin: &str, custom:
  Option<&str>, mode: Option<&str>) -> String` reused by both call sites.)

## 0074 — LLM project-discovery agent

New module `crates/makina-core/src/discovery.rs` (declare `pub mod discovery;` in
`lib.rs`). Mirrors `interpreter.rs`'s one-shot shape.

- **Result type** (deserialised from the model's JSON):

  ```rust
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
  pub struct DiscoveredGate { pub name: String, pub command: String }

  #[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
  pub struct DiscoveryResult {
      #[serde(default)]
      pub gates: Vec<DiscoveredGate>,
      /// Role name ("developer"/"reviewer"/"planner") → constraint prose.
      #[serde(default)]
      pub role_constraints: std::collections::BTreeMap<String, String>,
  }
  ```

- **Repo scan (deterministic, IO-light).** A `fn gather_repo_context(repo_root:
  &Path) -> String` reads a bounded set of files if present — manifests
  (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, `Makefile`) and the
  prose docs (`README*`, `CONTRIBUTING*`, `AGENTS*`) — truncating each to a cap
  and labelling each block with its filename. Return the concatenated context plus
  the list of files actually read (the latter feeds the `[discovery]` stamp in
  0075):

  ```rust
  pub struct RepoContext { pub text: String, pub scanned_files: Vec<String> }
  pub fn gather_repo_context(repo_root: &Path) -> RepoContext { /* … */ }
  ```

- **Discovery prompt.** A `DISCOVERY_SYSTEM_PROMPT` constant instructing the model
  to emit ONLY a JSON object matching `DiscoveryResult` (`gates`:
  name+shell-command that must exit 0; `role_constraints`: short prose per role),
  no prose/fences — same strictness as `PLANNER_SYSTEM_PROMPT`.

- **The pass.** Mirror `interpret` (`interpreter.rs:724`):

  ```rust
  pub async fn discover_project(
      backend: &dyn AgentBackend,
      repo_root: &Path,
  ) -> Result<(DiscoveryResult, Vec<String>), DiscoveryError> {
      let ctx = gather_repo_context(repo_root);
      let cfg = SessionConfig {
          working_dir: repo_root.to_path_buf(),
          system_prompt: DISCOVERY_SYSTEM_PROMPT.to_string(),
          mode: None, model: None, effort: None, extra: None,
      };
      let mut session = backend.spawn(cfg).await?;
      let mut stream = session.prompt(Prompt::new(format!(
          "Inspect this repository and output ONLY the discovery JSON:\n\n{}",
          ctx.text
      ))).await?;
      let mut raw = String::new();
      while let Some(item) = stream.next().await {
          match item? {
              ResponseEvent::TextChunk { text } => raw.push_str(&text),
              ResponseEvent::TurnComplete => break,
              _ => {}
          }
      }
      drop(stream);
      let _ = session.terminate().await;
      Ok((parse_discovery_result(&raw)?, ctx.scanned_files))
  }
  ```

- **Parser (the deterministic seam, unit-testable without a model).**

  ```rust
  pub fn parse_discovery_result(raw: &str) -> Result<DiscoveryResult, DiscoveryError> {
      let json = crate::json::extract_json_object(raw)
          .ok_or(DiscoveryError::NoJsonObject)?;
      serde_json::from_str(json).map_err(DiscoveryError::Deserialize)
  }
  ```

  `DiscoveryError` is a `thiserror` enum (`NoJsonObject`, `Deserialize(#[from]
  serde_json::Error)`, `Backend(#[from] BackendError)`), matching
  `VerdictParseError`'s shape (`roles.rs:243`). `extract_json_object` is currently
  `pub(crate)` (`json.rs:12`) — it is already used across `interpreter` and
  `roles`, so the in-crate `discovery` module can call it directly with no
  visibility change.

## 0075 — Discovery trigger + persistence

Edits in a new `makina-core` config-writer fn + `crates/makina/src/event.rs`
(generalised writer, trigger, action) + `api.rs` (an `Event` for surfacing).

### Generalised, merge-preserving config writer

The lossy `commit_provider_config` (`event.rs:323`) parses the project config as
`GlobalConfig`, which has **no `gates` field**. To persist gates + the
`[discovery]` stamp without losing other sections, introduce a dedicated writable
view of the **project** config and a writer in `makina-core` (so 0026/0028 can
reuse it):

- Add a `Serialize`-able `ProjectConfigWrite` (or extend `ProjectConfig` to derive
  `Serialize` — it currently derives only the parse side) covering `gates`,
  `base_branch`, `caps`, `concurrency`, plus the new `[discovery]` stamp and the
  role `system_prompt`s. Add `Serialize` to `GateConfig`'s derive (`config.rs:395`,
  currently `Deserialize` only) and a `#[serde(default)] pub source:
  Option<String>` field so each gate can carry `source = "discovered"`.
- Add the stamp type:

  ```rust
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
  pub struct DiscoveryStamp {
      /// RFC3339 timestamp of the last discovery run.
      pub last_run: String,
      /// Repo files the discovery pass actually read.
      pub scanned_files: Vec<String>,
  }
  ```

  with `#[serde(default)] pub discovery: Option<DiscoveryStamp>` on the project
  config. `Config::load` / `Config::validate` ignore it (it is metadata); only the
  writer reads/writes it.

- Add `makina_core::config::write_project_config(repo_root, &ProjectConfigWrite)
  -> io::Result<()>` that reads the existing `config_file(repo_root)` (round-trip
  via `toml`), applies the edit, and writes back — the merge-preserving pattern
  generalised from `commit_provider_config` so unrelated sections survive.

### Applying a `DiscoveryResult`

A pure `fn apply_discovery(project: &mut ProjectConfigWrite, roles: &mut
RolesConfig, result: &DiscoveryResult, now: &str, scanned: &[String])`:

1. For each `DiscoveredGate`, push a `GateConfig { name, command, image: None,
   source: Some("discovered") }` onto `project.gates` **after removing any prior
   `source == "discovered"` entries** (so a re-run replaces, not duplicates,
   discovered gates while leaving manual gates untouched).
2. For each `(role, constraint)` in `result.role_constraints`, fold it into that
   role's `RoleAssignment.system_prompt` via the **append** combine (creating a
   default `RoleAssignment` if the role is unset). This routes through the 0073
   append path, preserving the built-in contract.
3. Set `project.discovery = Some(DiscoveryStamp { last_run: now.into(),
   scanned_files: scanned.to_vec() })`.

### Trigger: auto on first open, idempotent

In `crates/makina/src/event.rs`, in the IO resolution of `OpenRun` (the
`execute(Command::OpenRun …)` path, `event.rs:265`), before issuing `OpenRun`:

- Read the project config; if its `[discovery]` stamp is **absent**, run
  `discovery::discover_project(backend, repo_root)`, `apply_discovery`, and
  `write_project_config`, then emit an `Event` so the TUI shows what happened
  (status line + the discovered gate count). If the stamp is present, skip — the
  idempotency guarantee.
- Discovery failure is **non-fatal**: log + status-message and proceed to
  `OpenRun` (a repo with no discoverable gates should still open). This mirrors the
  best-effort posture of seed-persist (`orchestrator.rs:808`).

### Re-runnable action

Add an `AppEvent::DiscoverProject` (an IO-resolved event, like
`ProviderEditorCommit`, `event.rs:294`) that force-re-runs discovery regardless of
the stamp (re-scans, replaces the `source = "discovered"` gates, re-stamps
`last_run`). Bind it to a concrete key/menu entry (the seed of the future Ctrl+P
"Discover project" command). After a force re-run, refresh `app.roles` from the
re-read config so the settings editor reflects the folded constraints.

### Surfacing event

Add `Event::ProjectDiscovered { gate_count: usize, scanned_files: usize }` to the
`Event` enum (locate by name; `api.rs:629` is a hint) — emitted on both the auto
and forced paths so the TUI can render a transient "Discovered N gates from M
files" status without re-reading disk.

## Testing notes

- **0073** (`roles.rs`): `append_extends_builtin`
  (`effective_system_prompt(Developer, Some{system_prompt:"X"}) ==
  format!("{DEVELOPER_SYSTEM_PROMPT}\n\nX")`); `replace_overrides`
  (`mode="replace"` ⇒ exactly `"X"`); `absent_uses_builtin` (`None` ⇒
  `DEVELOPER_SYSTEM_PROMPT`). Plus a `session_config_for` test that the effective
  prompt flows into `SessionConfig::system_prompt`.
- **0074** (`discovery.rs`): `parse_discovery_result_from_json` (a known JSON ⇒
  the expected `DiscoveryResult`); `malformed_response_errors_cleanly`
  (no-JSON ⇒ `NoJsonObject`; bad-shape ⇒ `Deserialize`). The pass itself is driven
  by a `StubBackend` (the `backend.rs:368` pattern) whose stream yields a fixed
  JSON `TextChunk` then `TurnComplete`, asserting `discover_project` returns the
  parsed result + scanned files — **no real model call**.
- **0075** (writer/trigger): `first_open_runs_and_writes_discovery` (temp repo +
  no-stamp config + stub backend ⇒ after the open path, `config.toml` contains the
  `source = "discovered"` gates and a `[discovery]` stamp);
  `second_open_skips_when_stamped` (stamp present ⇒ the stub backend is *not*
  spawned and gates are unchanged); `force_rerun_overwrites` (`DiscoverProject`
  re-runs even when stamped, replacing discovered gates and updating `last_run`);
  plus a writer round-trip test that a manual `[[gates]]` entry survives a
  discovery write (no loss). Use `tempfile` + a stub backend; no network.

`cargo test`, `clippy --all-targets -- -D warnings`, and `fmt --check` stay green.

## Interaction with prior/sibling plans

- **0011** shipped `ProviderEditor` + `commit_provider_config`; 0075 generalises
  that writer and 0073's new `system_prompt` fields show up in the same editor
  (the editor already round-trips `RolesConfig`, `app.rs:652`).
- **0027** (plan-directory discovery) is orthogonal: it surfaces `docs/plans/*/`
  in the sidebar; this plan inspects the *repo* for gates/conventions. The two
  share no symbols.
- **0028** (planner-generate a missing `TASKS.md`) reuses 0073's planner prompt
  wiring and 0075's writer; this plan does not add the generate path.
- **0024** owns the duration/model metrics header and the `usage:
  Option<UsageStats>` slot; no metric work here.
