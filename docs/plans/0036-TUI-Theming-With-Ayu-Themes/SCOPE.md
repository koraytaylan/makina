# Scope — Plan 0036

> Introduce a semantic theme abstraction to the Makina TUI render path, shipping three built-in Ayu variants (dark/mirage/light) sourced from `github.com/ayu-theme/ayu-colors`, with Ayu Dark as the default, live switching from the command palette, and persistence via `GlobalConfig`.

## Why this plan

**1. TUI rendering hardcodes ratatui `Color::` variants with no semantic abstraction.** `ui.rs` directly uses ratatui `Color` variants (180 occurrences — verify via `grep -oE 'Color::[A-Za-z]+' crates/makina/src/ui.rs | sort | uniq -c`: roughly DarkGray 61, Cyan 28, White 21, Red 21, Yellow 21, Green 15, Black 7, Magenta 6, Blue 4, Gray 2; `crates/makina/src/ui.rs`), plus 11 in `ansi.rs` (`apply_sgr` + `diff_line_style`). Each render call is a literal `Color::White`/`Color::DarkGray`; there is no central mapping of intent (foreground, accent, success/warning/error) to color, no theme abstraction, and no way to swap palettes at runtime.

**2. User theming is impossible without recompilation.** Without a theme abstraction, users cannot select a color scheme at runtime — the palette is baked into the binary. A user who prefers a light theme or Ayu Mirage must edit Rust source, recompile, and restart.

**3. The Ayu palette is a de-facto standard but is not shipped.** Ayu is a widely-adopted theme family (VS Code, Sublime, many TUIs). The canonical, maintained source of truth is `github.com/ayu-theme/ayu-colors` (`themes/{dark,mirage,light}.yaml`), which defines Dark, Mirage, and Light. The Makina TUI renders in a hand-chosen aesthetic close to Ayu but exposes no variants and gives users no control.

**4. Palette switching needs a discoverable UI action.** The command palette (`CommandPalette::default_actions()`, `crates/makina/src/app.rs:424–478`) is the discoverable action surface. A "Switch theme" action lets users browse and apply themes live without a restart.

**5. The selected theme must persist across restarts.** A live switch is lost on exit unless written to disk. The selection is persisted to `GlobalConfig` (`{repo_root}/.makina/config.toml`) via the same merge-writer pattern as `commit_settings` (`crates/makina/src/event.rs:571`) and restored on the next startup.

**Why this plan solves it:** it introduces a `theme` module with a `ThemeRole` enum (Background, Foreground, Dim, Accent, SelectionBg, Border, Success, Warning, Error, Info) plus a 16-entry ANSI palette (`ansi: [Color; 16]`), and a `Theme` struct mapping each to a ratatui `Color`. It encodes three Ayu variants as precomputed `Color::Rgb` tables sourced from the `ayu-colors` YAML (resolved to sRGB, no OKLCH dependency; full table pinned in ARCHITECTURE.md). It replaces every production `Color::` site in `ui.rs`/`ansi.rs` with `app.active_theme.get(ThemeRole::*)`/`ansi(..)` lookups and re-themes selection. It adds a "Switch theme" palette action that lists built-in themes, applies the selection live by mutating `app.active_theme` and redrawing, and stores the selection in `GlobalConfig`. **Ayu Dark is the default theme**; this unblocks runtime theming and future user-defined themes.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0001–0004):

- **0001 — Theme Core Abstraction.** Define the `ThemeRole` enum (10 semantic roles) + a 16-entry ANSI palette, the `Theme` struct (`name`, role→`Color` map, `ansi: [Color; 16]`), and the three Ayu variants as precomputed `Color::Rgb` tables sourced from `ayu-colors` (no OKLCH dependency). Add `App.active_theme` (default Ayu Dark). Ship a no-gap + value-pinning test asserting every built-in theme defines every role and ANSI entry with the pinned values.
- **0002 — Render Module Migration.** Replace every production `Color::` in `ui.rs` (180) and `ansi.rs` (11; `apply_sgr` extended to the full SGR color range mapped onto `theme.ansi[..]`, `diff_line_style` themed) with theme lookups, and re-theme `selection.rs::highlight` (was `Modifier::REVERSED`) to `SelectionBg`/`Foreground`. The render colors change to the Ayu palette (intended); the enumerated existing tests that assert the old named colors are updated to assert the theme-resolved values. `markup.rs` is not touched — it has no `Color::` (it styles via the caller-supplied `base: Style`).
- **0003 — Command Palette Theme Switcher.** Convert `PaletteAction` (struct → enum with `Regular`/`NestedThemeSelector`), migrate the field-access sites (`filtered()`, `render_command_palette`, the `CommandPaletteExecute` handler), add a "Switch theme" action that opens a nested, filterable theme list, applies the selection live by mutating `app.active_theme`, and keeps the palette open.
- **0004 — Theme Persistence & Startup.** Add `theme_name` to `GlobalConfig` (defaulted, backward-compatible), commit it on selection via a `commit_theme_selection` merge-writer modeled on `commit_settings`, restore it on startup (cloning the name before `config` is moved into `CoreApi`), and fall back to Ayu Dark on a missing/unknown name. A final automated validation probe gates the plan.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| TUI hardcodes ratatui `Color::` variants (180 in `crates/makina/src/ui.rs`, 11 in `crates/makina/src/ansi.rs`). | `0001`, `0002` |
| User theming / live switching impossible without recompilation (`crates/makina/src/app.rs:1004`). | `0001`, `0003` |
| Ayu palette (three variants) is standard but unshipped (`github.com/ayu-theme/ayu-colors` `themes/{dark,mirage,light}.yaml`). | `0001`, `0002` |
| Command palette enables discoverable switching (`crates/makina/src/app.rs:424–478`, `render_command_palette` at `crates/makina/src/ui.rs:2468`). | `0003` |
| `GlobalConfig` merge-writer establishes the persistence pattern (`crates/makina/src/event.rs:571`, `crates/makina-core/src/config.rs`). | `0004` |

