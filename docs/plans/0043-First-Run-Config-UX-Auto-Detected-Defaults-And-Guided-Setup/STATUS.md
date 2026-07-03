# Plan 0043 — First-Run Config UX: Auto-Detected Backend Defaults and Guided Setup — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** 📋 Planned.

_Last updated: 2026-07-03, against develop._

- **Goal:** A fresh clone with a supported agent CLI on PATH runs `cargo run --release` successfully via auto-detected backend defaults, and users without one get a guided detect→confirm→persist scaffold plus precise hard-fail diagnostics.
- **Root cause:** On a fresh clone the global `~/.makina/config.toml` is absent and the shipped `.makina/config.toml` sets no `[backend]`/`[[providers]]`, so `Config::resolve` yields an empty backend and `Config::validate` (config.rs:853-857) rejects it with `backend.command must not be empty`; resolution never consulted the pure PATH resolver already present in `preflight.rs` to supply a default.
- **Approach:** Three sequenced workstreams: add a `KNOWN_AGENTS` registry and pure `detect_backend_in_path` in `preflight.rs` and fold it into resolution via an injectable `Config::apply_detected_backend` (production reads real `$PATH`, `Config::load` stays hermetic); upgrade the Doctor `w` scaffold into a detect→confirm→persist writer and guide (not exit) on empty-backend startup; and enrich the validation diagnostic plus harden the shipped project-config comments so the trap can neither recur silently nor be re-introduced from the committed layer.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Backend Auto-Detection Defaults | `add-known-agent-registry`, `synthesize-detected-backend` | 📋 Planned |
| 0002 | Guided First-Run Setup And Persist | `detect-driven-scaffold`, `launch-doctor-on-empty-backend` | 📋 Planned |
| 0003 | Diagnostics And Shipped-Config Hardening | `enrich-empty-backend-diagnostics`, `harden-shipped-config` | 📋 Planned |
