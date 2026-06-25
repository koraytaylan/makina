# XAgent Plan 0037 — Audit and Fix Ayu Theme Distinctness and Rendering Gaps

This plan audits and fixes post-delivery issues with plan 0036's theme
implementation. It verifies the three Ayu variants (Dark, Mirage, Light) differ
meaningfully in RGB values across all rendering contexts; confirms each variant's
palette matches the canonical ayu-colors spec; checks that all color decisions
pull from the active theme and not hardcoded values; ensures truecolor (24-bit
RGB) escape sequences are emitted for Ghostty and modern terminals rather than
downsampled to 16-color ANSI; audits selection/focus styling to confirm theme
colors are applied and visually distinct within each variant; replaces hardcoded
`Modifier::DIM` and `Modifier::REVERSED` in markdown code blocks with theme-based
foreground/background colors (e.g. the new `CodeBlock` role); and adds rendering
tests that verify the three variants display distinct cells under `TestBackend`.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the concrete deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}`).
- **Depends on** lists *direct* prerequisites only; "—" means none.
- **Done when** is the verifiable criterion, and every task keeps the gate
  commands green — the full forms are `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`;
  abbreviated as "cargo test/clippy/fmt green" thereafter.
- GPU tests self-skip without an adapter.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0001 — Theme-Aware Markdown Code Block Styling

### audit-markdown-code-block-calls — Audit and Thread Theme into render_markdown Callsites

Before updating `render_markdown` itself, we must identify all callsites to
understand the scope and determine which have `app` in scope (so
`&app.active_theme` can be passed). The function is called from multiple places
in `ui.rs` to render exchange responses, plan accordions, and other markdown
content. A probe task now ensures we know exactly where the calls are and whether
they are reachable by `app`.

**Steps:**

1. Execute `grep -n 'render_markdown' /Users/koraytaylan/Workspace/makina/crates/makina/src/ui.rs` and record each line number and surrounding context.
2. For each callsite, verify in the source that `app` is in scope (either as a direct parameter or accessible via the enclosing function's context). Record the function name and whether `app` is available.
3. Identify which callsites are in production code (not test stubs) and which are in `#[cfg(test)]` blocks. Only production sites need updating.
4. Verify that all production callsites can receive `&app.active_theme` as a parameter. If a callsite is in a helper function that does not have `app` in scope, plan to add it as a parameter in the next task.

- **Depends on:** —
- **Done when:** A list of all `render_markdown` callsites in ui.rs is documented (file:line + function name + availability of `app`). At least 3 production callsites are identified. All sites note whether `app` is in scope or must be threaded as a parameter. No test discovered to fail; this is an exploration task. cargo test/clippy/fmt green.

---

### add-codblock-theme-role — Add CodeBlock Role to Theme and Ayu Variants

Markdown code blocks and inline code currently use hardcoded `Modifier::DIM` and
`Modifier::REVERSED` (e.g. `markup.rs:133`), ignoring the active theme. To make
code-block styling theme-aware, a new `CodeBlock` semantic role must be added to
`ThemeRole`, values assigned to each of the three Ayu variants (Dark, Mirage,
Light), and a test added to ensure every theme defines this role.

**Steps:**

1. Open `crates/makina/src/theme.rs`.
2. In the `ThemeRole` enum (line ~6), add a new variant: `CodeBlock,  // foreground color for code blocks and inline code`. Update `ALL_ROLES` array to include `ThemeRole::CodeBlock`.
3. In the `ayu_dark()` function (line ~60), add after the existing role insertions: `colors.insert(ThemeRole::CodeBlock, Color::Rgb(115, 184, 255));  // same as Info for consistency`. Rationale: Info role is blue-tinted and works well as a subtle code highlight in dark themes.
4. In the `ayu_mirage()` function (line ~98), add: `colors.insert(ThemeRole::CodeBlock, Color::Rgb(128, 191, 255));  // same as Info`.
5. In the `ayu_light()` function (line ~136), add: `colors.insert(ThemeRole::CodeBlock, Color::Rgb(71, 138, 204));  // same as Info, adjusted for light background`.
6. In the `#[cfg(test)]` section of `theme.rs`, locate the `ayu_dark_pins_expected_values` test (line ~193). Add an assertion: `assert_eq!(th.get(ThemeRole::CodeBlock), Color::Rgb(115, 184, 255));` to pin the CodeBlock value for Dark.
7. Add similar assertions for Mirage and Light variants (or create separate test functions).

