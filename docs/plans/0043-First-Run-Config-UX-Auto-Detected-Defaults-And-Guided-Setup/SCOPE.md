# Scope — Plan 0043

> Make a fresh `cargo run --release` succeed out of the box by auto-detecting a usable agent CLI on PATH, upgrading the Doctor `w` scaffold into a detect→confirm→persist flow, and hardening the empty-backend hard-fail into precise, actionable guidance.

## Why this plan

**1. A fresh clone hard-fails because an all-empty resolved config is rejected.** With no `~/.makina/config.toml`, the shipped `.makina/config.toml` sets no `[backend]` and no `[[providers]]`, so `Config::resolve` produces an empty backend and empty providers, and `validate` returns `ConfigError::Validation { reason: "backend.command must not be empty" }` at `config.rs:853` (the `if self.backend.command.is_empty() && self.providers.is_empty()` arm), aborting startup.

**2. `resolve` only synthesizes a provider from a *legacy* `[backend]`, never from what is installed.** `Config::resolve` (`config.rs:683`) synthesizes a `"default"` provider *only* when `providers.is_empty() && !global.backend.command.is_empty()` (`config.rs:714`), and `GlobalConfig::default` ships `backend: BackendConfig::default()` (empty `command`) with `providers: Vec::new()` (`config.rs:403`), so absent a global config there is nothing to synthesize from even when `gemini`/`claude-code-acp` is on `$PATH`. The pure PATH resolver already exists — `is_executable` (`preflight.rs:175`) and the `$PATH`-walking `resolve_in_path` (`preflight.rs:133`) — but is never consulted for defaults.

**3. The Doctor `w` scaffold hardcodes `gemini` and only writes commented templates.** `write_doctor_scaffold` (`event.rs:770`) writes a fixed global template with `command = "gemini"` (`event.rs:793`) regardless of what is installed and refuses if any config exists, so it neither detects the real backend nor persists a validated global config for the machine at hand.

**4. Startup treats every load failure as fatal with only generic guidance.** The `main` error arm (`main.rs:52`) prints a files-checked block and `std::process::exit(1)` (`main.rs:85`) for *any* `ConfigError`, including the recoverable empty-backend case — there is no guided path (e.g. opening the Doctor overlay to scaffold) even when the only problem is a missing backend.

**5. The empty-backend diagnostic names neither the file, the field, nor a fix.** The rejection reason is the bare string `"backend.command must not be empty"` (`config.rs:855`); it does not tell the operator that the backend belongs in the machine-specific `~/.makina/config.toml`, that the project `.makina/config.toml` must not set it, or which agent CLIs are auto-detected.

**6. The shipped project config documents a manual global-setup ritual that reproduces the trap.** `.makina/config.toml:9-21` tells users the global layer *must* supply the backend and shows a `[backend]`/`command = "gemini"` snippet to hand-write, so a user who skips that step lands squarely on finding 1; the shipped project config's comments are the on-ramp to the trap and must point at auto-detection and the `w` scaffold instead.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0001–0003):

- **0001 — Backend Auto-Detection Defaults.** Add a `KNOWN_AGENTS` registry and a pure `detect_backend_in_path(path_env)` helper in `preflight.rs` (reusing the existing `is_executable`/`$PATH`-walk logic, spawning nothing), then thread detection into config resolution via `Config::apply_detected_backend(&mut self, path_env)`, which — only when `providers` and `backend.command` are both empty — synthesizes a `"default"` provider from the first supported CLI found on `$PATH` and assigns it to any unset role. Detection is invoked from the production entry point `load_defaults_with_paths` (reading `$PATH` once) and kept OFF the deterministic `Config::load` path so existing tests stay hermetic; a fresh clone with a supported agent installed now validates.
- **0002 — Guided First-Run Setup And Persist.** Rewrite the Doctor `w` scaffold (`write_doctor_scaffold`, `event.rs:770`) from a fixed-template writer into a detect→confirm→persist flow: detect a supported CLI, write a minimal `~/.makina/config.toml` with the actually-detected `command`/`args` (persisting a config that validates), and when nothing is detected write a commented template that names every supported CLI so the user can pick one; and upgrade the startup hard-fail arm (`main.rs:51`) so an empty-backend failure guides the user (optionally launching the Doctor overlay) instead of unconditionally exiting.
- **0003 — Diagnostics And Shipped-Config Hardening.** Enrich the empty-backend rejection in `Config::validate` (`config.rs:853`) so its reason names the exact file (`~/.makina/config.toml`, machine-specific), the field (`[backend].command`), the fix, and the list of auto-detected CLIs from `KNOWN_AGENTS`, while preserving the substring `backend.command` so existing assertions still match; and rewrite the shipped `.makina/config.toml` comment block (`.makina/config.toml:9-21`) so it points at auto-detection and the Doctor `w` scaffold instead of instructing users to hand-write a `[backend]` in the global config.

