# XAgent Plan 0046 — Front Door — Makina's First Five Minutes for an Outside Developer

This plan fixes the first five minutes for an outside developer: it lands the gated 0043 `launch-doctor-on-empty-backend` task by classifying the recoverable empty-backend `ConfigError` in `crates/makina/src/main.rs` (adding `ConfigError::is_recoverable_empty_backend` in `config.rs` with a unit test) and guiding the user toward the Doctor `w` scaffold instead of a bare `std::process::exit(1)` (binary land-or-revert); it gives the `makina` binary a real CLI surface — a hand-rolled parser in a new `crates/makina/src/cli.rs`, a `crates/makina/build.rs` that stamps the git sha, and argv dispatch in `main.rs` so `makina --help` prints usage + config locations, `makina --version` prints the workspace version plus git sha, and `makina --doctor` runs a headless preflight (reusing `preflight::detect_backend_in_path`/`probe_providers`) and exits non-zero on failure while a bare `makina` still launches the TUI; it lowers the shipped `.makina/config.toml` `concurrency` from 10 to 2 so a first clone does not auto-approve ten agents; it commits a scriptable `docs/demo/makina.tape` vhs demo plus a render recipe so an outsider can see the TUI without building it; and it refreshes `README.md` — fixing the stale "requires the two config files" claim at line 148, documenting the Doctor overlay (`!`) and `w` scaffold and the new CLI flags, and moving the throwaway-clone safety guidance earlier — all with the three quality gates green.

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

## 0001 — Launch Doctor On Empty Backend

### launch-doctor-on-empty-backend — Guide the User Instead of Exiting on an Empty-Backend Failure (GATED)

