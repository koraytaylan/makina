# Makina Plan 0025 — Project Discovery & Role-Instruction Config

Add an **LLM-driven project-discovery pass** that inspects a repo (manifests +
`README`/`CONTRIBUTING`/`AGENTS`) and proposes gate commands and per-role
constraint instructions, then writes them to `config.toml` as the deterministic,
auditable, editable record. Only discovery is model-driven; execution stays
deterministic. Introduces per-role `system_prompt` config and the config **write**
path (the keystone for plans 0026 and 0028).

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0073 — Per-role system prompt config

### role-system-prompt-config — Configurable, append-by-default role instructions

Let a project attach prose instructions to each role's system prompt, appended to
the built-in constant by default (preserving every role's invariant contract,
especially the Reviewer JSON-verdict protocol), with a `replace` escape hatch.

**Steps:**

1. In `crates/makina-core/src/config.rs`, add two fields to `RoleAssignment`
   (`config.rs:157`), each `#[serde(default)]`:
   `pub system_prompt: Option<String>` and
   `pub system_prompt_mode: Option<String>` (`"append"` | `"replace"`; absent ⇒
   append). Existing configs must still parse (defaults make both optional). Keep
   `RoleAssignment`'s `Serialize` derive (it already derives `Serialize,
   Deserialize`) so the 0075 writer can round-trip the new fields.

2. In `crates/makina-core/src/roles.rs`, add a private
   `fn combine_prompt(builtin: &str, custom: Option<&str>, mode: Option<&str>) ->
   String` (`None` ⇒ `builtin.to_string()`; `Some(c)` + `mode == Some("replace")`
   ⇒ `c.to_string()`; else ⇒ `format!("{builtin}\n\n{c}")`) and a public
   `pub fn effective_system_prompt(role: Role, assignment: Option<&RoleAssignment>)
   -> String` that calls `combine_prompt(system_prompt_for(role),
   a.system_prompt.as_deref(), a.system_prompt_mode.as_deref())`. Leave the
   existing `system_prompt_for(Role) -> &'static str` (`roles.rs:166`) intact as
   the built-in lookup.

3. Wire it into `session_config_for` (`roles.rs:194`): set
   `system_prompt: effective_system_prompt(role, assignment.as_ref())`, reading the
   prompt from the borrowed `assignment` **before** the existing destructure that
   moves `mode/model/effort` out of it.

4. For the Planner, apply the same `combine_prompt` to `PLANNER_SYSTEM_PROMPT`
   (`interpreter.rs:604`) at the site where the orchestrator constructs the
   `ModelInterpreter` (it already calls `.with_system_prompt(…)`,
   `interpreter.rs:695`), using `RolesConfig::planner`'s assignment. If no such
   wiring site is reachable from this task's scope, expose a
   `pub fn planner_system_prompt(assignment: Option<&RoleAssignment>) -> String`
   in `interpreter.rs` (built on the same combine logic) and document it as the
   intended call site so 0028 can adopt it.

5. Add tests in `roles.rs`:

   ```rust
   #[test]
   fn append_extends_builtin() { /* assignment{system_prompt:Some("X"), mode:None}; effective_system_prompt(Developer,..) == format!("{DEVELOPER_SYSTEM_PROMPT}\n\nX") */ }
   #[test]
   fn replace_overrides() { /* mode:Some("replace"), system_prompt:Some("X") => exactly "X", no DEVELOPER_SYSTEM_PROMPT prefix */ }
   #[test]
   fn absent_uses_builtin() { /* system_prompt:None => DEVELOPER_SYSTEM_PROMPT; and session_config_for threads it into SessionConfig::system_prompt */ }
   ```

- **Depends on:** —
- **Done when:** the three tests pass; `RoleAssignment` carries
  `system_prompt`/`system_prompt_mode` (default append) and parses old configs
  unchanged; `effective_system_prompt` appends/replaces/falls-back correctly;
  `session_config_for` emits the effective prompt; the Reviewer's built-in JSON
  contract is preserved under append; cargo test/clippy/fmt green.

---

## 0074 — LLM project-discovery agent

### project-discovery-agent — Inspect the repo, propose gates + constraints

A `makina-core` discovery module that runs one model pass over the repo and
deterministically parses the model's JSON into a `DiscoveryResult`.

**Steps:**

1. Create `crates/makina-core/src/discovery.rs` and declare `pub mod discovery;`
   in `lib.rs`. Define `DiscoveredGate { name: String, command: String }` and
   `DiscoveryResult { gates: Vec<DiscoveredGate>, role_constraints:
   BTreeMap<String, String> }` (both `#[derive(Debug, Clone, PartialEq,
   Serialize, Deserialize)]`; `DiscoveryResult` also `Default`; both vec/map
   fields `#[serde(default)]`).

