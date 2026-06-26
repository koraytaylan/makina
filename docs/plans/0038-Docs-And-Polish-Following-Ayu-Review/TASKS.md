# XAgent Plan 0038 — Docs-And-Polish-Following-Ayu-Review

Update README.md to document the WorktreePolicy auto-allow feature and remove stale --yolo claims, eliminating the misleading "does not yet answer session/request_permission" statement that undersells the headline governance feature; remove the obsolete persistence claim now satisfied by plans 0029/0030. Then fix four TUI polish bugs in one concise PR: (1) change inactive tab label styling from unreadable `bg(Dim).fg(Dim)` to `bg(Dim).fg(Foreground)` or `bg(Dim).fg(Background)`, (2) seed `wall_clock_secs_config` from `self.caps.wall_clock_secs` when a run opens so the countdown displays the actual configured value, (3) add a full-screen help overlay keyed to `?` showing all keybindings grouped by category, and (4) add `ErrorPane` to the scrollable-panel routing map (mouse wheel via a position hitbox + `PgUp`/`PgDn`) so the error overlay scrolls while `error_pane_open`, with an `error_pane_auto_follow` toggle.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md) for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}`).
- **Depends on** lists *direct* prerequisites only; "—" means none.
- **Done when** is the verifiable criterion, and every task keeps the gate
  commands green — the full forms are `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`;
  abbreviated as "cargo test/clippy/fmt green" thereafter.
- GPU tests self-skip without an adapter.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0001 — README-Fix-Permissions-And-Persistence

### readme-permissions-fix — Fix README Stale Permission and Persistence Claims

The README at `README.md:151–153` states that Makina "does not yet answer `session/request_permission`" and recommends the `--yolo` flag. This is no longer true: `WorktreePolicy` (`makina-acp/permission.rs:82–136`) implements auto-allow-inside-worktree permission handling with full audit logging. Additionally, `README.md:154–155` says persistence is "not implemented yet", but plans 0029/0030 landed atomic `.makina/tasks/{slug}.json` persistence and crash recovery. These claims undersell the headline governance feature and are factually incorrect.

**Steps:**

1. Open `README.md` and locate the Limitations section starting at line 149.
2. Delete the sentence: "Some agents require an auto-approve flag (gemini's `--yolo`); without it the agent's permission prompt stalls the turn — Makina's ACP client does not yet answer `session/request_permission`." This is lines 151–153.
3. Locate the example config block around line 64 with `args = ["--acp", "--yolo"]`. Change it to `args = ["--acp"]` (remove `--yolo`).
4. Replace the persistence claim at lines 154–155 from "Task-graph state lives in memory; `.makina/tasks/{slug}.json` persistence and crash recovery are not implemented yet." to "Task-graph state and task metadata are persisted in `.makina/tasks/{slug}.json` with crash recovery (plans 0029/0030)."
5. If a Limitations section still exists after removing the permission claim, consider renaming it to "Limitations & Governance" and add a subsection documenting `WorktreePolicy`: "Permission requests are answered automatically via `WorktreePolicy`, which auto-allows operations inside the assigned worktree and audits every decision to an audit ledger." Do NOT cite a concrete path like `.makina/audit.log` — that path does not exist. The real on-disk ledger is written by `JsonlAuditSink` to a per-run `audit.jsonl` (`crates/makina-core/src/audit.rs`; path helper `paths::audit_log` → `…/runs/{run_id}/audit.jsonl`, `paths.rs:226`). Only cite a concrete path if you first grep `crates/makina-core/src/paths.rs` / `audit.rs` and use the exact one; otherwise describe it generically as "an audit ledger" to avoid introducing a new stale claim.
6. Verify the changes read naturally and the README is in sync with the current implementation.

- **Depends on:** —
- **Done when:** The README no longer claims that `session/request_permission` is unanswered; the `--yolo` example is removed; the persistence claim is updated to reflect plans 0029/0030; and a note on `WorktreePolicy` is present in the Limitations section or a new Governance section. The prose reads naturally and the changes are verifiable by a reviewer reading the diff. cargo test/clippy/fmt green. (Documentation-only task; no test required.)

---

## 0002 — TUI-Polish-Fixes

### inactive-tab-styling-fix — Fix Inactive Tab Label Styling (bg(Dim).fg(Dim) → Readable)

The tab-bar renderer at `ui.rs:936–938` styles inactive tabs with `Style::default().bg(Dim).fg(Dim)`. In Ayu Dark this renders as `#5A6378` foreground text on a `#5A6378` background — the labels are invisible. This is the most visible polish defect in the current build; opening multiple tabs causes the inactive ones to vanish. Active tabs use `Accent` bg + `Background` fg + bold, which is correctly readable.