## Origin -> workstream mapping

| Finding | Addressed by |
|---|---|
| A fresh clone hard-fails because the all-empty resolved config is rejected with `backend.command must not be empty` (`config.rs:853-857`). | `0001` |
| `resolve` only synthesizes a provider from a legacy `[backend]`, never from what is installed on `$PATH` (`config.rs:683-754`, `config.rs:403-415`, `preflight.rs:133-190`). | `0001` |
| The Doctor `w` scaffold hardcodes `gemini` and only writes commented templates (`event.rs:770-879`, `event.rs:787-800`). | `0002` |
| Startup treats every load failure as fatal with only generic guidance (`main.rs:51-86`). | `0002` |
| The empty-backend diagnostic names neither the file, the field, nor a fix (`config.rs:853-857`). | `0003` |
| The shipped project config documents a manual global-setup ritual that reproduces the trap (`.makina/config.toml:9-21`). | `0003` |

## Locked decisions

- **Detection is injectable and OFF the deterministic Config::load path.** `Config::apply_detected_backend` takes an explicit `path_env: &str`, and `load_with_labels` runs it only when its new `detect_path_env: Option<&str>` is `Some`. `Config::load` (the public test helper) passes `None`; only `load_defaults_with_paths` passes the real `$PATH`. This keeps every existing hermetic config test deterministic (they never see the operator's PATH) while production auto-detects. Revisit only if a future entry point needs detection outside `load_defaults`.
- **KNOWN_AGENTS is the single source of truth, ordered gemini → claude-code-acp → grok.** The registry lives once in `preflight.rs` and feeds detection, the scaffold template, and the validation message, so the supported-CLI list can never drift across those sites. Order is detection priority: `gemini --acp --yolo` first because it is the e2e-proven path (docs/trial/e2e-run.md:100-101,272-273); `claude-code-acp --acp` and `grok --acp` follow. The exact flag sets are sourced from that trial doc and the makina-acp crate docs; extending the registry (new agents/flags) is a one-line edit and does not change any call site.
- **The committed project config never carries a backend.** The backend is machine-specific and belongs to the global layer (`~/.makina/config.toml`); the shipped `.makina/config.toml` stays backend-free. 'Cannot reproduce the trap out of the box' is therefore delivered by auto-detection (WS0001) plus guidance (WS0002/0003), NOT by hardcoding a backend into version control — hardcoding one would break every machine that lacks that specific CLI.
- **The enriched validation message preserves the `backend.command` prefix.** The rewritten empty-backend reason begins with `backend.command` so the two existing assertions (`config.rs:1587`, `config.rs:1605`) and any downstream `contains("backend.command")` checks keep matching. This bounds test churn to additive assertions.
- **Launching the Doctor overlay on startup is a gated, revertible follow-up.** The empty-backend startup path is upgraded to guide (and ideally open the Doctor overlay) rather than `exit(1)`, but only if a partial-config `App` can be launched cleanly; otherwise it reverts to the enriched message plus `exit(1)` and records the deferral. The auto-detection (WS0001) already prevents the failure whenever a supported CLI is installed, so this task is strictly additive polish for the no-agent case.

## Out of scope

- An interactive multi-step setup wizard reading stdin at launch. Makina is a ratatui TUI, not a line-oriented prompt; the realistic 'guided' surface is the existing Doctor overlay's `w` detect→confirm→persist flow, not a new stdin wizard.
- Spawning candidate agent CLIs to verify they authenticate or speak ACP. Spawning an unauthenticated agent can itself hang (plan 0015); detection stays pure filesystem/PATH resolution, matching the existing preflight discipline.
- Auto-writing or mutating the committed .makina/config.toml at runtime. The project layer is version-controlled and shared; persistence targets only the machine-specific ~/.makina/config.toml. The shipped project config change here is comments-only.
- Adding new provider/role configurability or a provider-picker UI beyond selecting a detected default. Provider editing already exists (plans 0011/0017); this plan only supplies a safe default and guided first-run persistence, not new configuration surface.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
