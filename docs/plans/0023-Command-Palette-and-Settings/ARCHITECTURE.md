# Architecture — Plan 0023

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). This plan touches only the `makina` (TUI) crate; it adds no
> `makina-core` config schema.

## Current shape (what exists)

- **Modes** (`crates/makina/src/app.rs`, `pub enum Mode`): `Normal`,
  `FileBrowser`, `ProviderConfig` (plan 0011), `Doctor` (plan 0013). The active
  mode gates which keymap `event.rs` uses and which overlay `ui.rs` draws.
- **The modal pattern** (used by the provider editor): a state struct on `App`
  (`pub provider_editor: Option<ProviderEditor>`), an `is_*` helper
  (`App::is_editing_providers()` → `self.mode == Mode::ProviderConfig`), open/close/
  navigate/commit variants on `AppEvent` (`OpenProviderEditor`, `ProviderEditorUp`,
  `ProviderEditorDown`, `CloseProviderEditor`, `ProviderEditorCommit`), a
  mode-gated branch in `event.rs` `translate_key`, and a `render_*` overlay in
  `ui.rs` drawn last over the normal layout.
- **`translate_key`** (`crates/makina/src/event.rs`): signature
  `fn translate_key(key, browsing: bool, editing_providers: bool,
  viewing_doctor: bool, focused_panel: Panel) -> AppEvent`. It already handles one
  `CONTROL` chord — `Ctrl+C` → `AppEvent::Quit`
  (`key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)`)
  — *before* the per-mode `if browsing { … } else if editing_providers { … }`
  cascade. The Normal-mode arm binds `o`→`OpenBrowser`, `g`→`OpenProviderEditor`,
  `?`→`OpenDoctor`, `q`/`Esc`→`Quit`.
- **`translate_terminal_event`** (`event.rs`, the `CrosstermEvent::Key(key) =>
  translate_key(key, browsing, editing_providers, viewing_doctor, focused_panel)`
  call site near line 660) passes the per-mode booleans derived from `App`.
- **The config writer** (`event.rs` `async fn commit_provider_config(app: &App)
  -> Option<String>`): reads `{repo_root}/.makina/config.toml` via
  `makina_core::paths::config_file(&app.repo_root)`, parses it into a
  `GlobalConfig` (falling back to `GlobalConfig::default()`), rebuilds it preserving
  all unmanaged fields (`..existing_global`), `toml::to_string_pretty`s it, ensures
  the parent dir, and writes it. Reused verbatim-in-spirit by settings.
- **Modal render** (`ui.rs` `render_provider_editor` / `render_doctor`):
  `centered_rect(pct_x, pct_y, area)` → `frame.render_widget(Clear, popup)` →
  a `Block` with `Thick` cyan borders + title → a vertical `Layout` of
  `[Min(0) list, Length(2) footer]`, list rows as `ListItem`s with the focused row
  styled, and a footer hint line.
- **Status bar** (`ui.rs`, the `status_bar` `Paragraph`): the literal hint string
  ` [o] open  [s/p/c] start/pause/cancel  [Tab] panel  [v] view  [L] log  [?]
  doctor  `.
- **App caps today** (`app.rs`): the App holds only `idle_secs_config:
  Option<u64>` and `wall_clock_secs_config: u64` (plan 0015, for the live-activity
  header); it does **not** yet hold `gate_iterations`, `reviewer_iterations`, or
  `concurrency`. `App::with_config(api, runs, repo_root, providers, roles, probes,
  config_paths, base_branch_exists)` is the config-aware constructor; `main.rs`
  calls it with values cloned from the resolved `config: Config` (`config.providers`,
  `config.roles`, …).
- **Validation** (`makina-core/src/config.rs` `Config::validate`): returns
  `Err(ConfigError::Validation { reason })` with precise strings, e.g.
  `caps.gate_iterations must be at least 1`, `caps.reviewer_iterations must be at
  least 1`, `caps.wall_clock_secs must be at least 1`, and (plan 0015)
  `caps.idle_secs must be at least 1`.

## 0069 — Command palette

Edits in `crates/makina/src/app.rs`, `crates/makina/src/event.rs`, and
`crates/makina/src/ui.rs`.

