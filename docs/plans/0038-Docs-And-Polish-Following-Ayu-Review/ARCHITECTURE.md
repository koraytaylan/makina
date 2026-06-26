# Architecture — Plan 0038 (deltas)

> The concrete deltas. This plan touches
> `README.md`, `crates/makina/src/app.rs`,
> `crates/makina/src/event.rs`, and `crates/makina/src/ui.rs`.
> Line numbers are hints; locate by symbol.

## 0001 — README-Fix-Permissions-And-Persistence

Today the README (`README.md:151–153`) claims Makina "does not yet answer
`session/request_permission`" and recommends gemini's `--yolo` flag as the
workaround; the `--yolo` example appears in the config block at `README.md:64`.
Both are stale. `makina-acp` ships `WorktreePolicy`
(`crates/makina-acp/src/permission.rs:82–136`), which deterministically
auto-allows operations whose `working_dir` equals the assigned worktree and
audits every decision — the headline governance feature, now implemented. The
README also states at `README.md:154–155` that "Task-graph state lives in memory;
`.makina/tasks/{slug}.json` persistence and crash recovery are not implemented
yet", but plans 0029/0030 landed atomic `.makina/tasks/{slug}.json` persistence
and crash recovery. No code is touched in this workstream; the docs are brought
back in sync with the implementation.

**Edits:**

**Remove the stale permission claim.** Delete the Limitations bullet at
`README.md:151–153` that asserts the ACP client does not answer
`session/request_permission`:

```text
- Some agents require an auto-approve flag (gemini's `--yolo`); without it the
  agent's permission prompt stalls the turn — Makina's ACP client does not yet
  answer `session/request_permission`.
```

**Drop `--yolo` from the example.** In the `[backend]` block at `README.md:64`,
change `args = ["--acp", "--yolo"]` to `args = ["--acp"]` and remove the
trailing `# see "Limitations" re: --yolo` comment, so the example follows the
now-functional governance default instead of bypassing it.

**Document `WorktreePolicy`.** Under Limitations (or a new "Governance"
subsection), record that permission requests are now answered automatically by
`WorktreePolicy`, which auto-allows operations inside the assigned worktree and
audits every decision, and point to the audit ledger location:

```text
- Permission requests are answered automatically by `WorktreePolicy`, which
  auto-allows operations inside the assigned worktree and audits every decision.
```

**Update the persistence claim.** Replace the bullet at `README.md:154–155` so it
reflects plans 0029/0030:

```text
- Task-graph state and task metadata are persisted in `.makina/tasks/{slug}.json`
  with crash recovery (plans 0029/0030).
```

**Properties that make this safe:**

- Only documentation is changed; no code path is modified, so the governance
  feature and the persistence layer are untouched and cannot regress.
- `WorktreePolicy` and the audit ledger are already implemented and tested
  (`crates/makina-acp/src/permission.rs:82–136`), so the new prose describes
  shipped, exercised behavior rather than aspiration.
- Removing `--yolo` from the example steers users onto the intended governance
  path instead of the bypass, aligning the docs with the project's headline risk
  response.

## 0002 — TUI-Polish-Fixes

Today the TUI carries four distinct polish gaps. (1) Inactive tab labels are
styled in `render_tab_bar` (`crates/makina/src/ui.rs:936–938`) with
`Style::default().bg(Dim).fg(Dim)` — foreground and background the same colour,
so in Ayu Dark the `#5A6378`-on-`#5A6378` label is invisible; active tabs use
`Accent` bg + `Background` fg + bold and read fine. (2) `App` initializes
`wall_clock_secs_config: 600` as a hardcoded default (`crates/makina/src/app.rs:1625`)
and never reseeds it from `caps.wall_clock_secs`, so a `wall_clock_secs = 1200`
config still renders a 600s countdown even though the Supervisor enforces the
real cap. (3) Help is undiscoverable: `?` opens Doctor, not a keymap, and the
sole legend is the dense one-line status bar that clips on narrow terminals,
silently dropping its trailer. (4) `render_error_pane`
(`crates/makina/src/ui.rs:2208`) auto-scrolls to the newest entry but has no
`ScrollablePanel::ErrorPane` routing entry, so neither the wheel nor `↑/↓` can
walk back through error history.

**Edits:**

**Fix the inactive-tab foreground.** In `render_tab_bar`
(`crates/makina/src/ui.rs:936–938`), change the inactive branch's foreground from
`Dim` to `Foreground` so the label contrasts with the dim background:

```rust
// Inactive tab: keep the dim background, but draw the label in Foreground so it
// stays legible — Dim-on-Dim rendered the label invisible (Ayu Dark: same hue).
Style::default()
    .bg(app.active_theme.get(crate::theme::ThemeRole::Dim))
    .fg(app.active_theme.get(crate::theme::ThemeRole::Foreground))
```

**Seed `wall_clock_secs_config` when a run opens.** In `App::update`, in the
`Event::RunOpened` handler (`app.rs:3170`) and/or the `self.selected_run`
assignment sites (`app.rs:1396`/`1405`/`1443`), pull the configured cap off the
App's resolved caps so the countdown matches config. `RunView` has no `caps`
field; the value lives on `self.caps` (`CapsConfig`, `app.rs:1259`, seeded in
`App::with_config`, `app.rs:1676`), and `self.caps.wall_clock_secs` is the exact
path already read at `app.rs:2627`:

```rust
// Seed the displayed countdown from the resolved config cap on the App; the
// Supervisor still enforces the real cap. (RunView carries no caps field.)
self.wall_clock_secs_config = self.caps.wall_clock_secs;
```

**Add a help overlay bound to `?`.** Add a `help_mode_active: bool` field to `App`
(default false) and a `render_help_overlay` to `crates/makina/src/ui.rs`,
modelled on the file-browser modal, that draws a bordered full-screen block with
keybindings grouped by category (Run Control, Navigation, View, Accordion,
Selection, Other). In `crates/makina/src/event.rs`, retarget `?` to toggle help
instead of opening Doctor (move Doctor to `!` or a palette entry), and dispatch
the overlay last in the `render` path so it sits on top:

```rust
// `?` toggles the help overlay (Doctor moves to `!`); the overlay is the
// discoverable, non-clipping legend that the one-line status bar cannot be.
KeyCode::Char('?') => AppEvent::ToggleHelpMode,
```

```rust
/// Full-screen help overlay: keybindings grouped by category, dismissable with
/// Escape or `q`. Drawn after the normal layout so it overlays everything.
fn render_help_overlay(app: &App, frame: &mut Frame, area: Rect) { /* .. */ }
```

**Make the error pane scrollable.** Add `ErrorPane` to the `ScrollablePanel` enum
(`crates/makina/src/app.rs:1021`; existing variants are `Sidebar`/`Exchange`/
`PlanAccordion`/`DependencyView`/`TaskEntry`). The error pane is a toggled
overlay (`error_pane_open: bool`, `app.rs:1204`), not a focusable `Panel`
(the `Panel` enum is `{Sidebar, Main}` only, `app.rs:607`), so route on
`error_pane_open` + mouse position rather than "focus". For the MOUSE, push a
`PanelGeometry { panel: ScrollablePanel::ErrorPane, .. }` hitbox in
`crates/makina/src/ui.rs` (mirror the `panel_geoms.push(..)` sites at
`ui.rs:499`/`561`/`678`/`685`) so the existing
`ScrollUp/ScrollDown → ScrollUpAt/ScrollDownAt` routing (`event.rs:1200–1201`)
resolves the wheel by position. For the KEYBOARD, add `PgUp`/`PgDn` arms in
`translate_key` (`event.rs:1336–1437`) gated on `error_pane_open` — NOT `Up`/
`Down`, which are already bound to `SelectUp`/`SelectDown` (`event.rs:1419–1420`).
The per-panel offset lives in the existing `scroll_offsets`
(`HashMap<ScrollablePanel, u16>`, `app.rs:1176`); render via `app.panel_offset`
and mutate via `app.scroll_up`/`app.scroll_down` (`app.rs:1914`/`1933`/`1959`),
exactly as the other panels do (`ui.rs:323–324`, `ui.rs:2082–2083`). In
`render_error_pane`, read that offset honoring an `error_pane_auto_follow`
toggle instead of unconditionally auto-scrolling to the newest entry:

```rust
// Mirror the exchange pane (ui.rs:1733–1749): record scroll_max for clamping,
// then render at the bottom while following or at the stored offset otherwise.
let scroll_max = lines.len().saturating_sub(pane_height) as u16;
app.last_scroll_maxes
    .borrow_mut()
    .insert(ScrollablePanel::ErrorPane, scroll_max);
let scroll_offset = app.panel_offset(ScrollablePanel::ErrorPane, scroll_max);
let para = Paragraph::new(lines)
    .wrap(Wrap { trim: false })
    .scroll((scroll_offset, 0));
```

