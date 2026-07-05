# Plan 0046 — Front Door — Makina's First Five Minutes for an Outside Developer — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** 📋 Planned.

_Last updated: 2026-07-05, against develop._

- **Goal:** An outside developer's first five minutes are guided, not hostile: an empty-backend start routes to the Doctor `w` scaffold instead of `exit(1)`, `makina --help/--version/--doctor` behave as a real CLI, the shipped concurrency is a safe 2, a committed vhs demo shows the TUI, and the README matches shipped reality — all gates green.
- **Root cause:** The users who most need setup help (no backend configured, none auto-detected) are the only ones who never get it: `main.rs` hard-fails with `process::exit(1)` for every `ConfigError` instead of launching the gated 0043 Doctor scaffold, the binary has no CLI surface, the shipped config auto-approves ten agents, there is no committed demo, and the README still claims running requires two config files.
- **Approach:** Five workstreams: land the gated 0043 launch-doctor task behind a pure `is_recoverable_empty_backend` classifier (binary land-or-revert); add a hand-rolled CLI (`cli.rs` parser + `build.rs` git-sha stamp + argv dispatch in `main`) giving `--help`/`--version`/`--doctor` while a bare `makina` still launches the TUI; lower the shipped `concurrency` 10→2; commit a scriptable vhs demo tape and render recipe; and refresh the README against shipped reality (auto-detection, Doctor overlay, CLI flags, safety guidance first).

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Launch Doctor On Empty Backend | `launch-doctor-on-empty-backend` | 📋 Planned |
| 0002 | CLI Argument Surface | `add-cli-parser`, `add-git-sha-build-script`, `wire-cli-in-main` | 📋 Planned |
| 0003 | Safe Concurrency Default | `set-safe-concurrency-default` | 📋 Planned |
| 0004 | Committed Demo Recording | `add-demo-tape-and-recipe` | 📋 Planned |
| 0005 | README Refresh Against Shipped Reality | `readme-refresh-shipped-reality` | 📋 Planned |
