# Makina Plan 0023 — Command Palette & Settings Screen

Add a `Ctrl+P` **command palette** — one discoverable modal that lists and
fuzzy-filters every TUI action — and a **settings screen** reached from it that
edits the run caps (`gate_iterations`, `reviewer_iterations`, `wall_clock_secs`,
`idle_secs`, `concurrency`) and writes them back to `config.toml`
merge-preservingly via plan 0011's `GlobalConfig` config-writer pattern.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0069 — Command palette

### command-palette-state — `Mode::CommandPalette`, `CommandPalette` state, and AppEvents

Give `App` the state the palette needs and the pure handlers that mutate it.

**Steps:**

1. In `crates/makina/src/app.rs`, add `CommandPalette` to the `pub enum Mode`
   (next to `ProviderConfig`/`Doctor`). Add
   `pub struct PaletteAction { pub label: &'static str, pub event: AppEvent }`
   (`#[derive(Debug, Clone)]`) and
   `pub struct CommandPalette { pub filter: String, pub actions: Vec<PaletteAction>,
   pub selected: usize }`.

2. Add `impl CommandPalette` with
   `pub fn default_actions() -> Vec<PaletteAction>` listing, in order:
   `Open task list` → `AppEvent::OpenBrowser`, `Configure providers & roles` →
   `AppEvent::OpenProviderEditor`, `Settings` → `AppEvent::OpenSettings`,
   `Doctor` → `AppEvent::OpenDoctor`, `Retry failed task` →
   `AppEvent::RetryFocusedTask`, `Discover project` → `AppEvent::DiscoverProject`,
   `Quit` → `AppEvent::Quit`; and
   `pub fn filtered(&self) -> Vec<&PaletteAction>` (case-insensitive substring of
   `self.filter` against `label`; the whole list when `filter` is empty).

3. Add `pub command_palette: Option<CommandPalette>` to `App` (init `None` in
   `App::new`; `App::with_config` inherits it via `Self::new`) and
   `pub fn is_command_palette(&self) -> bool { self.mode == Mode::CommandPalette }`.

4. Add `AppEvent` variants: `OpenCommandPalette`, `CommandPaletteUp`,
   `CommandPaletteDown`, `CommandPaletteInput(char)`, `CommandPaletteBackspace`,
   `CommandPaletteExecute`, `CloseCommandPalette`, plus `OpenSettings`,
   `RetryFocusedTask`, `DiscoverProject`. Handle them in `App::update`:
   - `OpenCommandPalette` → `command_palette = Some(CommandPalette { filter: "".into(),
     actions: CommandPalette::default_actions(), selected: 0 })`,
     `mode = Mode::CommandPalette`.
   - `CommandPaletteInput(c)` → push `c` onto `filter`, clamp `selected` to
     `filtered().len().saturating_sub(1)`. `CommandPaletteBackspace` → `filter.pop()`,
     re-clamp.
   - `CommandPaletteUp`/`Down` → move `selected` within `0..filtered().len()`
     (saturating).
   - `CommandPaletteExecute` → `mode = Mode::Normal`, `command_palette = None`
     (the re-dispatch of the chosen `event` is done in `event.rs`, task
     `command-palette-keys`); return `true`.
   - `CloseCommandPalette` → `mode = Mode::Normal`, `command_palette = None`.
   - `RetryFocusedTask` / `DiscoverProject` → set `status_message` to
     `"retry not yet available (plan 0017)"` /
     `"project discovery not yet available (plan 0025)"`; return `true`.
   - `OpenSettings` is handled by task `settings-state` (workstream 0070); add a
     placeholder arm now only if needed for exhaustiveness, finalised there.

5. Add tests in `app.rs`:

   ```rust
   #[test]
   fn open_command_palette_sets_mode_and_seeds_actions() { /* update(OpenCommandPalette) => mode==CommandPalette, command_palette.is_some(), default_actions non-empty, selected==0 */ }
   #[test]
   fn palette_filter_narrows_and_clamps_selection() { /* type "doc" via CommandPaletteInput => filtered() == [Doctor]; selected clamped to 0; backspace restores the full list */ }
   #[test]
   fn palette_execute_and_close_return_to_normal() { /* CommandPaletteExecute and CloseCommandPalette each set mode==Normal and command_palette==None */ }
   ```

- **Depends on:** —
- **Done when:** the three tests pass; `Mode::CommandPalette`, `CommandPalette`,
  `PaletteAction`, the `is_command_palette` helper, and the listed `AppEvent`
  variants exist and behave as specified; `filtered()` narrows case-insensitively
  and `selected` stays in range; cargo test/clippy/fmt green.

