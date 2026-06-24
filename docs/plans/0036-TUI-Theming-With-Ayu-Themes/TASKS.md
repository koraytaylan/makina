# XAgent Plan 0036 — Makina TUI Theming with Ayu Built-In Themes

This plan replaces hardcoded ratatui `Color::` variants (180 in `ui.rs`, 11 in
`ansi.rs`; `selection.rs` and `markup.rs` carry none) with a central `theme`
module that maps semantic color roles (`Background`, `Foreground`, `Dim`,
`Accent`, `SelectionBg`, `Border`, `Success`, `Warning`, `Error`, `Info`) plus a
full 16-entry ANSI palette (`ansi: [Color; 16]`) to ratatui `Color` values. It
encodes three Ayu variants (dark, mirage, light) as precomputed `Color::Rgb`
tables sourced from the canonical **`github.com/ayu-theme/ayu-colors`**
`themes/{dark,mirage,light}.yaml` (the repo's `palette`/`surface`/`editor`/`ui`/
`common`/`vcs`/`terminal` blocks; the resolved hex table is pinned in
[ARCHITECTURE.md](ARCHITECTURE.md)). **Ayu Dark is the new default theme.** It
adds a "Switch theme" command-palette action that lists the built-in themes with
live switching + frame redraw, and persists the selected theme name in
`GlobalConfig` via the existing config-writer pattern (`commit_settings`),
restoring it on startup with a fallback to Ayu Dark.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the concrete deltas (including the full per-theme color table).

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}`).
- **Depends on** lists *direct* prerequisites only; "—" means none.
- **Done when** is the verifiable criterion, and every task keeps the gate
  commands green — the full forms are `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`;
  abbreviated as "cargo test/clippy/fmt green" thereafter.
- Line numbers are hints against `develop`; locate every site by the named
  symbol (grep). The migration changes the **rendered colors** — switching from
  the terminal's named palette (`Color::Cyan` etc.) to fixed `Color::Rgb`
  values is an intended, visible change, **not** byte-for-byte parity. Tests
  that assert the old named colors are enumerated per task and updated to assert
  the theme-resolved values.

---

## 0001 — Theme Core Abstraction

### theme-module-core — Theme Core Abstraction — ThemeRole, Theme & Built-In Ayu Palettes

The TUI renders with hardcoded ratatui `Color::` variants (180 in `ui.rs`, 11 in
`ansi.rs`). There is no semantic mapping from intent (foreground, accent, error)
to color, no ANSI palette indirection, and no way to swap palettes without
recompilation. Introduce a central `theme` module that maps semantic roles +
a 16-entry ANSI palette to colors and encodes the three Ayu variants as
precomputed tables from the upstream **ayu-colors** YAML.

**Steps:**

1. Create `crates/makina/src/theme.rs`. Define
   `pub enum ThemeRole { Background, Foreground, Dim, Accent, SelectionBg, Border, Success, Warning, Error, Info }`
   (derive `Debug, Clone, Copy, PartialEq, Eq, Hash`). Define
   `pub struct Theme { pub name: String, colors: std::collections::HashMap<ThemeRole, ratatui::style::Color>, ansi: [ratatui::style::Color; 16] }`.
2. Add a module const `pub const ALL_ROLES: [ThemeRole; 10] = [..]` listing every
   `ThemeRole` variant (used by the no-gap/value test).
3. Implement `pub fn get(&self, role: ThemeRole) -> Color` — HashMap lookup,
   `.expect(...)` with the role name on miss (no silent fallback). Implement
   `pub fn ansi(&self, index: usize) -> Color` returning `self.ansi[index]`
   (index 0–7 = normal black,red,green,yellow,blue,magenta,cyan,white; 8–15 =
   the bright set).
4. Implement `fn ayu_dark() -> Theme`, `fn ayu_mirage() -> Theme`,
   `fn ayu_light() -> Theme`, each populating **all 10** roles and **all 16**
   ANSI entries from the pinned table in
   [ARCHITECTURE.md](ARCHITECTURE.md#full-ayu-color-table) (every value is a
   literal `Color::Rgb(r, g, b)` — copy them verbatim). Expose `pub fn ayu_dark()`
   etc. (the App default and startup restore call them).
5. Implement `pub fn builtin_themes() -> Vec<Theme> { vec![ayu_dark(), ayu_mirage(), ayu_light()] }`.
6. Add `pub mod theme;` to `crates/makina/src/lib.rs` (alongside the existing
   `pub mod ansi;` … `pub mod ui;` block at lines 26–38) so both
   `crate::theme::*` (from `app.rs`/`ui.rs`) and `makina::theme::*` (from
   `main.rs`) resolve.
7. Add a `#[cfg(test)]` module with:
   (a) `themes_define_every_role_and_ansi_entry` — for every theme in
   `builtin_themes()`, assert `get(role)` succeeds for each role in `ALL_ROLES`
   and that every `ansi(i)` for `i in 0..16` is reachable;
   (b) `ayu_dark_pins_expected_values` — a **value-pinning** test asserting a
   representative set of exact `Color::Rgb` values for Ayu Dark
   (`get(Background) == Color::Rgb(13,16,23)`, `get(Accent) == Color::Rgb(230,180,80)`,
   `get(Error) == Color::Rgb(217,87,87)`, `ansi(1) == Color::Rgb(211,99,106)`,
   `ansi(9) == Color::Rgb(240,113,120)`), so a wrong palette fails the suite
   rather than silently passing a presence-only check.

