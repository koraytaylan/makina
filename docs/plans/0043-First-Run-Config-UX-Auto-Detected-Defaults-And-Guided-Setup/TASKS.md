# XAgent Plan 0043 — First-Run Config UX: Auto-Detected Backend Defaults and Guided Setup

This plan closes the fresh-clone onboarding trap where `Config::validate` rejects an all-empty config with `backend.command must not be empty`. It adds a pure PATH-based agent detector (`detect_backend_in_path`) plus a `KNOWN_AGENTS` registry in `preflight.rs`, threads detection into config resolution via `Config::apply_detected_backend` (invoked from `load_defaults_with_paths` with the real `$PATH`, kept off the deterministic `Config::load` test path) so a fresh clone synthesizes a `default` provider and role assignments whenever a supported CLI is present; it rewrites the Doctor `w` scaffold (`write_doctor_scaffold`) into a detect→confirm→persist flow that writes `~/.makina/config.toml` with the actually-detected command/args (or, when none is found, a commented template naming every supported CLI); it upgrades the startup hard-fail arm in `main.rs` so an empty-backend failure guides the user (optionally launching the Doctor overlay instead of exiting); it enriches the `validate()` empty-backend message to name the exact file, field, and fix and list supported CLIs; and it hardens the shipped `.makina/config.toml` comments so the project layer can never itself introduce the trap — all with the three quality gates green.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md) for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}`).
- **Depends on** lists *direct* prerequisites only; "—" means none.
- **Done when** is the verifiable criterion, and every task keeps the gate
  commands green — the full forms are `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`;
  abbreviated as "cargo test/clippy/fmt green" thereafter.
- GPU tests self-skip without an adapter.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0001 — Backend Auto-Detection Defaults

### add-known-agent-registry — Add KNOWN_AGENTS Registry and Pure detect_backend_in_path Helper

`preflight.rs` already resolves commands against `$PATH` without spawning any process — `is_executable` (`preflight.rs:175`) stats a candidate for the execute bit, and `resolve_in_path` (`preflight.rs:133`) walks a split `path_env`. Nothing, however, enumerates the agent CLIs Makina can drive or picks a default from what is installed. This task adds that registry and a pure detector so later tasks can synthesize a backend, scaffold a real config, and list supported CLIs from ONE source of truth.

**Steps:**

1. In `crates/makina-core/src/preflight.rs`, add a public registry near the top of the module:

   ```rust
   /// A first-party agent CLI Makina can auto-detect on `$PATH` and drive over ACP.
   /// `command` is the binary name searched on PATH; `args` are the ACP flags to pass.
   pub struct KnownAgent {
       pub name: &'static str,
       pub command: &'static str,
       pub args: &'static [&'static str],
   }

   /// Supported agents in detection-priority order. `gemini --acp --yolo` is the
   /// e2e-proven path (see docs/trial/e2e-run.md:100-101,272-273); `claude-code-acp`
   /// is the Zed-compatible Claude CLI named in makina-acp's docs (crates/makina-acp/src/lib.rs:4).
   pub const KNOWN_AGENTS: &[KnownAgent] = &[
       KnownAgent { name: "gemini", command: "gemini", args: &["--acp", "--yolo"] },
       KnownAgent { name: "claude-code-acp", command: "claude-code-acp", args: &["--acp"] },
       KnownAgent { name: "grok", command: "grok", args: &["--acp"] },
   ];
   ```

2. Add the result type and detector below the registry:

   ```rust
   /// A backend auto-detected on `$PATH`. `command`/`args` are ready to drop into a
   /// synthesized provider; `agent` is the matched KNOWN_AGENTS name; `resolved` is the binary path.
   #[derive(Debug, Clone)]
   pub struct DetectedBackend {
       pub agent: &'static str,
       pub command: String,
       pub args: Vec<String>,
       pub resolved: PathBuf,
   }

   /// Return the first KNOWN_AGENTS entry whose `command` resolves to an executable on
   /// `path_env`, or `None`. Pure: stats files via `is_executable`, spawns nothing.
   pub fn detect_backend_in_path(path_env: &str) -> Option<DetectedBackend> {
       if path_env.is_empty() { return None; }
       let separator = if cfg!(windows) { ";" } else { ":" };
       for agent in KNOWN_AGENTS {
           for dir in path_env.split(separator) {
               if dir.is_empty() { continue; }
               let candidate = PathBuf::from(dir).join(agent.command);
               if is_executable(&candidate) {
                   return Some(DetectedBackend {
                       agent: agent.name,
                       command: agent.command.to_string(),
                       args: agent.args.iter().map(|s| s.to_string()).collect(),
                       resolved: candidate,
                   });
               }
           }
       }
       None
   }
   ```

3. In the `#[cfg(test)] mod` of `preflight.rs`, add tests that build a temp dir containing a fake executable and inject it as `path_env` (Unix-gated, matching the crate's existing `#[cfg(unix)]` permission handling):

   ```rust
   #[cfg(unix)]
   #[test]
   fn detect_backend_in_path_finds_first_supported_agent() {
       use std::os::unix::fs::PermissionsExt;
       let dir = tempfile::tempdir().unwrap();
       let bin = dir.path().join("gemini");
       std::fs::write(&bin, "#!/bin/sh\n").unwrap();
       std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
       let d = detect_backend_in_path(dir.path().to_str().unwrap()).expect("gemini should be detected");
       assert_eq!(d.agent, "gemini");
       assert_eq!(d.command, "gemini");
       assert_eq!(d.args, vec!["--acp".to_string(), "--yolo".to_string()]);
   }

   #[test]
   fn detect_backend_in_path_returns_none_on_empty_path() {
       assert!(detect_backend_in_path("").is_none());
   }

   #[cfg(unix)]
   #[test]
   fn detect_backend_in_path_ignores_non_executable_match() {
       let dir = tempfile::tempdir().unwrap();
       std::fs::write(dir.path().join("gemini"), "x").unwrap(); // no exec bit
       assert!(detect_backend_in_path(dir.path().to_str().unwrap()).is_none());
   }
   ```

