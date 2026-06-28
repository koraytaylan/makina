# Plan 0042 — Detail-Pane Rendering And Interaction Fixes — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** ✅ Complete.

_Last updated: 2026-06-28, against develop._

- **Goal:** All six detail-pane defects are resolved and the task-detail UX enhancement is implemented: unfocused tabs are legible (`Border` surface, not `Dim`); code blocks render with syntect-backed syntax highlighting, a full-width `CodeBlockBg` band, and no spurious blank lines; the scrollbar thumb accurately tracks scroll position; the 'v' cycle gives immediate visible feedback and is documented; start/pause/cancel dispatch from the detail pane via `Ctrl+S`/`Ctrl+P`/`Ctrl+C` with the plan's run correctly selected; task tabs adopt an accordion with Scope and Execution sections. Users report improved readability, discoverability, and ease of interaction in the detail pane.
- **Root cause:** Six independent but related defects accumulated in the detail-pane rendering and interaction layer. Tab styling used `Dim` (a text color) as a surface background. Code blocks emitted a blank `Line` per source line (trailing `split('\n')` element), had no per-token coloring, and used a `Background`-colored, glyph-only band. The scrollbar passed `total_rendered_rows` to `ScrollbarState::new()` instead of `scroll_max`. The 'v' binding works and is tested but its effect is invisible outside the run/exchange pane and undocumented. Start was unreachable from a plan tab (`s` shadowed by the accordion toggle, and plan tabs never select their run). Task tabs lacked the accordion pattern already proven for plans.
- **Approach:** Execute six workstreams. WS1 (tab) and WS2 (code blocks, a self-contained island of three tasks) run in parallel; WS3→WS4→WS5→WS6 form a chain because they share `ui.rs`/`event.rs`/`app.rs` edit footprints and build on one another (scrollbar → 'v' feedback → run-control decoupling → accordion). Test each fix with the existing suite (`cargo test`/clippy/fmt) as the acceptance gate. Changes touch `ui.rs`, `markup.rs`, `event.rs`, `app.rs`, `theme.rs`, `Cargo.toml`, and add one new file `crates/makina/src/syntax.rs`.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Tab Styling And Visibility | `fix-unfocused-tab-background-color` | ✅ Done |
| 0002 | Code-Block Rendering With Syntax | `add-codeblock-background-theme-role`, `add-syntect-code-highlighter`, `fix-codeblock-line-splitting-and-full-width-band` | ✅ Done |
| 0003 | Scrollbar Position Accuracy | `fix-scrollbar-state-initialization` | ✅ Done |
| 0004 | Cycle-Views Key Binding | `verify-and-document-cycle-dependency-view-binding` | ✅ Done |
| 0005 | Plan Execution Controls | `restore-run-control-dispatch-from-detail-pane` | ✅ Done |
| 0006 | Task-Detail Accordion Structure | `add-accordion-state-for-task-details`, `refactor-task-entry-pane-to-accordion-sections`, `wire-accordion-toggles-for-task-tabs` | ✅ Done |