2. Add `pub struct RepoContext { pub text: String, pub scanned_files: Vec<String> }`
   and `pub fn gather_repo_context(repo_root: &Path) -> RepoContext`: read, if
   present, a bounded set — manifests (`Cargo.toml`, `package.json`,
   `pyproject.toml`, `go.mod`, `Makefile`) and prose (`README*`, `CONTRIBUTING*`,
   `AGENTS*`) — truncating each file to a byte cap, labelling each block with its
   filename, and recording the files actually read in `scanned_files`. Pure +
   IO-light (existence checks + reads only).

3. Add `pub const DISCOVERY_SYSTEM_PROMPT: &str` instructing the model to output
   ONLY a JSON object matching `DiscoveryResult` (gates = name + shell command
   that must exit 0; `role_constraints` = short prose keyed by
   `"developer"`/`"reviewer"`/`"planner"`), no prose, no fences — mirroring the
   strictness of `PLANNER_SYSTEM_PROMPT`.

4. Add `pub fn parse_discovery_result(raw: &str) -> Result<DiscoveryResult,
   DiscoveryError>` using `crate::json::extract_json_object` (`json.rs:12`,
   `pub(crate)` — callable in-crate) + `serde_json::from_str`. Define
   `DiscoveryError` as a `thiserror` enum (`NoJsonObject`,
   `Deserialize(#[from] serde_json::Error)`, `Backend(#[from] BackendError)`),
   matching `VerdictParseError`'s shape (`roles.rs:243`).

5. Add `pub async fn discover_project(backend: &dyn AgentBackend, repo_root:
   &Path) -> Result<(DiscoveryResult, Vec<String>), DiscoveryError>` that mirrors
   `ModelInterpreter::interpret` (`interpreter.rs:724`): `gather_repo_context`,
   spawn a `SessionConfig { working_dir: repo_root, system_prompt:
   DISCOVERY_SYSTEM_PROMPT, .. }`, send one prompt embedding the context, drain
   `ResponseEvent::TextChunk` to `TurnComplete`, `terminate`, then
   `parse_discovery_result`; return the result + `scanned_files`.

6. Add tests in `discovery.rs` (use the `StubBackend` pattern from
   `backend.rs:368`; no real model call):

   ```rust
   #[test]
   fn parse_discovery_result_from_json() { /* a fixed JSON {"gates":[{"name":"tests","command":"cargo test"}],"role_constraints":{"developer":"use workspace lints"}} => the expected DiscoveryResult */ }
   #[test]
   fn malformed_response_errors_cleanly() { /* "no json here" => Err(NoJsonObject); "{not valid for the shape" handled; "{\"gates\":3}" => Err(Deserialize) */ }
   #[tokio::test]
   async fn discover_project_uses_backend_stub() { /* StubBackend stream yields one TextChunk(fixed JSON) then TurnComplete => discover_project returns the parsed result; scanned_files reflects gather_repo_context over a temp repo */ }
   ```

- **Depends on:** role-system-prompt-config
- **Done when:** the tests pass; `discovery::discover_project` runs one backend
  pass and returns a parsed `DiscoveryResult` + scanned files; malformed model
  output errors cleanly (`NoJsonObject` / `Deserialize`); the model call is fully
  stubbable via `AgentBackend`; cargo test/clippy/fmt green.

---

## 0075 — Discovery trigger + persistence

### discovery-trigger-and-persist — First-open auto-run, write config, re-runnable

Generalise the 0011 writer to persist discovered gates + a `[discovery]` stamp +
folded role constraints; auto-run discovery on the first open of an un-stamped
repo and expose a force-re-run action.

**Steps:**

1. In `crates/makina-core/src/config.rs`, make the gate + project config writable:
   add `Serialize` to `GateConfig`'s derive (`config.rs:395`, currently
   `Deserialize` only) and `#[serde(default)] pub source: Option<String>` to it
   (gates carry `source = "discovered"`). Add
   `pub struct DiscoveryStamp { pub last_run: String, pub scanned_files:
   Vec<String> }` (`Serialize, Deserialize`) and a `#[serde(default)] pub
   discovery: Option<DiscoveryStamp>` field on the project config; ensure
   `Config::load`/`validate` ignore it (metadata only).

2. Add a merge-preserving writer `pub async fn write_project_config(repo_root:
   &Path, edit: impl FnOnce(&mut ProjectConfigWrite)) -> std::io::Result<()>` (or
   an equivalent struct-in/struct-out signature) in `config.rs`, generalised from
   `commit_provider_config` (`event.rs:323`): read the existing
   `config_file(repo_root)` (round-trip via `toml`), apply the edit, re-serialise,
   and write back so unrelated sections (providers, roles, caps, base_branch,
   manual gates) survive. Where `ProjectConfig` (`config.rs:446`) does not derive
   `Serialize`, add a `ProjectConfigWrite` view that does.