**Steps:**

1. Open `crates/makina/src/ui.rs` and locate the `render_tab_bar` function (line 887).
2. Find the inactive-tab style block at lines 936–939. It currently reads:

```rust
Style::default()
    .bg(app.active_theme.get(crate::theme::ThemeRole::Dim))
    .fg(app.active_theme.get(crate::theme::ThemeRole::Dim))
```

3. Replace line 938 (the `.fg(Dim)` call) with `.fg(app.active_theme.get(crate::theme::ThemeRole::Foreground))`. The corrected block should be:

```rust
Style::default()
    .bg(app.active_theme.get(crate::theme::ThemeRole::Dim))
    .fg(app.active_theme.get(crate::theme::ThemeRole::Foreground))
```

4. Save the file and verify the change is minimal (one line, one style attribute).
5. Compile and run the app with multiple tabs open to verify inactive tab labels are now readable (dark text on a slightly darker background, but with sufficient contrast).

- **Depends on:** —
- **Done when:** Inactive tab labels use `Foreground` colour instead of `Dim` for the foreground text, making them readable against the `Dim` background. Visually, opening multiple tabs shows all inactive tabs with readable labels (not invisible). cargo test/clippy/fmt green.

---

### wall-clock-sync-fix — Seed wall_clock_secs_config from Config on Run Open

`App` initializes `wall_clock_secs_config: 600` at `app.rs:1625` as a hardcoded default and never updates it from the resolved cap. When a user sets `wall_clock_secs = 1200` in `.makina/config.toml`, the TUI countdown displays 600s instead of the actual 1200s deadline. The Supervisor enforces the correct cap; only the displayed countdown is wrong. The resolved cap is already on the `App` as `self.caps.wall_clock_secs` (`app.rs:1259`, seeded in `App::with_config` at `app.rs:1676`); the fix simply reseeds the display field from it when a run opens. (Note: `RunView` has NO `caps` field — do not read `run.caps`.)

**Steps:**

1. Open `crates/makina/src/app.rs` and verify the `wall_clock_secs_config` field is at line 1165 (or nearby).
2. Locate the `update` method in `App` (search for `pub fn update`).
3. Find the `Event::RunOpened` handler (`app.rs:3170`) where a run is activated, and/or the sites where `self.selected_run` is assigned (`app.rs:1396`, `1405`, `1443`) where a run selection changes.
4. In that handler, after the run is selected/opened, seed `wall_clock_secs_config` from the App's resolved caps (NOT from the run — `RunView` has no `caps` field). The value lives on the `App` field `self.caps` (type `makina_core::config::CapsConfig`, declared at `app.rs:1259`, seeded in `App::with_config` at `app.rs:1676`; `wall_clock_secs` is at `config.rs:286`; the exact path `self.caps.wall_clock_secs` is already used at `app.rs:2627`). Add:

```rust
self.wall_clock_secs_config = self.caps.wall_clock_secs;
```

5. For the pattern reference, note that `self.caps.wall_clock_secs` is already read at `app.rs:2627` when seeding the settings modal; mirror that path here.
6. Confirm `self.caps.wall_clock_secs` compiles (it is the same path already used at `app.rs:2627`).

- **Depends on:** —
- **Done when:** When a run is opened or selected, `wall_clock_secs_config` is updated from `self.caps.wall_clock_secs`. The wall-clock countdown displayed in the exchange pane now matches the configured value, not the hardcoded 600s default. If a user sets `wall_clock_secs = 1200` in config and opens a run, the countdown displays the correct 1200s. cargo test/clippy/fmt green.

---

### help-overlay-implementation — Add Full-Screen Help Overlay (? Keybinding)

Help discoverability is a gap: `?` opens Doctor, not a keymap. The status bar (`ui.rs:807`) is a single dense line that clips on narrow terminals (`ui.rs:795–798`), silently dropping the trailer with focus and last-event hints. On narrow windows many keybindings are invisible. There is no `h`/`F1`/`?`-mode help overlay showing all keybindings grouped by category. This workstream adds a full-screen help overlay similar to the file-browser modal at `ui.rs:821–824`.

**Steps:**