### command-palette-keys — Bind `Ctrl+P`, route the keymap, re-dispatch on Enter

Wire keys so `Ctrl+P` opens the palette, typing filters it, and `Enter` runs the
selected action through the normal intent path.

**Steps:**

1. In `crates/makina/src/event.rs` `translate_key`, after the existing `Ctrl+C`
   guard (`key.code == KeyCode::Char('c') && key.modifiers.contains(
   KeyModifiers::CONTROL)`), add a `Ctrl+P` guard returning
   `AppEvent::OpenCommandPalette` (`KeyCode::Char('p')` +
   `KeyModifiers::CONTROL`). It must be checked **before** the per-mode cascade so
   it opens from Normal mode.

2. Add a `command_palette: bool` parameter to `translate_key` and thread it from
   the `translate_terminal_event` call site (derive it from `app.is_command_palette()`,
   alongside the existing `browsing`/`editing_providers`/`viewing_doctor` flags).
   Add a `} else if command_palette {` branch in the cascade: `Esc`→
   `CloseCommandPalette`, `Enter`→`CommandPaletteExecute`, `Up`→`CommandPaletteUp`,
   `Down`→`CommandPaletteDown`, `Backspace`→`CommandPaletteBackspace`,
   `Char(c)`→`CommandPaletteInput(c)`, `_`→`Tick`. (Use `Up`/`Down` only, not
   `j`/`k`, so letters feed the filter.)

3. In `resolve_io` (`event.rs`), handle `AppEvent::CommandPaletteExecute`: read
   the selected action's `event` from `app.command_palette` (via `filtered()` +
   `selected`) **before** `App::update` clears the palette, and resolve to that
   inner `AppEvent` so the event loop processes it exactly as a fresh intent
   (`OpenBrowser`/`OpenSettings`/`OpenDoctor`/`OpenProviderEditor`/`Quit`/the
   stubs). Keep this the single execution path — do not duplicate the open logic.

4. In `crates/makina/src/ui.rs`, extend the `status_bar` `Paragraph` hint string
   to advertise the palette (prepend `[^P] cmds  ` to the existing ` [o] open  …`
   literal).

5. Add tests in `event.rs` (mirror the existing `key_press` helper + translate
   tests):

   ```rust
   #[test]
   fn ctrl_p_opens_palette() { /* translate_key(key_press(Char('p'), CONTROL), … all-false flags …) == AppEvent::OpenCommandPalette */ }
   #[test]
   fn palette_enter_executes_selected_action() { /* with command_palette=true, Enter => CommandPaletteExecute; then resolve_io on an App whose palette selects "Settings" resolves to AppEvent::OpenSettings */ }
   #[test]
   fn palette_esc_closes() { /* with command_palette=true, Esc => CloseCommandPalette */ }
   ```

- **Depends on:** command-palette-state
- **Done when:** the three tests pass; `Ctrl+P` opens the palette from Normal
  mode; typing filters, `Up`/`Down` move the selection, `Enter` re-dispatches the
  selected action through the normal path, and `Esc` closes; the status bar shows
  `[^P]`; cargo test/clippy/fmt green.

### command-palette-render — Draw the centered palette modal

Render the palette as a modal mirroring `render_provider_editor`.

**Steps:**

1. In `crates/makina/src/ui.rs`, add
   `fn render_command_palette(palette: &crate::app::CommandPalette, frame: &mut
   Frame, area: Rect)`: `centered_rect(60, 60, area)`, `frame.render_widget(Clear,
   popup)`, a `Block` with `BorderType::Thick` + cyan border titled
   ` Command Palette `, then a vertical `Layout` of
   `[Length(3) filter, Min(0) list, Length(2) footer]`.

2. Render the filter line as `> {palette.filter}▏`; build one `ListItem` per
   `palette.filtered()` entry showing its `label`, highlighting the
   `palette.selected` row with the same cyan/yellow highlight style
   `render_provider_editor` uses for its focused row; render the footer hint
   `↑/↓ select · Enter run · Esc close`.

3. In the overlay section at the bottom of `render` (where
   `render_provider_editor` / `render_doctor` are dispatched), add
   `if app.is_command_palette() && let Some(p) = app.command_palette.as_ref() {
   render_command_palette(p, frame, area); }`, drawn so it sits above the normal
   layout.

4. Add a test in `ui.rs`:

   ```rust
   #[test]
   fn palette_renders_filtered_actions() { /* App with mode=CommandPalette, filter="set"; render to a test Buffer; assert it contains "Command Palette" and the "Settings" label and not the filtered-out "Doctor" label */ }
   ```

