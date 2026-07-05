# XAgent Plan 0045 — CI & Release Hygiene — Green Develop, Hermetic Tests, and a Tagged v0.1.0

This plan closes the never-implemented 0016-CI-and-Test-Hermeticity intent and the concrete failures found during 0044: it adds `rust-toolchain.toml` (channel 1.96.1 + rustfmt/clippy) and a `rust-version = "1.85"` MSRV floor so pre-1.85 toolchains fail with a clear message instead of a cryptic edition-2024 parse error, and corrects README's stale "built with 1.94"; it adds `.github/workflows/ci.yml` running `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` on every push/PR to develop so a red develop is caught before merge; it introduces one shared hermetic `test_support` helper (global/system git config neutralized + local `commit.gpgsign=false` shield), migrates the ~23 duplicated temp-repo helpers to it, and adds a CI canary that poisons the runner's global git config to prove hermeticity; and it fills `[workspace.package]` crates.io metadata, seeds `CHANGELOG.md` from plan history, adds a tag-triggered linux/macos release workflow, and tags a green v0.1.0.

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

## 0001 — Toolchain Pinning & MSRV

### pin-rust-toolchain — Pin The Toolchain In rust-toolchain.toml

The repo root has no `rust-toolchain.toml` (confirmed via `ls`), so every contributor and CI runner uses whatever toolchain happens to be installed. The actual dev toolchain is `1.96.1` (`rustc --version`), and a clippy 1.96 lint slipped through during the 0044 cycle (commit `6006c85`). Pinning gives CI and contributors one authoritative toolchain.

**Steps:**

1. Create `rust-toolchain.toml` at the repo root with exactly:

   ```toml
   # Single source of truth for the toolchain CI (via `rustup show`) and
   # contributors use. 1.96.1 is the current dev toolchain (rustc --version);
   # comfortably above the edition-2024 / MSRV 1.85 floor.
   [toolchain]
   channel    = "1.96.1"
   components = ["rustfmt", "clippy"]
   ```

2. Run `rustup show` and confirm it selects `1.96.1` (installing it if needed).

3. Run the full gate set to confirm the workspace still builds and passes under the pinned toolchain.

- **Depends on:** —
- **Done when:** `rust-toolchain.toml` exists at the repo root with `channel = "1.96.1"` and the `rustfmt`+`clippy` components; `rustup show` reports `1.96.1` as active; cargo test / cargo clippy --all-targets -- -D warnings / cargo fmt --check all green.

---

### add-workspace-msrv — Declare The MSRV Floor And Correct The README

`[workspace.package]` (`Cargo.toml:5-7`) sets `edition = "2024"` (which requires Rust ≥ 1.85) but declares no `rust-version`, so a pre-1.85 toolchain fails with a cryptic edition-2024 parse error instead of a clear MSRV message. `README.md:43` reads `- **Rust** (edition 2024; built with 1.94) and **git**.` — stale, since the dev toolchain is 1.96.1. This task adds the MSRV floor and fixes the README; it does not change what CI installs (that is the pin in `pin-rust-toolchain`).

**Steps:**

1. In `Cargo.toml`, add `rust-version = "1.85"` to the `[workspace.package]` table (`Cargo.toml:5-11`), with a comment: `# edition-2024 floor; the toolchain is pinned separately in rust-toolchain.toml`.

2. In each crate manifest — `crates/makina/Cargo.toml`, `crates/makina-core/Cargo.toml`, `crates/makina-acp/Cargo.toml` — add `rust-version.workspace = true` to the `[package]` table (next to the existing `version.workspace = true` line) so each crate inherits the floor.

3. In `README.md:43`, replace `- **Rust** (edition 2024; built with 1.94) and **git**.` with `- **Rust** (edition 2024; MSRV 1.85, pinned to 1.96.1 via \`rust-toolchain.toml\`) and **git**.`

4. Run `cargo metadata --format-version 1 --no-deps` and confirm each of the three crates reports `"rust_version": "1.85"`.