- **Depends on:** —
- **Done when:** `crates/makina/src/theme.rs` compiles with no warnings.
  `Theme::builtin_themes()` returns three themes named exactly `"Ayu Dark"`,
  `"Ayu Mirage"`, `"Ayu Light"`, each defining all 10 `ThemeRole` variants and
  all 16 ANSI entries with the `Color::Rgb` values pinned in ARCHITECTURE.md.
  `themes_define_every_role_and_ansi_entry` and `ayu_dark_pins_expected_values`
  pass. `pub mod theme;` is declared in `lib.rs`. cargo test/clippy/fmt green.

---

### app-active-theme-field — App Active Theme Field — Add Mutable Theme State

`App` (`crates/makina/src/app.rs`, struct at line 1004) holds all render and
interaction state. Render functions take `&App`, so an `active_theme` field on
`App` is in scope at every render site. It defaults to Ayu Dark and is later
restored from `GlobalConfig` on startup (workstream 0004) and mutated by the
"Switch theme" action (0003).

**Steps:**

1. In `crates/makina/src/app.rs`, in `pub struct App` (line 1004), add
   `pub active_theme: crate::theme::Theme,` with a doc comment:
   `/// Active color theme, read during render. Defaults to Ayu Dark; restored from GlobalConfig on startup and mutated by the 'Switch theme' palette action.`
2. Initialize it in `App::new` (the `pub fn new(api, initial_runs, repo_root)`
   constructor at app.rs:1472) with `active_theme: crate::theme::Theme::ayu_dark(),`.
3. Initialize it in `App::with_config` (app.rs:1550) the same way (startup in
   `main.rs` overrides it from `GlobalConfig`; workstream 0004). The field is
   added with a default value, not a new constructor parameter, so existing
   construction sites are unchanged.
4. `cargo check -p makina` compiles.

- **Depends on:** theme-module-core
- **Done when:** `App` has a `pub active_theme: Theme` field initialized to
  `Theme::ayu_dark()` in both `App::new` (app.rs:1472) and `App::with_config`
  (app.rs:1550). No other construction site needs changes. cargo test/clippy/fmt
  green.

---

## 0002 — Render Module Migration

### ui-migrate-colors — Render Module Migration — Replace 180 Color:: in ui.rs

`ui.rs` has 180 hardcoded `Color::` variants (verify counts via
`grep -oE 'Color::[A-Za-z]+' crates/makina/src/ui.rs | sort | uniq -c`: roughly
DarkGray 61, Cyan 28, White 21, Red 21, Yellow 21, Green 15, Black 7, Magenta 6,
Blue 4, Gray 2). Each is replaced with `app.active_theme.get(ThemeRole::*)`.
**This visibly changes the rendered colors to the Ayu Dark palette — that is the
intended outcome, not a regression.** Several existing render tests assert the
old named colors in the buffer; they are enumerated below and updated to assert
the theme-resolved values.