**Add an `error_pane_auto_follow` toggle.** Add `error_pane_auto_follow: bool`
(default `true`) to `App`, mirroring `exchange_auto_follow` (`app.rs:1171`, init
`app.rs:1613`). Extend `App::scroll_up`/`scroll_down` (`app.rs:1914`/`1933`) with
an `ErrorPane` branch that disengages follow on a manual scroll-up and re-engages
it when the offset returns to `scroll_max` — the same logic the `Exchange` branch
already uses — and make `panel_offset` honor `error_pane_auto_follow` for the
`ErrorPane` (as `effective_offset` does for `Exchange`). A new `push_error` then
re-pins to the bottom only while follow is engaged, so a fresh error never yanks
the view back down once the user has scrolled up.

**Properties that make this safe:**

- The inactive-tab change swaps one foreground colour; no state, layout, or
  interaction logic moves, so tab behavior cannot regress and the active-tab
  styling is untouched.
- `wall_clock_secs_config` is reseeded from the already-available `self.caps`
  field, and the field only drives the displayed countdown — the Supervisor's cap
  enforcement is a separate path and is unaffected.
- The help overlay is a new, optional, dismissable layer keyed off a single new
  bool; it adds no binding to existing keys (only `?`/`!` swap targets) and
  blocks no other interaction, so the normal layout stays clean.
- The error pane reuses the existing per-panel `scroll_offsets` map and the
  `scroll_up`/`scroll_down`/`panel_offset` helpers (no new scroll mechanism); the
  offset is clamped and persists across overlay toggles, and the new
  `error_pane_auto_follow` toggle (mirroring `exchange_auto_follow`) keeps live
  tailing the default while letting a manual scroll-up freeze the view, so manual
  inspection and live tailing coexist.

## Test strategy

- **0001 (README).** Documentation-only; verification is a reviewer reading the
  diff against the implementation — the README must no longer claim
  `session/request_permission` is unanswered, must drop the `--yolo` example,
  must document `WorktreePolicy`, and must state persistence is implemented
  (plans 0029/0030). No automated test is added.
- **0002 (inactive tabs).** A render-level assertion confirms the inactive-tab
  span carries `Foreground` (not `Dim`) as its foreground so the label is legible
  against the `Dim` background.
- **0002 (wall-clock sync).** A test sets `app.caps.wall_clock_secs = 1200`, drives
  the run-open path, and asserts `wall_clock_secs_config` is reseeded to `1200`
  (not the hardcoded `600`).
- **0002 (help overlay).** `test_help_overlay_opens_on_question_mark` asserts `?`
  toggles `help_mode_active`; `test_help_overlay_closes_on_escape` asserts Escape
  (or `q`) dismisses it, and that Doctor's binding moved off `?`.
- **0002 (error-pane scroll).** `test_error_pane_scrolls_up_on_pgup` and
  `test_error_pane_scrolls_down_on_pgdn` assert `PgUp`/`PgDn` adjust
  `scroll_offsets[ScrollablePanel::ErrorPane]` when `error_pane_open` is true, with
  the offset clamped to the valid range; `test_error_pane_auto_follow_disengages_on_scroll_up`
  and `test_error_pane_new_error_does_not_yank_when_scrolled_up` cover the
  `error_pane_auto_follow` behavior.
- All tests keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.

## Interaction with prior work

- **0029 / 0030 (relocated, atomic task-graph persistence).** 0001 documents the
  persistence and crash recovery these plans landed; it changes no code and only
  removes the stale "not implemented yet" claim.
- **0024 / `WorktreePolicy` permission teeth.** 0001 documents the auto-allow +
  audit behavior already shipped in `crates/makina-acp/src/permission.rs`; the
  policy and its audit ledger are unchanged.
- **0033 / mouse-position-aware scroll routing with scrollbars.** 0002 reuses the
  per-panel `scroll_offsets` map, the `PanelGeometry` hitbox routing, and the
  position-based `ScrollUpAt`/`ScrollDownAt` path, adding only the
  `ScrollablePanel::ErrorPane` variant, its hitbox, and a `PgUp`/`PgDn` keyboard
  arm — no new scroll mechanism is introduced.
- **0036 / 0037 (Ayu theming and distinctness audit).** 0002's inactive-tab fix
  consumes the `ThemeRole::Foreground` / `Dim` roles these plans defined, closing
  the last Dim-on-Dim contrast gap they left in the tab bar; no theme value is
  changed.
