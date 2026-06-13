# Scope — Plan 0019

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

The mouse wheel used to scroll the exchange (content) pane. It no longer does:
spinning the wheel either scrolls the host terminal's own scrollback or, worse,
does nothing the app reacts to — the user has lost wheel-scroll of the content
they came to read.

The routing inside the app is **already intact**. `event.rs` still maps wheel
events to scroll intents — the `CrosstermEvent::Mouse(m)` arm in
`translate_terminal_event` matches `MouseEventKind::ScrollUp`/`ScrollDown` and
returns `AppEvent::ScrollUp`/`AppEvent::ScrollDown`. `app.rs` still handles them
— the `AppEvent::ScrollUp`/`AppEvent::ScrollDown` arms in `App::update` call
`scroll_up()` / `scroll_down(self.last_scroll_max.get())` on the exchange-pane
scroll state (`exchange_scroll`, `exchange_auto_follow`, `last_scroll_max`,
`effective_offset`). And there is a passing unit (`wheel_translates_to_scroll`)
and app units (`scroll_event_changes_offset_not_selection`) that prove it.

The single missing piece: **the terminal never sends the app any mouse events.**
`Tui::init` (`crates/makina/src/tui.rs`) only does `enable_raw_mode` +
`EnterAlternateScreen` + `cursor::Hide` — it does **not** enable mouse capture.
It used to: commit `5dc881d` (task `tui-mouse-scroll`) had
`EnableMouseCapture` / `DisableMouseCapture` in `init`/`restore`, and the 0014
squash (commit `d1a3463`) dropped them when it rewrote the terminal-lifecycle
docs. Without capture, crossterm's `EventStream` never yields `Mouse(_)` events,
so the otherwise-correct scroll plumbing is dead.

The catch: naively turning capture back on **steals click-drag from the
terminal**, which is what users use to *select and copy* text. The user wants
**both at once** — wheel-scrolls-content **and** native text selection. Modern
terminals provide a bypass: holding a modifier (Shift on most terminals,
Option/Alt in iTerm2 on macOS) makes the terminal do its native selection even
while an app has mouse capture on. So as long as the app consumes **only the
wheel** (and never drags), modifier-drag selection keeps working unchanged.

This plan re-enables mouse capture so wheel events reach the app again, and
guarantees the app stays drag-blind so native modifier-bypass selection
coexists.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0061–0062):

- **0061 — Re-enable mouse capture for content scroll.** Add `EnableMouseCapture`
  to `Tui::init` and `DisableMouseCapture` to `Tui::restore`, and apply the same
  pair to the re-init path (`Tui::reinit`) and the standalone
  `restore_terminal()` so the `$PAGER` open-log suspend/resume cycle and the
  panic/signal teardown stay consistent. Confirm (via the existing units) that
  wheel events route to the exchange scroll once capture is on.
- **0062 — Selection coexistence.** Lock in that the app reacts to **only** the
  scroll kinds and treats every other mouse kind (down/up/drag/move) as a
  no-op `Tick`, so the terminal's native modifier-bypass selection still works
  with capture on. Document the per-terminal selection modifier (Shift-drag;
  Option-drag in iTerm2) in the README "Keys" list and the status-bar hint.
  Optionally route a single left-click onto a panel to `focused_panel`.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Wheel no longer scrolls content: `EnableMouseCapture` removed in the 0014 squash (`tui.rs` `init`/`restore`) | `0061` |
| Suspend/resume to `$PAGER` (`open_log`) and panic/signal teardown must keep capture state consistent | `0061` |
| Capture must not break native text selection (user wants both wheel-scroll **and** select+copy) | `0062` |
| No documentation of the per-terminal selection modifier | `0062` |

## Locked decisions

- **Capture on, drag ignored — "both at once".** Re-enabling
  `EnableMouseCapture` is what makes the wheel reach the app. To keep native
  selection working simultaneously, the app must consume **only**
  `MouseEventKind::ScrollUp`/`ScrollDown`; every other mouse kind stays a
  harmless `AppEvent::Tick`. The terminal's modifier-bypass (Shift-drag, or
  Option-drag in iTerm2) then performs native selection while capture is on.
  This is the deliberate "both at once" design — no per-terminal capture
  toggling, no copy-mode.
- **Mirror the pre-0014 wiring exactly.** `init` runs
  `EnterAlternateScreen, EnableMouseCapture, cursor::Hide`; `restore` runs
  `LeaveAlternateScreen, DisableMouseCapture, cursor::Show`, importing
  `DisableMouseCapture`/`EnableMouseCapture` from `ratatui::crossterm::event`
  (the same module path `cursor`/`terminal` use in this file). This restores the
  exact code commit `5dc881d` had before the squash dropped it.
- **All four teardown/reinit sites move together.** Capture must be enabled by
  both `init` and `reinit`, and disabled by both `restore` and the standalone
  `restore_terminal()`. The `open_log` path calls `tui.restore()` then
  `tui.reinit()` around `$PAGER`; the panic hook and the signal reaper call
  `restore_terminal()`. Any site that leaves the alternate screen must also
  disable capture, or `$PAGER`/the shell inherit a terminal still emitting mouse
  escape bytes.
- **No new `AppEvent`, no new scroll state.** The scroll handlers, scroll state
  (`exchange_scroll` / `exchange_auto_follow` / `last_scroll_max` /
  `effective_offset`), and the `event.rs` wheel→`ScrollUp`/`ScrollDown` mapping
  already exist and are tested; this plan does not change them.
- **Click-to-focus is optional and additive.** If added, a single left button
  press maps to a focus intent (sidebar click → `Panel::Sidebar`, main click →
  `Panel::Main`); drag/move/release stay no-ops so selection is untouched. The
  plan ships and verifies correctly without it.

## Out of scope

- A built-in scrollback/copy *mode* (tmux-style) or app-driven clipboard writes.
- Per-terminal detection of which modifier bypasses capture (we document both;
  we do not probe `$TERM_PROGRAM`).
- Dragging to select *within* the app's own panes, or app-rendered selection
  highlighting.
- Horizontal scroll, mouse-driven resize, or click-to-expand of the 0016 tree.
- Any change to the exchange scroll math or auto-follow behaviour (plan 0009 /
  the `tui-scroll-state` work already cover it).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
