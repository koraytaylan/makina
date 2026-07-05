# Plan 0045 — CI & Release Hygiene — Green Develop, Hermetic Tests, and a Tagged v0.1.0 — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** ✅ Complete — 9/10 tasks landed on `implement-plan/0045`; squash-merged into develop as `f48dc98d3b83157ee0f0ca95f17a899472ae4585`.

_Last updated: 2026-07-05, against develop._

- **Goal:** Every push/PR to develop runs the project's three gates on a pinned 1.96.1 toolchain with a 1.85 MSRV floor; the suite is hermetic against host git config (proven by a poison canary); and a metadata-complete, changelogged v0.1.0 is tagged with a linux/macos release workflow.
- **Root cause:** The repo defined a three-gate quality bar (.makina/config.toml) but never ran it in CI, pinned no toolchain or MSRV, left temp-repo tests inheriting host git config (only 2 of ~25 sites disable commit.gpgsign), and shipped an untagged 0.1.0 with no release metadata, changelog, or automation.
- **Approach:** Pin the toolchain and declare the 1.85 MSRV floor; add a GitHub Actions workflow running fmt/clippy/test on push/PR; introduce one shared hermetic git test helper, migrate every temp-repo site to it, and poison the CI runner's global git config to enforce hermeticity; fill workspace crates.io metadata, seed CHANGELOG.md, add a tag-triggered linux/macos release workflow, and tag a green v0.1.0.

| WS | Workstream | Task | State |
|---|---|---|---|
| 0001 | Toolchain Pinning & MSRV | `pin-rust-toolchain` | ✅ Done |
| 0001 | Toolchain Pinning & MSRV | `add-workspace-msrv` | ✅ Done |
| 0002 | GitHub Actions CI | `add-ci-workflow` | ✅ Done |
| 0003 | Test Hermeticity | `shared-hermetic-git-helper` | ✅ Done |
| 0003 | Test Hermeticity | `migrate-git-helpers` | ✅ Done |
| 0003 | Test Hermeticity | `ci-hermeticity-canary` | ✅ Done |
| 0004 | Versioning & Release Automation | `add-changelog` | ✅ Done |
| 0004 | Versioning & Release Automation | `add-workspace-metadata` | ✅ Done |
| 0004 | Versioning & Release Automation | `add-release-workflow` | ✅ Done |
| 0004 | Versioning & Release Automation | `tag-v0-1-0` (GATED) | 🔲 Gated — ready to run: CI (gates + canary) green on `6a9e041` (run #3, 2026-07-05), CHANGELOG dated, metadata + release.yml present; tag push awaits explicit maintainer approval since it publishes a public GitHub release |