3. Add a pure `pub fn apply_discovery(project: &mut ProjectConfigWrite, roles:
   &mut RolesConfig, result: &DiscoveryResult, now_rfc3339: &str, scanned:
   &[String])`: (a) drop existing `source == Some("discovered")` gates, then push
   one `GateConfig { name, command, image: None, source: Some("discovered") }` per
   `DiscoveredGate`; (b) for each `(role, constraint)`, fold it into that role's
   `RoleAssignment.system_prompt` via the **append** combine from task
   `role-system-prompt-config` (creating a default `RoleAssignment` when unset);
   (c) set `project.discovery = Some(DiscoveryStamp { last_run: now_rfc3339.into(),
   scanned_files: scanned.to_vec() })`.

4. In `crates/makina/src/event.rs`, in the `OpenRun` IO path (the
   `execute(Command::OpenRun …)` site, `event.rs:265`): before issuing `OpenRun`,
   read the project config; if `[discovery]` is **absent**, run
   `discovery::discover_project(backend, repo_root)`, `apply_discovery`, and
   `write_project_config`, then proceed. If the stamp is present, skip. Discovery
   failure is **non-fatal** (log + status message + proceed), mirroring
   best-effort seed-persist (`orchestrator.rs:808`).

5. Add `AppEvent::DiscoverProject`, IO-resolved like `ProviderEditorCommit`
   (`event.rs:294`), that **force-re-runs** discovery regardless of the stamp
   (re-scan, replace `source = "discovered"` gates, re-stamp `last_run`), then
   refresh `app.roles` from the re-read config so the settings editor reflects the
   folded constraints. Bind it to a concrete entry-point (the seed of the future
   "Discover project" palette command). Add `Event::ProjectDiscovered {
   gate_count: usize, scanned_files: usize }` to the `Event` enum (locate by name;
   `api.rs:629` is a hint), emitted on both the auto and forced paths.

6. Add tests:

   ```rust
   #[tokio::test]
   async fn first_open_runs_and_writes_discovery() { /* temp repo + config with no [discovery]; stub backend returns fixed gates; run the open path => config.toml now has source="discovered" gates + a [discovery] stamp (last_run, scanned_files) */ }
   #[tokio::test]
   async fn second_open_skips_when_stamped() { /* config already has [discovery]; run open path => stub backend is NOT spawned; gates unchanged */ }
   #[tokio::test]
   async fn force_rerun_overwrites() { /* stamped config; DiscoverProject => discovery re-runs, discovered gates replaced (not duplicated), last_run updated */ }
   #[test]
   fn writer_preserves_manual_gates() { /* config with a manual [[gates]] entry (no source); apply_discovery + write_project_config => the manual gate still present alongside the new source="discovered" gates */ }
   ```

- **Depends on:** project-discovery-agent
- **Done when:** the tests pass; the generalised writer persists discovered gates
  (`source = "discovered"`), a `[discovery]` stamp, and folded role constraints
  without losing other config sections; first open of an un-stamped repo auto-runs
  and writes discovery, stamped repos skip, and `DiscoverProject` force-re-runs;
  discovery failure is non-fatal; cargo test/clippy/fmt green.

---

### discovery-doc — Document the discovery contract and config shape

Record the discovery behaviour and the new config surface so the auditable record
is also a documented one.

**Steps:**

1. Add `docs/spec/project-discovery.md` describing: the LLM-driven discovery pass
   (inputs = manifests + `README`/`CONTRIBUTING`/`AGENTS`; output =
   `DiscoveryResult`), the determinism boundary (only discovery is model-driven;
   gates + prompts execute deterministically off `config.toml`), the
   `source = "discovered"` gate marker and how it merges into the single
   `[[gates]]` list, the `[discovery]` stamp (`last_run`, `scanned_files`) and its
   idempotency role, the `system_prompt`/`system_prompt_mode` append-vs-replace
   semantics, and the auto-on-first-open + force-re-run triggers.

2. Cross-link it from `docs/spec/structured-text-convention.md` (note that
   discovery never touches the task-list grammar — it configures gates + role
   prompts only) and reference it from this plan's ARCHITECTURE.

- **Depends on:** discovery-trigger-and-persist
- **Done when:** `docs/spec/project-discovery.md` exists and accurately describes
  the implemented behaviour (matching the symbols and config fields actually
  shipped by 0073–0075); the structured-text-convention doc links to it; no code
  change, so cargo test/clippy/fmt stay green.

---

**End of plan 0025 TASKS.** When every "Done when" bullet is green, opening a
fresh repo for the first time runs one LLM discovery pass that writes its proposed
gates and role constraints into `config.toml` — discovered gates merged into the
single `[[gates]]` list and run deterministically after the Developer and before
the Reviewer, role constraints appended to each role's invariant prompt — and that
written, auditable, editable config (re-runnable via "Discover project") is the
only thing execution ever reads.