- **Depends on:** command-palette-state
- **Done when:** the test passes; opening the palette draws a centered modal with
  a filter input, the filtered action list (selected row highlighted), and a
  footer hint; it overlays the normal layout; cargo test/clippy/fmt green.

---

## 0070 — Settings screen

### settings-state — `Mode::Settings`, `Settings` state, and caps on `App`

Give `App` the resolved caps to display and the settings modal state.

**Steps:**

1. In `crates/makina/src/app.rs`, add to `App`:
   `pub caps: makina_core::config::CapsConfig` and `pub concurrency: usize`
   (default `CapsConfig::default()` / `3` in `App::new`). In `App::with_config`,
   set them from the resolved config; update `crates/makina/src/main.rs`'s
   `App::with_config(...)` call to pass `config.caps.clone()` and
   `config.concurrency` (add the corresponding params to `with_config`). This plan
   adds **no** `makina-core` config schema — it edits only existing
   `CapsConfig`/`GlobalConfig` fields.

2. Add `Mode::Settings`; `pub enum SettingsField { GateIterations,
   ReviewerIterations, WallClockSecs, IdleSecs, Concurrency }`
   (`#[derive(Debug, Clone, Copy, PartialEq, Eq)]`); and
   `pub struct Settings { pub gate_iterations: String, pub reviewer_iterations:
   String, pub wall_clock_secs: String, pub idle_secs: String, pub concurrency:
   String, pub focused: SettingsField, pub error: Option<String> }`. Add
   `pub settings: Option<Settings>` to `App` (init `None`) and
   `pub fn is_settings(&self) -> bool { self.mode == Mode::Settings }`.

3. Add the `AppEvent::OpenSettings` handler in `App::update`: seed `settings` from
   the App's caps (`self.caps.gate_iterations.to_string()`, …; `idle_secs` ⇒ `""`
   when `None`) and `concurrency`; `focused = SettingsField::GateIterations`;
   `error = None`; `mode = Mode::Settings`.

4. Add a test in `app.rs`:

   ```rust
   #[test]
   fn open_settings_lists_current_caps() { /* App with caps {gate_iterations:7, idle_secs:None, …}, concurrency:4; update(OpenSettings) => settings.gate_iterations=="7", settings.idle_secs=="" , settings.concurrency=="4", mode==Settings */ }
   ```

- **Depends on:** command-palette-state
- **Done when:** the test passes; `App` exposes the resolved `caps` /
  `concurrency`; `OpenSettings` seeds the `Settings` modal from them; no new
  `makina-core` config schema is introduced; cargo test/clippy/fmt green.

### settings-edit-commit — Navigate, edit, validate, and persist settings

Make the fields editable, validate like `Config::validate`, and write
`config.toml` merge-preservingly.

**Steps:**

1. In `crates/makina/src/app.rs`, add `AppEvent`s `SettingsUp`, `SettingsDown`,
   `SettingsInput(char)`, `SettingsBackspace`, `CloseSettings`,
   `SettingsCommit`, and handle them in `App::update`:
   - `SettingsUp`/`SettingsDown` → cycle `settings.focused` through the
     `SettingsField` order.
   - `SettingsInput(c)` → when `focused` is a numeric field and `c.is_ascii_digit()`,
     append to that field's buffer; ignore otherwise. `SettingsBackspace` → pop the
     focused numeric buffer. After each edit, re-validate the focused field and set
     `settings.error` to the matching `Config::validate` reason
     (`caps.gate_iterations must be at least 1`, etc.) or a
     `"… must be a positive integer"` parse error, else `None`.
   - `CloseSettings` → `mode = Mode::Normal`, `settings = None` (no write).
   - `SettingsCommit` → on the parsed values, apply them back to `self.caps` /
     `self.concurrency` / `self.idle_secs_config` (`""` ⇒ `None`), then
     `mode = Mode::Normal`, `settings = None` (the disk write happens in
     `event.rs`, below); if validation fails, keep the modal open with
     `settings.error` set and do **not** mutate `self`.