**Steps:**

1. Audit the sites: `grep -n 'Color::' crates/makina/src/ui.rs`. Map each by
   semantic intent (see ARCHITECTURE.md §0002 for the full intent table):
   `White → Foreground`, `DarkGray`/`Gray` → `Dim`, `Green → Success`,
   `Yellow → Warning`, `Red → Error`, `Black → Background`, `Cyan → Accent`,
   `Blue → Info` (title bar), `Magenta → Accent`. Where two sites use the same
   named color for genuinely different intents, prefer the role that matches the
   *context* comment at the site (e.g. a "modified/info" badge → `Info`).
2. Replace each site locally with `app.active_theme.get(ThemeRole::*)`,
   leaving surrounding layout/text/modifiers untouched. `render(&app, …)` already
   has `app` in scope; for helper fns that lack it, thread `&app.active_theme`
   (or the resolved `Color`) through their signatures.
3. Update the enumerated render tests so each asserts the theme-resolved color
   instead of the old named variant. The pattern: at the top of the test build
   `let th = makina::theme::Theme::ayu_dark();` then assert e.g.
   `cell.fg == th.get(ThemeRole::Success)` (was `== Color::Green`). The tests
   (locate by name; line hints against `develop`):
   `ui.rs:4029` (selected-run bg, was `Cyan` → `Accent`),
   `4060` (running badge fg, `Green` → `Success`),
   `4085`/`4134`/`5557`/`6454` (`Red` → `Error`),
   `4173` (`Yellow` → `Warning`),
   `4354` (file-browser highlight bg, `Magenta` → `Accent`),
   `4932`/`4939`/`4946` (`Cyan`/`Red`/`Green` → `Accent`/`Error`/`Success`),
   `5280`/`5372`/`5376` (focused-task row, `Cyan` → `Accent`),
   `5636` (`assert_eq!(…, Some(Color::Cyan))` → `Some(th.get(ThemeRole::Accent))`),
   `5734` (`Green` → `Success`). Run
   `grep -nE 'Color::(Cyan|Magenta|Blue|Red|Green|Yellow|White|DarkGray|Black|Gray)' crates/makina/src/ui.rs`
   after editing and confirm the only remaining matches are inside `#[cfg(test)]`
   assertions that now reference `th.get(...)`/`th.ansi(...)`, or comments.
4. Verify production code has no literal `Color::` left:
   `grep -n 'Color::' crates/makina/src/ui.rs` should show matches only inside
   the test module or comments.

- **Depends on:** app-active-theme-field
- **Done when:** every production `Color::` site in `ui.rs` resolves through
  `app.active_theme.get(ThemeRole::*)`; layout/text/modifiers are unchanged; the
  default theme renders with the **Ayu Dark** palette. The enumerated render
  tests are updated to assert the theme-resolved colors and pass. No literal
  `Color::` remains outside the test module/comments. cargo test/clippy/fmt
  green.

---

### ansi-migrate-colors — ANSI Migration — Theme the SGR Parser, diff_line_style & ANSI-16 Passthrough

`ansi.rs` (206 lines) holds 11 `Color::` usages across two functions:
`apply_sgr` (lines 97–112; today handles only SGR `0`/`1`/`31`/`32`, mapping
31→`Color::Red`, 32→`Color::Green`) and `diff_line_style` (lines 128–141; maps
`+`→`Green`, `-`→`Red`, `@@`→`Cyan`). Both must resolve through the active
theme. The "full 16-color" decision means `apply_sgr` is extended to the
standard SGR color range and mapped onto `theme.ansi[..]` so agent output keeps
distinct ANSI colors.

**Steps:**

1. Thread the theme into the parser. Change `parse_ansi(input: &str)`
   (ansi.rs:36) → `parse_ansi(input: &str, theme: &crate::theme::Theme)` and
   `apply_sgr(style, params)` (ansi.rs:97) →
   `apply_sgr(style, params, theme: &crate::theme::Theme)`.
