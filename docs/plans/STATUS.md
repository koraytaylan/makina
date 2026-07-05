# Plans — roll-up board

One row per plan, no per-task detail; task-level status lives in each plan's own STATUS.md.

_Last updated: 2026-07-05, against develop._

| Plan | Title | Status | Tasks | Outcome | Status doc |
|---|---|---|---|---|---|
| 0043 | First-Run Config UX: Auto-Detected Backend Defaults and Guided Setup | ✅ Complete | 5/6 | A fresh clone with a supported agent CLI on PATH runs `cargo run --release` successfully via auto-detected backend defaults, and users without one get a guided detect→confirm→persist scaffold plus precise hard-fail diagnostics. | [status](0043-First-Run-Config-UX-Auto-Detected-Defaults-And-Guided-Setup/STATUS.md) |
| 0044 | Agent Coverage & Compatibility — Verified ACP Agent Support | ✅ Complete | 5/5 | Makina's ACP agent support is visible and verified: string-id agent requests are answered (no hung turns), the registry lists ten correctly-launched ACP agents, every registry-derived surface stays in sync, and the README publishes a compatibility matrix — all gates green. | [status](0044-Agent-Coverage-And-Compatibility/STATUS.md) |
| 0045 | CI & Release Hygiene — Green Develop, Hermetic Tests, and a Tagged v0.1.0 | 📋 Planned | 0/10 | Every push/PR to develop runs the project's three gates on a pinned 1.96.1 toolchain with a 1.85 MSRV floor; the suite is hermetic against host git config (proven by a poison canary); and a metadata-complete, changelogged v0.1.0 is tagged with a linux/macos release workflow. | [status](0045-CI-And-Release-Hygiene/STATUS.md) |
