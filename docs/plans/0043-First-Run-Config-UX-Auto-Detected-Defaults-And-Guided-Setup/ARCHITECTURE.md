# Architecture — Plan 0043 (deltas)

> The concrete deltas. This plan touches
> `crates/makina-core/src/preflight.rs`,
> `crates/makina-core/src/config.rs`, `crates/makina/src/event.rs`,
> `crates/makina/src/main.rs`, and `.makina/config.toml`.
> Line numbers are hints; locate by symbol.

## 0001 — Backend Auto-Detection Defaults

Today `GlobalConfig::default` ships `backend: BackendConfig::default()` (empty
`command`) and `providers: Vec::new()` (`config.rs:403-415`), and `Config::resolve`
(`config.rs:683-754`) only synthesizes a `"default"` provider when a *legacy*
`[backend].command` is non-empty (`config.rs:714`). Absent `~/.makina/config.toml`,
resolve yields an empty backend + empty providers and `validate` rejects it at
`config.rs:853`. The pure PATH machinery already exists — `is_executable`
(`preflight.rs:175`) and `resolve_in_path` (`preflight.rs:133`) — but is used only
for post-resolution probing, never for supplying defaults.

**Edits:**

**Add a supported-agent registry and a pure detector in `preflight.rs`.**

```rust
/// A first-party agent CLI Makina can auto-detect on $PATH and drive over ACP.
pub struct KnownAgent { pub name: &'static str, pub command: &'static str, pub args: &'static [&'static str] }
/// Detection priority order (gemini is the e2e-proven path, docs/trial/e2e-run.md).
pub const KNOWN_AGENTS: &[KnownAgent] = &[
    KnownAgent { name: "gemini", command: "gemini", args: &["--acp", "--yolo"] },
    KnownAgent { name: "claude-code-acp", command: "claude-code-acp", args: &["--acp"] },
    KnownAgent { name: "grok", command: "grok", args: &["--acp"] },
];
pub struct DetectedBackend { pub agent: &'static str, pub command: String, pub args: Vec<String>, pub resolved: PathBuf }
/// Walk `path_env` for the first KNOWN_AGENTS binary. Pure: stats files, spawns nothing.
pub fn detect_backend_in_path(path_env: &str) -> Option<DetectedBackend> { /* reuse is_executable over split path_env */ }
```

**Fold detection into resolution behind an explicit, injectable PATH.**

```rust
impl Config {
    /// When no backend is configured, synthesize one from the first supported CLI on `path_env`.
    /// Mirrors resolve()'s legacy back-compat block: pushes a "default" provider and assigns
    /// it to any unset role. No-op if a backend/provider is already set. Returns what it found.
    pub fn apply_detected_backend(&mut self, path_env: &str) -> Option<preflight::DetectedBackend> { /* .. */ }
}
```

`load_with_labels` gains a trailing `detect_path_env: Option<&str>` param; after
`Config::resolve(..)` and *before* `config.validate()?` it runs
`if let Some(pe) = detect_path_env { config.apply_detected_backend(pe); }`.
`Config::load` passes `None` (stays hermetic); `load_defaults_with_paths` passes
`Some(&std::env::var("PATH").unwrap_or_default())`.

**Properties that make this safe:**

- Detection is a no-op whenever a backend or any provider is already configured,
  so existing configs are untouched.
- It reuses the proven, spawn-free `is_executable` PATH walk.
- The synthesized provider is byte-identical in shape to `resolve()`'s legacy
  back-compat provider.
- Because detection is gated behind an explicit `Option<&str>` that
  `Config::load` passes as `None`, every existing hermetic config test keeps its
  current outcome while production reads the real `$PATH` exactly once.

## 0002 — Guided First-Run Setup And Persist

Today `write_doctor_scaffold` (`event.rs:770-879`) refuses when any config exists
and otherwise writes a *fixed* global template hardcoding `command = "gemini"`
(`event.rs:793`) and a project template — it never inspects `$PATH`, so it can
persist a config for an agent that is not installed. And `main` (`main.rs:51-86`)
prints a files-checked block and `std::process::exit(1)` for *every* `ConfigError`,
with no recovery path for the common missing-backend case.

**Edits:**

**Make the scaffold detect and persist the real backend.**

```rust
// In write_doctor_scaffold, before writing the global template:
let path_env = std::env::var("PATH").unwrap_or_default();
let global_template = match preflight::detect_backend_in_path(&path_env) {
    Some(d) => format!(
        "[backend]\ncommand = \"{}\"\nargs = {:?}\n\n[planner]\nmechanism = \"one-shot-agent\"\n",
        d.command, d.args,
    ),
    None => COMMENTED_TEMPLATE_LISTING_KNOWN_AGENTS, // names gemini / claude-code-acp / grok
};
```

The status message reports which agent was detected (or that none was, and which
CLIs are supported). The refuse-if-exists guard (`event.rs:779`) is unchanged.