5. Run the full gate set.

- **Depends on:** —
- **Done when:** `[workspace.package]` declares `rust-version = "1.85"`; each crate `[package]` has `rust-version.workspace = true` and `cargo metadata` reports `rust_version = 1.85` for all three crates; `README.md:43` no longer says "built with 1.94"; cargo test/clippy/fmt green.

---

## 0002 — GitHub Actions CI

### add-ci-workflow — Run The Project's Three Gates On Push And PR

`.github/workflows/` contains only `cla.yml` (confirmed via `ls`), so the three gates pinned in `.makina/config.toml:51-61` never run automatically; `develop` went red twice during the 0044 cycle (a clippy 1.96 lint and a `persist.rs` gitignore test) with no CI to catch it (commit `6006c85`). This workflow mirrors those gates, broadened to `--all-targets`, and installs the toolchain via the pin so the version lives in one place.

**Steps:**

1. Create `.github/workflows/ci.yml` with exactly:

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

2. Do NOT hardcode a toolchain version anywhere in the file — `rustup show` reads `rust-toolchain.toml` (from `pin-rust-toolchain`).

3. Confirm no extra setup is needed: the only test requiring a live agent is `#[ignore]`d (`crates/makina/tests/e2e.rs`), so `cargo test` is agent-free by construction.

- **Depends on:** pin-rust-toolchain
- **Done when:** `.github/workflows/ci.yml` exists, triggers on push and pull_request to `develop`, references no hardcoded toolchain version (the pin is the single source), and its three check steps are exactly `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`; the YAML is valid; cargo test/clippy/fmt green.

---

## 0003 — Test Hermeticity

### shared-hermetic-git-helper — One Shared test_support Helper, Hermetic And Loud

~23 helper definitions across the suites run bare `git init`/`git commit` inheriting host global config; `setup_temp_repo` (`crates/makina-core/tests/squash_merge.rs:48-55`) sets `user.email`/`user.name` but never `commit.gpgsign`. Only `exchange_observability.rs:145` and `e2e.rs:203` disable signing, so under `commit.gpgsign=true` the rest fail. This task adds the single hermetic helper AND its own poison test that pins the fixed behavior — it is the baseline the migration and CI canary build on. (`HOME_ENV_LOCK` at `crates/makina-core/src/lib.rs:49` already serializes HOME, so this helper only shields against git config.)

**Steps:**

1. Create `crates/makina-core/src/test_support.rs`:

   ```rust
   //! Hermetic git test helpers shared across the workspace test suites.
   //! Neutralizes global/system git config on every spawned git AND writes a
   //! repo-local `commit.gpgsign=false` shield, so engine-spawned git (which must
   //! honour real user config and is never env-neutralized) is shielded by the
   //! repo itself even on a host with `commit.gpgsign = true`.
   use std::path::Path;
   use std::process::{Command, Output};

   /// Run `git <args>` in `dir` with global/system config neutralized; assert success.
   pub fn run_git(dir: &Path, args: &[&str]) -> Output {
       let out = Command::new("git").args(args).current_dir(dir)
           .env("GIT_CONFIG_GLOBAL", "/dev/null")
           .env("GIT_CONFIG_SYSTEM", "/dev/null")
           .output().unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
       assert!(out.status.success(), "git {args:?} failed: {}",
           String::from_utf8_lossy(&out.stderr));
       out
   }

   /// Temp git repo on `develop` with one initial commit, hermetic against host
   /// git config. Keep the returned TempDir alive.
   pub fn setup_temp_repo() -> tempfile::TempDir {
       let dir = tempfile::tempdir().expect("create temp dir");
       let path = dir.path();
       run_git(path, &["init"]);
       run_git(path, &["config", "user.email", "test@example.com"]);
       run_git(path, &["config", "user.name", "Test User"]);
       run_git(path, &["config", "commit.gpgsign", "false"]);
       run_git(path, &["commit", "--allow-empty", "-m", "Initial commit"]);
       let head = run_git(path, &["rev-parse", "--abbrev-ref", "HEAD"]);
       let current = String::from_utf8_lossy(&head.stdout).trim().to_string();
       if current != "develop" {
           run_git(path, &["branch", "-m", &current, "develop"]);
       }
       dir
   }
   ```