4. Run the full gate commands.

- **Depends on:** —
- **Done when:** `preflight::KNOWN_AGENTS`, `KnownAgent`, `DetectedBackend`, and `detect_backend_in_path` exist and are public; the three new tests pass (detection finds `gemini` in an injected PATH, returns `None` on empty PATH, and ignores a non-executable match). cargo test/clippy/fmt green.

---

### synthesize-detected-backend — Synthesize a Detected Backend During Config Resolution

`GlobalConfig::default` ships an empty backend and `providers: Vec::new()` (`config.rs:403-415`), and `Config::resolve` only synthesizes a `"default"` provider from a legacy `[backend].command` (`config.rs:714-754`). So on a fresh clone with no `~/.makina/config.toml`, resolution yields an empty backend and `validate` rejects it (`config.rs:853`). This task adds `Config::apply_detected_backend` and threads it into the production load path (reading the real `$PATH`) while keeping the `Config::load` test path detection-free so existing hermetic tests are unaffected.

**Steps:**

1. In `crates/makina-core/src/config.rs`, add a method on `impl Config` (near `resolve`, ~`config.rs:669`):

   ```rust
   /// When NO backend is configured, synthesize one from the first supported agent CLI
   /// found on `path_env`. Mirrors resolve()'s legacy back-compat block: sets `backend`,
   /// pushes a `"default"` provider, and assigns it to any unset role. No-op (returns None)
   /// if a backend command or any provider is already present. Returns the detection for diagnostics.
   pub fn apply_detected_backend(&mut self, path_env: &str) -> Option<crate::preflight::DetectedBackend> {
       if !self.providers.is_empty() || !self.backend.command.is_empty() {
           return None;
       }
       let detected = crate::preflight::detect_backend_in_path(path_env)?;
       self.backend.command = detected.command.clone();
       self.backend.args = detected.args.clone();
       self.providers.push(ProviderConfig {
           name: "default".to_string(),
           command: detected.command.clone(),
           args: detected.args.clone(),
           env: BTreeMap::new(),
       });
       let assign = || RoleAssignment { provider: "default".to_string(), ..RoleAssignment::default() };
       if self.roles.planner.is_none() { self.roles.planner = Some(assign()); }
       if self.roles.developer.is_none() { self.roles.developer = Some(assign()); }
       if self.roles.reviewer.is_none() { self.roles.reviewer = Some(assign()); }
       Some(detected)
   }
   ```

