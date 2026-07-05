# Architecture — Plan 0046 (deltas)

> The concrete deltas. This plan touches
> `crates/makina/src/main.rs`, `crates/makina/src/cli.rs`,
> `crates/makina/build.rs`, `crates/makina/src/lib.rs`,
> `crates/makina-core/src/config.rs`, `.makina/config.toml`, `README.md`,
> `docs/demo/makina.tape`, `docs/demo/README.md`, `docs/demo/makina.gif`, and
> `docs/plans/0043-First-Run-Config-UX-Auto-Detected-Defaults-And-Guided-Setup/STATUS.md`.
> Line numbers are hints; locate by symbol.

## 0001 — Launch Doctor On Empty Backend

Today `main`'s `Err(e)` arm (`crates/makina/src/main.rs:52-86`) builds a
`global`/`project` files-checked block and prints a guidance message, then
calls `std::process::exit(1)` at `main.rs:85` for **every** `ConfigError` —
including the recoverable empty-backend case, whose enriched reason string
begins with `backend.command` (`crates/makina-core/src/config.rs:905-917`).
Plan 0043 already ships the Doctor `w` scaffold
(`crates/makina/src/event.rs:1031` `write_doctor_scaffold`) and the
`AppEvent::OpenDoctor` overlay (`crates/makina/src/app.rs:3773`), but left the
launch task GATED.

**Edits:**

**Add a recoverable-error classifier (`config.rs`).**

```rust
impl ConfigError {
    /// True iff this is the recoverable "no agent backend configured" case
    /// (empty `backend.command`, no providers) the binary can guide instead of
    /// hard-failing. Every other validation/parse/IO error is unrecoverable.
    pub fn is_recoverable_empty_backend(&self) -> bool {
        matches!(self, ConfigError::Validation { reason } if reason.starts_with("backend.command"))
    }
}
```

**Branch the `main.rs` Err arm.** After printing the files-checked block,
`if e.is_recoverable_empty_backend()` print the enriched guidance plus
`"Open Makina and press 'w' in the Doctor overlay to auto-detect and write
~/.makina/config.toml."` and attempt to launch the TUI seeded with
`AppEvent::OpenDoctor`; the unrecoverable path keeps `std::process::exit(1)`
(`main.rs:85`) verbatim.

**Properties that make this safe:**

- The classifier is pure and unit-tested against `Validation`/`Parse`
  variants.
- The enriched reason already begins with `backend.command` (`config.rs:907`),
  so `starts_with` is exact and does not match `caps.*`/`role`/provider
  reasons.
- The fallback preserves the existing enriched hard-fail message, so a revert
  leaves 0043's diagnostics intact.
- The deferral is recorded in STATUS, satisfying the binary
  land-or-revert-and-record clause.

## 0002 — CLI Argument Surface

Today `fn main()` (`crates/makina/src/main.rs:37`) never reads
`std::env::args` and jumps straight to `Config::load_defaults_with_paths()`
(`main.rs:51`); there is no `clap` dependency (`Cargo.toml` has none), so
`makina --help`/`--version` launch the full-screen TUI. The reusable preflight
probes already exist: `detect_backend_in_path`
(`crates/makina-core/src/preflight.rs:207`) and `probe_providers`
(`preflight.rs:141`).

**Edits:**

**New `crates/makina/src/cli.rs` (registered `pub mod cli;` in
`crates/makina/src/lib.rs`).**

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum CliAction { LaunchTui, ShowHelp, ShowVersion, RunDoctor, Unknown(String) }

/// Parse argv (already `.skip(1)`-ed). Bare invocation → LaunchTui.
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

`version_text()` returns
`format!("makina {} ({})", env!("CARGO_PKG_VERSION"), sha)` when
`option_env!("MAKINA_GIT_SHA")` is `Some`, else `makina {version}`.

**New `crates/makina/build.rs`** runs `git rev-parse --short HEAD` and emits
`cargo:rustc-env=MAKINA_GIT_SHA=…` (silent when git is absent).

**Dispatch in `main.rs`** at the top of `main`, before config load:
`match cli::parse_args(&std::env::args().skip(1).collect::<Vec<_>>())` —
Help/Version print and `return`; Doctor runs the headless report and
`std::process::exit(code)`; `LaunchTui` and `Unknown` fall through to the
existing startup (Unknown after printing a hint).

**Properties that make this safe:**

- Parsing is a pure function unit-tested per variant.
- The bare-invocation path returns `LaunchTui`, so the existing TUI startup is
  byte-for-byte unchanged.
- `--doctor` reuses the spawn-free preflight probes (honoring plan 0015).
- The git sha is `option_env!`, so a build without git still compiles and
  prints the plain version.

## 0003 — Safe Concurrency Default

Today `.makina/config.toml:30` sets `concurrency = 10` with a comment
(`.makina/config.toml:26-29`) that says "Set to 10 to parallelise
aggressively"; the README's Configure example shows `concurrency = 2`
(`README.md:107`), so the shipped reality and the documented example disagree,
and 10 auto-approved agents (`WorktreePolicy` auto-answers permissions) each
compiling the whole workspace is a hostile first-run default.

**Edits:**

**Lower the value and reframe the comment (`.makina/config.toml`).**

```toml
# How many tasks may run concurrently (each in its own worktree). Default 2 is a
# first-timer-safe ceiling: each gate pass compiles the whole workspace and agent
# turns drive real model calls, so a high value raises machine load and merge-lock
# contention. Raise it once you trust the run on this machine.
concurrency = 2
```