2. In `crates/makina-core/src/lib.rs`, declare the module near the other `pub mod` lines: `#[cfg(any(test, feature = "test-support"))] pub mod test_support;`.

3. In `crates/makina-core/Cargo.toml`: move `tempfile` from `[dev-dependencies]` (line 31) to `[dependencies]` as `tempfile = { version = "3", optional = true }`; add a `[features]` table with `test-support = ["dep:tempfile"]`; and add a self dev-dependency `makina-core = { path = ".", features = ["test-support"] }` so makina-core's own `tests/*.rs` see the module.

4. Append the embedded probe tests to `test_support.rs`:

   ```rust
   #[cfg(test)]
   mod tests {
       use super::*;
       #[test]
       #[should_panic(expected = "failed")]
       fn run_git_panics_on_nonzero_exit() {
           let dir = tempfile::tempdir().unwrap();
           run_git(dir.path(), &["init"]);
           run_git(dir.path(), &["definitely-not-a-git-subcommand"]);
       }
       #[test]
       fn setup_temp_repo_is_on_develop_with_initial_commit() {
           let repo = setup_temp_repo();
           let br = run_git(repo.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);
           assert_eq!(String::from_utf8_lossy(&br.stdout).trim(), "develop");
           let log = run_git(repo.path(), &["log", "--oneline"]);
           assert!(String::from_utf8_lossy(&log.stdout).contains("Initial commit"));
       }
       #[test]
       fn temp_repo_commits_despite_poisoned_global_config() {
           // Poison THIS child git's global config with gpgsign=true (no signing key
           // configured). The repo-local commit.gpgsign=false shield must still win.
           let poison = tempfile::tempdir().unwrap();
           let cfg = poison.path().join("gitconfig");
           std::fs::write(&cfg, "[commit]\n\tgpgsign = true\n").unwrap();
           let repo = setup_temp_repo();
           let out = Command::new("git")
               .args(["commit", "--allow-empty", "-m", "second"])
               .current_dir(repo.path())
               .env("GIT_CONFIG_GLOBAL", &cfg)
               .env("GIT_CONFIG_SYSTEM", "/dev/null")
               .output().unwrap();
           assert!(out.status.success(),
               "local commit.gpgsign=false must shield against poisoned global: {}",
               String::from_utf8_lossy(&out.stderr));
       }
   }
   ```

5. Run the full gate set.

- **Depends on:** add-workspace-msrv
- **Done when:** `crates/makina-core/src/test_support.rs` exports `run_git`/`setup_temp_repo`; the module is behind the `test-support` feature and reachable from makina-core's own tests; all three embedded tests pass — `temp_repo_commits_despite_poisoned_global_config` is red against a naive helper (no `commit.gpgsign=false`) and green with this one; cargo test/clippy/fmt green.

---

### migrate-git-helpers — Adopt The Shared Hermetic Helper At Every Temp-Repo Site

With the hermetic helper in place, the ~23 duplicated temp-repo helper definitions (`grep -rn 'fn setup_temp_repo'`/`'fn run_git'` under `crates/`) must adopt it so every temp-repo commit is shielded. Sites that need commit stdout call `String::from_utf8_lossy(&run_git(..).stdout)`. Production git identity — `folder_init`'s bootstrap commit (`crates/makina/src/folder_init.rs`, already env-pinned in `6006c85`) and the Developer actor's commit identity — is NOT a test helper and must be left untouched.

**Steps:**