- **Mode + state.** Add `Mode::CommandPalette` to `pub enum Mode` and a state
  struct + `App` field, mirroring `provider_editor`:

  ```rust
  /// A single selectable command in the palette: its display label and the
  /// `AppEvent` `Enter` re-dispatches through the normal update path.
  #[derive(Debug, Clone)]
  pub struct PaletteAction {
      pub label: &'static str,
      pub event: AppEvent,
  }

  /// State for the Ctrl+P command-palette modal.
  #[derive(Debug, Clone)]
  pub struct CommandPalette {
      /// Type-to-filter query (case-insensitive substring match on `label`).
      pub filter: String,
      /// The full static action set, in display order.
      pub actions: Vec<PaletteAction>,
      /// Selected index *into the filtered view* (clamped on every filter change).
      pub selected: usize,
  }

  impl CommandPalette {
      /// The default action set. Most entries carry an existing intent
      /// (`OpenBrowser`, `OpenDoctor`, `OpenProviderEditor`, `OpenSettings`,
      /// `Quit`); forward-referenced ones (`RetryFocusedTask`, `DiscoverProject`)
      /// carry the new stub variant.
      pub fn default_actions() -> Vec<PaletteAction> { /* … */ }
      /// Actions whose lowercased `label` contains the lowercased `filter`.
      pub fn filtered(&self) -> Vec<&PaletteAction> { /* … */ }
  }
  ```

  On `App`: `pub command_palette: Option<CommandPalette>` (`Some` only while
  `mode == Mode::CommandPalette`), initialised `None` in **both** `App::new` and
  (inheriting via `Self::new`) `App::with_config`; add
  `pub fn is_command_palette(&self) -> bool { self.mode == Mode::CommandPalette }`
  next to `is_editing_providers`.

- **AppEvent variants.** Add, alongside the `OpenProviderEditor` cluster:
  `OpenCommandPalette`, `CommandPaletteUp`, `CommandPaletteDown`,
  `CommandPaletteInput(char)`, `CommandPaletteBackspace`,
  `CommandPaletteExecute`, `CloseCommandPalette`. Add `OpenSettings` (consumed by
  0070) and the two forward-referenced stubs `RetryFocusedTask` and
  `DiscoverProject`.

- **`App::update` handlers.**
  - `OpenCommandPalette` → set `command_palette = Some(CommandPalette { filter:
    String::new(), actions: CommandPalette::default_actions(), selected: 0 })`,
    `mode = Mode::CommandPalette`.
  - `CommandPaletteInput(c)` → push `c` onto `filter`, clamp `selected` to the new
    `filtered().len()`. `CommandPaletteBackspace` → `filter.pop()`, re-clamp.
  - `CommandPaletteUp`/`Down` → move `selected` within `0..filtered().len()`
    (saturating / clamped), as the provider editor does with `selection_index`.
  - `CommandPaletteExecute` → read the selected `PaletteAction.event`, **close the
    palette** (`mode = Mode::Normal`, `command_palette = None`), then return that
    event so the event loop re-dispatches it (see event.rs below). `update` itself
    returns `true`; the actual re-dispatch is an IO-layer concern so `update` stays
    pure.
  - `CloseCommandPalette` → `mode = Mode::Normal`, `command_palette = None`.
  - `OpenSettings` is handled by 0070; `RetryFocusedTask` / `DiscoverProject` set a
    `status_message` (`StatusMessage`) like `"retry not yet available (plan 0017)"`
    / `"project discovery not yet available (plan 0025)"` and return `true`.

- **Key binding (`event.rs`).** In `translate_key`, **before** the per-mode
  cascade but **after** the `Ctrl+C` guard, add:

  ```rust
  // Ctrl+P opens the command palette from any non-modal context.
  if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL) {
      return AppEvent::OpenCommandPalette;
  }
  ```

  Thread a `command_palette: bool` flag through `translate_key`'s signature (and
  its `translate_terminal_event` caller, derived from `app.is_command_palette()`),
  and add a palette keymap branch in the cascade:

  ```rust
  } else if command_palette {
      match key.code {
          KeyCode::Esc => AppEvent::CloseCommandPalette,
          KeyCode::Enter => AppEvent::CommandPaletteExecute,
          KeyCode::Up => AppEvent::CommandPaletteUp,
          KeyCode::Down => AppEvent::CommandPaletteDown,
          KeyCode::Backspace => AppEvent::CommandPaletteBackspace,
          KeyCode::Char(c) => AppEvent::CommandPaletteInput(c),
          _ => AppEvent::Tick,
      }
  }
  ```

  (Note: the palette uses `Up`/`Down` only — **not** `j`/`k` — because typed
  letters feed the filter; this differs deliberately from the provider editor's
  `j`/`k` navigation.)

