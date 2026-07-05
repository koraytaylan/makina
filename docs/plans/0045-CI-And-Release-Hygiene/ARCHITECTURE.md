# Architecture — Plan 0045 (deltas)

> The concrete deltas. This plan touches
> `rust-toolchain.toml`, `Cargo.toml`, `crates/makina/Cargo.toml`,
> `crates/makina-core/Cargo.toml`, `crates/makina-acp/Cargo.toml`,
> `crates/makina-core/src/lib.rs`, `crates/makina-core/src/test_support.rs`,
> `.github/workflows/ci.yml`, `.github/workflows/release.yml`,
> `README.md`, `CHANGELOG.md`, `crates/makina-core/tests/squash_merge.rs`,
> `crates/makina-core/tests/worktree.rs`,
> `crates/makina/tests/exchange_observability.rs`,
> `crates/makina/tests/e2e.rs`, and
> `crates/makina-acp/tests/provider_and_role_wiring.rs`.
> Line numbers are hints; locate by symbol.

## 0001 — Toolchain Pinning & MSRV

Today the repo root has no `rust-toolchain.toml` (confirmed via `ls`), and
`[workspace.package]` (`Cargo.toml:5-7`) declares `edition = "2024"` (which
needs Rust ≥ 1.85) with no `rust-version`, so a pre-1.85 toolchain fails with
a cryptic edition-2024 parse error rather than a clear MSRV message.
`README.md:43` reads `- **Rust** (edition 2024; built with 1.94) and
**git**.` while the actual dev toolchain is `1.96.1` (`rustc --version`).

**Edits:**

**Pin the toolchain (`rust-toolchain.toml`).**

```toml
# Single source of truth for the toolchain CI and contributors use.
[toolchain]
channel    = "1.96.1"
components = ["rustfmt", "clippy"]
```

**Declare the MSRV floor (`Cargo.toml` + each crate).** Add
`rust-version = "1.85"` to `[workspace.package]` (the edition-2024 floor) and
`rust-version.workspace = true` to each crate's `[package]` so `cargo`
reports a clear "requires rustc 1.85" error on older toolchains.

```toml
# [workspace.package]
rust-version = "1.85"   # edition-2024 floor; pin (rust-toolchain.toml) is 1.96.1
```

**Correct the README (`README.md:43`).** Replace "built with 1.94" with
"MSRV 1.85; pinned to 1.96.1 via `rust-toolchain.toml`".

**Properties that make this safe:**

- The pin lives in exactly one file (`rustup show` honours it).
- The MSRV `rust-version` is orthogonal (a floor, not a pin) and does not
  change what CI installs.
- The README then matches both.

## 0002 — GitHub Actions CI

Today `.github/workflows/` contains only `cla.yml` (confirmed via `ls`), so
the three gates pinned in `.makina/config.toml:51-61` (`cargo test`,
`cargo clippy -- -D warnings`, `cargo fmt --check`) never run on push/PR;
`develop` went red twice during the 0044 cycle without CI catching it
(commit `6006c85`).

**Edits:**

**Add the workflow (`.github/workflows/ci.yml`).**

```yaml
name: CI
on:
  push:
    branches: [develop]
  pull_request:
    branches: [develop]
concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true
jobs:
  gates:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: rustup show            # installs the rust-toolchain.toml pin
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test
```

**Properties that make this safe:**

- CI installs whatever the pin (WS0001) says, so the version lives in one
  place.
