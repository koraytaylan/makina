# Scope — Plan 0045

> Give Makina the standard hygiene an outside adopter expects: a pinned toolchain with an enforced MSRV floor, a GitHub Actions workflow that runs the project's three gates on every push and PR to develop, a test suite hermetic against host git config (so `cargo test` passes under global commit signing), and a tagged, changelogged, release-automated v0.1.0.

## Why this plan

**1. No toolchain pin and no enforced MSRV floor.** The repo root has no `rust-toolchain.toml` (confirmed via `ls`), and `[workspace.package]` (`Cargo.toml:5-7`) sets `edition = "2024"` — which requires Rust ≥ 1.85 — with no `rust-version`, so a pre-1.85 toolchain fails with a cryptic edition-2024 parse error instead of a clear MSRV message. `README.md:43` claims `built with 1.94` while the actual dev toolchain is `1.96.1` (`rustc --version`), and a clippy 1.96 lint slipped through during the 0044 cycle (commit `6006c85`).

**2. Nothing runs the project's three gates on push or PR.** `.github/workflows/` contains only `cla.yml` (confirmed via `ls`), so the three gates the project pins in `.makina/config.toml:51-61` (`cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`) never run automatically; `develop` went red twice during 0044 — a clippy 1.96 `sort_by_key` lint and a `persist.rs` gitignore test — with no CI to catch it before merge (commit `6006c85`).

**3. The temp-repo test helpers are not hermetic against host git config.** ~23 helper definitions (`grep fn setup_temp_repo`/`fn run_git`) run bare `git init`/`git commit` inheriting the host's *global* git config; e.g. `setup_temp_repo` (`crates/makina-core/tests/squash_merge.rs:48-55`) sets `user.email`/`user.name` but never `commit.gpgsign`. Only `exchange_observability.rs:145` and `e2e.rs:203` disable signing, so on a machine with `commit.gpgsign = true` every other temp-repo commit tries to sign and `cargo test` fails. (Process-wide `HOME` mutation is already serialized by `HOME_ENV_LOCK` — `crates/makina-core/src/lib.rs:49` — so only the git-config shield and its CI proof remain.)

**4. No release metadata, no tag, no changelog, no release automation.** `[workspace.package]` (`Cargo.toml:5-11`) carries no `repository`/`homepage`/`keywords`/`categories`, the repo has zero git tags (`git tag` returns empty), there is no `CHANGELOG.md`, and no workflow builds binaries — so the shipped `0.1.0` version is untagged, unreleased, and unpublishable-metadata-wise.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0001–0004):

- **0001 — Toolchain Pinning & MSRV.** Add a repo-root `rust-toolchain.toml` pinning channel `1.96.1` with the `rustfmt`+`clippy` components, add `rust-version = "1.85"` to `[workspace.package]` (inherited by each crate via `rust-version.workspace = true`) so a pre-1.85 toolchain emits a clear MSRV error instead of a cryptic edition-2024 parse failure, and correct `README.md:43`'s stale "built with 1.94" to state the pinned toolchain and the 1.85 MSRV.
- **0002 — GitHub Actions CI.** Add `.github/workflows/ci.yml` that, on every push and pull_request targeting `develop`, checks out the repo, runs `rustup show` (installing the `rust-toolchain.toml` pin), caches builds with `Swatinem/rust-cache@v2`, and runs the project's three gates — `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` — so a red develop is caught before merge (the `#[ignore]`d real-agent e2e keeps CI agent-free).
- **0003 — Test Hermeticity.** Add one shared `makina_core::test_support` helper (`run_git`/`setup_temp_repo`) that neutralizes global/system git config on every spawned git AND writes a repo-local `commit.gpgsign=false` shield, migrate the ~23 duplicated temp-repo helpers across the test suites to it (leaving production git identity — `folder_init` bootstrap, the Developer actor — untouched), and add a CI step that poisons the runner's global git config so a hermeticity regression fails loudly.
- **0004 — Versioning & Release Automation.** Fill `[workspace.package]` crates.io metadata (`repository`/`homepage`/`keywords`/`categories`, inherited per-crate via `.workspace = true`), seed a `CHANGELOG.md` from plan history with a `0.1.0` entry, add a tag-triggered (`v*`) GitHub Actions release workflow that builds linux and macos binaries, and — gated on a green develop — create and push the annotated `v0.1.0` tag (publishing to crates.io deferred).

## Origin -> workstream mapping