1. Open `crates/makina/src/app.rs` and add a new field to `App` to track help-overlay visibility: `pub help_mode_active: bool` (initialized to false).
2. In `crates/makina/src/event.rs`, locate the key handler for `?` (search for `KeyCode::Char('?')`). Currently it opens Doctor (likely via `AppCommand::OpenDoctor` or similar). Change this to toggle help mode: `AppEvent::ToggleHelpMode` or set `app.help_mode_active = !app.help_mode_active`.
3. Move the Doctor keybinding to `!` (if it is a character) or map it to a different key/palette entry. Update the status bar at `ui.rs:807` to reflect the new binding.
4. In `crates/makina/src/ui.rs`, add a new function `render_help_overlay(app: &App, frame: &mut Frame, area: Rect)` that displays a full-screen overlay with keybindings grouped by category. Do NOT hand-author the binding list from memory — ENUMERATE the real bindings straight from `translate_key` (`event.rs:1336–1437`) and render exactly those, so the overlay can never drift from the actual key handler. The current real bindings include (verify each against `translate_key` before rendering):
   - **Run Control:** `[o]` open browser, `[s]` start, `[p]` pause, `[c]` cancel, `[r]` retry focused
   - **Navigation:** `[Tab]` next focus, `[Shift+Tab]`/`[BackTab]` prev focus, `[↑↓]`/`[j/k]` select up/down, `[←→]` focus/expand-collapse, `[Alt+←/→]` prev/next tab, `[Ctrl+W]` close tab
   - **View:** `[v]` cycle dependency view, `[e]` toggle error pane, `[l]` open log, `[Ctrl+O]` toggle verbose, `[g]` provider/role editor
   - **Accordion** (main pane, plan tab active): `[s/a/t/z]` toggle Scope/Architecture/Tasks/Status sections
   - **Tree:** `[Space]` toggle tree node, `[Enter]` open node / toggle section
   - **Other:** `[?]` this help, `[d]` dismiss provider warning, `[q]`/`[Esc]` quit
   - There is NO `Ctrl+A` "select all" binding and no generic "[d] dismiss"; `[d]` is specifically `DismissProviderWarning` and `Ctrl+W` is `CloseTab` (not a selection key). Render only bindings that actually exist in `translate_key`.
   - Include brief descriptions for each group. Render the overlay as a bordered block with centered text, dismissable with `Escape` or `q`.
5. In the main render path (`crates/makina/src/ui.rs`, the `render` function around line 350–490), add a dispatch at the end (after all other panes) to render the help overlay if `app.help_mode_active` is true, so it sits on top of the normal layout.
6. Add unit tests: `test_help_overlay_opens_on_question_mark` and `test_help_overlay_closes_on_escape`.

- **Depends on:** inactive-tab-styling-fix, wall-clock-sync-fix
- **Done when:** Pressing `?` opens a full-screen help overlay displaying all keybindings grouped by category (Run Control, Navigation, View, Accordion, Selection, Other). The overlay is dismissable with `Escape` or `q`. The `?` key no longer opens Doctor; Doctor is bound to `!` or a menu entry. The status bar is updated to show `[?] help` instead of `[?] doctor`. Tests pass. cargo test/clippy/fmt green.

---

### error-pane-scroll-support — Make Error Pane Scrollable (ScrollablePanel::ErrorPane)

`render_error_pane` at `ui.rs:2208` auto-scrolls to the latest message (`ui.rs:2250–2257`), but there is no `ScrollablePanel::ErrorPane` entry in the scroll-routing map and no key to manually scroll. A long error history cannot be inspected upward; older errors vanish once they scroll off. The error pane is the primary diagnostic surface for agent/gate failures, so this is a real usability gap. Note: the error pane is a toggled OVERLAY, not a focusable panel — it is controlled by `error_pane_open: bool` (`app.rs:1204`), toggled by `'e'` (`event.rs:1352`). The `Panel` enum is `{Sidebar, Main}` only (`app.rs:607`), so there is no "error-pane focus" state to route on; route on `error_pane_open` and mouse position instead.

**Steps:**