**Guide instead of unconditionally exiting on an empty-backend failure.** In
`main.rs`, when the failure is specifically the empty-backend validation, the
error arm prints the enriched guidance (naming the `w` scaffold) and — as a gated
follow-up — may launch the TUI directly into the Doctor overlay
(`AppEvent::OpenDoctor`) so the user can press `w` to detect-and-persist, rather
than calling `std::process::exit(1)`.

**Properties that make this safe:**

- The scaffold still never overwrites an existing config (the
  `global_exists || project_exists` guard is preserved), so persistence is
  strictly additive on a fresh machine.
- When an agent is detected the written config is exactly the shape
  `apply_detected_backend` would synthesize, so scaffolding and auto-detection
  agree.
- The startup change is scoped to the empty-backend branch, leaving parse/IO
  failures fatal as before.

## 0003 — Diagnostics And Shipped-Config Hardening

Today the empty-backend reason is the bare `"backend.command must not be empty"`
(`config.rs:855`) — it names no file, field location, or remedy. And the shipped
`.makina/config.toml` comments (`.makina/config.toml:9-21`) tell users the global
layer *must* supply the backend and show a `[backend]`/`command = "gemini"`
snippet to hand-write, which is precisely the manual step whose omission triggers
finding 1.

**Edits:**

**Enrich the validation message.**

```rust
return Err(ConfigError::Validation { reason: format!(
    "backend.command must not be empty — no agent backend is configured. \
     Set [backend].command in ~/.makina/config.toml (machine-specific, NOT committed), \
     or install a supported agent CLI on PATH for auto-detection ({}). \
     The project .makina/config.toml must not set a backend.",
    preflight::KNOWN_AGENTS.iter().map(|a| a.command).collect::<Vec<_>>().join(", "),
)});
```

**Harden the shipped project config comments.** Replace the
`.makina/config.toml:9-21` "minimal global config for driving this repo" ritual
with a note that Makina auto-detects a supported CLI on `$PATH` (listing them)
and that `~/.makina/config.toml` is only needed to override the detected default
— and point at the Doctor overlay's `w` scaffold. No `[backend]` is added to the
project layer (it belongs to the global layer).

**Properties that make this safe:**

- The message keeps the leading `backend.command` phrase, so the existing
  `reason.contains("backend.command")` assertions (`config.rs:1587`,
  `config.rs:1605`) stay green.
- The shipped-config edit is comments-only (no keys change), so parsing and
  precedence are unaffected.
- The supported-CLI list is sourced from the single `KNOWN_AGENTS` registry, so
  diagnostics, scaffold, and detection can never drift.

## Test strategy

- **0001 (auto-detection).** In `preflight.rs`:
  `detect_backend_in_path_finds_first_supported_agent` (Unix-gated: a temp dir
  with an executable `gemini` on the injected PATH resolves to the `gemini` entry
  with `--acp --yolo`), `detect_backend_in_path_returns_none_on_empty_path`, and
  `detect_backend_in_path_ignores_non_executable_match`. In `config.rs`:
  `load_autodetects_backend_when_supported_agent_on_path` (a fresh clone with
  `gemini` on an injected PATH now yields a single `"default"` provider and
  validates) and `load_without_detection_still_fails_on_empty_backend` (empty
  PATH → the same `backend.command` validation error), plus the two pre-existing
  empty-backend tests staying green.
- **0002 (scaffold + guidance).** `doctor_scaffold_writes_when_absent` extended
  so the detected branch persists valid TOML with an uncommented `[backend]` and
  the no-agent branch lists every `KNOWN_AGENTS` CLI in a comment;
  `doctor_scaffold_refuses_when_present` unchanged. For the gated startup change,
  `is_recoverable_empty_backend` unit-tested to classify a
  `Validation{reason:"backend.command …"}` as recoverable and a `Parse{..}` as
  not.
- **0003 (diagnostics + shipped config).**
  `empty_backend_error_lists_supported_agents_and_file` asserts the reason
  contains `backend.command`, `~/.makina/config.toml`, and `gemini`; the
  `.makina/config.toml` edit is comments-only and verified to remain valid TOML
  by the normal config-load path.
- All tasks keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.

## Interaction with prior work

- **0013 — Preflight, Doctor, and First-Run Guidance.** This plan extends 0013's
  Doctor overlay and its `w` scaffold seam (`write_doctor_scaffold`,
  `event.rs:770`) from a fixed-template writer into a detect→confirm→persist
  flow, and reuses 0013's pure preflight PATH resolver (`is_executable`,
  `resolve_in_path`) as the basis for `detect_backend_in_path`.
- **0015 — Idle-Hang Detection.** Detection stays spawn-free (pure PATH stat)
  precisely because 0015 established that spawning an unauthenticated agent can
  hang; no candidate CLI is executed to test it.
- **0011 / 0017 — Provider and role configuration.** The synthesized `"default"`
  provider and its role assignments mirror the shape those plans defined, so an
  auto-detected config is indistinguishable from a hand-written one downstream
  and the existing provider editor can still edit it.