| Finding | Addressed by |
|---|---|
| 1 — No toolchain pin / no MSRV floor; README claims "built with 1.94" (`README.md:43`; `Cargo.toml:5-7`). | `0001` |
| 2 — No CI runs the project gates on push/PR; only cla.yml exists; develop went red twice during 0044 (`.github/workflows/cla.yml`; `.makina/config.toml:51-61`; commit `6006c85`). | `0002` |
| 3 — Temp-repo test helpers inherit host git config and fail under commit.gpgsign=true; only 2 of ~25 sites disable signing (`squash_merge.rs:48-55`; `exchange_observability.rs:145`; `e2e.rs:203`). | `0003` |
| 4 — No crates.io metadata, no git tag, no CHANGELOG, no release automation (`Cargo.toml:5-11`; `git tag` empty). | `0004` |

## Locked decisions

- **CI mirrors the project's own three gates, broadened to --all-targets.** The workflow steps are `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` — the gates pinned in `.makina/config.toml:51-61`, broadened to `--all-targets` to match every plan's Conventions block. A PR that CI passes clears the same bar Makina imposes on its own agent-produced tasks. The `#[ignore]`d real-agent e2e keeps CI agent-free. Revisit only if a real-agent CI harness is added.
- **Toolchain pinned via rust-toolchain.toml (1.96.1); MSRV declared separately (1.85).** `rust-toolchain.toml` pins `channel = "1.96.1"` (the current dev toolchain; `rustc --version`) with `rustfmt`+`clippy` so CI installs exactly one version via `rustup show`. `rust-version = "1.85"` on `[workspace.package]` is the orthogonal edition-2024 floor (a clear MSRV error, not a pin). The two are independent: bumping the pin does not lower the floor.
- **Hermetic = neutralize global/system git config AND a repo-local commit.gpgsign=false shield; HOME is already serialized.** Helper-spawned git gets `GIT_CONFIG_GLOBAL=/dev/null` + `GIT_CONFIG_SYSTEM=/dev/null`; the temp repo additionally gets local `commit.gpgsign=false` so engine-spawned git (which must honour real user config and is never env-neutralized) is shielded by the repo itself. Process-wide `HOME` mutation is already serialized by `HOME_ENV_LOCK` (`crates/makina-core/src/lib.rs:49`), so this plan does NOT re-serialize HOME — it only completes the git-config shield and proves it in CI.
- **CI poisons, not just neutralizes, the runner's git config.** After the helpers are hermetic, CI sets `commit.gpgsign=true` + `init.defaultBranch=main` in the runner's global config before testing. A clean runner cannot catch a hermeticity regression; a poisoned one fails it on the spot, converting the implicit shield into an enforced gate.
- **Release workflow is a hand-rolled linux/macOS matrix; full cargo-dist and crates.io publishing are deferred.** cargo-dist's `dist init` cannot run in the sandbox, so `release.yml` is a hand-written `v*`-tag matrix (x86_64-linux, aarch64-darwin) that builds `-p makina` and attaches tarballs via `softprops/action-gh-release@v2`. Adopting cargo-dist proper and publishing crates to crates.io are explicitly deferred; the workspace metadata is added now so publishing is a later config-only step.
- **v0.1.0 is tagged only after CI (gates + canary) is green on develop.** The annotated tag is the final GATED step; it must point at a develop commit where the gate job and the hermeticity canary are both green and the metadata/changelog/release workflow are present. If any precondition fails, the tag is removed and the blocker recorded in STATUS.md rather than leaving a broken release.

## Out of scope

- Publishing crates to crates.io (`cargo publish`). The plan adds publish metadata and a release workflow but deliberately defers actual publication; crates.io publishing is a separate, credential-bearing step.
- Re-serializing process-wide HOME mutation. `HOME_ENV_LOCK` (`crates/makina-core/src/lib.rs:49`) already serializes every set_var("HOME") test site; the `6006c85` fix pinned folder_init's bootstrap identity. Only the git-config shield and its CI proof remain.
- Running the `#[ignore]`d real-agent e2e in CI. That test needs a live, authenticated agent CLI; CI stays agent-free by construction, exactly as the README promises.
- Windows release binaries and macOS signing/notarization. The release workflow targets linux + macos (aarch64) unsigned tarballs; codesigning, notarization, and a Windows target are follow-on packaging concerns.
- Adopting cargo-dist proper via `dist init`. cargo-dist's initializer cannot run in the sandbox; the hand-rolled matrix delivers the same linux/macos-on-tag outcome, and full cargo-dist adoption is deferred.
- Rewriting the README Limitations section and other stale docs. That was plan 0016's WS0054 (a broader docs-freshness concern); this plan only corrects the single toolchain claim on `README.md:43`.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
