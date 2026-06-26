# Scope — Plan 0038

> Fix stale README documentation on permissions and persistence, and address four high-impact TUI polish bugs (inactive tabs, hardcoded wall-clock, missing help screen, non-scrollable error pane).

## Why this plan

**1. README stale on session/request_permission — misleads users and hides the headline feature.** The README (`README.md:151–153`) says "Makina's ACP client does not yet answer `session/request_permission`" and recommends the `--yolo` flag as a workaround. However, `makina-acp` implements `WorktreePolicy` (`permission.rs:82–136`), which auto-allows inside the worktree and audits every decision. This is the project's headline governance feature, the response to the trial findings' largest risk, and it is now implemented — but the docs hide it behind the stale claim. The `--yolo` example at `README.md:64` is no longer necessary for the common case and actively bypasses the governance feature users should be using.

**2. Inactive tab labels are unreadable — `bg(Dim).fg(Dim)` is invisible.** The tab-bar renderer (`ui.rs:936–938`) styles inactive tabs with `Style::default().bg(Dim).fg(Dim)`. In Ayu Dark this renders as `#5A6378` text on a `#5A6378` background — the label disappears. This is the most visible polish defect in the current build; opening multiple tabs makes the inactive ones vanish entirely.

**3. Wall-clock countdown is hardcoded to 600s — never synced to config.** `App` initializes `wall_clock_secs_config: 600` at `app.rs:1625` and never updates it from `self.caps.wall_clock_secs`. If a user sets `wall_clock_secs = 1200` in `.makina/config.toml`, the TUI countdown displays the wrong deadline. (The Supervisor enforces the correct cap; only the displayed countdown is wrong.) The resolved cap lives on the `App` field `self.caps` (type `CapsConfig`, `app.rs:1259`, seeded in `App::with_config`, `app.rs:1676`) — `RunView` carries no `caps` field, so the fix reads `self.caps.wall_clock_secs` (the path already used at `app.rs:2627`).

**4. No help / keymap screen — discoverability gap and status bar clipping.** The `?` key opens Doctor, not a keymap. The only key legend is the status bar at `ui.rs:807`, which is a single dense line that clips on narrow terminals (`ui.rs:795–798`), silently dropping the trailer containing the focus label and last-event hint. On narrow windows many keybindings are invisible. There is no `h`/`F1`/`?`-mode help overlay.

**5. Error pane is not scrollable — cannot inspect error history.** `render_error_pane` (`ui.rs:2208`) auto-scrolls to the newest (`ui.rs:2250–2257`), but there is no `ScrollablePanel::ErrorPane` entry and no key to scroll within it. A long error history cannot be inspected upward; older errors vanish once they scroll off. The error pane is a toggled overlay (`error_pane_open: bool`, `app.rs:1204`, toggled by `'e'`), not a focusable `Panel`, so routing is by `error_pane_open` + mouse position, using the existing `scroll_offsets` map and per-panel scroll helpers. The error pane is the primary diagnostic surface for agent/gate failures, so this is a real usability gap.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0001–0002):