2. Change the signature of the private `load_with_labels` (`config.rs:953`) to add a trailing parameter `detect_path_env: Option<&str>`. Immediately after `let config = Config::resolve(global, project);` (`config.rs:1003`) and BEFORE `config.validate()?`, insert:

   ```rust
   let mut config = config;
   if let Some(pe) = detect_path_env {
       config.apply_detected_backend(pe);
   }
   ```

3. Update the two call sites: in `Config::load` (`config.rs:943`) pass `None` as the new last argument (keeps the test/public path detection-free and deterministic); in `load_defaults_with_paths` (`config.rs:1049`) pass `Some(&std::env::var("PATH").unwrap_or_default())` as the new last argument so production reads the real PATH once.

4. In the `#[cfg(test)] mod` of `config.rs`, add tests that exercise the private `load_with_labels` directly with an injected PATH (a temp dir holding a fake `gemini`), asserting auto-detection now makes a fresh clone validate:

   ```rust
   #[cfg(unix)]
   #[test]
   fn load_autodetects_backend_when_supported_agent_on_path() {
       use std::os::unix::fs::PermissionsExt;
       let bindir = tempfile::tempdir().unwrap();
       let bin = bindir.path().join("gemini");
       std::fs::write(&bin, "#!/bin/sh\n").unwrap();
       std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
       let no_global = std::path::Path::new("/tmp/__makina_no_global_detect.toml");
       let no_project = std::path::Path::new("/tmp/__makina_no_project_detect.toml");
       let cfg = Config::load_with_labels(Some(no_global), None, Some(no_project), None,
           Some(bindir.path().to_str().unwrap())).expect("detection should make a fresh clone validate");
       assert_eq!(cfg.providers.len(), 1);
       assert_eq!(cfg.providers[0].name, "default");
       assert_eq!(cfg.providers[0].command, "gemini");
       assert_eq!(cfg.backend.command, "gemini");
       assert_eq!(cfg.roles.developer.as_ref().unwrap().provider, "default");
   }

   #[test]
   fn load_without_detection_still_fails_on_empty_backend() {
       let no_global = std::path::Path::new("/tmp/__makina_no_global_x.toml");
       let no_project = std::path::Path::new("/tmp/__makina_no_project_x.toml");
       // Empty path_env => no detection => same validation error as before.
       match Config::load_with_labels(Some(no_global), None, Some(no_project), None, Some("")) {
           Err(ConfigError::Validation { reason }) => assert!(reason.contains("backend.command")),
           other => panic!("expected Validation error, got: {other:?}"),
       }
   }
   ```

5. Confirm the two pre-existing tests `load_missing_global_falls_back_to_defaults` (`config.rs:1557`) and `load_both_missing_returns_defaults_then_validation_error` (`config.rs:1598`) still pass unchanged: both call `Config::load`, which now passes `None`, so detection never runs and they still see the empty-backend validation error.

6. Run the full gate commands.

- **Depends on:** add-known-agent-registry
- **Done when:** `Config::apply_detected_backend` exists; `load_with_labels` takes `Option<&str>` and runs detection only when `Some`; `Config::load` passes `None` and `load_defaults_with_paths` passes the real `$PATH`. `load_autodetects_backend_when_supported_agent_on_path` and `load_without_detection_still_fails_on_empty_backend` pass, and the two pre-existing empty-backend tests remain green (red before this task for the fresh-clone case, green after). cargo test/clippy/fmt green.

---

## 0002 — Guided First-Run Setup And Persist