- The three steps are the project gates (broadened to `--all-targets`,
  matching every plan's Conventions block).
- The only test needing a live agent is `#[ignore]`d, so `cargo test` is
  agent-free by construction.

## 0003 — Test Hermeticity

Today ~23 helper definitions (`grep fn setup_temp_repo`/`fn run_git`) run
bare `git init`/`git commit` inheriting host global config; `setup_temp_repo`
(`crates/makina-core/tests/squash_merge.rs:48-55`) sets
`user.email`/`user.name` but never `commit.gpgsign`. Only
`exchange_observability.rs:145` and `e2e.rs:203` disable signing, so under
`commit.gpgsign=true` every other temp-repo commit fails. Process-wide `HOME`
mutation is ALREADY serialized by `HOME_ENV_LOCK`
(`crates/makina-core/src/lib.rs:49`), so this workstream only shields against
global git config and proves it in CI.

**Edits:**

**Add a hermetic helper (`crates/makina-core/src/test_support.rs`),
feature-gated `test-support`.**

```rust
/// Run `git <args>` in `dir` with global/system config neutralized; assert success.
pub fn run_git(dir: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new("git").args(args).current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output().unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    assert!(out.status.success(), "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr));
    out
}
/// Temp repo on `develop` with one commit; local commit.gpgsign=false shields
/// engine-spawned git (which is never env-neutralized) against a poisoned host.
pub fn setup_temp_repo() -> tempfile::TempDir { /* init, identity, gpgsign=false, commit, rename to develop */ }
```

**Wire Cargo.** Promote `tempfile` to an optional `[dependencies]` of
makina-core, add `test-support = ["dep:tempfile"]`, a self dev-dependency
enabling the feature, and enable it from `makina`/`makina-acp`
`[dev-dependencies]`.

**Migrate every temp-repo helper** to
`makina_core::test_support::{setup_temp_repo, run_git}` — deleting the ~23
duplicated defs — while leaving production identity config alone.

**Poison the runner in CI.** Add a step before `cargo test` that sets
`commit.gpgsign=true` + `init.defaultBranch=main` globally.

**Properties that make this safe:**

- The repo-local `commit.gpgsign=false` beats a poisoned global for both
  helper- and engine-spawned git.
- Production git identity is untouched.
- The canary turns implicit hermeticity into an enforced gate.
- `HOME_ENV_LOCK` already handles the HOME race, so this plan does not
  re-serialize HOME.

## 0004 — Versioning & Release Automation

Today `[workspace.package]` (`Cargo.toml:5-11`) declares only
`version`/`edition`/`authors`/`license` — no
`repository`/`homepage`/`keywords`/`categories`; the repo has zero git tags
(`git tag` empty), no `CHANGELOG.md`, and no release workflow.

**Edits:**

**Add metadata (`Cargo.toml` + each crate).**

```toml
# [workspace.package]
description = "Makina — a multi-agent software-factory orchestrator (ratatui TUI)"
repository  = "https://github.com/koraytaylan/makina"
homepage    = "https://github.com/koraytaylan/makina"
keywords    = ["cli", "tui", "agent", "orchestrator", "automation"]
categories  = ["command-line-utilities", "development-tools"]
```

Each crate opts in with `repository.workspace = true` /
`homepage.workspace = true` / `keywords.workspace = true` /
`categories.workspace = true` (keeping its own `description`).

**Seed `CHANGELOG.md`** in Keep-a-Changelog form with a `## [0.1.0]` entry
summarizing the plan-0001..0044 milestones.

**Add a release workflow (`.github/workflows/release.yml`)** — on `push`
tags `v*`, a matrix build (`ubuntu-latest`/x86_64-linux,
`macos-latest`/aarch64-darwin) running `cargo build --release --locked` and
uploading a tarball via `softprops/action-gh-release@v2`.

**Tag `v0.1.0`** (annotated) at the green develop HEAD once CI + canary are
green.

**Properties that make this safe:**

- Metadata is inherited via `.workspace = true` (one edit point).
- The release workflow only fires on `v*` tags.
- The tag is the final, gated step so it never points at a red commit.

## Test strategy

- **0001 (toolchain/MSRV).** No new unit test; verified by `rustup show`
  selecting 1.96.1 and `cargo metadata --no-deps` reporting
  `rust_version = 1.85` for all three crates, plus a full green gate run
  under the pin.
- **0002 (CI).** Verified by the workflow itself passing on its first
  push/PR to `develop`; the three steps are byte-equal to the project gates
  broadened to `--all-targets`.
- **0003 (hermeticity).** In `crates/makina-core/src/test_support.rs`:
  `run_git_panics_on_nonzero_exit` (`#[should_panic]`) proves loud failure;
  `setup_temp_repo_is_on_develop_with_initial_commit` pins the
  `develop`+initial-commit parity of the replaced helpers;
  `temp_repo_commits_despite_poisoned_global_config` writes a `gpgsign=true`
  global config for a child git and asserts the repo-local
  `commit.gpgsign=false` shield still commits (red against a naive helper,
  green with this one). The CI canary (`ci-hermeticity-canary`) then runs the
  whole suite under a poisoned runner global config.
- **0004 (release).** No unit test; verified by `cargo metadata` showing the
  repository URL, valid `CHANGELOG.md`/`release.yml`, and (GATED) the
  `v0.1.0` tag producing linux+macos artifacts.
- All tasks keep `cargo test`, `cargo clippy --all-targets -- -D warnings`,
  and `cargo fmt --check` green.

## Interaction with prior work

- **0016 — CI & Test Hermeticity (never implemented).** This plan realizes
  0016's intent: its WS0052 CI+toolchain-pin becomes WS0001/WS0002 here, and
  its WS0053 shared hermetic helper + poison canary becomes WS0003 — updated
  for today's codebase, where `HOME_ENV_LOCK` (lib.rs:49) already exists so
  only the git-config shield remains.
- **0044 — Agent Coverage & Compatibility.** The concrete failures this plan
  prevents are the two that reddened `develop` during 0044 (a clippy 1.96
  lint and a `persist.rs` gitignore test) and the HOME-race flake patched in
  commit 6006c85 (whose message defers the systemic hermeticity fix "to the
  CI plan" — this one).
- **0043 — First-Run Config UX.** The shipped `.makina/config.toml` stays
  backend-free and unchanged; CI reads its `[[gates]]` (lines 51-61) only as
  the source of the gate commands it mirrors.
