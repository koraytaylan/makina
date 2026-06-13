# Makina Plan 0019 — Mouse Scroll & Selection Coexistence

Restore **wheel-scrolls-content** by re-enabling the mouse capture the 0014
squash removed from `Tui::init`/`restore`, and keep **native text selection**
working at the same time by ensuring the app consumes only the wheel and never
drags — so the terminal's modifier-bypass selection (Shift-drag; Option-drag in
iTerm2) coexists with capture on.

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

## 0061 — Re-enable mouse capture for content scroll

### re-enable-mouse-capture — Wheel scrolls the focused content pane again

Add `EnableMouseCapture` to terminal entry and `DisableMouseCapture` to terminal
exit at **all four** lifecycle sites so the app receives wheel events again and
the `$PAGER`/panic teardown paths stay consistent.

**Steps:**

1. In `crates/makina/src/tui.rs`, extend the `use ratatui::crossterm::{…}`
   import group with `event::{DisableMouseCapture, EnableMouseCapture}`
   (the same module path `cursor`/`terminal` already use in this file).

2. In `Tui::init`, change the `execute!(stdout, EnterAlternateScreen,
   cursor::Hide)?` call to
   `execute!(stdout, EnterAlternateScreen, EnableMouseCapture, cursor::Hide)?`.

3. In `Tui::restore`, change the `execute!(self.terminal.backend_mut(),
   LeaveAlternateScreen, cursor::Show)` call to insert `DisableMouseCapture`
   before `cursor::Show`.

4. In `Tui::reinit` (the `$PAGER` resume path), add `EnableMouseCapture` between
   `EnterAlternateScreen` and `cursor::Hide` in its `execute!` so the wheel works
   again after viewing a log; in the free fn `restore_terminal()` add
   `DisableMouseCapture` before `cursor::Show` so the panic hook / signal reaper
   never leave the user's shell receiving mouse escape bytes. Verify `open_log`
   in `crates/makina/src/event.rs` still calls `tui.restore()` then
   `tui.reinit()` around the pager (no edit needed there — the capture state now
   rides on those two calls).

5. Confirm the existing wheel→scroll plumbing is untouched: the
   `CrosstermEvent::Mouse(m)` arm in `event.rs` `translate_terminal_event` still
   maps `MouseEventKind::ScrollUp`/`ScrollDown` to `AppEvent::ScrollUp`/
   `ScrollDown`, and the `AppEvent::ScrollUp`/`ScrollDown` arms in `App::update`
   still call `scroll_up()` / `scroll_down(self.last_scroll_max.get())`.

6. Add/extend tests:

   ```rust
   // crates/makina/src/tui.rs
   #[test]
   fn init_enables_mouse_capture() { /* assert the init terminal-entry sequence includes EnableMouseCapture (and restore includes DisableMouseCapture); keep the no-TTY-safe round-trip of init_then_restore_round_trips so CI without a TTY still passes */ }

   // crates/makina/src/app.rs
   #[test]
   fn scroll_events_adjust_exchange_state() { /* App with last_scroll_max set; update(ScrollDown) moves exchange_scroll and leaves selected_task unchanged; update(ScrollUp) moves the offset and disengages exchange_auto_follow */ }
   ```

- **Depends on:** —
- **Done when:** `Tui::init` and `Tui::reinit` emit `EnableMouseCapture` and
  `Tui::restore` and `restore_terminal()` emit `DisableMouseCapture`;
  `init_enables_mouse_capture` and `scroll_events_adjust_exchange_state` pass;
  the existing `wheel_translates_to_scroll` and
  `scroll_event_changes_offset_not_selection` still pass; spinning the wheel
  scrolls the exchange pane and an `open_log`/`$PAGER` round-trip leaves the
  wheel working on resume; cargo test/clippy/fmt green.

---

## 0062 — Selection coexistence

### mouse-selection-coexistence — Keep native select+copy with capture on

Guarantee the app ignores all non-scroll mouse motion (down/up/drag/move) so the
terminal's modifier-bypass selection keeps working with capture on, and document
the per-terminal selection modifier.

**Steps:**

1. In `crates/makina/src/event.rs`, keep the `CrosstermEvent::Mouse(m)` arm of
   `translate_terminal_event` matching **only** `MouseEventKind::ScrollUp`/
   `ScrollDown`, with every other kind falling through to `AppEvent::Tick`.
   Confirm the comment names the coexistence intent (Down/Up/Drag/Moved →
   no-op so native modifier-drag selection works).

2. In `README.md`, extend the existing combined navigation bullet
   (`README.md:117-122`: `- **↑/↓** (or j/k) — navigate · **Tab** — switch panel
   (Runs ↔ Detail)`) with a wheel/selection note —
   `· **wheel** — scroll content · hold **Shift** (or **Option** in iTerm2) and
   drag to select & copy text`.

3. In `crates/makina/src/ui.rs`, append a terse hint to the status-bar span (the
   `" [o] open  [s/p/c] start/pause/cancel  [Tab] panel  [v] view  [L] log  [?]
   doctor  "` literal), e.g. add `[wheel] scroll  ` before the error badge. Keep
   it short — the trailer already clips on narrow terminals.

4. *(Optional — only if click-to-focus is wanted.)* Add an
   `AppEvent::FocusPanelAt { column, row }`, map
   `MouseEventKind::Down(MouseButton::Left)` to it in the `event.rs` `Mouse`
   arm, and handle it in `app.rs` by setting `focused_panel` to `Panel::Sidebar`
   when `column` falls in the sidebar band (body splits `Percentage(30)` /
   `Percentage(70)`) else `Panel::Main`. Leave `Drag`/`Up`/`Moved` as `Tick`.

5. Add tests:

   > Note: the event.rs test-module `use` line currently imports `MouseEvent`
   > only — add `MouseButton` to it so `MouseEventKind::Drag(MouseButton::Left)`
   > compiles.

   ```rust
   // crates/makina/src/event.rs
   #[test]
   fn mouse_drag_is_noop() { /* build CrosstermEvent::Mouse with kind = MouseEventKind::Drag(MouseButton::Left) and one with MouseEventKind::Moved; translate_terminal_event(...) returns AppEvent::Tick for both, proving the app never consumes drag so native selection coexists */ }

   // crates/makina/src/app.rs — ONLY if step 4 (click-to-focus) is implemented
   #[test]
   fn left_click_focuses_panel() { /* FocusPanelAt in the sidebar band => focused_panel == Panel::Sidebar; in the main band => Panel::Main; a drag/up does not change focused_panel */ }
   ```

- **Depends on:** re-enable-mouse-capture
- **Done when:** `mouse_drag_is_noop` passes (a drag/move mouse event maps to
  `AppEvent::Tick`, producing no app state change); with capture on, holding the
  documented modifier and dragging still selects & copies text in the host
  terminal; the README "Keys" list and the status bar name the wheel and the
  Shift/Option selection modifier; if click-to-focus was added,
  `left_click_focuses_panel` passes and drag/up never change focus; cargo
  test/clippy/fmt green.

---

**End of plan 0019 TASKS.** When every "Done when" bullet is green, the mouse
wheel scrolls the exchange pane again — the capture the 0014 squash removed is
back — while holding Shift (or Option in iTerm2) and dragging still selects and
copies text natively, so the user gets wheel-scroll and selection **at the same
time**.