1. Open `crates/makina/src/app.rs` and locate the `ScrollablePanel` enum (`app.rs:1021`). Its real variants are `Sidebar`, `Exchange`, `PlanAccordion`, `DependencyView`, `TaskEntry`.
2. Add a new variant `ErrorPane` to that enum (at the end).
3. The per-panel scroll state is `app.scroll_offsets`, a `HashMap<ScrollablePanel, u16>` (`app.rs:1176`, initialized at `app.rs:1614`). No new map is needed — reuse it. Render reads the offset via the existing accessor `app.panel_offset(panel, max)` (`app.rs:1959`) and input mutates it via the existing helpers `app.scroll_up(panel)` / `app.scroll_down(panel, max)` (`app.rs:1914`/`1933`), exactly as the Sidebar/Exchange/PlanAccordion panels do (see `ui.rs:323–324`, `ui.rs:2082–2083`).
4. Add an error-pane hitbox so the existing position-based mouse routing can target it. In `crates/makina/src/ui.rs`, where `render_error_pane`'s content `Rect` is known, push a `PanelGeometry { panel: ScrollablePanel::ErrorPane, .. }` entry into the `panel_geoms` vector (mirror the other `panel_geoms.push(PanelGeometry { .. })` sites at `ui.rs:499`/`561`/`678`/`685`). The existing position-based `MouseEventKind::ScrollUp/ScrollDown → AppEvent::ScrollUpAt/ScrollDownAt` routing (`event.rs:1200–1201`) then resolves the wheel to this panel automatically. Gate this push on `app.error_pane_open` so the hitbox only exists when the overlay is visible.
5. For KEYBOARD scroll, use `PgUp`/`PgDn` (per review B4) routed to the error pane only while `error_pane_open` is true. Do NOT reuse `Up`/`Down`: those are already bound to `AppEvent::SelectUp`/`SelectDown` at `event.rs:1419–1420` and rebinding them would shadow sidebar/list navigation. Add `KeyCode::PageUp`/`KeyCode::PageDown` arms in `translate_key` (`event.rs:1336–1437`) that emit error-pane scroll events when `error_pane_open` is set.
6. Resolve the "manual scroll up" vs "auto-scroll to newest on a new error" tension by adding an `error_pane_auto_follow: bool` field to `App` (default `true`), mirroring the existing `exchange_auto_follow` field (`app.rs:1171`, init `app.rs:1613`). Reference implementation — copy the exchange-pane pattern exactly:
   - **Render** (`render_error_pane`, `ui.rs:2208`): compute `scroll_max` from `lines.len()` and the inner pane height, record it into `app.last_scroll_maxes` for `ScrollablePanel::ErrorPane`, then pick the offset the same way the exchange pane does at `ui.rs:1733–1749` — when following, render at `scroll_max` (pinned to bottom); otherwise render at the stored manual offset clamped to `scroll_max`. This replaces the unconditional `total_lines - pane_height` auto-scroll currently at `ui.rs:2250–2257`.
   - **Scroll up** (in `App::scroll_up`, `app.rs:1914`): mirror the `Exchange` branch — when `error_pane_open` scroll-up disengages `error_pane_auto_follow` and anchors the manual offset to the last rendered bottom.
   - **Scroll down** (in `App::scroll_down`, `app.rs:1933`): mirror the `Exchange` branch — re-engage `error_pane_auto_follow` once the offset reaches `scroll_max` (the user scrolled back to the bottom).
   - **New error arrives**: a new `push_error` re-asserts the bottom view only while `error_pane_auto_follow` is engaged; if the user has scrolled up, the stored offset is left untouched so a new error does NOT yank the view back down.
7. Add tests: `test_error_pane_scrolls_up_on_pgup` and `test_error_pane_scrolls_down_on_pgdn` (asserting the events adjust `scroll_offsets[ScrollablePanel::ErrorPane]` when `error_pane_open` is true), plus `test_error_pane_auto_follow_disengages_on_scroll_up` and `test_error_pane_new_error_does_not_yank_when_scrolled_up`.

- **Depends on:** inactive-tab-styling-fix, wall-clock-sync-fix, help-overlay-implementation
- **Done when:** The error pane (`render_error_pane`) is scrollable while `error_pane_open` is true. When the error pane overlay is open and its content overflows, `PgUp`/`PgDn` scroll it up/down, and mouse wheel over the pane's hitbox scrolls it too. The scroll offset persists when the overlay is toggled away and back. Old error messages (that scrolled off) become visible again when scrolling upward. With `error_pane_auto_follow` engaged (default), a new error snaps the view to the newest entry; once the user has scrolled up (auto-follow disengaged), a new error does NOT yank the view back down, and scrolling back to the bottom re-engages auto-follow — exactly mirroring `exchange_auto_follow`. cargo test/clippy/fmt green.

---

**End of plan 0038 TASKS.** When every "Done when" bullet is green, the README
reflects the current implementation (permissions answered via `WorktreePolicy`,
persistence done in plans 0029/0030) and four high-impact TUI polish bugs are
fixed: inactive tab labels are readable, the wall-clock countdown matches the
configured cap, a discoverable `?` help overlay lists every keybinding, and the
error pane scrolls so the full error history can be inspected — all with the
gate commands green.