1. Enable the feature on the makina-core dev-dependency in `crates/makina/Cargo.toml` (line 24 region) and `crates/makina-acp/Cargo.toml` (line 18 region): add `makina-core = { workspace = true, features = ["test-support"] }` under `[dev-dependencies]`.

2. Replace each in-file `fn setup_temp_repo`/`fn run_git`/`fn init_git_repo`/`fn git` definition inside a `#[cfg(test)]` module with `use makina_core::test_support::{setup_temp_repo, run_git};` at these confirmed sites: the `crates/makina-core/tests/*.rs` files (`concurrency.rs`, `continue_on_failure.rs`, `develop_review_loop.rs`, `gate_runner.rs`, `normalizer_integration.rs`, `orchestrator_read_path.rs`, `per_task_logs.rs`, `plan_branch.rs`, `run_metadata.rs`, `squash_merge.rs`, `supervisor_audit_registry.rs`, `supervisor_tracing_transitions.rs`, `supervisor_write_path.rs`, `termination_caps.rs`, `transcript_persistence.rs`, `worktree.rs`), the in-crate `#[cfg(test)]` mods of `crates/makina-core/src/{worktree.rs,merge.rs,orchestrator.rs,actors/developer.rs}`, and the makina/makina-acp integration tests (`crates/makina/tests/exchange_observability.rs`, `crates/makina-acp/tests/provider_and_role_wiring.rs`).

3. For the e2e clone-based helper (`crates/makina/tests/e2e.rs`) — a different shape (clone of the live repo) — leave its structure but ensure its spawned git carries `.env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_SYSTEM", "/dev/null")` and keeps the existing local `commit.gpgsign=false` (`e2e.rs:203`).

4. Do NOT alter production identity config: leave `crates/makina/src/folder_init.rs` (the `GIT_AUTHOR_*`/`GIT_COMMITTER_*` env pin and its `user.email` fallback at lines 29-46) and any Developer-actor commit identity untouched — only `#[cfg(test)]` helpers are migrated.

5. Run the full suite to confirm this is a pure refactor (every migrated helper still yields a `develop` branch with an initial commit).

- **Depends on:** shared-hermetic-git-helper
- **Done when:** `grep -rn 'user.email' crates/` shows temp-repo identity only inside `test_support.rs`, the e2e clone helper, and production identity paths (`folder_init.rs`, Developer actor); no `#[cfg(test)]` module defines its own `setup_temp_repo`/`run_git`; every temp-repo git invocation in test setup asserts success and disables signing via the shared helper; cargo test/clippy/fmt green.

---

### ci-hermeticity-canary — Poison The Runner's Git Config In CI

A clean CI runner has `commit.gpgsign` off by default, so it cannot catch a hermeticity regression — a reverted shield would still pass. Once the shared helper is adopted everywhere (`migrate-git-helpers`) and CI exists (`add-ci-workflow`), poison the runner's global git config before the tests so any non-hermetic temp-repo commit fails immediately. This edits the same `.github/workflows/ci.yml` as `add-ci-workflow`, so it depends on it.

**Steps:**

1. In `.github/workflows/ci.yml`, add a step immediately after `Swatinem/rust-cache@v2` and before `cargo fmt --check`:

   ```yaml
         - name: Poison global git config (hermeticity canary)
           run: |
             git config --global commit.gpgsign true
             git config --global init.defaultBranch main
   ```

   No signing key is configured on the runner, so any temp-repo commit that is NOT locally shielded fails immediately; setting `init.defaultBranch=main` keeps the rename-to-`develop` guard honest.

2. Confirm the full suite still passes under the poison — the hermetic helper's local `commit.gpgsign=false` shield (from `shared-hermetic-git-helper`) makes the poison invisible to the tests.

3. Sanity-check the canary bites: temporarily reverting one migrated helper to a non-shielded form makes CI fail at that helper's first temp-repo commit (do not commit the revert).

