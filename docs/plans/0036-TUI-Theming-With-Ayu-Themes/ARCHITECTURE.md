# Architecture — Plan 0036 (deltas)

> The concrete deltas. This plan touches
> `crates/makina/src/theme.rs` (new file),
> `crates/makina/src/lib.rs` (module declaration),
> `crates/makina/src/app.rs`, `crates/makina/src/ui.rs`,
> `crates/makina/src/ansi.rs`, `crates/makina/src/selection.rs`,
> `crates/makina/src/event.rs`, `crates/makina/src/main.rs`, and
> `crates/makina-core/src/config.rs`.
> `markup.rs` is **not** touched — it carries no `Color::` (it styles via the
> caller-supplied `base: Style`, which `ui.rs` already feeds).
> Line numbers are hints against `develop`; locate by symbol.

## 0001 — Theme Core Abstraction

Today render calls build styled widgets with literal colors, e.g. the title bar
at `ui.rs:101–102` uses `Style::default().bg(Color::Blue).fg(Color::White)`, and
this pattern repeats 180× across `ui.rs`. There is no indirection.

**Edits:**

**Define `ThemeRole`, `Theme`, and the ANSI palette.** Add
`crates/makina/src/theme.rs`:

```rust
use ratatui::style::Color;
use std::collections::HashMap;

/// Semantic intent of a color — looked up from the active theme at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeRole {
    Background,  // panes, empty states
    Foreground,  // normal text
    Dim,         // secondary/disabled text, hints
    Accent,      // selection, active badge, task ids
    SelectionBg, // text-selection background
    Border,      // frames, dividers
    Success,     // completed / added
    Warning,     // partial / pending
    Error,       // failed / removed
    Info,        // informational / modified
}

pub const ALL_ROLES: [ThemeRole; 10] = [
    ThemeRole::Background, ThemeRole::Foreground, ThemeRole::Dim, ThemeRole::Accent,
    ThemeRole::SelectionBg, ThemeRole::Border, ThemeRole::Success, ThemeRole::Warning,
    ThemeRole::Error, ThemeRole::Info,
];

/// A complete theme: semantic roles + a 16-entry ANSI palette (0–7 normal
/// black,red,green,yellow,blue,magenta,cyan,white; 8–15 the bright set).
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    colors: HashMap<ThemeRole, Color>,
    ansi: [Color; 16],
}

impl Theme {
    /// Role color; panics (with the role name) if undefined — the value-pinning
    /// test guards every built-in theme so this never fires at render time.
    pub fn get(&self, role: ThemeRole) -> Color {
        *self.colors.get(&role).unwrap_or_else(|| panic!("theme {:?} missing role {:?}", self.name, role))
    }
    /// ANSI palette entry (index 0–15).
    pub fn ansi(&self, index: usize) -> Color { self.ansi[index] }

    pub fn builtin_themes() -> Vec<Theme> { vec![ayu_dark(), ayu_mirage(), ayu_light()] }
}
```

**Encode the three Ayu variants.** Each `ayu_*()` constructor populates all 10
roles and all 16 ANSI entries with the literal `Color::Rgb` values from the table
below, sourced from `github.com/ayu-theme/ayu-colors`
(`themes/{dark,mirage,light}.yaml`) — no OKLCH/external color crate, so the
module stays dependency-free:

