# Project Discovery Contract and Configuration

Version: 1.0  
Status: Normative  
Depends on: [structured-text-convention.md](./structured-text-convention.md)

---

## 1. Overview

**Project Discovery** is an LLM-driven inspection pass that examines a repository's structure, manifests, and documentation to propose:

- **Gate commands** — test, lint, build, or other validation steps to run between task phases
- **Role constraints** — prose instructions to append to each role's system prompt, refining their behaviour on this specific project

The pass runs once on first open of a repository, writes its findings to `config.toml`, and those written, auditable, editable results become the deterministic record. The pass is re-runnable; execution always reads from the config file, never from the model.

---

## 2. The Determinism Boundary

| Component | Authority | Model-driven? | Deterministic? |
|-----------|-----------|---------------|----------------|
| **Discovery pass** | LLM reads repo context → outputs JSON | Yes | Once written to disk, frozen |
| **Gate execution** | Config file (`source = "discovered"` gates) | No | Yes — shell commands, always deterministic |
| **Role constraint prompts** | Config file (`system_prompt`, folded per-role) | No | Yes — merged at `config.toml` read-time |
| **Task orchestration** | Validated per-task plan documents | Typed blueprint may be model-assisted | Authoring assistance only; orchestration deterministic |

**Key principle:** Only the initial discovery is model-driven. All execution — gate commands, role prompts, task scheduling — is deterministic, auditable, and editable via the config file. The model's output is **not** re-interpreted on every run.

---

## 3. Discovery Inputs

The discovery pass examines a bounded set of repository files:

### 3.1 Manifests (project structure)
- `Cargo.toml` (Rust)
- `package.json` (Node.js)
- `pyproject.toml` (Python)
- `go.mod` (Go)
- `Makefile` (Make)

Each file is **read if present** and **truncated to a byte cap** (e.g., 8 KB) to bound the model input.

### 3.2 Documentation (conventions)
- `README*` (e.g., `README.md`, `README.rst`)
- `CONTRIBUTING*` (development guidelines)
- `AGENTS*` (agent/governance config hints)

Each matching file is read and labelled with its name in the context sent to the model.

### 3.3 Actual files scanned
The discovery pass records which files were actually read (not just checked for existence) in the `[discovery]` stamp's `scanned_files` field. This list:
- Aids reproducibility audits
- Helps detect when a new manifest type (e.g., `Gemfile`) is added and discovery should be re-run
- Is **metadata only** — Makina does not enforce re-discovery based on file changes

---

## 4. Discovery Output: `DiscoveryResult`

The model is instructed to output ONLY a JSON object matching this schema:

```json
{
  "gates": [
    {"name": "tests", "command": "cargo test"},
    {"name": "lint", "command": "cargo clippy --all-targets -- -D warnings"}
  ],
  "role_constraints": {
    "developer": "Use workspace lints and test templates; see CONTRIBUTING.md.",
    "reviewer": "Approve only if all gates pass and code follows the style guide.",
    "planner": "Generate typed plan blueprints with explicit test-first tasks."
  }
}
```

### 4.1 Gates array
- **name:** A label for the gate (e.g., `"tests"`, `"lint"`, `"build"`)
- **command:** A shell command that must exit 0 to pass (e.g., `"cargo test"`)
- The discovered gates are **merged into the single `[[gates]]` list** in `config.toml` with `source = "discovered"` (see §5)

