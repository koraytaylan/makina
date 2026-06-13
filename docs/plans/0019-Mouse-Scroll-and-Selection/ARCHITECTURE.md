# Architecture — Plan 0019 (deltas)

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). This plan touches only the `makina` (TUI) crate —
> `crates/makina/src/tui.rs`, `event.rs`, `app.rs`, `ui.rs`, plus `README.md`.

## Current shape (what exists)

- **Terminal lifecycle** (`crates/makina/src/tui.rs`):
  - `Tui::init()` runs `install_panic_hook()`, `enable_raw_mode()?`, then
    `execute!(stdout, EnterAlternateScreen, cursor::Hide)?`. **No mouse capture.**
  - `Tui::restore(&mut self)` does `disable_raw_mode()` then
    `execute!(self.terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)`.
    **No mouse capture.**
  - `Tui::reinit(&mut self)` (re-entry after `$PAGER`) re-runs
    `enable_raw_mode()?` + `execute!(…, EnterAlternateScreen, cursor::Hide)?` then
    `self.terminal.clear()`. **No mouse capture.**
  - `restore_terminal()` (free fn, used by the panic hook and the signal reaper)
    does `disable_raw_mode()` + `execute!(io::stdout(), LeaveAlternateScreen,
    cursor::Show)`. **No mouse capture.**
  - Imports are `use ratatui::crossterm::{cursor, execute, terminal::{…}}`.
- **Wheel translation** (`crates/makina/src/event.rs`,
  `translate_terminal_event`): the `CrosstermEvent::Mouse(m)` arm matches
  `MouseEventKind::ScrollUp => AppEvent::ScrollUp`,
  `MouseEventKind::ScrollDown => AppEvent::ScrollDown`, `_ => AppEvent::Tick`.
  This is **already correct** and unit-tested (`wheel_translates_to_scroll`).
- **Suspend/resume to `$PAGER`** (`crates/makina/src/event.rs`, `open_log`):
  resolves the focused task's log path, calls `tui.restore()`, spawns
  `Command::new(pager_cmd).arg(&log_path).status()`, then `tui.reinit()`. Invoked
  from the event-loop `OpenLog` arm (around `event.rs` `if matches!(event,
  AppEvent::OpenLog)`), which holds `&mut Tui`.
- **Scroll handlers + state** (`crates/makina/src/app.rs`): the
  `AppEvent::ScrollUp` arm calls `self.scroll_up()`; the `AppEvent::ScrollDown`
  arm calls `self.scroll_down(self.last_scroll_max.get())`. State:
  `exchange_scroll: u16`, `exchange_auto_follow: bool`,
  `last_scroll_max: std::cell::Cell<u16>`, plus
  `effective_offset(scroll_max) -> u16`. **Unchanged by this plan.**
- **Focus + panels** (`crates/makina/src/app.rs`): `enum Panel { Sidebar, Main }`,
  `pub focused_panel: Panel`, and `AppEvent::FocusNext` toggles between them.
- **Status bar** (`crates/makina/src/ui.rs`, the `// ── Status bar ──` block): a
  `Paragraph` whose first span is the literal hint string
  `" [o] open  [s/p/c] start/pause/cancel  [Tab] panel  [v] view  [L] log  [?]
  doctor  "`.
- **README "Keys" list** (`README.md`, under `## Run`): navigation and panel
  switching live on **one combined bullet** (`README.md:117-122`):
  `- **↑/↓** (or j/k) — navigate · **Tab** — switch panel (Runs ↔ Detail)`.
  No mouse/wheel note yet.

> History note: commit `5dc881d` (task `tui-mouse-scroll`) had
> `EnableMouseCapture`/`DisableMouseCapture` in `init`/`restore`, imported from
> `ratatui::crossterm::event`. The 0014 squash (`d1a3463`) rewrote the
> terminal-lifecycle docs and dropped them. This plan restores them and extends
> them to the reinit site (new) and the restore_terminal site (which existed but
> never disabled capture).

## 0061 — Re-enable mouse capture for content scroll

Edits in `crates/makina/src/tui.rs`.

- **Import the capture commands.** Add `event::{DisableMouseCapture,
  EnableMouseCapture}` to the existing `ratatui::crossterm` import group:

  ```rust
  use ratatui::crossterm::{
      cursor,
      event::{DisableMouseCapture, EnableMouseCapture},
      execute,
      terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
  };
  ```

- **`init` — enable capture.** Put `EnableMouseCapture` between
  `EnterAlternateScreen` and `cursor::Hide` (order matches the pre-0014 wiring):

  ```rust
  execute!(stdout, EnterAlternateScreen, EnableMouseCapture, cursor::Hide)?;
  ```

- **`restore` — disable capture.** Symmetrically, before showing the cursor:

  ```rust
  let _ = execute!(
      self.terminal.backend_mut(),
      LeaveAlternateScreen,
      DisableMouseCapture,
      cursor::Show
  );
  ```

- **`reinit` — re-enable capture.** The `$PAGER` resume path must turn capture
  back on or the wheel stays dead after viewing a log:

  ```rust
  execute!(
      self.terminal.backend_mut(),
      EnterAlternateScreen,
      EnableMouseCapture,
      cursor::Hide
  )?;
  ```

- **`restore_terminal` — disable capture.** The panic hook and the signal reaper
  leave the alternate screen here; they must also stop the terminal emitting
  mouse escape bytes into the user's shell:

  ```rust
  let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture, cursor::Show);
  ```