2. In `crates/makina/src/event.rs`, add `async fn commit_settings(app: &App) ->
   Option<String>` mirroring `commit_provider_config`: parse + validate every
   field (returning `Some(reason)` and writing nothing on the first failure, using
   the same predicates as `Config::validate`); read
   `config_file(&app.repo_root)` into a `GlobalConfig` (or default); rebuild it
   preserving all other fields (`..existing_global`) with the new
   `CapsConfig`/`concurrency`; `toml::to_string_pretty`; `create_dir_all` the
   parent; write; return `Some("Settings saved")` or the error message. Wire
   `AppEvent::SettingsCommit` in `resolve_io` to call it and surface the result as
   a `StatusMessage`, exactly as `ProviderEditorCommit` calls
   `commit_provider_config`. (Like `commit_provider_config`, this writer round-trips
   the file through `GlobalConfig`, which has no `gates`/`base_branch` field, so the
   `ProjectConfig` `[[gates]]` / `base_branch` tables are **not** preserved through
   it; the gates-aware project-config writer lands in plan 0025. This plan writes
   only `GlobalConfig` caps + concurrency.)

3. In `crates/makina/src/event.rs` `translate_key`, thread a `settings: bool` flag
   (from `app.is_settings()`) and add a Settings keymap branch: `Esc`→
   `CloseSettings`, `Enter`→`SettingsCommit`, `Up`→`SettingsUp`, `Down`→
   `SettingsDown`, `Backspace`→`SettingsBackspace`, `Char(c)`→`SettingsInput(c)`,
   `_`→`Tick`.

4. Add tests:

   ```rust
   // in app.rs
   #[test]
   fn invalid_value_rejected() { /* seed Settings, focus GateIterations, clear buffer + input '0'; assert settings.error == Some("caps.gate_iterations must be at least 1"); SettingsCommit keeps the modal open and does not mutate app.caps */ }
   // in event.rs
   #[test]
   async fn edit_and_commit_writes_config() { /* repo_root = tempdir; pre-write a config.toml with an unrelated GlobalConfig section (e.g. a [[providers]] / [roles] entry); drive settings input + resolve_io(SettingsCommit); re-read config.toml => new caps applied AND the unrelated GlobalConfig section preserved. (Do NOT assert a [[gates]] entry survives: it rides ProjectConfig, which this GlobalConfig writer does not round-trip — plan 0025's writer owns gates.) */ }
   ```

- **Depends on:** settings-state
- **Done when:** both tests pass; settings fields are navigable and editable;
  invalid values are rejected at the field with the same reason
  `Config::validate` emits and never written; a valid commit writes
  `{repo_root}/.makina/config.toml` merge-preservingly (caps + concurrency written;
  the other `GlobalConfig` fields — providers, roles — preserved; `ProjectConfig`
  gates/base_branch are **not** touched by this writer and are owned by plan 0025)
  and reports `Settings saved`; `Esc` cancels without writing; cargo
  test/clippy/fmt green.

### settings-render — Draw the settings modal

Render the settings screen as a modal mirroring `render_provider_editor`.

**Steps:**

1. In `crates/makina/src/ui.rs`, add
   `fn render_settings(settings: &crate::app::Settings, frame: &mut Frame, area:
   Rect)`: `centered_rect(70, 70, area)`, `Clear`, a `Thick` cyan `Block` titled
   ` Settings `, and a vertical `Layout` of `[Min(0) list, Length(2) footer]`.

2. Render one row per field — `  Gate iterations: {v}`, `  Reviewer iterations:
   {v}`, `  Wall-clock (s): {v}`, `  Idle (s): {v or "—"}`, `  Concurrency: {v}` —
   highlighting the `settings.focused` row and showing a trailing `▏` cursor on the
   focused numeric field. When `settings.error` is `Some`, render it in red beneath
   the list. Footer hint:
   `↑/↓ field · 0-9 edit · Enter save · Esc cancel`.

3. In the overlay section of `render`, add
   `if app.is_settings() && let Some(s) = app.settings.as_ref() {
   render_settings(s, frame, area); }`.

4. Add a test in `ui.rs`:

   ```rust
   #[test]
   fn settings_renders_current_values() { /* App mode=Settings, caps gate_iterations=7, concurrency=4; render; assert the buffer contains "Settings", "7", and "4" */ }
   ```

- **Depends on:** settings-edit-commit
- **Done when:** the test passes; the settings modal renders every field with its
  current value, highlights the focused field, shows the validation error in red
  when present, and overlays the normal layout; cargo test/clippy/fmt green.

---

**End of plan 0023 TASKS.** When every "Done when" bullet is green, `Ctrl+P`
opens one discoverable, fuzzy-filterable palette of every TUI action — ending the
scramble for scarce one-key chords — and its **Settings** entry opens a modal that
reads the loaded run caps and writes edited caps and concurrency back to
`config.toml` merge-preservingly (via the `GlobalConfig` writer), all without
leaving the app.