**Properties that make this safe:**

- The change is a single scalar plus a comment.
- No test asserts the shipped `concurrency` value (grep of `crates` for a
  `concurrency == 10` assertion is empty), and `Config::validate` only
  rejects `concurrency < 1`, so `2` is valid.
- The file stays valid TOML with no `[backend]`/`[[providers]]` added
  (honoring plan 0043's backend-free project-layer decision).

## 0004 — Committed Demo Recording

Today `docs/demo` does not exist and no demo tooling is committed (repo-wide
grep for `vhs`/`asciinema`/`.tape`/`.cast` is empty); the README's "Status:
MVP" block (`README.md:14-17`) links only to prose findings, so an outsider
cannot see the TUI without a full `cargo build`.

**Edits:**

**New `docs/demo/makina.tape`** (vhs script) drives a deterministic session:

```
# Render with: vhs docs/demo/makina.tape
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

**New `docs/demo/README.md`** documents the render recipe, the vhs install
pointer, and that the committed `makina.gif` is regenerated by re-running the
tape.

**Properties that make this safe:**

- The tape and recipe are plain text, always committable and deterministic.
- They touch no Rust, so all three gates stay trivially green.
- The binary GIF render is best-effort and, when deferred, is recorded in
  STATUS so the outsider-visible artifact is tracked, not silently skipped.

## 0005 — README Refresh Against Shipped Reality

Today `README.md:148` states running `requires the two config files above`,
contradicting the auto-detection the same README documents at
`README.md:72-80` (which plan 0043 shipped, making the global config
optional). The Doctor overlay key `!` (`crates/makina/src/event.rs:1821`) and
the `w` scaffold are undocumented, the new CLI flags do not exist in the
README, and the "Caution — running a list mutates the repository" safety
block sits *after* the Run section (`README.md:162-172`).

**Edits:**

**Fix the stale Run claim (`README.md:148`).** Change the comment to reflect
auto-detection, e.g.
`cargo run -p makina    # auto-detects an installed agent — no global config required`.

**Document the CLI** in the Run section: `makina --help` (usage + config file
locations), `makina --version` (version + git sha), `makina --doctor`
(headless preflight, non-zero on failure).

**Document the Doctor overlay** in the Keys list: `!` opens the Doctor
health-check overlay, `w` writes a starter `~/.makina/config.toml` via
detect→confirm→persist.

**Reorder** the throwaway-clone "Caution" block to precede the Run section.

**Properties that make this safe:**

- Documentation-only, no compiled surface.
- The flag descriptions are diffed against `crates/makina/src/cli.rs` and the
  overlay keys against `event.rs`, so docs cannot drift from code.
- The reorder is a block move with no content change.

## Test strategy

- **0001 (empty-backend classifier).** In `crates/makina-core/src/config.rs`:
  `empty_backend_is_recoverable_but_other_errors_are_not` asserts
  `is_recoverable_empty_backend` is true for a `Validation` reason beginning
  `backend.command`, and false for a `caps.*` `Validation` and a `Parse` (red
  before: the method did not exist). The `main.rs` branch itself is exercised
  manually since it drives a TTY; its logic is delegated to the tested
  classifier.
- **0002 (CLI).** In `crates/makina/src/cli.rs`:
  `parse_args_dispatches_each_flag` covers every `CliAction` variant incl.
  bare-launch and unknown; `version_text_starts_with_pkg_version` asserts the
  version contains `CARGO_PKG_VERSION`; `doctor_report_exit_codes` asserts
  `render_doctor_report` returns exit 0 when a backend is detected or config
  loads and 1 otherwise. The `main` wiring is verified by running
  `cargo run -p makina -- --help/--version/--doctor`.
- **0003 (concurrency).** The config crate parses `.makina/config.toml` as
  part of `cargo test`; `Config::validate` accepts `concurrency = 2` (it only
  rejects `< 1`), so the suite proves the file still loads.
- **0004/0005 (demo, README).** Documentation-only; verified by review that
  the tape launches the real binary, the recipe names the render command, and
  the README's flag descriptions match `cli.rs` and the Doctor keys match
  `event.rs`.
- All tasks keep `cargo test`, `cargo clippy --all-targets -- -D warnings`,
  and `cargo fmt --check` green.

## Interaction with prior work

- **0043 — First-Run Config UX / Auto-Detected Defaults.** WS0001 lands the
  `launch-doctor-on-empty-backend` task 0043 recorded GATED
  (`docs/plans/0043-.../STATUS.md:18`), consuming the merged
  `detect-driven-scaffold` (`event.rs:1031`) and the enriched empty-backend
  reason (`config.rs:905-917`); the land-or-revert-and-record clause from
  `docs/plans/0043-.../TASKS.md:263` is honored.
- **0044 — Agent Coverage & Compatibility.** WS0002's `--doctor` and WS0005's
  README reuse the registry-derived surfaces 0044 stabilized (`KNOWN_AGENTS`,
  `detect_backend_in_path`); the README compatibility matrix 0044 published is
  left intact and only reconciled.
- **0045 — CI & Release Hygiene.** WS0005 reconciles the README
  install/release story with 0045's pinned 1.96.1 toolchain, MSRV 1.85,
  `CHANGELOG.md`, and `release.yml`; WS0002's git-sha stamp complements the
  tagged v0.1.0 story.
- **0015 — Idle-Hang Detection.** WS0002's headless `--doctor` reuses the
  spawn-free preflight probes (`probe_providers`, `detect_backend_in_path`)
  and never launches an agent, honoring 0015's discipline.