### 4.2 Role constraints object
- **Keys:** Role names — `"developer"`, `"reviewer"`, or `"planner"`
- **Values:** Prose instructions (short, 1–3 sentences recommended)
- Each constraint is **appended to the role's built-in system prompt** (see §6), preserving the role's invariant contract (e.g., Reviewer's JSON-verdict protocol)
- Constraints are optional; a missing role key means no constraint for that role

---

## 5. Gates: Discovered vs. Manual

### 5.1 The `source` field
Each gate in `config.toml` may carry an optional `source` field:

```toml
[[gates]]
name = "unit-tests"
command = "cargo test"
source = "discovered"

[[gates]]
name = "integration-suite"
command = "bash scripts/integration.sh"
# no source field — this gate is "manual" (written by hand)
```

- `source = "discovered"` — created by a discovery pass; subject to replacement on re-run
- Absent (or `source = null`) — created manually; preserved across discovery runs

### 5.2 Merging discovered gates
When discovery is applied (initial or re-run):

1. **Remove stale entries:** Delete all gates where `source == "discovered"`.
2. **Append new entries:** Add one `GateConfig` per `DiscoveredGate` in the discovery result, each with `source = "discovered"`.
3. **Preserve manual gates:** Gates without a `source` field (or with a different `source` value) are never touched.

Result: The single `[[gates]]` list contains both manual and discovered gates, in the order they were added. Discovery re-runs replace discovered gates without losing manual ones.

### 5.3 Gate execution
All gates — discovered and manual — are executed in the order they appear in `config.toml`. There is no distinction at execution time; the `source` field is metadata for auditing and re-discovery.

---

## 6. Role Prompts: `system_prompt` and `system_prompt_mode`

### 6.1 Configuration fields
Each role in the `[roles]` table may carry two optional fields:

```toml
[roles]

[roles.developer]
provider = "anthropic"
model = "claude-opus"
system_prompt = "Prefer workspace lints; see CONTRIBUTING.md."
system_prompt_mode = "append"

[roles.reviewer]
provider = "anthropic"
model = "claude-opus"
system_prompt = "Require all gates to pass before approving."
system_prompt_mode = "append"

[roles.planner]
provider = "anthropic"
model = "claude-opus"
system_prompt = "Generate typed plan blueprints for canonical Rust rendering."
system_prompt_mode = "append"
```

### 6.2 The two modes

| Mode | Semantics |
|------|-----------|
| `"append"` (default) | Append the custom prompt to the role's **built-in** system prompt; the built-in is the foundation, the custom constraint refines it. |
| `"replace"` | Use **only** the custom prompt, discarding the built-in entirely; use with caution — the role's invariant contract (e.g., Reviewer's JSON verdict format) may be lost. |

Absent `system_prompt_mode` is treated as `"append"`.

### 6.3 Built-in prompts
Each role has a hard-coded, invariant system prompt constant:

| Role | Constant | Location |
|------|----------|----------|
| Developer | `DEVELOPER_SYSTEM_PROMPT` | `crates/makina-core/src/roles.rs:98` |
| Reviewer | `REVIEWER_SYSTEM_PROMPT` | `crates/makina-core/src/roles.rs:133` |
| Planner | `PLANNER_SYSTEM_PROMPT` | `crates/makina-core/src/interpreter.rs:604` |

These constants define each role's fundamental responsibility and response format. For example, the Reviewer's built-in prompt specifies the JSON verdict protocol (`{"approved": true/false, "reason": "…"}`).

### 6.4 Combining prompts (effective system prompt)
At session-creation time, Makina computes the **effective system prompt** for a role:

```
if system_prompt is None:
    effective = builtin
else:
    if system_prompt_mode == "replace":
        effective = custom
    else:  // "append" or absent
        effective = builtin + "\n\n" + custom
```

The effective prompt is what the role actually receives; the decision is made once per session, not per turn.

### 6.5 Discovery-applied constraints
When discovery is applied, the model's `role_constraints` are folded into the config:

1. For each role in the result (e.g., `"developer"`), look up the role's current `system_prompt` in `config.toml`.
2. If the role has no `RoleAssignment` entry, create a default one.
3. Set `system_prompt` to the constraint text and `system_prompt_mode` to `"append"` (default; respects the built-in contract).
4. If a role already has a `system_prompt`, append the new constraint (via the same `"append"` logic) so constraints from multiple discovery runs accumulate.

---

## 7. The Discovery Stamp

### 7.1 The `[discovery]` section
Once discovery runs, the project config gains a `[discovery]` metadata section:

```toml
[discovery]
last_run = "2026-06-14T10:30:00Z"
scanned_files = [
  "Cargo.toml",
  "README.md",
  "CONTRIBUTING.md"
]
```

### 7.2 Fields
- **last_run:** RFC3339 timestamp of the most recent discovery pass (e.g., `"2026-06-14T10:30:00Z"`); used for audit trails and "when was discovery last refreshed?" queries.
- **scanned_files:** List of files actually read during discovery; useful for detecting if the repo has gained new manifest types and discovery should be re-run manually.

### 7.3 Idempotency guarantee
The presence of the `[discovery]` section signals that discovery has already been run for this repo. On subsequent opens:

- If `[discovery]` is **absent**, discovery runs automatically (first open).
- If `[discovery]` is **present**, discovery is **skipped** (already initialized).

This ensures that opening a repo twice does not spawn the model twice or duplicate gates.

### 7.4 No automatic re-run on file changes
Makina **does not** automatically re-run discovery if new files are added to the repo (e.g., a new `package.json`). The admin must explicitly trigger "Discover project" to refresh (see §8.2).

The `scanned_files` list is provided to **human inspection** to notice when the repo has evolved and a manual re-run might be useful.

---

## 8. Triggers: Auto-Run and Force Re-Run

### 8.1 First open (automatic)
When a project is opened for the first time:

1. Makina reads the project config file.
2. If the config **lacks** a `[discovery]` section, discovery runs automatically:
   - `discover_project(backend, repo_root)` spawns one model session.
   - The model outputs a `DiscoveryResult`.
   - `apply_discovery()` merges the result into the in-memory config (gates + role prompts).
   - `write_project_config()` persists the config back to disk (round-trip: read existing file, merge, write).
   - The `[discovery]` stamp is recorded with `last_run = now` and `scanned_files = [actual files read]`.
3. The TUI displays a transient message: `"Discovered N gates from M files"` (sourced from the `ProjectDiscovered` event).
4. If discovery **fails** (e.g., model unavailable, bad repo context), the failure is logged and the project opens anyway (non-fatal). The `[discovery]` section is **not** added; the next open will retry.

### 8.2 Force re-run (manual)
The user can manually re-run discovery via the "Discover project" action (planned as a command-palette entry):

1. Makina reads the current project config (ignoring the `[discovery]` stamp).
2. `discover_project()` runs again (re-scan files, call model, parse result).
3. `apply_discovery()` applies the new result:
   - Removes and replaces all `source == "discovered"` gates.
   - Appends new role constraints (does not erase old ones; constraints accumulate unless manually edited).
   - Updates `last_run` and `scanned_files` in the stamp.
4. `write_project_config()` persists the updated config.
5. The app re-reads the config and refreshes the settings editor so the user sees the updated constraints.
6. A `ProjectDiscovered` event is emitted for the TUI status display.

---

## 9. Writing Config: Merge-Preserving Persistence

### 9.1 Round-trip pattern
To persist discovery results without losing unrelated config sections (e.g., manual gate definitions, provider credentials), Makina uses a **merge-preserving writer**:

1. **Read** the existing `config.toml` as a TOML document.
2. **In-memory:** Apply edits to specific sections (`gates`, `[roles]`, `[discovery]`).
3. **Write back:** Re-serialize and persist only the modified sections; leave others unchanged.

This prevents the lossy pattern where parsing as a `GlobalConfig` struct (which has no `gates` field) would drop any `[[gates]]` entries on save.

### 9.2 Sections touched by discovery
- **`[[gates]]`:** Discovered gates added/replaced; manual gates untouched.
- **`[roles.{role}].system_prompt` / `system_prompt_mode`:** Constraints appended or created.
- **`[discovery]`:** Stamp created/updated with `last_run` and `scanned_files`.

