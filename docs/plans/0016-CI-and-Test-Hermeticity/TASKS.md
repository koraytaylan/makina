# Makina Plan 0016 — CI & Test Hermeticity

Give the repo the CI it already defines gates for (`.makina/config.toml` —
test/clippy/fmt, mirrored in a GitHub workflow with a pinned toolchain), make
every temp-repo git helper hermetic against host git config and loud on
failure, and bring README.md back in line with what the code actually does.

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

## 0052 — CI workflow + toolchain pin

### pin-rust-toolchain — Pin the toolchain in `rust-toolchain.toml`

`README.md:41` claims "built with 1.94" but nothing enforces it; the workspace
is edition 2024 / resolver 3 (`Cargo.toml:7,2`), so any pin must be ≥ 1.85.

**Steps:**

1. Add `rust-toolchain.toml` at the repo root:

   ```toml
   [toolchain]
   channel    = "1.94"
   components = ["rustfmt", "clippy"]
   ```

2. Confirm the full local gate set passes under the pinned toolchain
   (`rustup show` then `cargo test` / `cargo clippy --all-targets -- -D
   warnings` / `cargo fmt --check`).

- **Depends on:** —
- **Done when:** `rust-toolchain.toml` exists with channel 1.94 and the
  rustfmt/clippy components; the whole suite builds and passes under the pin;
  cargo test/clippy/fmt green.

### add-ci-workflow — Run the project's own gates on push/PR

`.github/workflows/` contains only `cla.yml`. The workflow mirrors the gates
in `.makina/config.toml:53–63`, broadened to `--workspace`/`--all-targets`.

**Steps:**

1. Add `.github/workflows/ci.yml` per the sketch in
   [ARCHITECTURE.md](ARCHITECTURE.md): trigger on `push` to `develop`
   (`.makina/config.toml:26`) and on `pull_request`; one `checks` job on
   `ubuntu-latest` with `actions/checkout@v4`, `rustup show` (installs the
   `rust-toolchain.toml` pin), `Swatinem/rust-cache@v2`, then the three
   steps: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test --workspace`.

2. Set `env: { GIT_CONFIG_GLOBAL: /dev/null }` at job level so the runner's
   (clean) global git config provably cannot influence tests. (Replaced by
   the poison canary in `ci-hermeticity-canary` once 0053 lands.)

3. Sanity-check that no extra setup is needed for tests: the only test
   needing a live agent is `#[ignore]`d (`e2e.rs:277`; `README.md:49`), so
   `cargo test --workspace` is agent-free by construction.

- **Depends on:** pin-rust-toolchain
- **Done when:** the workflow exists, references no hardcoded toolchain
  version (the pin is the single source), and its three check steps are
  verbatim-equivalent to the project gates broadened to workspace/all-targets;
  the workflow passes on its first push/PR; cargo test/clippy/fmt green.

---

## 0053 — Hermetic git test helpers

### shared-hermetic-git-helper — One `test_support` helper, hermetic and loud

Nineteen sites set `user.email` on a temp repo (fourteen
`crates/makina-core/tests/*.rs` copies, `orchestrator.rs:1343`,
`actors/mod.rs:119`, `exchange_observability.rs:140`, `e2e.rs:200`,
`provider_and_role_wiring.rs:53`); only two guard `commit.gpgsign`
(`e2e.rs:203`, `exchange_observability.rs:142`).

**Steps:**

1. Add `crates/makina-core/src/test_support.rs` (declared
   `#[cfg(any(test, feature = "test-support"))]` in `lib.rs`) with the
   `run_git` + `setup_temp_repo` from [ARCHITECTURE.md](ARCHITECTURE.md):
   `GIT_CONFIG_GLOBAL=/dev/null` + `GIT_CONFIG_SYSTEM=/dev/null` on every
   spawned git; **local** repo config `user.email`/`user.name`/
   `commit.gpgsign=false` (shields engine-spawned git, which is never
   env-neutralized); the existing rename-to-`develop` guard; and
   `assert!(output.status.success(), …stderr…)` on every invocation.

2. Wire Cargo: `tempfile` becomes an optional `[dependencies]` entry of
   makina-core, `test-support = ["dep:tempfile"]` in `[features]`, and a self
   dev-dependency `makina-core = { path = ".", features = ["test-support"] }`
   so makina-core's own integration tests see the module.

3. Add tests in the module:

   ```rust
   #[test]
   #[should_panic]
   fn run_git_panics_on_nonzero_exit() { /* run_git(path, &["bogus-subcommand"]) panics with stderr at the failing step */ }
   #[test]
   fn setup_temp_repo_is_on_develop_with_initial_commit() { /* parity with the replaced helpers */ }
   #[test]
   fn temp_repo_commits_despite_poisoned_global_config() { /* child git commit with GIT_CONFIG_GLOBAL→poison(gpgsign=true) on that child only succeeds: local shield wins */ }
   ```