**Gate:** This is the conditional Phase-2 follow-up recorded GATED in plan 0043 (`docs/plans/0043-.../STATUS.md:18`; spec `docs/plans/0043-.../TASKS.md:261-280`). Its upstreams — `synthesize-detected-backend` and `detect-driven-scaffold` — are already merged and green on develop, so it may land now. If constructing a partial-config `App` cleanly is infeasible (the orchestrator's `CoreApi` requires a validated `Config`), the change must fall back to the enriched-message-plus-`exit(1)` form and record that deferral in STATUS — binary land-or-revert-and-record, leaving the enriched hard-fail message in place.

Today `main`'s `Err(e)` arm (`crates/makina/src/main.rs:52-86`) prints a files-checked block and calls `std::process::exit(1)` (`main.rs:85`) for every `ConfigError`, including the recoverable empty-backend case whose enriched reason begins with `backend.command` (`crates/makina-core/src/config.rs:905-917`). There is no in-app path to the detect→confirm→persist Doctor scaffold (`crates/makina/src/event.rs:1031`) that would fix it.

**Steps:**

1. In `crates/makina-core/src/config.rs`, add an `impl ConfigError` method just below the `enum ConfigError` block (after `config.rs:78`):

   ```rust
   impl ConfigError {
       /// True iff this is the recoverable "no agent backend configured" case
       /// (empty `backend.command`, no providers) — the binary can guide the user
       /// through detect→confirm→persist instead of hard-failing. Every other
       /// validation/parse/IO error is unrecoverable at startup.
       pub fn is_recoverable_empty_backend(&self) -> bool {
           matches!(self, ConfigError::Validation { reason } if reason.starts_with("backend.command"))
       }
   }
   ```

2. In the `#[cfg(test)] mod tests` of `config.rs`, add the verbatim unit test:

   ```rust
   #[test]
   fn empty_backend_is_recoverable_but_other_errors_are_not() {
       let empty = ConfigError::Validation {
           reason: "backend.command must not be empty — no agent backend is configured.".to_string(),
       };
       assert!(empty.is_recoverable_empty_backend());
       let caps = ConfigError::Validation {
           reason: "caps.gate_iterations must be at least 1".to_string(),
       };
       assert!(!caps.is_recoverable_empty_backend());
       let parse = ConfigError::Parse {
           file: "~/.makina/config.toml".to_string(),
           message: "expected `=`".to_string(),
       };
       assert!(!parse.is_recoverable_empty_backend());
   }
   ```

3. In `crates/makina/src/main.rs`, inside the `Err(e)` arm (`main.rs:54-86`), keep the `global_status`/`project_status` block and the existing `eprintln!` guidance, then replace the unconditional `std::process::exit(1);` (`main.rs:85`) with a branch: `if e.is_recoverable_empty_backend() { … } else { std::process::exit(1); }`.

4. In the recoverable branch, first `eprintln!` the explicit pointer `"\nOpen Makina and press 'w' in the Doctor overlay to auto-detect and write ~/.makina/config.toml."`. Then attempt to construct the `App` from the partially-resolved `load_paths` and launch the TUI seeded with an initial `AppEvent::OpenDoctor` (the overlay is opened by `app.update(makina::app::AppEvent::OpenDoctor)` — see `crates/makina/src/app.rs:3773` and the `ui.rs:9778` call site).

5. If wiring a partial-config `App` cleanly is not feasible within this task's footprint (because `CoreApi::with_audit_registry` at `main.rs:289` requires a fully-validated `Config`), fall back to retaining `std::process::exit(1)` in the recoverable branch after printing the enriched guidance and `w`-pointer, and record in `docs/plans/0043-.../STATUS.md` (flip the `launch-doctor-on-empty-backend` row from `🔲 Gated (not run)` to a `deferred` note) that the overlay-launch was deferred while the guidance landed.

6. Run the full gate commands.

- **Depends on:** —
- **Done when:** `ConfigError::is_recoverable_empty_backend` exists and `empty_backend_is_recoverable_but_other_errors_are_not` passes (true for the empty-backend `Validation`, false for a `caps` `Validation` and a `Parse`); AND either (A) the empty-backend startup branch launches the TUI seeded with `AppEvent::OpenDoctor` instead of `exit(1)`, or (B) it prints the enriched guidance plus the `w`-Doctor pointer and retains `exit(1)`, with the overlay-launch deferral recorded in `docs/plans/0043-.../STATUS.md`. Binary land-or-revert-and-record. cargo test/clippy/fmt green.

---

## 0002 — CLI Argument Surface

### add-cli-parser — Add A Pure CLI Parser Module With Help, Version, And Doctor Renderers

The `makina` binary has no argument parsing: `fn main()` (`crates/makina/src/main.rs:37`) never reads `std::env::args` and there is no `clap` dependency. This task adds a pure, unit-tested parser module so the wiring task can dispatch on it; keeping the parser and its renderers pure (no env reads) makes every branch testable without a TTY. The headless-doctor renderer reuses the spawn-free preflight probes `detect_backend_in_path` (`crates/makina-core/src/preflight.rs:207`) and `probe_providers` (`preflight.rs:141`).

**Steps:**

1. Create `crates/makina/src/cli.rs` and register it in `crates/makina/src/lib.rs` by adding `pub mod cli;` next to the other `pub mod` lines (`lib.rs:26-43`).

2. In `cli.rs`, add the parser:

   ```rust
   #[derive(Debug, PartialEq, Eq)]
   pub enum CliAction { LaunchTui, ShowHelp, ShowVersion, RunDoctor, Unknown(String) }

   /// Parse argv already stripped of argv[0]. A bare invocation launches the TUI.
   pub fn parse_args(args: &[String]) -> CliAction {
       match args.first().map(String::as_str) {
           None => CliAction::LaunchTui,
           Some("-h") | Some("--help") => CliAction::ShowHelp,
           Some("-V") | Some("--version") => CliAction::ShowVersion,
           Some("--doctor") => CliAction::RunDoctor,
           Some(other) => CliAction::Unknown(other.to_string()),
       }
   }
   ```

3. Add the renderers: `pub fn help_text() -> String` returning a usage block that names the config file locations (`~/.makina/config.toml` global, `.makina/config.toml` project), the flags (`--help`, `--version`, `--doctor`), and a docs pointer (`README § Configure`); and

   ```rust
   pub fn version_text() -> String {
       match option_env!("MAKINA_GIT_SHA") {
           Some(sha) => format!("makina {} ({sha})", env!("CARGO_PKG_VERSION")),
           None => format!("makina {}", env!("CARGO_PKG_VERSION")),
       }
   }
   ```

4. Add the headless-doctor renderer as a pure function so it is testable without env reads:

   ```rust
   /// Render the headless `--doctor` report and its process exit code. A backend is
   /// resolvable when the config already loads OR a KNOWN_AGENTS binary is detected
   /// on PATH; otherwise exit non-zero so scripts can gate on it.
   pub fn render_doctor_report(detected: Option<&str>, config_loaded: bool) -> (String, i32) {
       let mut out = String::from("makina doctor\n");
       match detected {
           Some(agent) => out.push_str(&format!("  detected agent on PATH: {agent}\n")),
           None => out.push_str("  detected agent on PATH: none\n"),
       }
       out.push_str(&format!("  config loads: {config_loaded}\n"));
       let ok = config_loaded || detected.is_some();
       out.push_str(if ok { "  status: OK\n" } else { "  status: FAIL — no agent backend configured or detected\n" });
       (out, if ok { 0 } else { 1 })
   }
   ```

5. Add verbatim unit tests in a `#[cfg(test)] mod tests` in `cli.rs`:

   ```rust
   #[test]
   fn parse_args_dispatches_each_flag() {
       use super::*;
       assert_eq!(parse_args(&[]), CliAction::LaunchTui);
       assert_eq!(parse_args(&["--help".into()]), CliAction::ShowHelp);
       assert_eq!(parse_args(&["-h".into()]), CliAction::ShowHelp);
       assert_eq!(parse_args(&["--version".into()]), CliAction::ShowVersion);
       assert_eq!(parse_args(&["-V".into()]), CliAction::ShowVersion);
       assert_eq!(parse_args(&["--doctor".into()]), CliAction::RunDoctor);
       assert_eq!(parse_args(&["--nope".into()]), CliAction::Unknown("--nope".into()));
   }
   #[test]
   fn version_text_starts_with_pkg_version() {
       assert!(super::version_text().contains(env!("CARGO_PKG_VERSION")));
   }
   #[test]
   fn doctor_report_exit_codes() {
       assert_eq!(super::render_doctor_report(Some("gemini"), false).1, 0);
       assert_eq!(super::render_doctor_report(None, true).1, 0);
       assert_eq!(super::render_doctor_report(None, false).1, 1);
   }
   ```

6. Run the full gate commands.

- **Depends on:** —
- **Done when:** `crates/makina/src/cli.rs` exports `CliAction`, `parse_args`, `help_text`, `version_text`, and `render_doctor_report`, is registered as `pub mod cli;` in `lib.rs`, and `parse_args_dispatches_each_flag`, `version_text_starts_with_pkg_version`, and `doctor_report_exit_codes` pass. cargo test/clippy/fmt green.

---

### add-git-sha-build-script — Stamp The Git Sha Into The Binary Via A Build Script

`makina --version` should print the git sha when available, but there is no build script in any crate (`find crates -name build.rs` is empty). This task adds a `crates/makina/build.rs` that captures the short git sha at build time and exposes it as the `MAKINA_GIT_SHA` compile-time env var read by `cli::version_text` via `option_env!`. It touches a new, disjoint file, so it runs in parallel with the parser task.

**Steps:**

1. Create `crates/makina/build.rs`:

   ```rust
   use std::process::Command;

   fn main() {
       // Re-run when HEAD moves so the stamped sha stays current.
       println!("cargo:rerun-if-changed=../../.git/HEAD");
       if let Ok(out) = Command::new("git").args(["rev-parse", "--short", "HEAD"]).output() {
           if out.status.success() {
               if let Ok(sha) = String::from_utf8(out.stdout) {
                   let sha = sha.trim();
                   if !sha.is_empty() {
                       println!("cargo:rustc-env=MAKINA_GIT_SHA={sha}");
                   }
               }
           }
       }
       // When git is unavailable the env var is simply unset; option_env! yields None.
   }
   ```

2. Confirm no `crates/makina/Cargo.toml` change is needed (Cargo auto-detects `build.rs` at the crate root); do not add a `[build-dependencies]` section since the script only shells out to `git`.

3. Run the full gate commands (a clean `cargo build` must succeed with and without git on PATH — the script is best-effort and never fails the build).

- **Depends on:** —
- **Done when:** `crates/makina/build.rs` exists and emits `cargo:rustc-env=MAKINA_GIT_SHA=…` when `git rev-parse --short HEAD` succeeds, and the workspace still builds when git is absent (the env var is simply unset). cargo test/clippy/fmt green.

---

### wire-cli-in-main — Dispatch On Argv At Startup So --help/--version/--doctor Do Not Launch The TUI

With the parser (`cli::parse_args`) and git-sha stamp in place, this task wires them into `fn main()` (`crates/makina/src/main.rs:37`) so the binary reads argv before doing any TUI or config work. A bare `makina` must still launch the TUI exactly as today (the `LaunchTui` path falls through to the existing startup at `main.rs:51`). This task also edits the `main.rs` config-load region, so it serializes after the empty-backend task that edits the same file.

**Steps:**

1. At the very top of `fn main()` (`crates/makina/src/main.rs:37`, before `Config::load_defaults_with_paths()` at `main.rs:51`), collect argv and dispatch:

   ```rust
   let args: Vec<String> = std::env::args().skip(1).collect();
   match makina::cli::parse_args(&args) {
       makina::cli::CliAction::ShowHelp => { println!("{}", makina::cli::help_text()); return; }
       makina::cli::CliAction::ShowVersion => { println!("{}", makina::cli::version_text()); return; }
       makina::cli::CliAction::RunDoctor => { std::process::exit(run_headless_doctor()); }
       makina::cli::CliAction::Unknown(flag) => {
           eprintln!("unknown flag: {flag}\n\n{}", makina::cli::help_text());
           std::process::exit(2);
       }
       makina::cli::CliAction::LaunchTui => { /* fall through to the existing TUI startup */ }
   }
   ```

2. Add a `fn run_headless_doctor() -> i32` helper (a free function in `main.rs`) that reads `$PATH` once, calls `makina_core::preflight::detect_backend_in_path(&path)` for the detected agent name, attempts `makina_core::config::Config::load_defaults()` for `config_loaded`, calls `makina::cli::render_doctor_report(detected.as_deref(), config_loaded)`, prints the report, and returns its exit code. It must spawn no agent process (reuse the pure probes only).

3. Verify the `LaunchTui` fall-through leaves the existing startup (config load `main.rs:51`, backend build, event loop) untouched — no behavioral change for a bare `makina`.

4. Run the full gate commands, then manually verify `cargo run -p makina -- --help`, `-- --version`, and `-- --doctor` each print and exit without entering raw-mode TUI.

- **Depends on:** add-cli-parser, add-git-sha-build-script, launch-doctor-on-empty-backend
- **Done when:** `main` dispatches on `cli::parse_args` before config load so `makina --help`/`--version` print and exit 0, `makina --doctor` prints the preflight report and exits 0 when a backend is configured or detected (non-zero otherwise), `makina --badflag` exits 2 with the help text, and a bare `makina` still launches the TUI unchanged. cargo test/clippy/fmt green.

---

## 0003 — Safe Concurrency Default

### set-safe-concurrency-default — Lower The Shipped Concurrency Default From 10 To 2

The shipped project config sets `concurrency = 10` (`.makina/config.toml:30`) while the README example shows `concurrency = 2` (`README.md:107`); ten concurrent auto-approved agents mutating a fresh clone is a hostile first-run default and contradicts the docs. No test asserts the shipped value (grep of `crates` for a shipped `concurrency == 10` assertion is empty), and `Config::validate` only rejects `concurrency < 1`, so lowering to `2` is safe. This edits a single disjoint file (`.makina/config.toml`) with no code dependency.

**Steps:**

1. In `.makina/config.toml`, change line 30 from `concurrency = 10` to `concurrency = 2`.

2. Rewrite the preceding comment (`.makina/config.toml:26-29`) so it no longer says "Set to 10 to parallelise aggressively" but instead: `# How many tasks may run concurrently (each in its own worktree). Default 2 is a` / `# first-timer-safe ceiling: each gate pass compiles the whole workspace and agent` / `# turns drive real model calls, so a high value raises machine load and merge-lock` / `# contention. Raise it once you trust the run on this machine.`

3. Do not add any `[backend]` or `[[providers]]` table and leave `base_branch`, `[caps]`, and `[[gates]]` unchanged; the file must remain valid TOML (the config crate parses it during `cargo test`).

4. Run the full gate commands.

- **Depends on:** —
- **Done when:** `.makina/config.toml` sets `concurrency = 2` with a comment explaining the first-timer-safe default and how to raise it, no backend/provider table is added, and the file remains valid TOML. cargo test/clippy/fmt green.

---

## 0004 — Committed Demo Recording

### add-demo-tape-and-recipe — Commit A Scriptable VHS Demo Tape And Render Recipe

There is no committed demo (`docs/demo` is absent; a repo-wide grep for `vhs`/`asciinema`/`.tape`/`.cast` is empty), so an outsider cannot see the TUI without a full build. This task commits a deterministic, reproducible charmbracelet/vhs tape plus a render recipe. The tape and recipe are plain text and always committable; the binary GIF render is best-effort (vhs may be absent on the build host) and, if not rendered, the deferral is recorded.

**Steps:**

1. Create `docs/demo/makina.tape`:

   ```
   # Makina TUI demo. Render with: vhs docs/demo/makina.tape
   Output docs/demo/makina.gif
   Set FontSize 14
   Set Width 1200
   Set Height 700
   Type "cargo run -p makina"
   Enter
   Sleep 6s
   Type "o"
   Sleep 3s
   Type "q"
   Sleep 1s
   ```

2. Create `docs/demo/README.md` documenting: the one-line render command `vhs docs/demo/makina.tape`, the vhs install pointer (`https://github.com/charmbracelet/vhs`), that the output is `docs/demo/makina.gif`, and that re-running the tape regenerates the GIF.

3. If `vhs` is available on the build host, run `vhs docs/demo/makina.tape` and commit the resulting `docs/demo/makina.gif`. If vhs is unavailable, do NOT fabricate a binary; instead note in `docs/demo/README.md` that the rendered GIF is pending a vhs render and record the deferral in the plan STATUS.

4. Run the full gate commands (no Rust is touched, so the gates must remain green as-is).

- **Depends on:** —
- **Done when:** Documentation-only (no runtime surface). `docs/demo/makina.tape` and `docs/demo/README.md` exist, the tape's `Type` line launches the real binary (`cargo run -p makina`) and its `Output` targets `docs/demo/makina.gif`, and either `docs/demo/makina.gif` is committed or its render is recorded as deferred in `docs/demo/README.md`. cargo test/clippy/fmt green.

---

## 0005 — README Refresh Against Shipped Reality

### readme-refresh-shipped-reality — Refresh The README Against Shipped Reality

The README still says `cargo run -p makina    # requires the two config files above` (`README.md:148`), contradicting the auto-detection it documents at `README.md:72-80` (which plan 0043 shipped); the Doctor overlay key `!` (`crates/makina/src/event.rs:1821`) and the `w` scaffold are undocumented, the new CLI flags are absent, and the throwaway-clone safety guidance sits after the Run section (`README.md:162-172`). This documentation-only task reconciles the README with shipped behavior, the new CLI (`cli.rs`), the safe concurrency default, and the committed demo.

**Steps:**

1. In `README.md`, replace the stale Run comment (`README.md:148`) `cargo run -p makina    # requires the two config files above` with one reflecting auto-detection, e.g. `cargo run -p makina    # auto-detects an installed agent — no global config required`.

2. In the Run section, document the CLI flags introduced by `wire-cli-in-main`: `makina --help` (usage + config file locations), `makina --version` / `-V` (version + git sha), and `makina --doctor` (headless preflight; exits non-zero when no backend is configured or detected). Keep the descriptions byte-consistent with `crates/makina/src/cli.rs` `help_text`.

3. In the Keys list (`README.md:151-156`), add the Doctor overlay: `!` opens the Doctor health-check overlay and `w` writes a starter `~/.makina/config.toml` via detect→confirm→persist (matching `crates/makina/src/event.rs:1821` and the `w` scaffold at `event.rs:1031`).

4. Move the `## Caution — running a list mutates the repository` block (`README.md:162-172`) to appear immediately BEFORE the `## Run` section so a first-timer sees the throwaway-clone guidance before running.

5. Add a demo reference near the top (after the Status block, `README.md:14-17`) pointing at `docs/demo/makina.gif` (or, if the GIF render was deferred, at `docs/demo/makina.tape` and the recipe).

6. Reconcile the install/release story: note that the shipped `.makina/config.toml` now uses the first-timer-safe `concurrency = 2` (matching the example at `README.md:107`), and confirm the toolchain/MSRV line (`README.md:43`, 1.96.1 pinned / MSRV 1.85 from plan 0045) and add a pointer to `CHANGELOG.md` and the release workflow for prebuilt binaries.

7. Run the full gate commands (README is not compiled, but the gates must remain green).

- **Depends on:** wire-cli-in-main, set-safe-concurrency-default, add-demo-tape-and-recipe
- **Done when:** Documentation-only (no runtime surface, so the red-green gate is exempt). `README.md:148`'s "requires the two config files" claim is replaced with auto-detection reality; the Run section documents `--help`/`--version`/`--doctor`; the Keys list documents the Doctor `!` overlay and `w` scaffold; the Caution/throwaway-clone block precedes the Run section; the demo is referenced; and the concurrency/toolchain/CHANGELOG notes match shipped reality. cargo test/clippy/fmt green.

---

**End of plan 0046 TASKS.** When every "Done when" bullet is green, an outside
developer's first five minutes are guided, not hostile: an empty-backend start
routes to the Doctor `w` scaffold instead of `exit(1)`,
`makina --help/--version/--doctor` behave as a real CLI, the shipped
concurrency is a safe 2, a committed vhs demo shows the TUI, and the README
matches shipped reality — all with the gate commands green.