- **0001 — README-Fix-Permissions-And-Persistence.** Remove stale claims about `session/request_permission` and persistence from README.md. Document `WorktreePolicy` auto-allow feature, remove `--yolo` example, and update the Limitations section to reflect that permissions are now answered and persistence is implemented (plans 0029/0030).
- **0002 — TUI-Polish-Fixes.** Fix four high-impact TUI polish bugs: (1) change inactive tab label styling from invisible `bg(Dim).fg(Dim)` to readable `bg(Dim).fg(Foreground)`, (2) seed `wall_clock_secs_config` from `self.caps.wall_clock_secs` when a run opens, (3) add a full-screen help overlay for `?` showing all keybindings, (4) make the error pane scrollable via a new `ScrollablePanel::ErrorPane` (mouse wheel + `PgUp`/`PgDn`) with an `error_pane_auto_follow` toggle.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| README stale on `session/request_permission` — claims permissions are unanswered and recommends `--yolo`, hiding the implemented `WorktreePolicy` governance feature. | `0001` |
| Persistence claim in README already satisfied by plans 0029/0030 — README still says `.makina/tasks/{slug}.json` persistence and crash recovery are unimplemented. | `0001` |
| Inactive tab label styling unreadable — `bg(Dim).fg(Dim)` renders labels invisible against the dim background. | `0002` |
| Wall-clock countdown hardcoded — `wall_clock_secs_config: 600` is never synced to `caps.wall_clock_secs`, so the displayed deadline is wrong. | `0002` |
| No help / keymap screen — `?` opens Doctor, and the single-line status bar clips on narrow terminals, hiding keybindings. | `0002` |
| Error pane not scrollable — no `ScrollablePanel::ErrorPane` entry and no scroll keys, so older errors cannot be inspected. | `0002` |

## Locked decisions

- **Inactive tab styling uses Foreground, not Background.** Inactive tabs will use `Style::default().bg(Dim).fg(Foreground)` instead of `bg(Dim).fg(Background)`. This choice favours readability: Foreground is typically a brighter/more visible colour than Background in most themes (especially Ayu Dark, where Background is very dim), ensuring good contrast against the Dim background. The alternative `bg(Dim).fg(Background)` might be unreadable in light themes. Foreground is the safer default.
- **Wall-clock sync happens on RunOpened, reads the App caps field.** Wall-clock config is synced to `wall_clock_secs_config` when a run is opened or selected (`Event::RunOpened` handler, `app.rs:3170`, and/or the `self.selected_run` assignment sites). The source is the resolved-config field `self.caps.wall_clock_secs` (`app.rs:1259`/`2627`) — `RunView` has no `caps` field — so a single assignment `self.wall_clock_secs_config = self.caps.wall_clock_secs;` suffices and stays consistent with the rest of the codebase.
- **Help overlay is a dismissable full-screen modal, not embedded in status bar.** The help overlay is a full-screen modal similar to the file-browser modal at `ui.rs:821`, not an attempt to extend or restructure the status bar. The status bar remains a single dense line for normal operation; the help overlay is a separate interactive layer that the user toggles with `?`. This keeps the normal layout clean and the help discoverable.
- **Error pane uses an auto-follow toggle, mirroring `exchange_auto_follow`.** A new `error_pane_auto_follow: bool` (default `true`) governs the pane, exactly like the exchange pane's `exchange_auto_follow` (`app.rs:1171`). While engaged, the pane pins to the newest message (so a fresh error stays visible); a user scroll-up disengages follow and freezes the manual offset (so reviewing old errors is not interrupted by new ones); scrolling back to the bottom re-engages follow. Keyboard scroll is `PgUp`/`PgDn` (not `Up`/`Down`, which are bound to `SelectUp`/`SelectDown`); mouse wheel routes by position via a new `ScrollablePanel::ErrorPane` hitbox. This preserves the user's inspection context while keeping live tailing as the default.

## Out of scope

- Syntax highlighting in code blocks within Markdown. Plan 0020 already defers language-specific tokenization for code blocks; this plan does not introduce new Markdown rendering and inherits that deferral.
- Customizable keybindings (e.g. user-defined key remapping). All keybindings in this plan are hardcoded. User customization via config is out of scope and belongs to a separate plan.
- Doctor tool modifications or relocation to a different key. Doctor is currently bound to `?`. This plan moves it to `!` or a palette entry, but does not change Doctor's functionality or internals. Doctor itself is out of scope.
- Restructuring the status bar layout or wrapping long text. The status bar remains a single dense line. The help overlay replaces the need for a scrollable or wrapped status bar legend. Restructuring the status bar is out of scope.
- Persistence audit ledger location or audit log viewer. The README documents the audit ledger existence, but implementing an audit-log viewer or changing where it is stored is out of scope.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
