# Plans — roll-up board

One row per plan, no per-task detail; task-level status lives in each plan's own STATUS.md.

_Last updated: 2026-07-05, against develop._

| Plan | Title | Status | Tasks | Outcome | Status doc |
|---|---|---|---|---|---|
| 0043 | First-Run Config UX: Auto-Detected Backend Defaults and Guided Setup | ✅ Complete | 5/6 | A fresh clone with a supported agent CLI on PATH runs `cargo run --release` successfully via auto-detected backend defaults, and users without one get a guided detect→confirm→persist scaffold plus precise hard-fail diagnostics. | [status](0043-First-Run-Config-UX-Auto-Detected-Defaults-And-Guided-Setup/STATUS.md) |
| 0044 | Agent Coverage & Compatibility — Verified ACP Agent Support | ⛔ Blocked | 0/5 | Makina's ACP agent support is visible and verified: string-id agent requests are answered (no hung turns), the registry lists ten correctly-launched ACP agents, every registry-derived surface stays in sync, and the README publishes a compatibility matrix — all gates green. | [status](0044-Agent-Coverage-And-Compatibility/STATUS.md) |