2. In `apply_sgr`, replace the literal arms and add the full color range,
   mapping onto the theme's ANSI palette:
   foreground `30..=37 → style.fg(theme.ansi(n - 30))`,
   bright fg `90..=97 → theme.ansi(8 + (n - 90))`,
   background `40..=47 → style.bg(theme.ansi(n - 40))`,
   bright bg `100..=107 → theme.ansi(8 + (n - 100))`,
   `39`/`49` reset fg/bg to default, keep `0` (reset) and `1` (bold). Leave
   unrecognized params as a no-op `_ => {}` (256/truecolor SGR is out of scope —
   see SCOPE.md).
3. Migrate `diff_line_style(line: &str)` (ansi.rs:128) →
   `diff_line_style(line: &str, theme: &crate::theme::Theme)`, mapping
   `+`→`theme.get(ThemeRole::Success)`, `-`→`theme.get(ThemeRole::Error)`,
   `@@`→`theme.get(ThemeRole::Info)`.
4. Thread the theme through the call chain. `parse_ansi`/`diff_line_style` are
   reached from `diff_overlaid_content_line(text_line: &str)` (ui.rs:2059, calls
   them ~ui.rs:2062); give it a `theme: &crate::theme::Theme` param and pass it
   at its caller `exchange_entry_lines(entry, app, width)` (call site ~ui.rs:2213,
   where `app` — and thus `&app.active_theme` — is in scope).
5. Update the existing `ansi.rs` tests (in `#[cfg(test)]`): build a theme
   (`let th = crate::theme::Theme::ayu_dark();`), pass it to `parse_ansi`, and
   assert the theme-resolved colors:
   `green_sgr_sets_green_foreground` (ansi.rs:148) → `Some(th.ansi(2))`,
   `red_sgr_sets_red_foreground` (ansi.rs:156) → `Some(th.ansi(1))`,
   `reset_sgr_returns_to_default_style` (ansi.rs:172) → pre-reset span
   `Some(th.ansi(2))`, `diff_line_style_colors_prefixes` (ansi.rs:195) →
   `Some(th.get(ThemeRole::Success))`/`Error`/`Info`.
6. Reconcile the downstream `ui.rs` exchange test
   `exchange_render_shows_thought_and_tool_entries` (asserts `Color::Green` from
   diff styling ~ui.rs:5734, also touched by ui-migrate-colors) so it asserts
   `th.get(ThemeRole::Success)`.

- **Depends on:** app-active-theme-field, ui-migrate-colors
- **Done when:** `apply_sgr` resolves SGR codes 30–37/40–47/90–97/100–107 onto
  `theme.ansi[..]` (red vs green vs blue stay distinct), `diff_line_style`
  resolves through `Success`/`Error`/`Info`, and the theme is threaded through
  `parse_ansi` → `diff_overlaid_content_line` → `exchange_entry_lines`. No
  production `Color::` literal remains in `ansi.rs`. The four updated `ansi.rs`
  tests and the `ui.rs` exchange test pass against theme-resolved colors. cargo
  test/clippy/fmt green.

---

### selection-migrate-colors — Selection Highlight — Themed Selection Colors

`selection.rs` (343 lines) renders the text-selection highlight in `highlight`
(line 135). **It does not use any `Color::` today** — the body (line 139) is
`Style::default().add_modifier(Modifier::REVERSED)` and the module doc
(lines 129–134) describes the reverse-video inversion. Per the locked decision,
selection moves to **explicit themed colors** (`SelectionBg`/`Foreground`),
replacing the REVERSED design; the doc and its two tests are updated to match.

**Steps:**

1. In `crates/makina/src/selection.rs`, change `pub fn highlight(&self, buf: &mut Buffer)`
   (line 135) → `pub fn highlight(&self, buf: &mut Buffer, theme: &crate::theme::Theme)`.
2. Replace the body's style (line 139) with
   `let style = Style::default().bg(theme.get(crate::theme::ThemeRole::SelectionBg)).fg(theme.get(crate::theme::ThemeRole::Foreground));`
   (add the needed imports; `Color` was not previously imported here). Keep the
   empty-selection early return and the per-cell painting loop unchanged.
3. Update the module doc comment (lines 129–134) to describe themed selection
   colors instead of a "reversed style".