### detect-driven-scaffold — Upgrade the Doctor Scaffold into a Detect→Confirm→Persist Flow

`write_doctor_scaffold` (`event.rs:770-879`) refuses when any config exists and otherwise writes a fixed global template hardcoding `command = "gemini"` (`event.rs:793`), so it can persist a config for an agent that is not installed and never reflects the machine's actual PATH. This task makes the `w` scaffold detect a supported CLI and persist a `~/.makina/config.toml` whose backend is the one actually found (or, when none is found, a commented template naming every supported CLI so the user can pick).

**Steps:**

1. In `crates/makina/src/event.rs`, in `write_doctor_scaffold` (`event.rs:770`), keep the existing refuse-if-exists guard (`event.rs:779`) untouched. Before building `global_template`, detect the backend:

   ```rust
   let path_env = std::env::var("PATH").unwrap_or_default();
   let detected = makina_core::preflight::detect_backend_in_path(&path_env);
   ```

2. Replace the hardcoded `global_template` (`event.rs:787-800`) with a detection-driven value. When an agent is detected, write a config that validates immediately; when not, write a commented template listing every supported CLI from `KNOWN_AGENTS`:

   ```rust
   let global_template = match &detected {
       Some(d) => format!(
           "# Makina global configuration — machine-specific, not committed.\n\
            # Auto-detected backend: {} (found at {}).\n\n\
            [backend]\ncommand = \"{}\"\nargs = {:?}\n\n\
            [planner]\nmechanism = \"one-shot-agent\"\n",
           d.agent, d.resolved.display(), d.command, d.args,
       ),
       None => {
           let supported: Vec<&str> = makina_core::preflight::KNOWN_AGENTS.iter().map(|a| a.command).collect();
           format!(
               "# Makina global configuration — machine-specific, not committed.\n\
                # No supported agent CLI was found on PATH. Install one of: {}\n\
                # then set [backend].command below (see docs/trial/e2e-run.md).\n\n\
                [backend]\n# command = \"gemini\"\n# args = [\"--acp\", \"--yolo\"]\n\n\
                [planner]\nmechanism = \"one-shot-agent\"\n",
               supported.join(", "),
           )
       }
   };
   ```

3. Update the final status message (the `written_paths` success arm, `event.rs:874`) to report the detection result, e.g. `format!("Starter configs written to: {} (backend: {})", written_paths.join(", "), detected.as_ref().map(|d| d.agent).unwrap_or("none detected — edit [backend] before running"))`.

4. Update the existing scaffold tests. `doctor_scaffold_writes_when_absent` (`event.rs:3262`) asserts on the written template contents — extend/adjust it so that: (a) when a fake supported agent is placed on an injected PATH the written global config parses as valid TOML and contains an uncommented `[backend]` with `command = "gemini"`; and (b) when PATH has no supported agent, the written global config lists the supported CLIs in a comment and leaves `command` commented out. Keep `doctor_scaffold_refuses_when_present` (`event.rs:3215`) unchanged. If the test cannot control the process `$PATH` deterministically, assert only the branch reachable in CI (the no-detection commented template listing `KNOWN_AGENTS`) and cover the detected branch by asserting `detect_backend_in_path` drives the format, keeping the assertion deterministic.

5. Run the full gate commands.

- **Depends on:** add-known-agent-registry, synthesize-detected-backend
- **Done when:** The Doctor `w` scaffold detects a supported CLI on `$PATH` and persists a `~/.makina/config.toml` whose `[backend].command` is the detected binary (verified valid TOML), or a commented template naming every `KNOWN_AGENTS` CLI when none is found; the refuse-if-exists guard is preserved; updated scaffold tests pass. cargo test/clippy/fmt green.

---

### launch-doctor-on-empty-backend — Guide the User Instead of Exiting on an Empty-Backend Failure (GATED)

**Gate:** This is a conditional Phase-2 follow-up that lands only if `synthesize-detected-backend` and `detect-driven-scaffold` are both merged and green; if it cannot be made to launch cleanly it must be reverted and recorded, leaving the enriched hard-fail message from those tasks in place.