- **Depends on:** —
- **Done when:** The `ThemeRole` enum includes `CodeBlock`. All three Ayu variants (ayu_dark, ayu_mirage, ayu_light) insert `CodeBlock` colors into their `colors` HashMap. The `ALL_ROLES` array includes `CodeBlock`. Value-pinning tests assert exact `Color::Rgb` values for `CodeBlock` in all three themes. `cargo test --lib theme` passes. cargo test/clippy/fmt green.

---

### update-render-markdown-signature — Update render_markdown Signature to Accept Theme Parameter

The `render_markdown` function at `markup.rs:76` currently has signature
`pub fn render_markdown(text: &str, base: Style, width: u16) -> Vec<Line<'static>>`.
To apply theme-aware colors to code blocks, the function must accept a `Theme`
parameter so code-block styling can call `theme.get(ThemeRole::CodeBlock)`.

**Steps:**

1. Open `crates/makina/src/markup.rs`.
2. Change the function signature (line 76) from: `pub fn render_markdown(text: &str, base: Style, width: u16) -> Vec<Line<'static>>` to: `pub fn render_markdown(text: &str, base: Style, width: u16, theme: &crate::theme::Theme) -> Vec<Line<'static>>`.
3. The function body will use the `theme` parameter in the code-block arms (next task). For now, ensure the parameter is accepted and stored (the compiler will flag any unused-parameter warnings post-implementation).

- **Depends on:** add-codblock-theme-role, audit-markdown-code-block-calls
- **Done when:** The `render_markdown` function signature includes `theme: &crate::theme::Theme` as the fourth parameter. The function compiles without errors. Existing callsites are now compile-time errors (unfixed; expected — next task will thread theme through callsites). `cargo build` reports errors only for missing theme arguments at callsites. cargo test/clippy/fmt green.

---

### replace-code-block-hardcoded-modifiers — Replace Hardcoded Modifiers with Theme-Aware Code Block Colors

In `markup.rs`, code blocks and inline code apply hardcoded modifiers
(`Modifier::DIM`, `Modifier::REVERSED`) without theme context. These must be
replaced with theme-based foreground and background colors from the new
`CodeBlock` role and the active theme's `Background` role.

**Steps:**

1. Open `crates/makina/src/markup.rs`.
2. Locate the `Event::Code` arm (line ~130–134). Replace the current span styling:

   ```rust
   spans.push(Span::styled(
       t.to_string(),
       base.add_modifier(Modifier::DIM | Modifier::REVERSED),
   ));
   ```

   with theme-aware colors:

   ```rust
   spans.push(Span::styled(
       t.to_string(),
       Style::default()
           .fg(theme.get(crate::theme::ThemeRole::CodeBlock))
           .bg(theme.get(crate::theme::ThemeRole::Background))
           .add_modifier(Modifier::DIM),  // keep DIM for subtle emphasis, but add color
   ));
   ```

   Rationale: CodeBlock foreground makes the code visible; Background background ensures contrast; DIM is retained for subtle emphasis without overwhelming.
3. Locate the code-block text rendering at line ~128 and line ~143 (in the `in_code_block` context). Replace `base.add_modifier(Modifier::DIM)` with the same styled color pair: `Style::default().fg(theme.get(crate::theme::ThemeRole::CodeBlock)).bg(theme.get(crate::theme::ThemeRole::Background))`.
4. Review the full `render_markdown` function to ensure all code-block related styling paths have been updated. Search for any remaining hardcoded `Modifier::REVERSED` or other hardcoded modifier-only styling in code-block contexts.

- **Depends on:** update-render-markdown-signature
- **Done when:** All `Modifier::DIM | Modifier::REVERSED` and standalone `Modifier::DIM` in code-block contexts are replaced with theme-resolved style pairs. Inline code at `Event::Code` is styled with `fg(theme.get(CodeBlock))` + `bg(theme.get(Background))` + `Modifier::DIM`. Code-block body text at `in_code_block` context uses the same styling. The function still compiles despite callsites being broken (theme parameter not yet threaded). Grep confirms no `Modifier::REVERSED` remains in production code (outside tests). `cargo build` succeeds (errors only at callsites, not within markup.rs). cargo test/clippy/fmt green.

---

### thread-theme-through-render-markdown-callsites — Thread Active Theme Through All render_markdown Callsites

