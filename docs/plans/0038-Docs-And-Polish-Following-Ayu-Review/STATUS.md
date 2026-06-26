# Plan 0038 — Docs-And-Polish-Following-Ayu-Review — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** 📋 Planned.

_Last updated: 2026-06-26, against develop._

- **Goal:** README reflects the current implementation (permissions answered, persistence done); four high-impact TUI polish bugs are fixed (readable inactive tabs, correct wall-clock countdown, discoverable help overlay, scrollable error pane); all gates pass.
- **Root cause:** Stale README documentation undersells the headline governance feature (WorktreePolicy), and four small TUI bugs accumulate to undercut the polish that plans 0036/0037 aimed for (invisible inactive tabs, hardcoded countdown, undiscoverable keybindings, non-scrollable error history).
- **Approach:** This is a brief, high-leverage plan addressing the top-priority findings from the glm-5.2 architecture review. Workstream 0001 (README) is pure documentation: remove stale claims about permissions and persistence, and document the WorktreePolicy feature. Workstream 0002 (TUI Polish) bundles four independent, one- to two-file fixes with high UX impact: styling, config sync, help overlay, scroll routing. All tasks are junior-executable, depend on no prior work, and can be implemented in parallel. The plan closes with all gates green.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | README-Fix-Permissions-And-Persistence | `readme-permissions-fix` | 📋 Planned |
| 0002 | TUI-Polish-Fixes | `inactive-tab-styling-fix`, `wall-clock-sync-fix`, `help-overlay-implementation`, `error-pane-scroll-support` | 📋 Planned |