Today `main` (`main.rs:51-86`) prints a files-checked block and calls `std::process::exit(1)` for EVERY `ConfigError`, including the recoverable empty-backend case — there is no in-app path to detect-and-persist. This task upgrades the empty-backend branch to guide the user, ideally launching the TUI directly into the Doctor overlay (`AppEvent::OpenDoctor`) so pressing `w` runs the new detect→confirm→persist flow, instead of exiting.

**Steps:**

1. In `crates/makina/src/main.rs`, in the `Err(e)` arm (`main.rs:54`), classify the failure: match on `e` being `makina_core::config::ConfigError::Validation { reason }` where `reason` contains `"backend.command"` (the empty-backend case). For all other errors keep the current print-and-`exit(1)` behavior unchanged (`main.rs:85`).

2. For the empty-backend case, first print the enriched guidance (the message produced by `enrich-empty-backend-diagnostics`) plus an explicit pointer: `"Open Makina and press 'w' in the Doctor overlay to auto-detect and write ~/.makina/config.toml."`.

3. Then, gated behavior: instead of `exit(1)`, construct the `App` with the partially-resolved paths (`load_paths`) and launch the TUI opening the Doctor overlay by seeding an initial `AppEvent::OpenDoctor` (`app.rs:850`). If wiring a partial-config `App` cleanly is not feasible within this task's footprint (e.g. the orchestrator requires a validated `Config`), fall back to keeping `exit(1)` but with the enriched message — and record that the overlay-launch was deferred in the plan STATUS, satisfying the revert-and-record clause.

4. Add or adjust a test asserting the empty-backend branch is classified distinctly from parse/IO failures (e.g. a helper `fn is_recoverable_empty_backend(e: &ConfigError) -> bool` unit-tested for a `Validation{reason: "backend.command ..."}` returning true and a `Parse{..}` returning false), so the branch logic is mechanically verifiable without driving a TTY.

5. Run the full gate commands.

- **Depends on:** synthesize-detected-backend, detect-driven-scaffold
- **Done when:** Either the empty-backend startup failure launches the Doctor overlay (or prints the enriched guide-and-scaffold message) instead of a bare `exit(1)`, with `is_recoverable_empty_backend` unit-tested; OR, if a clean launch is infeasible, the change is reverted to the enriched-message-plus-`exit(1)` form and that deferral is recorded in STATUS. Binary land-or-revert. cargo test/clippy/fmt green.

---

## 0003 — Diagnostics And Shipped-Config Hardening

### enrich-empty-backend-diagnostics — Enrich the Empty-Backend Validation Diagnostic

The empty-backend rejection reason is the bare string `"backend.command must not be empty"` (`config.rs:855`); it names no file, no field location, and no remedy, and does not mention that the backend belongs in the machine-specific `~/.makina/config.toml` or that supported CLIs are auto-detected. This task rewrites that reason to be precise and actionable while preserving the `backend.command` substring so existing assertions keep matching.

**Steps:**

1. In `crates/makina-core/src/config.rs`, replace the reason string in the empty-backend arm (`config.rs:853-857`) with a message that BEGINS with `backend.command` (to satisfy `reason.contains("backend.command")` at `config.rs:1587` and `config.rs:1605`) and then names the file, field, fix, and supported CLIs:

   ```rust
   return Err(ConfigError::Validation { reason: format!(
       "backend.command must not be empty — no agent backend is configured. \
        Set [backend].command in ~/.makina/config.toml (machine-specific, NOT committed), \
        or install a supported agent CLI on PATH for auto-detection ({}). \
        The project .makina/config.toml must NOT set a backend.",
       crate::preflight::KNOWN_AGENTS.iter().map(|a| a.command).collect::<Vec<_>>().join(", "),
   )});
   ```