- **Execute re-dispatch (`event.rs`).** The palette's `Enter` must run the chosen
  intent. In the event loop's `resolve_io` (or the loop that calls `App::update`),
  after handling `AppEvent::CommandPaletteExecute`, take the just-closed palette's
  selected action's `event` and feed it back through the same dispatch the loop
  uses for a fresh key event. Concretely: have `CommandPaletteExecute` resolve in
  `resolve_io` to the selected `PaletteAction.event` (reading
  `app.command_palette` *before* `update` clears it), so the loop processes
  `OpenBrowser` / `OpenSettings` / `Quit` / etc. exactly as if the user had pressed
  its chord. Keep this in one place so there is a single execution path.

- **Render (`ui.rs`).** Add `render_command_palette(palette, frame, area)`
  mirroring `render_provider_editor`: `centered_rect(60, 60, area)`, `Clear`, a
  `Thick` cyan `Block` titled ` Command Palette `, a vertical `Layout` of
  `[Length(3) filter-input, Min(0) list, Length(2) footer]`. The filter line shows
  `> {filter}▏`; the list renders one `ListItem` per `palette.filtered()` entry
  with the `palette.selected` row highlighted (the same cyan/yellow highlight the
  provider editor uses); the footer hints `↑/↓ select · Enter run · Esc close`.
  Draw it in the overlay block at the bottom of `render` guarded by
  `app.is_command_palette()`.

- **Status bar.** Extend the literal hint to advertise the palette, e.g. prepend
  `[^P] cmds  ` to the existing ` [o] open  …` string.

## 0070 — Settings screen

Edits in `crates/makina/src/app.rs`, `crates/makina/src/event.rs`,
`crates/makina/src/ui.rs`, and `crates/makina/src/main.rs` (to seed the resolved
caps).

- **App must know the caps.** The App holds only `wall_clock_secs_config` /
  `idle_secs_config` today. Give it the rest by adding either a single
  `pub caps: makina_core::config::CapsConfig` and `pub concurrency: usize`, or the
  individual fields, set in `App::with_config` from the resolved `config` in
  `main.rs` (`config.caps.clone()`, `config.concurrency`). Default them in
  `App::new` to `CapsConfig::default()` / `3`.

- **Mode + state.** Add `Mode::Settings` and:

  ```rust
  /// Which settings field is focused / being edited.
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum SettingsField {
      GateIterations,
      ReviewerIterations,
      WallClockSecs,
      IdleSecs,        // empty buffer ⇒ None (disabled)
      Concurrency,
  }

  /// State for the settings modal: an editable text buffer per numeric field,
  /// seeded from the App's loaded caps, plus the focused field.
  #[derive(Debug, Clone)]
  pub struct Settings {
      pub gate_iterations: String,
      pub reviewer_iterations: String,
      pub wall_clock_secs: String,
      pub idle_secs: String,        // "" ⇒ None
      pub concurrency: String,
      pub focused: SettingsField,
      /// Last validation error (rendered under the field), or `None`.
      pub error: Option<String>,
  }
  ```

  On `App`: `pub settings: Option<Settings>`; `pub fn is_settings(&self) -> bool`.

