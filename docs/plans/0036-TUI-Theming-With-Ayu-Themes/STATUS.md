# Plan 0036 — Makina TUI Theming with Ayu Built-In Themes — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** ✅ Complete.

_Last updated: 2026-06-25, against develop._

- **Goal:** The Makina TUI renders through a semantic `Theme` abstraction, ships three built-in Ayu palettes (dark/mirage/light) sourced from `github.com/ayu-theme/ayu-colors`, and persists the theme selection across restarts. Users switch themes live via the command palette (Ctrl+P → "Switch theme"); the active theme drives every render context (title bar, sidebar, exchange pane, themed selection highlight, and the 16-color ANSI agent output). **Ayu Dark is the default.** Migrating from the terminal's named palette to fixed `Color::Rgb` is an intended, visible color change (not byte-for-byte parity); tests asserting the old named colors are updated to the theme-resolved values. The codebase is unblocked for future user-defined themes.
- **Root cause:** No semantic color abstraction in the render path — 180 `Color::` variants in `ui.rs` and 11 in `ansi.rs` are hardcoded, making runtime theming impossible and locking the aesthetic to one hand-chosen palette. (`selection.rs` and `markup.rs` carry no `Color::`.)
- **Approach:** Add a `theme` module mapping 10 semantic roles + a 16-entry ANSI palette to ratatui `Color`, with three Ayu variants as precomputed `Color::Rgb` tables (pinned in ARCHITECTURE.md, value-tested). Add `App.active_theme` (default Ayu Dark). Migrate the production `Color::` sites in `ui.rs`/`ansi.rs` to theme lookups (extending `apply_sgr` to the full SGR range and theming `diff_line_style`), and re-theme `selection.rs::highlight` (was `Modifier::REVERSED`) to `SelectionBg`/`Foreground`. Add a "Switch theme" palette action (converting `PaletteAction` to an enum and migrating its field-access sites) for live switching. Persist the selection to `GlobalConfig` via a `commit_theme_selection` merge-writer modeled on `commit_settings`, restoring it on startup with a fallback to Ayu Dark.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Theme Core Abstraction | `theme-module-core`, `app-active-theme-field` | ✅ Done |
| 0002 | Render Module Migration | `ui-migrate-colors`, `ansi-migrate-colors`, `selection-migrate-colors` | ✅ Done |
| 0003 | Command Palette Theme Switcher | `palette-theme-switcher` | ✅ Done |
| 0004 | Theme Persistence & Startup | `globalconfig-theme-name`, `theme-load-startup`, `theme-commit-selection`, `theme-validation-test` | ✅ Done |
