# Plan 0039 — UX-Completeness-Hardening — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** 📋 Planned.

_Last updated: 2026-06-26, against develop._

- **Goal:** Five short-term UX completeness fixes land: theme robustness (graceful fallback on missing role), markdown caching (eliminate O(n) re-parses), sidebar resizing (user control + small-terminal guard), key feedback (status message on silent no-ops), and honest provider editor (renamed to read-only view). All gates green, TUI polish complete.
- **Root cause:** Five polish gaps in the TUI from the glm-5.2 review: (1) theme hardness — missing role panics. (2) perf — markdown re-parses every frame. (3) UX usability — sidebar fixed 30%, no small-terminal guard. (4) discoverability — overloaded keys silent outside context. (5) honest labeling — editor read-only but titled configure.
- **Approach:** Probe/baseline tasks first (none needed for this plan—all changes are forward). Implement the five fixes in dependency order: theme (no deps) and markdown cache (no deps) in parallel, then key feedback (no deps) and sidebar resize (no deps) in parallel, then provider rename (no deps). All five are independent and can run in parallel. Verify each task is a simple, isolated change to a single or dual source file. Gate commands green throughout.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Theme-Robustness-And-Markdown-Caching | `theme-fallback`, `markdown-cache` | 📋 Planned |
| 0002 | Key-Feedback-Sidebar-And-Provider-Editor | `key-feedback-status`, `sidebar-resize` | 📋 Planned |
| 0003 | Provider-Editor-Rename-Or-Implement | `provider-editor-rename` | 📋 Planned |