### 9.3 Sections preserved
- **`[providers]`:** (not touched by discovery)
- **`[roles.{role}]` fields other than `system_prompt`/`system_prompt_mode`:** e.g., `provider`, `model`, `effort` (not touched)
- **`base_branch`, `caps`, `concurrency`:** (not touched)
- Any custom or future sections: (preserved via round-trip)

---

## 10. Config Example: Before and After Discovery

### 10.1 Before (first open, no stamp)

```toml
[providers.anthropic]
# … credentials …

[roles.developer]
provider = "anthropic"
model = "claude-opus"

[roles.reviewer]
provider = "anthropic"
model = "claude-opus"

base_branch = "main"

[[gates]]
name = "integration-suite"
command = "bash scripts/e2e.sh"
# Manual gate, no source field
```

### 10.2 After (discovery applied)

```toml
[providers.anthropic]
# … credentials … (unchanged)

[roles.developer]
provider = "anthropic"
model = "claude-opus"
system_prompt = "Use workspace lints; see CONTRIBUTING.md."
system_prompt_mode = "append"

[roles.reviewer]
provider = "anthropic"
model = "claude-opus"
system_prompt = "Require all gates to pass before approving."
system_prompt_mode = "append"

[roles.planner]
provider = "anthropic"
model = "claude-opus"
system_prompt = "Generate typed plan blueprints for canonical Rust rendering."
system_prompt_mode = "append"

base_branch = "main"

[[gates]]
name = "unit-tests"
command = "cargo test"
source = "discovered"

[[gates]]
name = "lint"
command = "cargo clippy --all-targets -- -D warnings"
source = "discovered"

[[gates]]
name = "integration-suite"
command = "bash scripts/e2e.sh"
# Manual gate, no source field (preserved)

[discovery]
last_run = "2026-06-14T10:30:00Z"
scanned_files = ["Cargo.toml", "README.md", "CONTRIBUTING.md"]
```

---

## 11. Interaction with Task Lists and Gates

### 11.1 Project discovery does not mutate plan documents
Project discovery reads manifests and repository documentation but does not mutate plan bundles. Plan discovery is a separate read-only pass over directories containing `tasks/`; task documents remain source input, not `DiscoveryResult` payload.

Task-list authoring is a human responsibility, and the [structured-text-convention](./structured-text-convention.md) is its own normative document.

### 11.2 Discovery gates are separate from task-level gates
- **Task-level gates:** Declared by the closed `gated` field in each `tasks/*.md` document.
- **Project-level gates:** Discovered (or manually defined) in `config.toml`.

Project-level gates run **between task phases** (e.g., after Developer turn, before Reviewer); they are not per-task. This document specifies project-level discovery only.

---

## 12. Failure Modes and Recovery

### 12.1 Discovery failure on first open
If the discovery pass fails (e.g., model unavailable, malformed response):

1. The error is logged.
2. A status message is displayed (e.g., `"Discovery failed: backend unavailable; proceeding without gates."`).
3. The project opens without the `[discovery]` stamp.
4. On the next open, discovery is retried.

This is a non-fatal, best-effort approach; a repo without discovered gates is valid and usable.

### 12.2 Malformed discovery result
If the model's JSON response is invalid (no `{…}` object found, or fields don't match the schema):

1. `DiscoveryError::NoJsonObject` or `DiscoveryError::Deserialize` is returned.
2. Same recovery as above: log, display message, proceed without the stamp.
3. User can force a re-run manually (§8.2) after fixing the issue (e.g., cleaning up repo, improving context) or choosing a different model.

### 12.3 Config file corruption
If the config file is corrupted (invalid TOML) before or after discovery:

1. The existing config-load errors apply (§`Config::load` error handling).
2. Discovery is not attempted if the config cannot be read.
3. User must fix the TOML syntax before opening the project.

---

## 13. Comparison: Discovery vs. Manual Gates