- **Depends on:** add-ci-workflow, migrate-git-helpers
- **Done when:** `.github/workflows/ci.yml` sets `commit.gpgsign=true` + `init.defaultBranch=main` in the runner's global config before the gate steps; the full suite passes green WITH the poison active; reverting any migrated helper to a non-hermetic form fails CI at the broken git step; cargo test/clippy/fmt green.

---

## 0004 — Versioning & Release Automation

### add-changelog — Seed CHANGELOG.md From Plan History

The repo has no `CHANGELOG.md`, so an adopter has no human-readable record of what shipped in `0.1.0`. Seed a Keep-a-Changelog file with a single `0.1.0` entry summarizing the plan-0001..0044 milestones (from `docs/plans/*/`). Documentation-only.

**Steps:**

1. Create `CHANGELOG.md` at the repo root in Keep-a-Changelog 1.1.0 format: an intro line, an `## [Unreleased]` section (empty), and a `## [0.1.0] - 2026-07-05` section.

2. Under `0.1.0`, add `### Added` bullets summarizing the shipped feature set drawn from `docs/plans/*/SCOPE.md` (e.g. the actor/state-machine orchestration engine, ACP agent backend, ratatui TUI with sidebar/detail/exchange panes, provider/role config, worktree isolation and squash-merge, plan auto-discovery and generated tasks, ayu theming, first-run config UX, and the 0044 agent-coverage/compatibility work).

3. Add reference links at the bottom (`[Unreleased]`, `[0.1.0]`) pointing at `https://github.com/koraytaylan/makina`.

4. Confirm the file is valid Markdown and mentions version `0.1.0`.

- **Depends on:** —
- **Done when:** Documentation-only (no runtime surface). `CHANGELOG.md` exists at the repo root in Keep-a-Changelog form with an `## [Unreleased]` section and a `## [0.1.0]` section listing the shipped feature set; cargo test/clippy/fmt green (docs-only change — the suite simply still passes).

---

### add-workspace-metadata — Add crates.io Publish Metadata To The Workspace

`[workspace.package]` (`Cargo.toml:5-11`) carries no `repository`/`homepage`/`keywords`/`categories`, so the manifests advertise no project URL or discovery metadata. This task adds workspace-level metadata and has each crate inherit it via `.workspace = true` (each crate keeps its own `description`). It edits the same `Cargo.toml` as `add-workspace-msrv`, so it depends on that task to serialize the file.

**Steps:**

1. In `Cargo.toml` `[workspace.package]`, add: `description = "Makina — a multi-agent software-factory orchestrator (ratatui TUI)"`, `repository = "https://github.com/koraytaylan/makina"`, `homepage = "https://github.com/koraytaylan/makina"`, `keywords = ["cli", "tui", "agent", "orchestrator", "automation"]`, and `categories = ["command-line-utilities", "development-tools"]` (each keyword ≤ 20 chars, ≤ 5 keywords — crates.io limits).

2. In each crate `[package]` (`crates/makina/Cargo.toml`, `crates/makina-core/Cargo.toml`, `crates/makina-acp/Cargo.toml`), add `repository.workspace = true`, `homepage.workspace = true`, `keywords.workspace = true`, and `categories.workspace = true`. Leave each crate's existing `description = "…"` line as-is (it overrides the workspace default per-crate).

3. Run `cargo metadata --format-version 1 --no-deps` and confirm each crate reports the `repository` URL.

4. Run the full gate set.

- **Depends on:** add-workspace-msrv, shared-hermetic-git-helper, migrate-git-helpers
- **Done when:** `[workspace.package]` declares `repository`, `homepage`, `keywords`, and `categories`; each crate inherits `repository`/`homepage`/`keywords`/`categories` via `.workspace = true`; `cargo metadata` shows the repository URL for all three crates; cargo test/clippy/fmt green.

---

### add-release-workflow — Add A Tag-Triggered Linux/macOS Release Workflow