The probe task identified all `render_markdown` callsites in `ui.rs`. Each must
now be updated to pass `&app.active_theme` as the fourth argument. This unblocks
the code-block styling changes and ensures all markdown rendering is theme-aware.

**Steps:**

1. Open `crates/makina/src/ui.rs`.
2. For each production callsite of `render_markdown` identified in the probe task:
3. 1. Verify `app` is in scope (either as a parameter or via `self` if in an impl method). If not, add it as a parameter.
4. 2. Replace `render_markdown(text, base_style, width)` with `render_markdown(text, base_style, width, &app.active_theme)`.
5. 3. Update any helper functions that call `render_markdown` to accept `theme: &crate::theme::Theme` as a parameter and pass it through.
6. Verify the exact callsites from the probe (e.g., lines ~1047, ~1829, ~2144, ~2472, ~2520). Update each one individually.
7. Ensure that loop variables, buffer operations, and other rendering context are not affected by adding the theme parameter.

- **Depends on:** audit-markdown-code-block-calls, replace-code-block-hardcoded-modifiers
- **Done when:** All production `render_markdown` callsites in ui.rs pass `&app.active_theme` as the fourth argument. Helper functions that call `render_markdown` accept and thread the theme parameter. No compile errors remain. `cargo build` succeeds. `cargo test` passes for all ui rendering tests. Code blocks and inline code now render with theme-aware colors; switching themes updates code-block appearance. Existing exchange-pane and markdown tests remain green. cargo test/clippy/fmt green.

---

## 0002 — Selection and Focus Highlight Distinctness Audit

### add-selection-distinctness-test — Add Visual Regression Test for Selection Color Distinctness

The three Ayu variants should render selections with visually distinct colors.
Today there is one theme-validation test (`render_with_ayu_mirage_theme_resolves_colors`
at `ui.rs:8923–8958`) that checks a single cell's color, but no test that proves
the three variants render *different* selection highlight colors. This task adds
an automated regression test that renders a selection region under each theme and
asserts the color pairs differ.

**Steps:**

1. Open `crates/makina/src/ui.rs` and locate the tests section near the end (around line 8923).
2. After the existing `render_with_ayu_mirage_theme_resolves_colors` test, add a new test function `test_three_ayu_variants_render_distinct_selection_colors()`. Use the template below:

   ```rust
   #[test]
   fn three_ayu_variants_render_distinct_selection_colors() {
       // Render the same app with Dark, Mirage, and Light themes.
       // Set a selection region and verify each variant renders the SelectionBg + Foreground pair distinctly.
       // For each theme: render to TestBackend, extract the selected region's cells, and collect the (bg, fg) color pairs.
       // Assert Dark's pair is distinct from Mirage's pair and from Light's pair (not all three the same).
       // This proves selection styling responds to theme changes and variants are visually different.

       let themes = vec![
           crate::theme::ayu_dark(),
           crate::theme::ayu_mirage(),
           crate::theme::ayu_light(),
       ];

       let mut seen_colors: std::collections::HashSet<(ratatui::style::Color, ratatui::style::Color)> = std::collections::HashSet::new();

       for theme in themes {
           let mut terminal = make_terminal(80, 24);
           let api = Arc::new(PlaceholderApi::new());
           let mut app = App::new(api, vec![], std::path::PathBuf::from("."));
           app.active_theme = theme.clone();

           // Create a selection region (e.g., from (10, 5) to (20, 5)).
           app.selection = Some(crate::selection::Selection::start(10, 5, Rect::new(0, 0, 80, 24)));
           app.selection.as_mut().unwrap().extend(20, 5);

           terminal.draw(|frame| crate::ui::render(&app, frame)).expect("draw");

           let buffer = terminal.backend().buffer().clone();

           // Collect the (bg, fg) pair from cells in the selection region.
           // (For simplicity, just grab the first selected cell's colors.)
           for cell in buffer.content() {
               if cell.bg != ratatui::style::Color::default() {
                   seen_colors.insert((cell.bg, cell.fg));
                   break;  // One representative color pair per theme
               }
           }
       }

       // Assert we saw at least 2 distinct (bg, fg) pairs (proving themes differ).
       // In the best case, we'd see 3 distinct pairs (one per theme).
       assert!(seen_colors.len() >= 2, "Expected at least 2 distinct selection color pairs across themes, got {}", seen_colors.len());
   }
   ```