```rust
pub fn ayu_dark() -> Theme {
    let mut colors = HashMap::new();
    colors.insert(ThemeRole::Background, Color::Rgb(13, 16, 23));   // #0D1017 surface.base
    colors.insert(ThemeRole::Foreground, Color::Rgb(191, 189, 182)); // #BFBDB6 editor.fg
    colors.insert(ThemeRole::Dim, Color::Rgb(90, 99, 120));        // #5A6378 ui.fg
    colors.insert(ThemeRole::Accent, Color::Rgb(230, 180, 80));    // #E6B450 common.accent
    colors.insert(ThemeRole::SelectionBg, Color::Rgb(25, 49, 85)); // #193155 selection@.25 over bg
    colors.insert(ThemeRole::Border, Color::Rgb(27, 31, 41));      // #1B1F29 ui.line
    colors.insert(ThemeRole::Success, Color::Rgb(112, 191, 86));   // #70BF56 vcs.added
    colors.insert(ThemeRole::Warning, Color::Rgb(255, 180, 84));   // #FFB454 palette.yellow
    colors.insert(ThemeRole::Error, Color::Rgb(217, 87, 87));      // #D95757 common.error
    colors.insert(ThemeRole::Info, Color::Rgb(115, 184, 255));     // #73B8FF vcs.modified
    let ansi = [
        Color::Rgb(10, 14, 20), Color::Rgb(211, 99, 106), Color::Rgb(150, 191, 67), Color::Rgb(224, 158, 74),
        Color::Rgb(78, 171, 224), Color::Rgb(185, 146, 224), Color::Rgb(131, 202, 179), Color::Rgb(191, 189, 182),
        Color::Rgb(104, 104, 104), Color::Rgb(240, 113, 120), Color::Rgb(170, 217, 76), Color::Rgb(255, 180, 84),
        Color::Rgb(89, 194, 255), Color::Rgb(210, 166, 255), Color::Rgb(149, 230, 203), Color::Rgb(255, 255, 255),
    ];
    Theme { name: "Ayu Dark".to_string(), colors, ansi }
}
// ayu_mirage() and ayu_light() follow the same shape with the table values below.
```

