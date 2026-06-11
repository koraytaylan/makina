# Scope — Plan 0016

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Findings from the full-codebase review (2026-06-11).

**The repo defines a quality bar it never runs automatically.** The project
config pins three gates — `cargo test`, `cargo clippy -- -D warnings`,
`cargo fmt --check` (`.makina/config.toml:53–63`) — and the workspace carries
~536 test functions (`#[test]`/`#[tokio::test]`; exactly one is `#[ignore]`d —
the real-agent e2e, `e2e.rs:277`). Yet `.github/workflows/` contains only
`cla.yml`: **zero build/test CI**. Nothing runs the gates on push or PR — the
README even notes "no agent needed for CI" (`README.md:49`) for a CI that does
not exist. There is also no `rust-toolchain.toml`; `README.md:41` claims
"built with 1.94" but nothing pins it (the workspace is edition 2024 /
resolver 3 — `Cargo.toml:7,2` — which needs ≥ 1.85).

**The test suite is not hermetic against host git config.** Every temp-repo
helper runs plain `git init` → `git commit`, inheriting the **host's** global
git config. Nineteen sites set `user.email` (grep): fourteen copies across
`crates/makina-core/tests/*.rs`, plus `orchestrator.rs:1343` (in-crate test
mod), `actors/mod.rs:119`, `exchange_observability.rs:140`, `e2e.rs:200`, and
`provider_and_role_wiring.rs:53`. Only **two** of them guard `commit.gpgsign`
(`e2e.rs:203`, `exchange_observability.rs:142`); on a machine with
`commit.gpgsign = true` (a common corporate setup) every other temp-repo
commit tries to sign and **~40 tests fail**. The helpers already guard
`init.defaultBranch` (renaming the initial branch to `develop`) but nothing
else. Compounding it, the makina-acp `run_git`
(`provider_and_role_wiring.rs:40–46`) calls `.output().expect(...)` — which
panics only if the process fails to **spawn**, not on a non-zero exit — so a
broken git setup surfaces as a baffling downstream `Failed != Done` assertion
instead of failing at the broken step. (The makina-core copy does assert
`status.success()` — `orchestrator.rs:1363–1371`.)

**README.md is stale on its two headline limitations.** `README.md:149–151`
claims "Makina's ACP client does not yet answer `session/request_permission`"
and `README.md:62` recommends gemini's `--yolo` — but the permission gateway
**is** implemented: the transport answers the request via policy and records
an audit entry (`transport.rs:451–541`), and the e2e module docs state "The
`--yolo` bypass is no longer needed or used" (`e2e.rs:49–67`).
`README.md:152–153` claims `.makina/tasks/{slug}.json` persistence "is not
implemented yet" — false: `persist.rs` writes it atomically, `open_run`
prefers the persisted artifact (`orchestrator.rs:659,666`), and the repo ships
committed `.makina/tasks/*.json`. The keys list (`README.md:117–122`) omits
`e`/`v`/`g`/`r` (all bound — `event.rs:505,503,509,517`), the
`[[providers]]`/`[roles]` config from plan 0011 is undocumented, and the
trial-findings link (`README.md:14–16`) is presented as the current "known
gaps" doc although its top two gaps (`trial-findings.md:71,134`) are both
fixed.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0052–0054):

- **0052 — CI workflow + toolchain pin.** Add `.github/workflows/ci.yml`
  running `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  and `cargo test --workspace` on push/PR — the same three gates the project
  imposes on its own agent-produced tasks. Add `rust-toolchain.toml` pinning
  the toolchain the README claims.
- **0053 — Hermetic git test helpers.** One shared `setup_temp_repo`/`run_git`
  in makina-core, hermetic against host global/system git config, asserting
  success on every git invocation; adopt it everywhere; add a CI canary that
  *poisons* the runner's global config so hermeticity regressions fail loudly.
- **0054 — README freshness.** Rewrite Limitations to the *actual* current
  limitations, drop `--yolo`, document the missing keys and the
  `[[providers]]`/`[roles]` layer, reframe trial-findings as historical.

## Origin → workstream mapping

| Finding (full-codebase review, 2026-06-11) | Addressed by |
|---|---|
| Gates defined in `.makina/config.toml` but no build/test CI; only `cla.yml` exists | `0052` |
| No toolchain pin; README claims 1.94 with nothing enforcing it | `0052` |
| ~19 duplicated temp-repo git helpers inherit host git config; `commit.gpgsign=true` fails ~40 tests | `0053` |
| makina-acp `run_git` swallows non-zero git exits; failures surface downstream | `0053` |
| README Limitations claim permission gateway + persistence are unimplemented (both are implemented) | `0054` |
| README recommends `--yolo`, omits `e`/`v`/`g`/`r` keys and `[[providers]]`/`[roles]`, links trial-findings as current | `0054` |

## Locked decisions

- **CI mirrors the project's own gates.** The workflow steps are the three
  gates from `.makina/config.toml:53–63`, broadened for full coverage
  (`--workspace`, `--all-targets` — matching the contribution-hygiene wording
  in every plan's Conventions block). A task that Makina would merge and a PR
  that CI would merge pass the *same* bar. The `#[ignore]`d e2e keeps CI
  agent-free, exactly as `README.md:49` promises.
- **Pin via `rust-toolchain.toml`, not in the workflow.** `channel = "1.94"`
  (the README's claim; comfortably ≥ the 1.85 edition-2024 floor) with
  `rustfmt` + `clippy` components. CI installs whatever the pin says
  (`rustup show` honours the file), so the version lives in exactly one place.
- **One shared helper, feature-gated in makina-core.** A
  `makina_core::test_support` module behind a `test-support` cargo feature
  (with `tempfile` promoted to an optional dependency, and a self
  dev-dependency enabling the feature for makina-core's own integration
  tests). makina and makina-acp enable it from `[dev-dependencies]`. This
  kills the ~17 duplicated helper definitions.
- **Hermetic = env *and* local config.** Helper-spawned git gets
  `GIT_CONFIG_GLOBAL=/dev/null` + `GIT_CONFIG_SYSTEM=/dev/null`; the temp repo
  additionally gets **local** config (`user.email`, `user.name`,
  `commit.gpgsign=false`) so that *engine*-spawned git (worktree add, the
  Developer's commit, squash-merge) — which must **not** be env-neutralized,
  because in real repos it must honour real user config — is shielded by the
  repo itself. The `develop` branch-rename guard stays.
- **Every helper asserts success**, panicking with the captured stderr at the
  failing git step (the `e2e.rs:101–115` shape), never silently continuing.
- **CI poisons, not just neutralizes.** Once helpers are hermetic, the
  workflow sets `commit.gpgsign=true` + `init.defaultBranch=main` in the
  runner's *global* git config before testing. A clean runner can't catch a
  hermeticity regression; a poisoned one fails it on the spot.

## Out of scope

- Fixing the permission *policy* itself (the permissive worktree-scoped MVP,
  `permission.rs:77–80`) — a future permission-policy plan (0024). 0054 only
  documents it honestly.
- Any TUI changes (plans 0014/0015/0017 own those).
- Release automation, publishing, packaging, or running the `#[ignore]`d
  real-agent e2e in CI.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
