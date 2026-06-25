# Architecture — Plan 0037 (deltas)

> The concrete deltas. This plan touches
> `crates/makina/src/theme.rs` (two new `ThemeRole` variants + value pins),
> `crates/makina/src/markup.rs` (`render_markdown` signature + code-block arms),
> `crates/makina/src/ui.rs` (thread the theme through `render_markdown`
> callsites, re-style the focused accordion-section header, new
> distinctness/truecolor render tests), and — only if the audit finds a real
> concern — any production `Color::` literal it surfaces in `ui.rs`/`ansi.rs`.
> No new module and no new dependency: the two new roles reuse the existing
> `ThemeRole` enum + `HashMap<ThemeRole, Color>` shape from plan 0036.
> Line numbers are hints against `develop` (plan 0036 has landed); locate by
> symbol with the greps the tasks name.

## 0001 — Theme-Aware Markdown Code Block Styling

Today `render_markdown` (`markup.rs:76`) has signature
`pub fn render_markdown(text: &str, base: Style, width: u16) -> Vec<Line<'static>>`
— it carries no `Theme`, so it cannot resolve a role. Inside it, code is styled
with hardcoded modifiers, not theme colors: inline code at the `Event::Code` arm
(`markup.rs:130–134`) is `base.add_modifier(Modifier::DIM | Modifier::REVERSED)`,
and fenced code-block body text in the `in_code_block` context
(`markup.rs:127–128, 138–143`) is `base.add_modifier(Modifier::DIM)`. Because
both depend only on the terminal-default `base` plus a modifier, code blocks look
identical in all three Ayu variants — they never read the active palette. The
five production callsites in `ui.rs` (`render_markdown` at `ui.rs:1047, 1829,
2144, 2472, 2520`) pass only `(text, base, width)`; the `ui.rs:2144` site is
inside `render_accordion_section` (`ui.rs:2096`), which already has `app` in
scope.

**Edits:**

**Add a `CodeBlock` role to the palette.** In `crates/makina/src/theme.rs`, add
`CodeBlock` to the `ThemeRole` enum (`theme.rs:6`) and to `ALL_ROLES`
(`theme.rs:19`) — note `ALL_ROLES` is typed `[ThemeRole; 10]` and **must** grow
to `[ThemeRole; 11]` (then `12` once `FocusBg` lands in 0002). Each `ayu_*()`
constructor (`ayu_dark` `theme.rs:60`, `ayu_mirage` `theme.rs:98`, `ayu_light`
`theme.rs:136`) inserts a `CodeBlock` value; reuse the variant's existing `Info`
hue so code reads as a calm, harmonious highlight rather than an inverted block:

```rust
// ayu_dark():   colors.insert(ThemeRole::CodeBlock, Color::Rgb(115, 184, 255)); // = Info
// ayu_mirage(): colors.insert(ThemeRole::CodeBlock, Color::Rgb(128, 191, 255)); // = Info
// ayu_light():  colors.insert(ThemeRole::CodeBlock, Color::Rgb( 71, 138, 204)); // = Info
```

The `#[cfg(test)]` value-pinning test (`ayu_dark_pins_expected_values`,
`theme.rs:193`) gains an assertion pinning the exact `Color::Rgb` for `CodeBlock`
(at minimum for Dark), so a wrong palette fails the suite rather than rendering
wrong. The existing no-gap test (`theme.rs:182`, iterating `ALL_ROLES`)
automatically covers presence of the new role in all three variants once it is in
the array.

**Thread `Theme` into `render_markdown`.** Change the signature
(`markup.rs:76`) to
`pub fn render_markdown(text: &str, base: Style, width: u16, theme: &crate::theme::Theme) -> Vec<Line<'static>>`.
Replace the hardcoded code styling with a theme-resolved color pair, keeping
`Modifier::DIM` for subtle emphasis but dropping `Modifier::REVERSED` (which
inverts unconditionally and looks jarring in the light variant):

```rust
// Event::Code arm (markup.rs:130–134) and in_code_block body (markup.rs:127–128, 138–143):
let code_style = Style::default()
    .fg(theme.get(crate::theme::ThemeRole::CodeBlock))
    .bg(theme.get(crate::theme::ThemeRole::Background))
    .add_modifier(Modifier::DIM);            // was Modifier::DIM | Modifier::REVERSED
```

After this edit a grep for `Modifier::REVERSED` in `markup.rs` production code
returns nothing.

