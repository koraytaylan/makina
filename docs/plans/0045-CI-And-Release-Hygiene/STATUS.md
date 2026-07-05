# Plan 0045 — CI & Release Hygiene — Green Develop, Hermetic Tests, and a Tagged v0.1.0 — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** 📋 Planned.

_Last updated: 2026-07-05, against develop._

- **Goal:** Every push/PR to develop runs the project's three gates on a pinned 1.96.1 toolchain with a 1.85 MSRV floor; the suite is hermetic against host git config (proven by a poison canary); and a metadata-complete, changelogged v0.1.0 is tagged with a linux/macos release workflow.
- **Root cause:** The repo defined a three-gate quality bar (.makina/config.toml) but never ran it in CI, pinned no toolchain or MSRV, left temp-repo tests inheriting host git config (only 2 of ~25 sites disable commit.gpgsign), and shipped an untagged 0.1.0 with no release metadata, changelog, or automation.
- **Approach:** Pin the toolchain and declare the 1.85 MSRV floor; add a GitHub Actions workflow running fmt/clippy/test on push/PR; introduce one shared hermetic git test helper, migrate every temp-repo site to it, and poison the CI runner's global git config to enforce hermeticity; fill workspace crates.io metadata, seed CHANGELOG.md, add a tag-triggered linux/macos release workflow, and tag a green v0.1.0.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Toolchain Pinning & MSRV | `pin-rust-toolchain`, `add-workspace-msrv` | 📋 Planned |
| 0002 | GitHub Actions CI | `add-ci-workflow` | 📋 Planned |
| 0003 | Test Hermeticity | `shared-hermetic-git-helper`, `migrate-git-helpers`, `ci-hermeticity-canary` | 📋 Planned |
| 0004 | Versioning & Release Automation | `add-changelog`, `add-workspace-metadata`, `add-release-workflow`, `tag-v0-1-0` | 📋 Planned |
