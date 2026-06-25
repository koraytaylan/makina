# Plan 0037 — Audit and Fix Ayu Theme Distinctness — status

Task-level status lives here.

**Status:** 🔲 Not started — authored, awaiting implementation.

_Last updated: 2026-06-25, against develop._

- **Goal:** The three built-in Ayu variants (Dark, Mirage, Light) render visibly
  distinct in every color-bearing context. Markdown code blocks pull from the
  active theme (a new `CodeBlock` role) instead of hardcoded
  `Modifier::DIM`/`Modifier::REVERSED`; the focused accordion-section header uses
  a distinctive `FocusBg` band instead of the mid-tone `Dim`; selection and focus
  styling are proven distinct across the three variants by render tests; the
  palette is pinned to truecolor (`Color::Rgb`) and verified to reach the ratatui
  buffer un-downsampled; and a grep sweep confirms no production `Color::` literal
  survives outside test stubs in `ui.rs`/`ansi.rs`.
- **Root cause:** Plan 0036 delivered the `Theme` abstraction and three Ayu
  palettes, but not every render path was migrated. `render_markdown`
  (`markup.rs:76`) takes no `Theme`, so code blocks (`markup.rs:127–143`) stay on
  terminal-default modifiers and never change with the theme; the focused
  accordion header (`ui.rs:2118–2123`) uses `Dim` as its background, which is a
  weak focus cue in the light variant; and there is no test proving the variants
  resolve distinct selection/focus colors or that the palette stays truecolor.
- **Approach:** Add two semantic roles (`CodeBlock`, `FocusBg`) to the existing
  `ThemeRole` enum, value-pinned per variant. Thread `&app.active_theme` through
  `render_markdown` and its five `ui.rs` callsites, replacing the hardcoded
  code-block modifiers with `CodeBlock`/`Background` colors. Re-style the focused
  accordion header to `FocusBg`/`Foreground`. Add `TestBackend` render tests that
  assert the three variants produce distinct selection and focus color pairs, a
  test that the whole palette (12 roles + 16 ANSI) is `Color::Rgb`, and a test
  that rendered buffer cells stay `Rgb`/`Reset` (never `Indexed`). Run a grep
  audit of `ui.rs`/`ansi.rs` and fix any surviving production literal via a gated
  task.
- **Outcome:** _(pending implementation)_ — to be recorded here when the plan
  lands.

| WS | Workstream | Task | State |
|---|---|---|---|
| 0001 | Theme-Aware Markdown Code Block Styling | `audit-markdown-code-block-calls` | 🔲 Pending |
| 0001 | Theme-Aware Markdown Code Block Styling | `add-codblock-theme-role` | 🔲 Pending |
| 0001 | Theme-Aware Markdown Code Block Styling | `update-render-markdown-signature` | 🔲 Pending |
| 0001 | Theme-Aware Markdown Code Block Styling | `replace-code-block-hardcoded-modifiers` | 🔲 Pending |
| 0001 | Theme-Aware Markdown Code Block Styling | `thread-theme-through-render-markdown-callsites` | 🔲 Pending |
| 0002 | Selection and Focus Highlight Distinctness Audit | `add-selection-distinctness-test` | 🔲 Pending |
| 0002 | Selection and Focus Highlight Distinctness Audit | `update-accordion-focus-styling` | 🔲 Pending |
| 0003 | Hardcoded Color Sweep and Truecolor Verification | `hardcoded-color-grep-audit` | 🔲 Pending |
| 0003 | Hardcoded Color Sweep and Truecolor Verification | `add-rgb-type-assertion-test` | 🔲 Pending |
| 0003 | Hardcoded Color Sweep and Truecolor Verification | `add-buffer-truecolor-assertion` | 🔲 Pending |
| 0003 | Hardcoded Color Sweep and Truecolor Verification | `verify-hardcoded-colors-are-fixed` (GATED) | 🔲 Pending |