| Aspect | Discovered | Manual |
|--------|-----------|--------|
| **Definition** | `source = "discovered"` in `config.toml` | `source` absent (or any other value) |
| **Lifetime** | Replaced on each discovery re-run | Preserved across discovery runs |
| **Edited by** | `apply_discovery()` function (during discovery) | Human hand-edit to `config.toml` |
| **Audit trail** | `[discovery]` stamp records when written | No automatic record (human responsibility) |
| **Re-run impact** | Old ones removed, new ones from model added | None; not affected by discovery |
| **Recommended use** | Standard gates inferred from repo structure | Custom project-specific overrides |

---

## 14. Future Extensions

The following are **out of scope** for this release but noted for future work:

- **Scheduled re-discovery:** Automatically re-run discovery on cron-like schedules or when manifest files change.
- **Partial discovery:** Re-scan only one aspect (e.g., gates) without re-running the full pass.
- **Discovery-driven plan changes:** Any future extension must enter through the typed plan schema and coordinator-owned authoring transaction.
- **Multiple discovery profiles:** Support different discovery results per environment (e.g., dev vs. CI).

---

## 15. Files Changed (Plan 0025)

| Task | File | Role |
|------|------|------|
| 0073 (role-system-prompt-config) | `crates/makina-core/src/config.rs` | Add `system_prompt` and `system_prompt_mode` fields to `RoleAssignment` |
| 0073 | `crates/makina-core/src/roles.rs` | Add `combine_prompt()` and `effective_system_prompt()` functions |
| 0074 (project-discovery-agent) | `crates/makina-core/src/discovery.rs` (new) | Define `DiscoveryResult`, `RepoContext`, discovery pass logic |
| 0074 | `crates/makina-core/src/lib.rs` | Declare `pub mod discovery;` |
| 0075 (discovery-trigger-and-persist) | `crates/makina-core/src/config.rs` | Add `DiscoveryStamp` and `source` field to `GateConfig`; implement `write_project_config()` |
| 0075 | `crates/makina/src/event.rs` | Implement discovery trigger in `OpenRun` handler and `DiscoverProject` action |
| 0075 | `crates/makina-core/src/api.rs` | Add `Event::ProjectDiscovered` |
| Discovery-doc (this task) | `docs/spec/project-discovery.md` (new) | Normative specification (this document) |
| Discovery-doc | `docs/spec/structured-text-convention.md` | Cross-link to this document |

---

## 16. Testing Approach

All tests use deterministic stubs (`StubBackend`) or fixtures; no real model calls are required for test passes.

- **0073 tests** (`roles.rs`): `append_extends_builtin`, `replace_overrides`, `absent_uses_builtin` — verify prompt combination logic.
- **0074 tests** (`discovery.rs`): `parse_discovery_result_from_json`, `malformed_response_errors_cleanly`, `discover_project_uses_backend_stub` — verify parsing and the discovery pass with stubbed backend.
- **0075 tests** (`event.rs` / `config.rs`): `first_open_runs_and_writes_discovery`, `second_open_skips_when_stamped`, `force_rerun_overwrites`, `writer_preserves_manual_gates` — verify trigger, idempotency, and merge-preserving writes.

---

## 17. Acceptance Criteria

This specification is complete and normative when:

1. This file (`docs/spec/project-discovery.md`) exists and accurately describes the implemented behaviour:
   - Symbol names match the code (e.g., `DiscoveryResult`, `effective_system_prompt`, `apply_discovery`).
   - Config field names match the TOML serialization (e.g., `source`, `system_prompt`, `system_prompt_mode`).
   - Behaviour descriptions match implementation (e.g., append-by-default, non-fatal discovery failure, merge-preserving writes).
   - Examples (§10) are valid TOML and reflect real use cases.

2. This document is cross-linked from `docs/spec/structured-text-convention.md` (§11 — the task-list grammar is separate from project config discovery).

3. No code changes are required for this task (0073–0075 implement the feature, discovery-doc documents it).

4. All tests for 0073–0075 pass: `cargo test -p makina-core -p makina` and the full suite.

5. Format and lint are clean: `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`.