- **Open.** `AppEvent::OpenSettings` (the palette's "Settings" action) →
  `settings = Some(Settings { … seeded from self.caps / self.concurrency /
  self.idle_secs_config … })`, `mode = Mode::Settings`. The palette closes first
  (0069), so opening settings is just the re-dispatched intent.

- **Navigate + edit.** Add `SettingsUp`/`SettingsDown` (cycle `focused` through the
  `SettingsField`s), `SettingsInput(char)` (append a digit to the focused numeric
  field's buffer; ignore non-digits), and `SettingsBackspace`. On every numeric
  edit, **re-validate that field** with the same rule `Config::validate` uses and
  set `settings.error` to the matching reason string on failure (e.g.
  `caps.gate_iterations must be at least 1`); a parse failure (non-numeric/empty
  where a value is required) sets a `"… must be a positive integer"` error.

- **Commit (`event.rs`).** Add `AppEvent::SettingsCommit` (bound to `Enter`) and an
  `async fn commit_settings(app: &App) -> Option<String>` that follows
  `commit_provider_config`'s recipe exactly:

  ```rust
  // 1. Parse + validate every field; on any error return Some(reason) (no write).
  // 2. Read {repo_root}/.makina/config.toml → GlobalConfig (or default).
  // 3. Rebuild preserving everything else:
  let updated = GlobalConfig {
      caps: CapsConfig { gate_iterations, reviewer_iterations,
                         wall_clock_secs, idle_secs },
      concurrency,
      ..existing_global
  };
  // 4. toml::to_string_pretty + write.
  ```

  Validate the assembled `GlobalConfig`/`CapsConfig` against the same predicates
  `Config::validate` enforces *before* writing; on success write the file and
  return `Some("Settings saved")`, mirroring `commit_provider_config`'s
  `"Config saved"`. `App::update`'s `SettingsCommit` arm applies the parsed values
  back onto `self.caps`/`self.concurrency`/`self.idle_secs_config` and closes the
  modal (`mode = Mode::Normal`), exactly as `ProviderEditorCommit` applies the
  editor's state.

  > **Writer scope.** Because `commit_provider_config`'s recipe round-trips the
  > file through `GlobalConfig` (which has **no `gates`/`base_branch` field**),
  > the writer preserves only the other `GlobalConfig` sections (providers, roles)
  > via `..existing_global`; it does **not** round-trip the `ProjectConfig`
  > `[[gates]]` / `base_branch` tables. This plan edits only `GlobalConfig` caps +
  > concurrency, so that is sufficient here; the gates-aware project-config writer
  > lands in plan 0025.

- **Cancel.** `AppEvent::CloseSettings` (bound to `Esc`) → `mode = Mode::Normal`,
  `settings = None`, **no write**.

- **Render (`ui.rs`).** Add `render_settings(settings, frame, area)` mirroring
  `render_provider_editor`: `centered_rect(70, 70, area)`, `Clear`, `Thick` cyan
  `Block` titled ` Settings `, a vertical `[Min(0) list, Length(2) footer]`. Each
  field is one row `  {label}: {value}` with the focused row highlighted and (when
  numeric+editing) a trailing `▏` cursor. When `settings.error` is `Some`, render
  it in red under the list. Footer hints `↑/↓ field · 0-9 edit · Enter save · Esc
  cancel`. Draw guarded by `app.is_settings()`.

- **Key binding (`event.rs`).** Thread a `settings: bool` flag through
  `translate_key` and add a Settings keymap branch: `Esc`→`CloseSettings`,
  `Enter`→`SettingsCommit`, `Up`→`SettingsUp`, `Down`→`SettingsDown`,
  `Backspace`→`SettingsBackspace`, `Char(c)`→`SettingsInput(c)`.

## Testing notes

- 0069 logic is pure: test `CommandPalette::filtered` (filter narrows the list)
  and the `update` handlers (`OpenCommandPalette` sets the mode; input mutates the
  filter + clamps `selected`; `CommandPaletteExecute` resolves to the selected
  action's `event` and closes; `CloseCommandPalette` returns to Normal). Drive
  `Ctrl+P` through `translate_key`/`translate_terminal_event` to assert it emits
  `OpenCommandPalette`. A render test builds a fixture palette, renders to a test
  `Buffer`, and asserts the modal shows the title + a filtered action label.
- 0070: `settings_lists_current_caps` seeds `App` with known caps and asserts the
  rendered modal contains each value; `edit_and_commit_writes_config` points
  `repo_root` at a `tempfile::tempdir()`, drives input+`SettingsCommit` through
  `resolve_io`, and asserts the written `config.toml` parses back with the new caps
  while preserving an unrelated `GlobalConfig` section (e.g. a pre-written
  `[[providers]]` / `[roles]` entry). (Do **not** assert a `[[gates]]` entry
  survives: `commit_provider_config`'s writer round-trips through `GlobalConfig`,
  which has no `gates`/`base_branch` field, so a project-config `[[gates]]` entry
  is **not** preserved through this writer — the gates-aware project-config writer
  lands in plan 0025.) `invalid_value_rejected` sets `gate_iterations` to `0`,
  asserts `settings.error` carries `caps.gate_iterations must be at least 1`, and
  that no file was written.

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay green
throughout.

## Interaction with prior plans

- Reuses the **plan-0011** modal pattern (`ProviderEditor` + mode-gated keymap +
  `render_provider_editor`) and its config writer `commit_provider_config` (the
  read-merge-write recipe) for settings persistence.
- Reuses **plan 0013**'s `Mode::Doctor`/`OpenDoctor` and **plan 0015**'s
  `idle_secs_config`/`wall_clock_secs_config` App fields (the settings screen now
  owns editing the values the live-activity header reads).
- Forward-references **plan 0017**'s retry surface (the `RetryFocusedTask` palette
  stub) and the **discovery plan (0024/0025)**'s `DiscoverProject` action; both are
  reserved here as palette-action stubs and wired by their owning plans. Plan 0025
  owns the `[discovery]` stamp (a `ProjectConfig` record) and the gates-aware
  project-config writer — this plan defines neither.