- **Why all four sites.** `open_log` does `tui.restore()` → spawn `$PAGER` →
  `tui.reinit()`. If only `init`/`restore` are touched, the first `$PAGER` open
  leaves capture off on resume (`reinit` never re-enabled it) and the wheel is
  dead for the rest of the session; and if `restore_terminal` is skipped, a panic
  mid-run leaves the shell receiving raw mouse sequences. The two halves stay
  paired: every alternate-screen *enter* enables capture, every *leave* disables
  it.

- **No `event.rs`/`app.rs` change needed for scroll.** Once capture is on, the
  existing `CrosstermEvent::Mouse(m) => match m.kind { ScrollUp/ScrollDown … }`
  arm and the `AppEvent::ScrollUp`/`ScrollDown` handlers do the rest. This
  workstream's app-side test simply re-asserts that contract against `App`.

## 0062 — Selection coexistence

Edits in `crates/makina/src/event.rs` (assert/keep the drag-blind contract),
`crates/makina/src/ui.rs` + `README.md` (document the modifier), and optionally
`crates/makina/src/app.rs` (click-to-focus).

- **Keep the app drag-blind (the coexistence guarantee).** The
  `CrosstermEvent::Mouse(m)` arm must continue to match **only**
  `MouseEventKind::ScrollUp`/`ScrollDown` and fall through every other kind to
  `AppEvent::Tick`. With capture on, the terminal still performs native selection
  when the user holds the bypass modifier (Shift on most terminals; Option/Alt in
  iTerm2) because the app never acts on `Down`/`Up`/`Drag`/`Moved`. A regression
  test pins this: a drag event translates to `Tick`, i.e. **no state change**.

  ```rust
  // event.rs — unchanged behaviour, now load-bearing for selection:
  CrosstermEvent::Mouse(m) => match m.kind {
      MouseEventKind::ScrollUp => AppEvent::ScrollUp,
      MouseEventKind::ScrollDown => AppEvent::ScrollDown,
      _ => AppEvent::Tick, // Down/Up/Drag/Moved → no-op so native selection works
  },
  ```

- **Document the modifier — README.** Add a wheel/scroll note to the existing
  combined navigation bullet (`README.md:117-122`,
  `- **↑/↓** (or j/k) — navigate · **Tab** — switch panel (Runs ↔ Detail)`),
  e.g. extend that line with:

  ```text
  · **wheel** — scroll content · hold **Shift** (or **Option** in iTerm2)
  and drag to select & copy text
  ```

- **Document the modifier — status bar.** Append a short hint to the status-bar
  span in `ui.rs` (the `" [o] open  …  [?] doctor  "` literal), e.g.
  `… [?] doctor  [wheel] scroll  `. Keep it terse; the status bar already clips
  on narrow terminals (the trailer is the part that gets clipped).

- **Optional — click-to-focus.** If included: in the `event.rs` `Mouse` arm,
  match `MouseEventKind::Down(MouseButton::Left)` to a new
  `AppEvent::FocusPanelAt { column, row }` (carry the click column; the body
  splits sidebar `Percentage(30)` / main `Percentage(70)`), and in `app.rs`
  handle it by setting `focused_panel = Panel::Sidebar` when the column falls in
  the sidebar band else `Panel::Main`. **Drag/Up/Moved stay `Tick`** so selection
  is unaffected. This is the only path that may introduce an `AppEvent`; skip it
  and the plan is still complete.

## Test strategy

- **0061**
  - `init_enables_mouse_capture` (`tui.rs`): the `init → restore` round-trip is
    already smoke-tested (`init_then_restore_round_trips`); extend/assert that the
    `init` path includes `EnableMouseCapture` (assert against the command sequence
    the helper emits, or — since `execute!` writes escape bytes to the real TTY —
    keep the existing round-trip and add a focused unit on a small extracted
    `enter_screen`/`leave_screen` helper if one is introduced, asserting the
    capture command is present). Under no-TTY CI, `init` returns `Err`; the test
    must still pass (assert the *intended* sequence, not a live terminal).
  - `scroll_events_adjust_exchange_state` (`app.rs`): reuse the contract of
    `scroll_event_changes_offset_not_selection` — drive `AppEvent::ScrollDown`
    then `ScrollUp` through `App::update`, assert `exchange_scroll` moves and
    `exchange_auto_follow` disengages, and `selected_task` is untouched.
- **0062**
  - `mouse_drag_is_noop` (`event.rs`): build a `CrosstermEvent::Mouse` with
    `kind = MouseEventKind::Drag(MouseButton::Left)` (and one with
    `Moved`) via the existing `wheel`-style helper, and assert
    `translate_terminal_event(...)` returns `AppEvent::Tick` — proving the app
    never consumes drag, so native selection coexists. (The test-module `use`
    line currently imports `MouseEvent` only — add `MouseButton` to it.)
  - (if click-to-focus added) `left_click_focuses_panel` (`app.rs`): a left-down
    in the sidebar band sets `focused_panel == Panel::Sidebar`, in the main band
    sets `Panel::Main`; a drag/up does not change focus.

`cargo test`, `cargo clippy --all-targets -- -D warnings`, and
`cargo fmt --check` stay green.

## Interaction with prior plans

- Builds on the `tui-mouse-scroll` wiring (wheel → `ScrollUp`/`ScrollDown`) and
  the `tui-scroll-state` work (`scroll_up`/`scroll_down`/`effective_offset`/
  `last_scroll_max`) already in `event.rs`/`app.rs`; it only restores the
  capture toggle the 0014 squash removed.
- Coexists with the plan 0016 sidebar tree (`Panel`, `focused_panel`,
  `TreeNode`/`tree_cursor`/`tree_move`/`tree_toggle_expand`): wheel scroll targets
  the exchange pane, not the tree, and the optional click-to-focus only sets
  `focused_panel` — it does not move `tree_cursor`. No dependency on plan 0017.