4. Update the single call site `sel.highlight(frame.buffer_mut())` (ui.rs:734)
   to `sel.highlight(frame.buffer_mut(), &app.active_theme)`.
5. Update the two `selection.rs` tests to construct a theme and assert the
   themed cells: `highlight_reverses_only_cells_within_bounds` (line 301, rename
   to `highlight_styles_only_cells_within_bounds`) → assert in-bounds cells have
   `bg == th.get(SelectionBg)`/`fg == th.get(Foreground)` and out-of-bounds cells
   are unstyled; `highlight_skips_empty_selection` (line 330) → call
   `highlight(&mut buf, &th)` and assert no cell received the selection style.

- **Depends on:** app-active-theme-field, ui-migrate-colors, ansi-migrate-colors
- **Done when:** `highlight` takes `&Theme` and paints `SelectionBg`/`Foreground`
  from the active theme; the call site (ui.rs:734) passes `&app.active_theme`;
  the doc comment and both updated tests reflect themed colors and pass. cargo
  test/clippy/fmt green.

---

## 0003 — Command Palette Theme Switcher

### palette-theme-switcher — Command Palette Theme Switcher — Nested Theme Selector

The command palette lists actions via `CommandPalette::default_actions()`
(app.rs:447 — currently **7** actions: Open task list, Configure providers &
roles, Settings, Doctor, Retry failed task, Discover project, Quit). `PaletteAction`
is a `struct { label: &'static str, event: AppEvent }` (app.rs:424). Add a
"Switch theme" action that opens a nested, filterable list of built-in theme
names and applies the selection live by mutating `app.active_theme`. Because
`PaletteAction` becomes an enum, every field-access site must migrate.

**Steps:**

1. Convert `PaletteAction` (app.rs:424) to
   `pub enum PaletteAction { Regular { label: &'static str, event: AppEvent }, NestedThemeSelector { label: &'static str } }`
   and add `pub fn label(&self) -> &str` matching both arms.
2. Migrate every field-access site (the conversion makes these hard compile
   errors — fix each): `CommandPalette::filtered()` reads `action.label`
   (app.rs:488) → `action.label()`; `render_command_palette` reads `action.label`
   (ui.rs:2508) → `action.label()`; the `CommandPaletteExecute` handler
   `action.event.clone()` (event.rs:393) → `match action { Regular { event, .. } => /* dispatch */, NestedThemeSelector { .. } => /* enter theme mode, see step 5 */ }`.
3. Add `pub theme_selector: Option<Vec<String>>` to `CommandPalette` (None =
   normal action list; Some = theme-selector mode showing theme names).
   Initialize to `None` everywhere `CommandPalette` is built.
4. In `default_actions()` (app.rs:447) append
   `PaletteAction::NestedThemeSelector { label: "Switch theme" }` (the list now
   has **8** actions).
5. On Enter over `NestedThemeSelector`, set
   `palette.theme_selector = Some(Theme::builtin_themes().iter().map(|t| t.name.clone()).collect())`,
   clear the filter, reset `selected = 0`, and **do not close** the palette.
6. On Enter inside theme-selector mode, find the selected name in
   `builtin_themes()`, assign it to `app.active_theme`, set
   `palette.theme_selector = None`, return `true` (redraw). On Esc in
   theme-selector mode, set `theme_selector = None` (return to actions) without
   changing the theme; a second Esc closes the palette as before.
7. Filtering + rendering branch on mode: when `theme_selector.is_some()`, filter
   and render the theme names (`render_command_palette`, ui.rs:2468, reads
   `palette.theme_selector`); otherwise the existing action path.
8. Update the count test `palette_filter_narrows_and_clamps_selection`
   (app.rs:6389): expect **8** actions (`assert_eq!(palette.filtered().len(), 8)`
   at ~app.rs:6395, fix the "all 7 default actions" comment) and update
   `filtered[0].label` (~app.rs:6410) → `filtered[0].label()`. Add a new test:
   entering `NestedThemeSelector` sets `theme_selector = Some(names)` and keeps
   the palette open; selecting `"Ayu Mirage"` sets `app.active_theme.name ==
   "Ayu Mirage"` and clears `theme_selector`; Esc in nested mode clears it
   without changing the theme; typing `mirage` narrows the nested list to one.