**Update the five callsites in `ui.rs`.** Each `render_markdown(text, base,
width)` becomes `render_markdown(text, base, width, &app.active_theme)`. Four of
the five sites have `app` in render scope directly; the `ui.rs:2144` site is
inside `render_accordion_section`, whose `app: &App` first parameter
(`ui.rs:2097`) already supplies it — no new parameter is needed for any callsite.

**Properties that make this safe:** the role addition is purely additive and
guarded by the existing no-gap + value-pinning tests, so a missing/wrong color
fails at `cargo test`, never at render time; the signature change makes every
unmigrated callsite a hard compile error (the `add-codblock-theme-role` →
`update-render-markdown-signature` → `replace-code-block-hardcoded-modifiers` →
`thread-theme-through-render-markdown-callsites` chain lands them in dependency
order, so the workspace only compiles once all five are threaded); `base` still
flows through for non-code spans, so headings/lists/links are unchanged; the
audit probe (`audit-markdown-code-block-calls`) enumerates the callsites before
any are touched, so no production site is missed.

## 0002 — Selection and Focus Highlight Distinctness Audit

Today selection highlighting is already themed —
`selection.rs::highlight(buf, theme)` (`selection.rs:136`) paints
`SelectionBg`/`Foreground` (`selection.rs:141–142`) — but nothing proves the
three variants resolve to *different* selection colors; the only theme render
test, `render_with_ayu_mirage_theme_resolves_colors` (`ui.rs:8923`), checks a
single Mirage cell against the Dark default and never compares the variants
pairwise. Separately, the focused accordion-section header in
`render_accordion_section` (`ui.rs:2096`) uses
`title_style.bg(app.active_theme.get(ThemeRole::Dim)).add_modifier(Modifier::BOLD)`
(`ui.rs:2118–2123`). `Dim` is a mid-tone used widely for secondary text, so in
the light variant a `Dim` background under the `Accent` title (`ui.rs:2114–2116`)
gives a weak, ambiguous focus cue.

**Edits:**

**Add a `FocusBg` role.** Mirroring the `CodeBlock` addition in 0001, add
`FocusBg` to `ThemeRole` (`theme.rs:6`) and `ALL_ROLES` (`theme.rs:19`, now
`[ThemeRole; 12]`), and insert a value per variant chosen to be clearly distinct
from both `Background` and `Foreground` (a saturated blue band, pale in the light
variant):

```rust
// ayu_dark():   colors.insert(ThemeRole::FocusBg, Color::Rgb( 40,  80, 120));
// ayu_mirage(): colors.insert(ThemeRole::FocusBg, Color::Rgb( 70, 110, 160));
// ayu_light():  colors.insert(ThemeRole::FocusBg, Color::Rgb(200, 215, 240));
```

Pin at least the Dark `FocusBg` value in the value-pinning test; the no-gap test
covers presence in all three variants once `FocusBg` is in `ALL_ROLES`.

**Re-style the focused accordion header.** In the `if focused {` branch of
`render_accordion_section` (`ui.rs:2118–2123`), swap the `Dim` background for
`FocusBg` and set `Foreground` explicitly so the title stays legible against the
band:

```rust
if focused {
    title_style = title_style
        .bg(app.active_theme.get(crate::theme::ThemeRole::FocusBg))      // was Dim
        .fg(app.active_theme.get(crate::theme::ThemeRole::Foreground))   // explicit contrast
        .add_modifier(Modifier::BOLD);
}
```

**Add two distinctness render tests.** Next to the existing Mirage test
(`ui.rs:8923`), add `three_ayu_variants_render_distinct_selection_colors` and
`accordion_focused_state_colors_differ_per_theme`. Each renders the same app
under `ayu_dark()`/`ayu_mirage()`/`ayu_light()` via the `make_terminal`
`TestBackend` helper (`ui.rs:3463`), collects a representative `(bg, fg)` pair
from the selection region (resp. the focused header row) into a
`HashSet<(Color, Color)>`, and asserts the set holds ≥ 2 distinct pairs — so the
suite fails the moment a variant stops being visually distinct in that context.

**Properties that make this safe:** `FocusBg` is additive and test-guarded like
`CodeBlock`; the styling swap is a single local substitution in one branch and
leaves the unfocused path untouched; the new tests assert *distinctness*
(set size), not byte-exact colors, so they stay green across future palette
tweaks while still catching a regression that collapses the variants;
`add-selection-distinctness-test` runs before `update-accordion-focus-styling`,
so the distinctness guard exists before the focus styling changes under it.

## 0003 — Hardcoded Color Sweep and Truecolor Verification