3. Create a small app with a selection region (e.g., from (10, 5) to (20, 5) on a (80, 24) terminal).
4. For each of the three themes (Dark, Mirage, Light), render the app to a TestBackend, extract the buffer, and collect the (bg, fg) color pair from the first selected cell.
5. Store all three (bg, fg) pairs in a HashSet<(Color, Color)>. After processing all themes, assert the set's size is at least 2 (proving at least two themes render different colors; ideally 3 for full distinctness).
6. Add a second test `test_accordion_focused_state_colors_differ_per_theme()` that renders an accordion pane with a focused header and verifies the focused header's colors differ per theme. Follow the same pattern: collect (bg, fg) pairs for each theme and assert distinctness.

- **Depends on:** —
- **Done when:** `test_three_ayu_variants_render_distinct_selection_colors` and `test_accordion_focused_state_colors_differ_per_theme` are added to ui.rs and pass. Each test renders three distinct themes, collects color pairs, and asserts the pairs differ. `cargo test` passes for both new tests and all existing ui tests. The tests serve as regression guards: if future changes make selection colors identical across themes, the tests will fail. cargo test/clippy/fmt green.

---

### update-accordion-focus-styling — Update Accordion Focused State to Use Distinctive Background (Workstream 0002)

Accordion section headers in plan tabs use `Dim` as the focused background color
(ui.rs:2120–2124). In light themes, Dim is a mid-tone and may not provide
sufficient contrast with Foreground for a focused/active indicator. To improve
visual distinctness, the focused state should use a more prominent background —
either a new `FocusBg` role or reuse `SelectionBg`. This task updates the styling
logic.

**Steps:**

