# Architecture — Plan 0016 (deltas)

> Edits in `.github/workflows/ci.yml` (new), `rust-toolchain.toml` (new),
> `crates/makina-core/src/test_support.rs` (new) plus the ~19 git-helper sites
> across `crates/*/tests/` and in-crate test mods, and `README.md`. Line
> numbers are hints; locate by symbol.

## 0052 — CI workflow + toolchain pin

Today `.github/workflows/` holds only `cla.yml`. The three checks below are
the project's own gates (`.makina/config.toml:53–63` — `cargo test`,
`cargo clippy -- -D warnings`, `cargo fmt --check`), broadened to
`--workspace`/`--all-targets` per the contribution-hygiene wording used in
every plan's Conventions block.

- **New `.github/workflows/ci.yml`:**

  ```yaml
  name: CI
  on:
    push:
      branches: [develop]
    pull_request:
  jobs:
    checks:
      runs-on: ubuntu-latest
      steps:
        - uses: actions/checkout@v4
        # Installs the toolchain pinned in rust-toolchain.toml (incl. rustfmt, clippy).
        - run: rustup show
        - uses: Swatinem/rust-cache@v2
        # Simulate a hostile host config so non-hermetic test helpers fail loudly
        # (see 0053; added by ci-hermeticity-canary once the helpers are hermetic).
        - run: |
            git config --global commit.gpgsign true
            git config --global init.defaultBranch main
        - run: cargo fmt --check
        - run: cargo clippy --all-targets -- -D warnings
        - run: cargo test --workspace
  ```

  `push` targets `develop` because that is the integration branch every task
  squash-merges into (`.makina/config.toml:26`); `pull_request` covers
  everything else. The real-agent e2e stays out automatically — it is
  `#[ignore]`d (`e2e.rs:277`), which is exactly the "no agent needed for CI"
  contract `README.md:49` states.

  Until 0053's canary lands, the workflow ships with
  `env: { GIT_CONFIG_GLOBAL: /dev/null }` at job level instead of the poison
  step (the suite passes on a clean runner today; the canary is flipped on by
  `ci-hermeticity-canary`).

- **New `rust-toolchain.toml`** at the repo root:

  ```toml
  [toolchain]
  channel    = "1.94"
  components = ["rustfmt", "clippy"]
  ```

  1.94 is the version `README.md:41` already claims; edition 2024 / resolver 3
  (`Cargo.toml:7,2`) require ≥ 1.85, so the pin is comfortably valid. The
  workflow contains **no** toolchain version of its own — `rustup show` reads
  the pin, so bumping the toolchain is a one-file change.

## 0053 — Hermetic git test helpers

Inventory: 19 sites configure `user.email` on a temp repo — 14 copies in
`crates/makina-core/tests/*.rs` (e.g. `develop_review_loop.rs:48–90`), the
in-crate mods `orchestrator.rs:1339–1371` and `actors/mod.rs:108–122`,
`exchange_observability.rs:124–159`, `e2e.rs:199–203`, and
`provider_and_role_wiring.rs:48–73`. Only `e2e.rs:203` and
`exchange_observability.rs:142` set `commit.gpgsign=false`; nothing
neutralizes `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`. And the makina-acp
`run_git` (`provider_and_role_wiring.rs:40–46`) ignores exit status entirely.

Edits:

- **New `crates/makina-core/src/test_support.rs`**, declared in `lib.rs` as
  `#[cfg(any(test, feature = "test-support"))] pub mod test_support;`:

  ```rust
  /// Run `git -C {path} {args}` hermetically: the host's global/system git
  /// config is neutralized, and a non-zero exit panics with the captured
  /// stderr — a broken setup fails AT the broken step, never downstream.
  pub fn run_git(path: &Path, args: &[&str]) {
      let output = std::process::Command::new("git")
          .arg("-C").arg(path)
          .args(args)
          .env("GIT_CONFIG_GLOBAL", "/dev/null")
          .env("GIT_CONFIG_SYSTEM", "/dev/null")
          .output()
          .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
      assert!(
          output.status.success(),
          "git {args:?} in {path:?} failed (code {:?}):\nstderr: {}",
          output.status.code(),
          String::from_utf8_lossy(&output.stderr),
      );
  }

  /// Minimal repo on `develop` with one commit. The LOCAL config lines shield
  /// engine-spawned git too (worktree add / Developer commit / squash-merge run
  /// WITHOUT the env neutralization — in real repos they must honour real user
  /// config — but local config beats global, so the temp repo protects itself).
  pub fn setup_temp_repo() -> tempfile::TempDir {
      let dir = tempfile::tempdir().expect("create temp dir");
      let path = dir.path();
      run_git(path, &["init"]);
      run_git(path, &["config", "user.email", "test@example.com"]);
      run_git(path, &["config", "user.name", "Test User"]);
      run_git(path, &["config", "commit.gpgsign", "false"]);
      run_git(path, &["commit", "--allow-empty", "-m", "Initial commit"]);
      // …existing branch-rename-to-develop guard, via run_git…
      dir
  }
  ```

- **Cargo wiring.** In `crates/makina-core/Cargo.toml`: promote `tempfile` to
  an optional `[dependencies]` entry, add
  `[features] test-support = ["dep:tempfile"]`, and add the self
  dev-dependency `makina-core = { path = ".", features = ["test-support"] }`
  so makina-core's *integration* tests (which build the lib without
  `cfg(test)`) see the module. makina and makina-acp add the feature to their
  `[dev-dependencies]` makina-core entry.