- **Depends on:** app-active-theme-field, ui-migrate-colors, ansi-migrate-colors, selection-migrate-colors
- **Done when:** `PaletteAction` is an enum with a `label()` method; `filtered()`
  (app.rs:488), `render_command_palette` (ui.rs:2508), and the
  `CommandPaletteExecute` handler (event.rs:393) compile against the new API.
  `default_actions()` returns 8 actions including "Switch theme". Selecting a
  theme in the nested list mutates `app.active_theme` and redraws live; the
  palette stays open; Esc unwinds nested mode then the palette; filtering works
  in both modes. The updated count test and the new nested-selector tests pass.
  cargo test/clippy/fmt green.

---

## 0004 — Theme Persistence & Startup

### globalconfig-theme-name — GlobalConfig Extension — Add theme_name Field

`GlobalConfig` (`crates/makina-core/src/config.rs`) holds provider/role config
and serializes to `{repo_root}/.makina/config.toml` (it already uses
`#[serde(default)]` on most fields). Add a defaulted `theme_name` so existing
configs deserialize unchanged.

**Steps:**

1. In `crates/makina-core/src/config.rs`, add to `pub struct GlobalConfig`
   (locate by symbol): `#[serde(default = "default_theme_name")] pub theme_name: String,`
   with doc `/// Active theme name (e.g. "Ayu Dark"). Absent/unknown ⇒ "Ayu Dark".`
2. Add module-level `fn default_theme_name() -> String { "Ayu Dark".to_string() }`.
3. Add `#[test] fn globalconfig_deserializes_without_theme_name()` parsing a TOML
   without `theme_name` and asserting `theme_name == "Ayu Dark"`; and assert a
   TOML with `theme_name = "Ayu Mirage"` round-trips to that value.

- **Depends on:** theme-module-core
- **Done when:** `GlobalConfig` has `#[serde(default = "default_theme_name")] pub theme_name: String`.
  A TOML lacking `theme_name` deserializes to `"Ayu Dark"`; one with
  `"Ayu Mirage"` yields that value. cargo test/clippy/fmt green.

---

### theme-load-startup — App Startup — Restore Theme from GlobalConfig

`main.rs` constructs the App via `App::with_config` (app.rs:1550, called at
~main.rs:284). The config value (`config`, bound ~main.rs:52) is **moved** into
`CoreApi` at ~main.rs:277, so it is not in scope afterward — the theme name must
be cloned out **before** the move, mirroring the existing `*_for_app` clones at
main.rs:266–269.

**Steps:**

1. In `crates/makina/src/main.rs`, alongside the `*_for_app` clones (≈ lines
   266–269, before the `config` move at ≈277), add
   `let theme_name_for_app = config.theme_name.clone();`.
2. After the App is constructed (`App::with_config`, ≈main.rs:284), resolve the
   theme and assign it:
   ```rust
   let active_theme = makina::theme::Theme::builtin_themes()
       .into_iter()
       .find(|t| t.name == theme_name_for_app)
       .unwrap_or_else(makina::theme::Theme::ayu_dark);
   app.active_theme = active_theme;
   ```
3. Comment the fallback: an unknown/renamed saved theme resolves to Ayu Dark with
   no panic.
4. Add a unit test (in `theme.rs` or `main`-adjacent test module): resolving a
   known name yields that theme; an unknown name yields `ayu_dark()`. (Startup is
   otherwise headless-tested via this resolver, not by launching the TUI.)

- **Depends on:** globalconfig-theme-name, app-active-theme-field, palette-theme-switcher
- **Done when:** on startup, a valid saved `theme_name` sets `app.active_theme`
  to the matching built-in theme; an absent/unknown name falls back to Ayu Dark
  with no panic. The resolver is unit-tested for the hit and fallback paths. The
  snippet references `theme_name_for_app` (cloned pre-move), not a moved
  `config`. cargo test/clippy/fmt green.

---

### theme-commit-selection — Theme Persistence — Commit Selected Theme to GlobalConfig