There is no workflow that builds distributable binaries, so a tag pushed today produces no artifacts. Add a `push`-on-`v*`-tags workflow that builds a linux and a macos `makina` binary and attaches them to the GitHub release. cargo-dist's `dist init` cannot run in this sandbox, so the workflow is hand-rolled (a locked decision records that full cargo-dist adoption is deferred).

**Steps:**

1. Create `.github/workflows/release.yml` with exactly:

   ```yaml
   name: Release
   on:
     push:
       tags: ["v*"]
   permissions:
     contents: write
   jobs:
     build:
       strategy:
         matrix:
           include:
             - os: ubuntu-latest
               target: x86_64-unknown-linux-gnu
             - os: macos-latest
               target: aarch64-apple-darwin
       runs-on: ${{ matrix.os }}
       steps:
         - uses: actions/checkout@v4
         - run: rustup show
         - run: rustup target add ${{ matrix.target }}
         - run: cargo build --release --locked --target ${{ matrix.target }} -p makina
         - name: Package
           run: tar -czf makina-${{ matrix.target }}.tar.gz -C target/${{ matrix.target }}/release makina
         - uses: softprops/action-gh-release@v2
           with:
             files: makina-${{ matrix.target }}.tar.gz
   ```

2. Confirm `Cargo.lock` is committed at the repo root (the `--locked` flag requires it); if it is not, this task must not add `--locked` — instead drop the flag and note it.

3. Validate the YAML is well-formed and that the binary target is `-p makina` (the TUI binary crate).

- **Depends on:** —
- **Done when:** `.github/workflows/release.yml` exists, triggers only on `v*` tag pushes, builds `-p makina` for both `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin`, and uploads a per-target tarball via `softprops/action-gh-release@v2`; the YAML is valid; cargo test/clippy/fmt green.

---

### tag-v0-1-0 — Create And Push The Annotated v0.1.0 Tag (GATED)

**Gate:** this task runs ONLY after `add-ci-workflow`, `ci-hermeticity-canary`, `add-workspace-metadata`, `add-changelog`, and `add-release-workflow` have landed and CI is green on `develop`. Tagging a red or metadata-incomplete commit would produce a broken release, so the tag is the final, conditional step. The repo currently has zero tags (`git tag` empty); `0.1.0` is the version in `Cargo.toml:6`.

**Steps:**

1. Confirm the CI workflow is green on the tip of `develop` (both the gate job and the hermeticity canary) and that `CHANGELOG.md`, the workspace metadata, and `release.yml` are present on that commit.

2. In `CHANGELOG.md`, ensure the `## [0.1.0]` section carries the release date (e.g. `## [0.1.0] - 2026-07-05`) matching the tag date.

3. Create the annotated tag at the green develop commit: `git tag -a v0.1.0 -m "Makina v0.1.0"` and push it (`git push origin v0.1.0`), which triggers `release.yml`.

4. If any gate is red, the metadata/changelog is incomplete, or the release build fails: delete the tag (`git tag -d v0.1.0` and, if pushed, `git push origin :refs/tags/v0.1.0`) and record the blocker in this plan's STATUS.md instead of leaving a broken tag.

- **Depends on:** add-ci-workflow, ci-hermeticity-canary, add-workspace-metadata, add-changelog, add-release-workflow
- **Done when:** Binary land-or-revert: EITHER an annotated `v0.1.0` tag exists pointing at a develop commit where CI (gates + canary) is green and `release.yml` produced linux+macos artifacts, with `CHANGELOG.md`'s `0.1.0` section dated; OR the tag is removed and the blocking reason is recorded in STATUS.md. cargo test/clippy/fmt green on the tagged commit.

---

**End of plan 0045 TASKS.** When every "Done when" bullet is green, every
push/PR to develop runs the project's three gates on a pinned 1.96.1 toolchain
with a 1.85 MSRV floor; the test suite is hermetic against host git config
(proven by a poison canary on the CI runner); and a metadata-complete,
changelogged v0.1.0 is tagged with a linux/macos release workflow — all with
the gate commands green.