- **Adoption.** Replace the local `setup_temp_repo`/`run_git` definitions at
  all 14 `crates/makina-core/tests/*.rs` sites, `orchestrator.rs:1339–1371`,
  `actors/mod.rs:108–122` (`init_git_repo`), `exchange_observability.rs:124–159`,
  and `provider_and_role_wiring.rs:40–73` (this deletes the swallowing
  `run_git`) with imports of `makina_core::test_support::{setup_temp_repo,
  run_git}`. The e2e helpers (`e2e.rs:101–132`) keep their clone-based shape
  (they operate on a clone of the live repo, already assert success, and
  already set the local shield at `e2e.rs:199–203`) but gain the same two
  `env(...)` lines.

- **CI canary** (after adoption): flip `ci.yml` from
  `GIT_CONFIG_GLOBAL=/dev/null` to the poison step shown under 0052 —
  `commit.gpgsign=true` with no signing key makes any non-hermetic temp-repo
  commit fail immediately; `init.defaultBranch=main` keeps the branch-rename
  guard honest.

## 0054 — README freshness

All edits in `README.md`; verify each claim against the cited symbol before
writing the new text.

1. **Limitations (`README.md:147–156`) — rewrite to reality.** Delete the two
   false claims: the permission gateway IS implemented (transport intercepts
   `session/request_permission`, decides via policy, replies, and records an
   `AuditEntry` — `transport.rs:451–541`; "`--yolo` bypass is no longer needed
   or used" — `e2e.rs:49–67`), and `.makina/tasks/{slug}.json` persistence IS
   implemented (`persist.rs` `persist_graph`/`load_graph`; `open_run` prefers
   the artifact — `orchestrator.rs:659,666`; committed `.makina/tasks/*.json`
   ship in-repo). Replace with the *actual* MVP limitations:
   - the permission policy is a permissive worktree-scoped MVP — it keys off
     the session working dir only and does not validate tool-call paths
     (`permission.rs:77–80`); every decision is audit-logged;
   - no idle/stall detection below the wall-clock cap
     (`supervisor.rs:126–127`) until plan 0015 lands;
   - the provider editor (`g`) is a read-only list (plan 0017).
2. **Drop `--yolo`** from the global-config example (`README.md:62`):
   `args = ["--acp"]`, with a line noting permission requests are answered by
   Makina's gateway.
3. **Keys list (`README.md:117–122`)** — add the four missing bindings, each
   verified in `event.rs`: `e` error pane (`event.rs:505`), `v` dependency
   views (`event.rs:503`), `g` provider/role editor (`event.rs:509`), `r`
   reinterpret (`event.rs:517`).
4. **Configure section** — add a short `[[providers]]` / `[roles]` example for
   the global layer (plan 0011; `ProviderConfig` `config.rs:111`,
   `RolesConfig` `config.rs:158`, carried on `GlobalConfig`
   `config.rs:293,297`), pointing at `config.rs` docs for the full shape.
5. **Status blockquote (`README.md:14–16`)** — reframe the
   `docs/trial/trial-findings.md` link as the *historical* trial record (its
   top-two gaps — permission flow `trial-findings.md:71`, persistence
   `trial-findings.md:134` — are both fixed); same for the closing pointer at
   `README.md:155–156`.
6. **Requirements (`README.md:41`)** — "built with 1.94" becomes "pinned via
   `rust-toolchain.toml`" once 0052 lands, so the claim can never drift again.

## Test strategy

- `run_git_panics_on_nonzero_exit` (`#[should_panic]`, in `test_support`'s
  test mod): `run_git(path, &["bogus-subcommand"])` panics at the failing
  step with stderr in the message.
- `setup_temp_repo_is_on_develop_with_initial_commit`: behaviour parity with
  the replaced helpers (branch is `develop`, one commit exists) so adoption is
  a pure refactor.
- `temp_repo_commits_despite_poisoned_global_config`: write a poison global
  config file (`commit.gpgsign = true`) to a temp path; run a raw
  `git commit --allow-empty` **child** in a helper-created repo with
  `GIT_CONFIG_GLOBAL` pointing at the poison file *on that one child only*
  (no process-global env mutation); assert it succeeds — the repo-local
  `commit.gpgsign=false` shield wins, proving engine-spawned git is covered.
- The workflow proves itself on its first PR (fmt/clippy/test steps green),
  and the canary step turns any future non-hermetic helper into a CI failure
  rather than a contributor-machine mystery.
- 0054 is prose; the existing suite plus `cargo fmt --check` on the touched
  crates confirm nothing regressed.

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay green.

## Interaction with prior plans

- 0008 (gate sandboxing) defined the gate semantics CI now mirrors; the
  workflow runs the same three checks the gates run, so Makina-merged tasks
  and human PRs face one bar. 0011's `[[providers]]`/`[roles]` get their
  README section in 0054. 0013 (doctor) owns interactive config-error UX —
  untouched here. 0015 and 0017 are *referenced* by the new Limitations text
  but are not prerequisites. The permission-policy hardening itself is
  deferred to a future plan (0024).