Selecting a theme in the palette (0003) applies it in memory; to persist it,
write `theme_name` to `GlobalConfig` using the same merge-preserving recipe as
`commit_settings` (event.rs:571), so providers/roles round-trip untouched.

**Steps:**

1. In `crates/makina/src/event.rs`, add
   `async fn commit_theme_selection(app: &App, theme_name: &str) -> Option<String>`
   modeled on `commit_settings` (event.rs:571): validate `theme_name` ∈
   `Theme::builtin_themes()` (else return `Some("Unknown theme".into())`); read
   the current `GlobalConfig` from `makina_core::paths::config_file(&app.repo_root)`
   (default on read error); rebuild it as
   `GlobalConfig { theme_name: theme_name.to_string(), ..existing }`; ensure the
   parent dir; `toml::to_string_pretty` + `tokio::fs::write`; return
   `Some("Theme saved")` or `Some(err)`.
2. Wire it to the nested-selector Enter (0003): when a theme is committed,
   call `commit_theme_selection` and surface its returned string as a status
   message.
3. Add `#[tokio::test] async fn commit_theme_selection_writes_to_config()`:
   write to a temp config, call with a valid name, read back and assert
   `theme_name` is set and other fields (providers/roles) are preserved; calling
   with an unknown name returns the error string and does not write a bogus name.

- **Depends on:** globalconfig-theme-name, palette-theme-switcher
- **Done when:** committing a theme in the palette writes `theme_name` to
  `{repo_root}/.makina/config.toml` via `commit_theme_selection`, preserving
  other `GlobalConfig` fields, and surfaces a status message. An unknown name is
  rejected. The async test verifies write + field preservation + rejection.
  cargo test/clippy/fmt green.

---

### theme-validation-test — Validation Probe — Automated Theme Coverage & Switching

The integration probe for plan 0036 (depends on all prior tasks; by topological
order it lands last; it belongs to workstream 0004's persistence/startup
closure). It establishes **automated** assertions only — no manual-only criteria,
since `implement-plan`'s reviewer runs headless gate commands with no TTY.

**Steps:**

1. `cargo test --workspace` is green (no regressions; the color-asserting tests
   updated in 0002 now assert theme-resolved values).
2. Assert palette/theme integration in a headless render test: build an App with
   `TestBackend` (reuse the existing `ui.rs`/`app.rs` test scaffolding), set
   `app.active_theme = Theme::ayu_mirage()`, render, and assert a known cell
   carries `Theme::ayu_mirage().get(role)` — proving theme switching reaches the
   render path and that Mirage differs from Dark at that cell.
3. Assert the value-pinning + no-gap tests from 0001, the startup-fallback test
   from `theme-load-startup`, the persistence test from `theme-commit-selection`,
   the nested-selector tests from `palette-theme-switcher`, and the ANSI-mapping
   tests from `ansi-migrate-colors` all run under `cargo test`.

- **Depends on:** theme-module-core, app-active-theme-field, ui-migrate-colors, ansi-migrate-colors, selection-migrate-colors, palette-theme-switcher, globalconfig-theme-name, theme-load-startup, theme-commit-selection
- **Done when:** `cargo test --workspace` is green with no regressions; a
  headless `TestBackend` render under `Theme::ayu_mirage()` asserts a cell holds
  the Mirage-resolved color (distinct from Ayu Dark); the value-pinning, no-gap,
  startup-fallback, persistence, nested-selector, and ANSI-mapping tests all
  pass. cargo test/clippy/fmt green.

---

**End of plan 0036 TASKS.** When every "Done when" bullet is green, the Makina
TUI renders through a semantic `Theme` abstraction: a `ThemeRole` enum + 16-entry
ANSI palette map every color intent to a ratatui `Color`; three built-in Ayu
palettes (Dark, Mirage, Light) ship as `Color::Rgb` tables sourced from
`ayu-theme/ayu-colors`; every production `Color::` site in `ui.rs`, `ansi.rs`,
and `selection.rs` reads the active theme; a "Switch theme" command-palette
action switches themes live with Ayu Dark as the default; and the selection
persists to `GlobalConfig` and restores on startup with a fallback to Ayu Dark —
all with the gate commands green.