- **Depends on:** —
- **Done when:** all three tests pass; the helper neutralizes global/system
  config, sets the local shield, and panics with stderr on any non-zero git
  exit; cargo test/clippy/fmt green.

### migrate-git-helpers — Adopt the shared helper at every site

**Steps:**

1. Enable the `test-support` feature on the makina-core entry in
   `[dev-dependencies]` of `crates/makina` and `crates/makina-acp`.

2. Replace the local helper definitions with
   `makina_core::test_support::{setup_temp_repo, run_git}` at: the fourteen
   `crates/makina-core/tests/*.rs` sites (grep `fn setup_temp_repo`), the
   in-crate mods `orchestrator.rs:1339–1371` and `actors/mod.rs:108–122`
   (`init_git_repo`), `exchange_observability.rs:124–159`, and
   `provider_and_role_wiring.rs:40–73` — the last one deletes the `run_git`
   that swallows non-zero exits (`.output().expect(...)` panics only on
   spawn failure, `provider_and_role_wiring.rs:40–46`).

3. Keep the e2e clone-based helpers (`e2e.rs:101–132` — different shape:
   clone of the live repo, already success-asserting and locally shielded at
   `e2e.rs:199–203`) but add the same two `env(...)` neutralization lines.

4. Run the full suite to confirm pure-refactor behaviour (every replaced
   helper produced a `develop` branch + initial commit, which
   `setup_temp_repo_is_on_develop_with_initial_commit` pins).

- **Depends on:** shared-hermetic-git-helper
- **Done when:** `grep -rn "user.email" crates/` finds temp-repo identity
  config only inside `test_support` and the e2e clone helper; no test file
  defines its own `setup_temp_repo`/`run_git`; every git invocation in test
  setup asserts success; cargo test/clippy/fmt green.

### ci-hermeticity-canary — Poison the runner's git config in CI

A clean runner cannot catch a hermeticity regression; a poisoned one fails it
on the spot.

**Steps:**

1. In `.github/workflows/ci.yml`, remove the job-level
   `GIT_CONFIG_GLOBAL=/dev/null` and add a step before the checks:

   ```yaml
   - run: |
       git config --global commit.gpgsign true
       git config --global init.defaultBranch main
   ```

   (No signing key is configured, so any non-hermetic temp-repo commit fails
   immediately; `main` keeps the rename-to-`develop` guard honest.)

2. Verify the workflow still passes — the hermetic helpers make the poison
   invisible to the suite.

- **Depends on:** add-ci-workflow, migrate-git-helpers
- **Done when:** CI runs the full suite green *with* the poisoned global
  config; reverting any helper to a non-hermetic form fails CI at the broken
  git step; cargo test/clippy/fmt green.

---

## 0054 — README freshness

### refresh-readme — Make README claims match the code

**Steps:**

1. Rewrite **Limitations** (`README.md:147–156`): delete the
   permission-gateway claim (implemented — `transport.rs:451–541`,
   `e2e.rs:49–67`) and the persistence claim (implemented — `persist.rs`,
   `open_run` `orchestrator.rs:659,666`, committed `.makina/tasks/*.json`).
   Replace with the actual limitations: permissive worktree-scoped permission
   policy (working-dir check only, no tool-call path validation —
   `permission.rs:77–80`; audit-logged); no idle detection below the
   wall-clock cap (`supervisor.rs:126–127`; plan 0015); provider editor is a
   read-only list (plan 0017).

2. Drop `--yolo` from the backend example (`README.md:62` →
   `args = ["--acp"]`) and note Makina's gateway answers permission requests.

3. Extend the keys list (`README.md:117–122`) with `e` (error pane,
   `event.rs:505`), `v` (dependency views, `event.rs:503`), `g`
   (provider/role editor, `event.rs:509`), `r` (reinterpret,
   `event.rs:517`) — re-verify each binding in `event.rs` before listing.

4. Add a brief `[[providers]]` / `[roles]` global-config example to the
   Configure section (plan 0011; `ProviderConfig` `config.rs:111`,
   `RolesConfig` `config.rs:158`).

5. Reframe both `trial-findings.md` links (`README.md:14–16`, `:155–156`) as
   the historical trial record — its top-two gaps (`trial-findings.md:71`,
   `:134`) are fixed — and update Requirements (`README.md:41`) to point at
   the `rust-toolchain.toml` pin.

- **Depends on:** pin-rust-toolchain
- **Done when:** every remaining README claim is backed by a current code
  symbol (spot-check the five edits above against the cited sites); no
  mention of `--yolo` as a requirement; keys/`[[providers]]` documented;
  cargo test/clippy/fmt green (docs-only change — the suite simply still
  passes).

---

**End of plan 0016 TASKS.** When every "Done when" bullet is green, every push
and PR runs the same three gates Makina itself enforces on a pinned toolchain,
the test suite passes on any machine regardless of host git config (and CI
proves it by poisoning its own), and README tells the truth.