Today plan 0036 migrated the production `Color::` sites in `ui.rs` and `ansi.rs`
to `theme.get(...)`/`theme.ansi(...)`, but no enumeration proves none survives,
and nothing asserts the resolved colors stay truecolor end-to-end. As a baseline
(against `develop` @ `4394dc8`): a grep already shows `ansi.rs` carries **zero**
`Color::` literals — it resolves all 16 ANSI slots via `theme.ansi(...)` and the
39/49 resets via `theme.get(Foreground)`/`get(Background)` — and `ui.rs` carries a
single `Color::` that lives inside a test (`ui.rs:8944`). The audit task below
*confirms* this by grep rather than assuming any particular count, so its expected
outcome on the current tree is **zero concerns** (the gated fix records n/a). A
`Color::Indexed` or named color slipping into a palette (e.g. via copy-paste from
an old table) would silently downsample on capable terminals (Ghostty, iTerm2),
defeating the distinctness goal, and the current tests would not catch it.

**Edits:**

**Audit `ui.rs`/`ansi.rs` for surviving literals.** The
`hardcoded-color-grep-audit` probe runs
`grep -n 'Color::' crates/makina/src/ui.rs` and the same over `ansi.rs`,
classifying every non-test match as *legitimate* (`Color::Reset`/default),
*test-only* (inside `#[cfg(test)]`), or *concern* (a hardcoded literal in
production). It produces a file:line table; concerns feed the gated fix below.

**Pin the palette to truecolor.** Add `theme_colors_are_rgb_not_downsampled` to
the `theme.rs` `#[cfg(test)]` module (`theme.rs:174`): for every theme in
`Theme::builtin_themes()` (`theme.rs:55`), every role in `ALL_ROLES` and every
`ansi(0..16)` entry must be a `Color::Rgb(..)` variant — any `Color::Indexed`,
named, or `Reset` color panics with the theme name, role/index, and the offending
color. This guards the whole palette (now 12 roles + 16 ANSI per variant).

**Assert truecolor reaches the buffer.** Add `render_produces_truecolor_not_ansi16`
beside the Mirage test (`ui.rs:8923`): render the app under `make_terminal`
(`ui.rs:3463`), clone `terminal.backend().buffer()`, and scan a sample of cells
asserting each `fg`/`bg` is `Color::Rgb` or `Color::Reset` (default) — never
`Color::Indexed` or a named color. This proves colors flow from the palette
through the render logic into the ratatui `Buffer` as truecolor rather than being
downsampled in the application layer. (Per the SCOPE locked decision, verifying
the *terminal escape bytes* would need a real PTY and is out of scope; ratatui's
crossterm backend owns terminal-specific emission.)

**Fix surfaced concerns (gated).** `verify-hardcoded-colors-are-fixed` is the
land-or-revert-and-record gate: if the audit found zero concerns it records n/a;
otherwise it maps each hardcoded literal to the right `ThemeRole` and replaces it
with `app.active_theme.get(...)`/`.ansi(...)`, threading `&app.active_theme` into
any helper that lacks it, then a re-grep confirms none remain.

**Properties that make this safe:** the two assertion tests are pure read-only
guards over already-built themes/buffers (no render-path mutation) and pin the
*shape* (`Rgb`) not exact values, so they catch downsampling without breaking on
palette tweaks; `add-rgb-type-assertion-test` depends on both 0001's
`add-codblock-theme-role` and 0002's `update-accordion-focus-styling`, so it runs
only after the role count is final (12) and exercises every role; the gated fix
touches production code only when a concern is real, and its binary
land-or-record outcome keeps the workstream from blocking on an empty audit.

## Test strategy

- **0001 (code-block theming).** Value-pinning test pins `CodeBlock` per variant;
  the no-gap test (`theme.rs:182`) covers presence; a grep confirms no
  `Modifier::REVERSED` survives in `markup.rs` production code; existing
  exchange-pane / accordion markdown tests stay green once the theme is threaded.
- **0002 (selection & focus distinctness).**
  `three_ayu_variants_render_distinct_selection_colors` and
  `accordion_focused_state_colors_differ_per_theme` each assert ≥ 2 distinct
  `(bg, fg)` pairs across the three variants; the existing
  `render_with_ayu_mirage_theme_resolves_colors` (`ui.rs:8923`) stays green.
- **0003 (sweep & truecolor).** `theme_colors_are_rgb_not_downsampled` (palette is
  all `Color::Rgb`) and `render_produces_truecolor_not_ansi16` (buffer cells are
  `Rgb`/`Reset`, never `Indexed`); the grep audit enumerates `ui.rs`/`ansi.rs`
  production `Color::` sites; the gated task fixes any concern and re-greps clean.
- All tasks keep the gate commands green — `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`.
