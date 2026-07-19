# Plan 0048 — Per-Task Plan Documents and Transactional Status — 📋 Planned

The roll-up row in [../STATUS.md](../STATUS.md) must stay in sync with this file. Task-level truth lives in [tasks/](tasks/) frontmatter; Makina's integration coordinator updates both layers.

**Status:** 📋 Planned.

- **Goal:** make a typed `tasks/*.md` plan bundle Makina's only executable plan contract, with durable status tied to verifiable Git landings instead of manual bookkeeping or unchecked runtime JSON.
- **Root cause:** `TASKS.md` is interpreted independently by discovery, preview, ingestion, generation, persistence, and UI code, while no serialized component owns task frontmatter, plan status, and root roll-up transitions.
- **Approach:** five workstreams — a one-task bootstrap plus strict plan documents; runtime projection; an application-wide plan-directory identity; cross-process serialization with private integration workspaces and recoverable status/finalization; then atomic generation and a clean authoring/tooling cutover.
- **Progress:** 0/15 tasks done; 0 blocked; 0 dropped.
- **Integration:** `planned`; run —; base `develop` @ `d428defcde155e54e7cdc2ed06f5b32c2b013ec6`; validation base —; mode —; final integration —.
- **Exceptions:** — (coordinator-owned blocked/dropped reasons are recorded here).
- **Outcome:** Makina uses typed `tasks/*.md` bundles as its only executable plan format, and coordinator-owned status transitions are backed by verifiable Git landings rather than manual bookkeeping or unchecked runtime JSON.

_Last updated: 2026-07-19, against `develop` @ `d428def`._