2. In the `#[cfg(test)] mod` of `config.rs`, add a test asserting the enriched content:

   ```rust
   #[test]
   fn empty_backend_error_lists_supported_agents_and_file() {
       let no_global = std::path::Path::new("/tmp/__makina_msg_g.toml");
       let no_project = std::path::Path::new("/tmp/__makina_msg_p.toml");
       match Config::load_with_labels(Some(no_global), None, Some(no_project), None, Some("")) {
           Err(ConfigError::Validation { reason }) => {
               assert!(reason.contains("backend.command"));
               assert!(reason.contains("~/.makina/config.toml"));
               assert!(reason.contains("gemini"));
           }
           other => panic!("expected Validation error, got: {other:?}"),
       }
   }
   ```

3. Confirm the two pre-existing assertions `reason.contains("backend.command")` (`config.rs:1587`, `config.rs:1605`) still hold with the new message.

4. Run the full gate commands.

- **Depends on:** add-known-agent-registry, synthesize-detected-backend
- **Done when:** The empty-backend `ConfigError::Validation` reason begins with `backend.command`, names `~/.makina/config.toml`, states the project config must not set a backend, and lists the `KNOWN_AGENTS` commands; `empty_backend_error_lists_supported_agents_and_file` passes and the two pre-existing `backend.command` assertions remain green. cargo test/clippy/fmt green.

---

### harden-shipped-config — Harden the Shipped .makina/config.toml Comments Against the Trap

The shipped `.makina/config.toml` comment block (`.makina/config.toml:9-21`) tells users the global layer MUST supply the backend and shows a `[backend]`/`command = "gemini"` snippet to hand-write — the exact manual step whose omission triggers the empty-backend hard-fail. This task rewrites those comments to describe auto-detection and the Doctor `w` scaffold so a supported-agent user never hits the trap and a user without one is guided, without adding any backend key to the (committed) project layer.

**Steps:**

1. In `.makina/config.toml`, rewrite the comment block at lines 9-21 (the `# The *global* layer ...` paragraph and its `[backend]`/`[planner]` example) to state that Makina auto-detects a supported agent CLI on `$PATH` (list them: `gemini`, `claude-code-acp`, `grok`), that `~/.makina/config.toml` is only needed to OVERRIDE the auto-detected default, and that the Doctor overlay's `w` key writes a starter global config for the detected agent. Keep the pointer to `docs/trial/e2e-run.md`.

2. Do NOT add any `[backend]` or `[[providers]]` table to `.makina/config.toml` — the backend is machine-specific and belongs to the global layer; the committed project config must remain backend-free. Leave `base_branch`, `concurrency`, `[caps]`, and the `[[gates]]` tables (`.makina/config.toml:26-63`) unchanged.

3. Verify the file still parses: run `cargo test` (the config crate parses fixtures) and confirm the shipped file is valid TOML by loading the repo normally.

4. Run the full gate commands.

- **Depends on:** —
- **Done when:** Documentation-only (comments in a shipped config); no new code path, so no red-green test is required. The `.makina/config.toml:9-21` comments describe auto-detection and the `w` scaffold instead of instructing manual `[backend]` entry, no backend/provider table is added to the project layer, and the file remains valid TOML. cargo test/clippy/fmt green.

---

**End of plan 0043 TASKS.** When every "Done when" bullet is green, a fresh
clone with a supported agent CLI on `$PATH` runs `cargo run --release`
successfully via auto-detected backend defaults (the `KNOWN_AGENTS` registry
and pure `detect_backend_in_path` in `preflight.rs`, threaded through
`Config::apply_detected_backend` on the production load path only, with
`Config::load` kept hermetic); the Doctor `w` scaffold detects, confirms, and
persists a `~/.makina/config.toml` whose backend is the one actually found (or
a commented template naming every supported CLI); the empty-backend startup
failure guides the user (gated land-or-revert on launching the Doctor overlay)
instead of a bare `exit(1)`; the validation diagnostic names the exact file,
field, fix, and supported CLIs; and the shipped `.makina/config.toml` comments
point at auto-detection and the `w` scaffold instead of the manual
global-setup ritual — all with the gate commands green.