## Locked decisions

- **Source of truth is `github.com/ayu-theme/ayu-colors`.** Colors come from that repo's `themes/{dark,mirage,light}.yaml` (`palette`/`surface`/`editor`/`ui`/`common`/`vcs`/`terminal` blocks), resolved to concrete `Color::Rgb` and pinned in ARCHITECTURE.md. (The older `ayu-theme/ayu-theme` editor repo holds the legacy/classic palette and is **not** used.)
- **Semantic roles via enum + HashMap, plus an explicit ANSI-16 array.** Ten semantic roles cover UI intent; a `[Color; 16]` array carries the ANSI passthrough palette so agent output keeps distinct ANSI colors. A missing role is a hard error (panic) caught by the value-pinning test, not a silent fallback.
- **sRGB hex→`Color::Rgb`, no external color library.** Values are pinned literals (resolved offline from the YAML), keeping the module dependency-free. `SelectionBg`/`Border` flatten the spec's alpha colors over the background; ANSI `normal` = the spec hue darkened to match the YAML `terminal:` `-L` step, `bright` = the base hue.
- **Ayu Dark is the default theme; the rendered colors change (no byte-for-byte parity).** Migrating from the terminal's named palette (`Color::Cyan` etc.) to fixed `Color::Rgb` is an intended, visible change. Tests asserting the old named colors are updated to assert the theme-resolved values; the plan makes **no** "renders identically" claim.
- **Full 16-color ANSI passthrough.** `apply_sgr` is extended to handle SGR `30–37`/`40–47`/`90–97`/`100–107` mapped onto `theme.ansi[..]` (red, green, blue stay distinct), with `39`/`49` resetting to default. `diff_line_style` maps `+`/`-`/`@@` to `Success`/`Error`/`Info`.
- **Themed selection colors (replaces `Modifier::REVERSED`).** `selection.rs::highlight` moves from reverse-video to explicit `SelectionBg`/`Foreground`; its doc and two tests are updated accordingly.
- **Live switching via `app.active_theme` mutation, no restart.** Selecting a theme mutates `app.active_theme` and the next render applies it — no event re-dispatch, no restart.
- **Theme switcher stays open; palette persists across selections.** The "Switch theme" action opens a nested list that stays open; Esc exits nested mode, a second Esc closes the palette.
- **Persistence via `GlobalConfig` merge-writer; backward-compatible.** `theme_name` is stored in `{repo_root}/.makina/config.toml`; the writer preserves all other fields; a missing/invalid name falls back to Ayu Dark on startup.
- **Three built-in Ayu themes only; user themes deferred.** Extensible (`builtin_themes()` can grow), but loading user-defined themes is out of scope.

## Out of scope

- User-defined themes (TOML/JSON files or an in-app editor). Built-in Ayu variants only; a follow-on plan can load custom themes from `{repo_root}/.makina/themes/`.
- 256-color and truecolor SGR escapes (`38;5;n`, `38;2;r;g;b`, and the `48;…` background forms). `apply_sgr` handles the standard 16-color SGR range; extended-color parsing is a follow-on.
- Syntax highlighting of Markdown code blocks (language tokenization). Deferred from plan 0020; reused as-is. Theme colors apply to the overall UI and agent output, not to code-block syntax.
- System dark/light auto-switching. Manual selection via the palette is sufficient for v1.
- Per-role sub-customization (separate colors for warning-text vs. warning-badge). The 10 roles + ANSI-16 cover the primary intent hierarchy.
- Integration of theme selection into the settings screen. Theme switching is a dedicated palette action; settings integration can follow.
- Hot-reloading themes from disk. Themes are built-in and immutable at runtime in this plan.
- Colorblind-friendly / high-contrast variants. A separate accessibility-focused plan.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits and the full color table.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