<a id="full-ayu-color-table"></a>
**Full Ayu color table (pin these exact `Color::Rgb` values).** Sourced from the
`ayu-colors` YAML; `SelectionBg`/`Border` flatten the spec's alpha colors over
the background; ANSI `normal` = the spec hue darkened ~12% (matching the YAML
`terminal:` block's `-L` step), `bright` = the base hue.

| Role | Ayu Dark | Ayu Mirage | Ayu Light |
|---|---|---|---|
| Background | `Rgb(13,16,23)` #0D1017 | `Rgb(31,36,48)` #1F2430 | `Rgb(248,249,250)` #F8F9FA |
| Foreground | `Rgb(191,189,182)` #BFBDB6 | `Rgb(204,202,194)` #CCCAC2 | `Rgb(92,97,102)` #5C6166 |
| Dim | `Rgb(90,99,120)` #5A6378 | `Rgb(112,122,140)` #707A8C | `Rgb(130,142,159)` #828E9F |
| Accent | `Rgb(230,180,80)` #E6B450 | `Rgb(255,204,102)` #FFCC66 | `Rgb(242,151,24)` #F29718 |
| SelectionBg | `Rgb(25,49,85)` #193155 | `Rgb(43,70,104)` #2B4668 | `Rgb(215,228,246)` #D7E4F6 |
| Border | `Rgb(27,31,41)` #1B1F29 | `Rgb(48,56,67)` #303843 | `Rgb(231,234,237)` #E7EAED |
| Success | `Rgb(112,191,86)` #70BF56 | `Rgb(135,217,108)` #87D96C | `Rgb(108,191,67)` #6CBF43 |
| Warning | `Rgb(255,180,84)` #FFB454 | `Rgb(255,205,102)` #FFCD66 | `Rgb(235,164,0)` #EBA400 |
| Error | `Rgb(217,87,87)` #D95757 | `Rgb(255,102,102)` #FF6666 | `Rgb(230,80,80)` #E65050 |
| Info | `Rgb(115,184,255)` #73B8FF | `Rgb(128,191,255)` #80BFFF | `Rgb(71,138,204)` #478ACC |

ANSI-16 (`ansi[0..16]` = normal black,red,green,yellow,blue,magenta,cyan,white then the bright set):

| Slot | Ayu Dark normal / bright | Ayu Mirage normal / bright | Ayu Light normal / bright |
|---|---|---|---|
| black (0/8) | `Rgb(10,14,20)` / `Rgb(104,104,104)` | `Rgb(25,30,42)` / `Rgb(104,104,104)` | `Rgb(92,97,102)` / `Rgb(50,50,50)` |
| red (1/9) | `Rgb(211,99,106)` / `Rgb(240,113,120)` | `Rgb(213,119,106)` / `Rgb(242,135,121)` | `Rgb(211,99,99)` / `Rgb(240,113,113)` |
| green (2/10) | `Rgb(150,191,67)` / `Rgb(170,217,76)` | `Rgb(187,224,113)` / `Rgb(213,255,128)` | `Rgb(118,158,0)` / `Rgb(134,179,0)` |
| yellow (3/11) | `Rgb(224,158,74)` / `Rgb(255,180,84)` | `Rgb(224,180,90)` / `Rgb(255,205,102)` | `Rgb(207,144,0)` / `Rgb(235,164,0)` |
| blue (4/12) | `Rgb(78,171,224)` / `Rgb(89,194,255)` | `Rgb(101,183,224)` / `Rgb(115,208,255)` | `Rgb(30,144,202)` / `Rgb(34,164,230)` |
| magenta (5/13) | `Rgb(185,146,224)` / `Rgb(210,166,255)` | `Rgb(196,168,224)` / `Rgb(223,191,255)` | `Rgb(143,107,180)` / `Rgb(163,122,204)` |
| cyan (6/14) | `Rgb(131,202,179)` / `Rgb(149,230,203)` | `Rgb(131,202,179)` / `Rgb(149,230,203)` | `Rgb(67,168,135)` / `Rgb(76,191,153)` |
| white (7/15) | `Rgb(191,189,182)` / `Rgb(255,255,255)` | `Rgb(204,202,194)` / `Rgb(255,255,255)` | `Rgb(252,252,252)` / `Rgb(255,255,255)` |

**No-gap + value-pinning tests.** A `#[cfg(test)]` module asserts (a) every theme
in `builtin_themes()` resolves all `ALL_ROLES` and all 16 ANSI entries, and
(b) a representative set of exact `Color::Rgb` values for Ayu Dark
(`Background == Rgb(13,16,23)`, `Accent == Rgb(230,180,80)`, `Error == Rgb(217,87,87)`,
`ansi(1) == Rgb(211,99,106)`, `ansi(9) == Rgb(240,113,120)`), so a wrong palette
fails the suite rather than passing a presence-only check.

**Properties that make this safe:** themes are immutable and built once (no IO on
the render path); `get` panics only on a missing role, exercised by the no-gap
test; no external color dependency; the enum + HashMap + `[Color;16]` shape is
extensible (a future user-defined theme is a new constructor with the same test
coverage).

## 0002 — Render Module Migration

`ui.rs` has 180 literal `Color::` sites; `ansi.rs` has 11 (in `apply_sgr` and
`diff_line_style`); `selection.rs` and `markup.rs` have **none**. The migration
swaps `ui.rs`/`ansi.rs` literals for theme lookups, and (per the locked
decision) re-themes selection. **This changes the rendered colors to the Ayu
palette — an intended, visible change, not byte-for-byte parity.** Tests that
assert the old named colors are updated to assert the theme-resolved values
(enumerated in TASKS.md per task).

**Add `active_theme` to `App`.** `crates/makina/src/app.rs` (`pub struct App`,
line 1004) gains `pub active_theme: crate::theme::Theme,`, initialized to
`Theme::ayu_dark()` in `App::new` (app.rs:1472) and `App::with_config`
(app.rs:1550). Added with a default, not a constructor parameter, so existing
construction sites are untouched; startup (0004) overrides it from `GlobalConfig`.

**Replace `Color::` in `ui.rs` by intent.** Each of the 180 sites becomes
`app.active_theme.get(ThemeRole::*)` — locally, no surrounding refactor:

```rust
// Before: Style::default().bg(Color::Blue).fg(Color::White)  (title bar)
let title = Paragraph::new(title_text).style(
    Style::default()
        .bg(app.active_theme.get(ThemeRole::Info))       // was Color::Blue
        .fg(app.active_theme.get(ThemeRole::Foreground)) // was Color::White
        .add_modifier(Modifier::BOLD),
);
```

Intent table: `White → Foreground`, `DarkGray`/`Gray` → `Dim`,
`Green → Success`, `Yellow → Warning`, `Red → Error`, `Black → Background`,
`Cyan → Accent`, `Blue → Info`, `Magenta → Accent`. Where a single named color
serves two distinct intents, pick the role matching the site's context comment
(e.g. a "modified" marker → `Info`, a task-id → `Accent`). `render(&app, …)` has
`app` in scope; helper fns that lack it take `&app.active_theme` (or the resolved
`Color`) as a parameter.

**Thread the theme into the ANSI parser and diff styler.**
`parse_ansi(input)` (ansi.rs:36) and `apply_sgr(style, params)` (ansi.rs:97) gain
a `&Theme`; `apply_sgr` maps SGR `30..=37`/`40..=47`/`90..=97`/`100..=107` onto
`theme.ansi(..)` (so red/green/blue stay distinct), with `39`/`49` resetting to
default and `0`/`1` unchanged. `diff_line_style(line)` (ansi.rs:128) gains a
`&Theme` and maps `+`→`Success`, `-`→`Error`, `@@`→`Info`:

```rust
fn apply_sgr(mut style: Style, params: &str, theme: &crate::theme::Theme) -> Style {
    match params {
        "0" | "" => style = Style::default(),
        "1" => style = style.add_modifier(Modifier::BOLD),
        n if (30..=37).contains(&n.parse().unwrap_or(-1)) =>
            style = style.fg(theme.ansi(n.parse::<usize>().unwrap() - 30)),
        // 90..=97 → ansi(8 + n-90); 40..=47 → bg(ansi(n-40)); 100..=107 → bg(ansi(8+n-100));
        // 39/49 → reset fg/bg; _ => {}  (256/truecolor SGR out of scope, see SCOPE.md)
        _ => {}
    }
    style
}
```

The theme reaches these via `diff_overlaid_content_line(text_line)` (ui.rs:2059,
which calls `parse_ansi`/`diff_line_style` ~ui.rs:2062): give it a `&Theme` param
and pass `&app.active_theme` at its caller `exchange_entry_lines(entry, app, width)`
(call site ~ui.rs:2213, where `app` is in scope).

**Re-theme selection (replaces REVERSED).** `selection.rs::highlight` (line 135)
today is `Style::default().add_modifier(Modifier::REVERSED)` (line 139). Per the
locked decision it becomes themed:

```rust
// Selection now paints explicit theme colors (was Modifier::REVERSED).
pub fn highlight(&self, buf: &mut Buffer, theme: &crate::theme::Theme) {
    if self.is_empty() { return; }
    let style = Style::default()
        .bg(theme.get(crate::theme::ThemeRole::SelectionBg))
        .fg(theme.get(crate::theme::ThemeRole::Foreground));
    // .. unchanged per-cell painting within bounds ..
}
```

The single call site is `sel.highlight(frame.buffer_mut())` (ui.rs:734) →
`sel.highlight(frame.buffer_mut(), &app.active_theme)`. The module doc
(selection.rs:129–134) is rewritten to describe themed colors, and the two tests
(`highlight_reverses_only_cells_within_bounds` at selection.rs:301,
`highlight_skips_empty_selection` at 330) are updated to assert the
`SelectionBg`/`Foreground` cells instead of `Modifier::REVERSED`.

**Tests updated in 0002 (named-color assertions → theme-resolved).** `ui.rs`:
4029, 4060, 4085, 4134, 4173, 4354, 4932/4939/4946, 5280/5372/5376, 5557, 5636,
5734, 6454. `ansi.rs`: `green_sgr_sets_green_foreground` (148),
`red_sgr_sets_red_foreground` (156), `reset_sgr_returns_to_default_style` (172),
`diff_line_style_colors_prefixes` (195). `selection.rs`: 301, 330.

**Properties that make this safe:** each replacement is a local substitution;
`ansi.rs`/`selection.rs` change only the three named functions; the value-pinning
test (0001) proves each role/ANSI entry resolves; the migration's intended
visual change is captured by updating the enumerated tests to the theme-resolved
colors (no false "renders identically" claim).

## 0003 — Command Palette Theme Switcher

The palette renders a flat list from `CommandPalette::default_actions()`
(app.rs:447 — **7** actions today: Open task list, Configure providers & roles,
Settings, Doctor, Retry failed task, Discover project, Quit). `PaletteAction` is a
`struct { label: &'static str, event: AppEvent }` (app.rs:424); Enter dispatches
the action's `event` and closes the palette. There is no nested sub-list.

**Edits:**

**`PaletteAction` struct → enum.** (app.rs:424)

```rust
#[derive(Debug, Clone)]
pub enum PaletteAction {
    Regular { label: &'static str, event: AppEvent },
    NestedThemeSelector { label: &'static str },
}
impl PaletteAction {
    pub fn label(&self) -> &str {
        match self { PaletteAction::Regular { label, .. } | PaletteAction::NestedThemeSelector { label } => label }
    }
}
```

**Migrate every field-access site** (the enum makes these hard compile errors —
all must be fixed): `CommandPalette::filtered()` `action.label` (app.rs:488) →
`action.label()`; `render_command_palette` `action.label` (ui.rs:2508) →
`action.label()`; the `CommandPaletteExecute` handler `action.event.clone()`
(event.rs:393) → a `match` that dispatches `Regular { event, .. }` and, for
`NestedThemeSelector`, enters theme mode instead of dispatching.

**Track nested mode + register the action.** Add
`pub theme_selector: Option<Vec<String>>` to `CommandPalette` (None everywhere it
is constructed), and append `PaletteAction::NestedThemeSelector { label: "Switch theme" }`
to `default_actions()` (now **8** actions).

**Enter nested mode / apply live / exit.** On Enter over `NestedThemeSelector`:
`palette.theme_selector = Some(Theme::builtin_themes().iter().map(|t| t.name.clone()).collect())`,
clear filter, `selected = 0`, keep the palette open. On Enter inside nested mode:
find the selected name in `builtin_themes()`, assign to `self.active_theme`, clear
`theme_selector`, return `true` (redraw). On Esc in nested mode: clear
`theme_selector` (back to actions); a second Esc closes the palette.

**Filter + render branch on mode.** When `theme_selector.is_some()`, filtering and
`render_command_palette` (ui.rs:2468) operate on the theme names; otherwise the
existing action path. `render_command_palette` takes `(palette, frame, area)` (no
`&App`) and reads `palette.theme_selector`, so no signature change is needed
beyond the `.label()` swap.

**Tests.** Update `palette_filter_narrows_and_clamps_selection` (app.rs:6389):
expect 8 actions (`len() == 8` at ~6395) and fix the stale "7 default actions"
comment and `filtered[0].label` (~6410) → `filtered[0].label()`. Add nested-mode
tests: entering `NestedThemeSelector` sets `theme_selector = Some(names)` and
keeps the palette open; selecting `"Ayu Mirage"` sets `app.active_theme.name ==
"Ayu Mirage"` and clears `theme_selector`; Esc clears nested mode without
changing the theme; typing `mirage` narrows the nested list to one.

**Properties that make this safe:** live switch is a single `active_theme`
mutation + redraw (no event re-dispatch); the palette stays open across
selections; only `NestedThemeSelector` uses the nested path, so regular actions
keep close-on-Enter; persistence is 0004, so this workstream lands without a
disk-write dependency.

## 0004 — Theme Persistence & Startup

`GlobalConfig` (`crates/makina-core/src/config.rs`) holds `providers`/`roles`,
serializes to `{repo_root}/.makina/config.toml`, and already uses
`#[serde(default)]` widely. It has no theme preference; a theme switched via 0003
is ephemeral.

**Edits:**

**Add a defaulted `theme_name`.** (config.rs)

```rust
/// Active theme name (e.g. "Ayu Dark"). Absent or unknown ⇒ "Ayu Dark".
#[serde(default = "default_theme_name")]
pub theme_name: String,

fn default_theme_name() -> String { "Ayu Dark".to_string() }
```

**Restore on startup — clone the name before `config` is moved.** In `main.rs`
the `config` value (bound ~main.rs:52) is **moved** into `CoreApi` at ~main.rs:277,
so it is out of scope afterward. Clone `theme_name` alongside the existing
`*_for_app` clones (≈ lines 266–269, before the move), then resolve after
`App::with_config` (≈main.rs:284):

```rust
let theme_name_for_app = config.theme_name.clone();   // beside the other *_for_app clones, pre-move
// .. config moved into CoreApi (~main.rs:277); App::with_config (~main.rs:284) ..
let active_theme = makina::theme::Theme::builtin_themes()
    .into_iter()
    .find(|t| t.name == theme_name_for_app)
    .unwrap_or_else(makina::theme::Theme::ayu_dark);   // stale/renamed name ⇒ Ayu Dark, no panic
app.active_theme = active_theme;
```

**Persist on commit via the merge-writer.** Add `commit_theme_selection` in
`event.rs`, modeled on `commit_settings` (event.rs:571):

```rust
async fn commit_theme_selection(app: &App, theme_name: &str) -> Option<String> {
    // validate name ∈ builtin_themes() (else Some("Unknown theme"));
    // read existing GlobalConfig from makina_core::paths::config_file(&app.repo_root) (default on miss);
    let updated = GlobalConfig { theme_name: theme_name.to_string(), ..existing };
    // toml::to_string_pretty, ensure parent dir, tokio::fs::write; Some("Theme saved") | Some(err)
}
```

The nested-selector Enter (0003) calls it and surfaces the returned string as a
status message.

**Validation probe.** `theme-validation-test` asserts (automated only): the
workspace test suite is green; a headless `TestBackend` render under
`Theme::ayu_mirage()` shows a Mirage-resolved cell distinct from Ayu Dark; and the
value-pinning, no-gap, startup-fallback, persistence, nested-selector, and
ANSI-mapping tests all run under `cargo test`. No manual-only criteria (the
headless reviewer has no TTY).

**Properties that make this safe:** the `GlobalConfig` addition is
backward-compatible (`#[serde(default)]`); startup resolves an unknown name to Ayu
Dark with no crash; the writer round-trips via `..existing`, preserving
providers/roles; `theme_name` is a single string key in the existing config.toml.

## Test strategy

- **0001 (theme core).** `themes_define_every_role_and_ansi_entry` (presence) and
  `ayu_dark_pins_expected_values` (exact `Color::Rgb` for a representative role +
  ANSI set) — a missing or wrong color fails the suite, never a render-time panic.
- **0002 (render migration).** After migration, a grep confirms no production
  `Color::` literal remains in `ui.rs`/`ansi.rs` (remaining matches are in
  `#[cfg(test)]` and now reference `th.get(...)`/`th.ansi(...)`). The enumerated
  `ui.rs`/`ansi.rs`/`selection.rs` tests are updated to assert theme-resolved
  colors. An `ansi.rs` test feeds `apply_sgr`/`parse_ansi` a `&Theme` and asserts
  distinct codes map to distinct `theme.ansi(..)` entries (red ≠ green). A
  `selection.rs` test asserts `highlight` paints `SelectionBg`/`Foreground`.
- **0003 (palette switcher).** Tests: entering `NestedThemeSelector` opens the
  nested list and keeps the palette open; selecting a name mutates
  `app.active_theme` and clears `theme_selector`; Esc clears nested mode without
  changing the theme; filtering narrows the nested list; the action count is 8.
- **0004 (persistence & startup).** `globalconfig_deserializes_without_theme_name`
  (default "Ayu Dark"); `commit_theme_selection_writes_to_config` (round-trips
  `theme_name`, preserves providers/roles, rejects unknown names); a resolver test
  for the startup hit/fallback; a `TestBackend` render under Mirage.
- All tests keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