1. Open `crates/makina/src/theme.rs`.
2. In the `ThemeRole` enum (line ~6), add a new variant: `FocusBg,  // active/focused widget backgrounds`. Update `ALL_ROLES` to include it.
3. Add `FocusBg` values to each theme. For visual distinctness in all three variants, use colors that are noticeably different from Background and Foreground: (a) Ayu Dark: `Color::Rgb(40, 80, 120)` (darker, more saturated blue), (b) Ayu Mirage: `Color::Rgb(70, 110, 160)` (similar dark-blue tone), (c) Ayu Light: `Color::Rgb(200, 215, 240)` (pale blue, distinct from white Background #F8F9FA).
4. Update the value-pinning test to assert exact `FocusBg` values for at least one theme (e.g., Dark).
5. Open `crates/makina/src/ui.rs` and locate the `render_accordion_section` function (around line 2096).
6. Find the focused-state styling block (lines ~2119–2124). Replace:

   ```rust
   if focused {
       title_style = title_style
           .bg(app.active_theme.get(crate::theme::ThemeRole::Dim))
           .add_modifier(Modifier::BOLD);
   }
   ```

   with:

   ```rust
   if focused {
       title_style = title_style
           .bg(app.active_theme.get(crate::theme::ThemeRole::FocusBg))  // was Dim
           .fg(app.active_theme.get(crate::theme::ThemeRole::Foreground))  // ensure good contrast
           .add_modifier(Modifier::BOLD);
   }
   ```

   Rationale: FocusBg is more distinctive; explicitly set Foreground to ensure text contrast.

- **Depends on:** add-selection-distinctness-test, thread-theme-through-render-markdown-callsites, audit-markdown-code-block-calls
- **Done when:** `FocusBg` role is defined in `ThemeRole` and added to `ALL_ROLES`. All three Ayu variants include `FocusBg` color values (Rgb tuples). Value-pinning test asserts at least one `FocusBg` value. Accordion section headers render with `FocusBg` background when focused (not Dim). Visual tests and existing accordion tests pass. Accordion focused state is now visually distinct in all three themes. `cargo test --lib` passes. cargo test/clippy/fmt green.

---

## 0003 — Hardcoded Color Sweep and Truecolor Verification

### hardcoded-color-grep-audit — Audit ui.rs and ansi.rs for Remaining Hardcoded Color:: Literals

Plan 0036 claims all production `Color::` sites in ui.rs and ansi.rs were
migrated to theme lookups, but an explicit enumeration and verification was not
performed. This probe task executes a grep sweep to identify any remaining
hardcoded `Color::` literals in production code (outside test blocks), categorizes
them, and confirms they are either legitimate (e.g., Color::Reset for defaults) or
test-only stubs.

**Steps:**

1. Execute: `grep -n 'Color::' /Users/koraytaylan/Workspace/makina/crates/makina/src/ui.rs | grep -v '#\[cfg(test)\]' | head -30` and record all non-test matches.
2. For each match, open the file and verify whether it is in a `#[cfg(test)]` block or a comment. Record the result (line number, context, and classification).
3. Repeat for ansi.rs: `grep -n 'Color::' /Users/koraytaylan/Workspace/makina/crates/makina/src/ansi.rs | grep -v '#\[cfg(test)\]' | head -20`.
4. Classify each non-test match as one of: (a) legitimate (e.g., `Color::Reset` for default/inherited), (b) test-only (accidentally matched), (c) concern (hardcoded literal in production code). Note: legitimate uses of `Color::Reset` or other defaults are acceptable; any hardcoded RGB or named colors (e.g., `Color::Blue`, `Color::White`) outside tests are concerns.
5. Summarize findings: total matches, breakdown by category, and any concerns that require fixing.

- **Depends on:** audit-markdown-code-block-calls, thread-theme-through-render-markdown-callsites, update-accordion-focus-styling, add-selection-distinctness-test
- **Done when:** Grep audit results documented. All non-test `Color::` matches in ui.rs and ansi.rs are enumerated and classified. Any production hardcoded colors (Concern category) are identified with file:line and context. If zero Concerns found, audit is complete and confirms plan 0036's migration was thorough. If Concerns found, they are logged for follow-up fixes. cargo test/clippy/fmt green.

---

### add-rgb-type-assertion-test — Add Test Asserting All Theme Colors Are Color::Rgb (Not Downsampled)

Ratatui and crossterm backends support truecolor (24-bit RGB) on capable
terminals, but downsampling to 16-color ANSI can occur if a backend doesn't
support truecolor or if colors are defined as `Color::Indexed` instead of
`Color::Rgb`. This task adds a test to the `theme` module that asserts all builtin
theme colors (both roles and ANSI palette entries) are defined as `Color::Rgb`,
guaranteeing no accidental downsampling in the application layer.

**Steps:**

1. Open `crates/makina/src/theme.rs` and locate the `#[cfg(test)]` module at the end (around line 174).
2. Add a new test function after `ayu_dark_pins_expected_values`: `fn all_theme_colors_are_rgb_not_indexed()`. The test iterates over all builtin themes and all roles, asserting each `theme.get(role)` is a `Color::Rgb` variant.
3. Pseudocode for the test body:

   ```rust
   #[test]
   fn theme_colors_are_rgb_not_downsampled() {
       // Verify that builtin themes use only Color::Rgb, not Color::Indexed or named colors.
       for theme in crate::theme::Theme::builtin_themes() {
           for &role in &crate::theme::ALL_ROLES {
               let color = theme.get(role);
               match color {
                   ratatui::style::Color::Rgb(_, _, _) => {},  // OK
                   _ => panic!("Theme {:?} role {:?} is not Color::Rgb, got {:?}", theme.name, role, color),
               }
           }
           for i in 0..16 {
               let ansi_color = theme.ansi(i);
               match ansi_color {
                   ratatui::style::Color::Rgb(_, _, _) => {},  // OK
                   _ => panic!("Theme {:?} ANSI[{}] is not Color::Rgb, got {:?}", theme.name, i, ansi_color),
               }
           }
       }
   }
   ```

4. Ensure the test is comprehensive: it checks all roles (10 after adding CodeBlock and FocusBg) and all 16 ANSI entries for all three builtin themes. No role or ANSI entry should use `Color::Indexed`, `Color::Ansi`, or named colors.

- **Depends on:** add-codblock-theme-role, update-accordion-focus-styling
- **Done when:** Test `all_theme_colors_are_rgb_not_indexed` is added to theme.rs and passes. The test asserts all roles and ANSI entries in all three themes are `Color::Rgb` variants. If any theme defines a role as a non-RGB color, the test fails with a descriptive panic message. `cargo test --lib theme` passes. This test serves as a guardrail: accidental introduction of downsampled colors (e.g., via copy-paste from an old palette) will be caught immediately. cargo test/clippy/fmt green.

---

### add-buffer-truecolor-assertion — Add Integration Test: Render Produces Truecolor Cells (Not ANSI16)

While the previous task verifies that the `Theme` struct defines colors as
`Color::Rgb`, this task verifies that those colors actually reach the rendered
ratatui Buffer (not downsampled). This is an integration test that renders a full
app to TestBackend and inspects the resulting buffer cells, asserting that styled
cells contain `Color::Rgb` values.

**Steps:**

1. Open `crates/makina/src/ui.rs` and locate the tests section (around line 8900).
2. Add a new test function `test_render_produces_truecolor_cells_not_ansi16()`. The test creates a terminal, app, renders the app, and inspects the buffer:

   ```rust
   #[test]
   fn render_produces_truecolor_not_ansi16() {
       let mut terminal = make_terminal(80, 24);
       let api = Arc::new(PlaceholderApi::new());
       let app = App::new(api, vec![], std::path::PathBuf::from("."));

       terminal.draw(|frame| render(&app, frame)).expect("draw");

       let buffer = terminal.backend().buffer().clone();

       // Sample cells from different parts of the UI (title bar, sidebar, main pane).
       // For each, verify fg and bg are Color::Rgb, not Color::Indexed or named colors.
       for cell in buffer.content().iter().take(200) {  // Scan first 200 cells
           // Allow Color::Reset (default), but require Rgb for any *styled* colors.
           match cell.fg {
               ratatui::style::Color::Reset => {},  // OK (inherits from terminal)
               ratatui::style::Color::Rgb(_, _, _) => {},  // OK (truecolor)
               other => panic!("Unexpected foreground color in cell: {:?}", other),
           }
           match cell.bg {
               ratatui::style::Color::Reset => {},  // OK
               ratatui::style::Color::Rgb(_, _, _) => {},  // OK
               other => panic!("Unexpected background color in cell: {:?}", other),
           }
       }
   }
   ```

3. Render the app to a TestBackend (use `make_terminal` helper). Extract the buffer via `terminal.backend().buffer().clone()`.
4. Iterate over the buffer's cells (via `buffer.content()`). For cells with non-default colors, assert that `cell.fg` and `cell.bg` are `Color::Rgb` variants (or `Color::Reset` for defaults), never `Color::Indexed` or other non-RGB variants.
5. Sample at least 200–500 cells to provide good coverage of different UI regions (title bar, sidebar, main pane).
6. If a non-RGB color is found, panic with a descriptive message naming the cell position and the unexpected color type.

- **Depends on:** add-rgb-type-assertion-test, audit-markdown-code-block-calls, thread-theme-through-render-markdown-callsites, update-accordion-focus-styling, add-selection-distinctness-test, hardcoded-color-grep-audit
- **Done when:** Test `test_render_produces_truecolor_cells_not_ansi16` is added to ui.rs and passes. The test renders the app and inspects 200+ buffer cells, asserting all styled cells use `Color::Rgb`. If any cell contains a non-RGB color (e.g., `Color::Indexed`), the test fails with file:line + color details. `cargo test ui::tests::test_render_produces_truecolor_cells_not_ansi16` passes. This test verifies end-to-end: colors flow from the theme through the render logic into the buffer as truecolor, not downsampled. cargo test/clippy/fmt green.

---

### verify-hardcoded-colors-are-fixed — Fix Any Production Hardcoded Colors Discovered in Audit (GATED)

**Gate:** This task is conditional on the `hardcoded-color-grep-audit` probe task
identifying production hardcoded `Color::` literals. If the audit finds zero
Concerns (all hardcoded colors are in test blocks or are legitimate defaults like
`Color::Reset`), this task is skipped (landed as n/a). If Concerns are found, this
task fixes them by replacing hardcoded colors with `theme.get()` or `theme.ansi()`
calls.

**Steps:**

1. Review the audit results from `hardcoded-color-grep-audit`. If no Concerns are listed, this task is done (no-op).
2. For each Concern (hardcoded color in production code), locate the file:line and context.
3. Determine the intent of the color (e.g., is it a background, foreground, emphasis, warning?). Map it to the appropriate `ThemeRole`.
4. Replace the hardcoded `Color::*` literal with `app.active_theme.get(ThemeRole::*)` or `app.active_theme.ansi(index)` as appropriate.
5. Verify the surrounding code has `app` in scope; if not, thread it as a parameter.
6. Test the change by running `cargo build` and `cargo test`.

- **Depends on:** hardcoded-color-grep-audit
- **Done when:** This is a GATED task; it lands or is reverted-and-recorded as n/a depending on audit results. If audit found zero Concerns: done when the audit report confirms zero hardcoded colors in production ui.rs/ansi.rs. If audit found Concerns: all identified hardcoded colors are replaced with theme-aware lookups, `cargo build` succeeds, and a grep re-audit confirms no remaining hardcoded colors in production code. cargo test/clippy/fmt green.

---

**End of plan 0037 TASKS.**
