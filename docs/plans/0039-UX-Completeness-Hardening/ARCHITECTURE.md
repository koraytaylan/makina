# Architecture — Plan 0039 (deltas)

> The concrete deltas. This plan touches
> `crates/makina/src/theme.rs`, `crates/makina/src/markup.rs`,
> `crates/makina/src/app.rs`, `crates/makina/src/ui.rs`, and
> `crates/makina/src/event.rs`.
> Line numbers are hints; locate by symbol.

## 0001 — Theme-Robustness-And-Markdown-Caching

Today `Theme::get` (`crates/makina/src/theme.rs:48–53`) panics if a role is
missing: it looks the role up in the `colors` map and calls
`unwrap_or_else(|| panic!(...))`. All built-in themes define every `ThemeRole`,
guarded by the `themes_define_every_role_and_ansi_entry` test
(`crates/makina/src/theme.rs:245`), but the architecture invites custom themes
(semantic roles, persisted name in config), so a half-built custom theme would
crash the app at render time instead of degrading gracefully. Separately,
`render_markdown` (`crates/makina/src/markup.rs:76–337`) is a full
`pulldown_cmark` parse-and-render called per frame across **5** production sites
in `ui.rs`: the task-preview body in `render_plan_task_pane`
(`crates/makina/src/ui.rs:1047`, fn at `ui.rs:959`), the task entry text in
`render_task_entry_pane` (`ui.rs:1834`, fn at `ui.rs:1771`), the accordion
section body in `render_accordion_section` (`ui.rs:2152`, fn at `ui.rs:2102`),
and the streaming response body + verbose thought body in `render_exchange_pane`
(`ui.rs:2481` and `ui.rs:2535`, fn at `ui.rs:1528`). For large plan-accordion
bodies this re-parses every section body on every 250ms tick, with no caching.
A **streaming** response (`ExchangeContent::Response`, `complete == false`) grows
its text every frame and appends a cursor (`ui.rs:2489`), so its hash changes
every frame and it never hits the cache mid-stream — the cache helps the static
accordion/task bodies and **complete** responses, not in-flight streams.

**Edits:**

**`Theme::get` returns a neutral fallback instead of panicking.** In
`crates/makina/src/theme.rs:48–53`, replace the panicking arm with a logged
fallback to `Color::Reset` so a missing role degrades gracefully and the user
can fix the config without the app dying at render time:

```rust
// Missing role: degrade gracefully to the terminal default and warn, rather
// than panicking at render time — custom themes may omit roles transiently.
// `tracing::warn!` (fully qualified, no import) — the makina crate has no `log`
// dependency; it logs via tracing. `Color::Reset` (not `Foreground`) so the
// fallback can never recurse through `get(Foreground)` and loop.
self.colors.get(&role).copied().unwrap_or_else(|| {
    tracing::warn!("theme {} missing role {:?}, using fallback", self.name, role);
    Color::Reset
})
```

**Cache markdown parsing by `(text_hash, width)` on `App` via interior
mutability.** `render` is `&App`-only (`ui.rs:60`) and the module doc
(`ui.rs:3–5`) pins it to hold "no mutable state", so the cache field is a
`RefCell` — exactly like the existing `last_scroll_maxes: RefCell<HashMap<...>>`
field (`app.rs:1182`) that the `&App` render pass already writes each frame
(`ui.rs:1059–1061`). Add a `RefCell` cache field to `App`
(`crates/makina/src/app.rs`), cleared on tab/content change, plus a
deterministic text hasher, so identical `(text, width)` renders are computed
once:

```rust
/// Rendered-markdown cache keyed by (hash(text), pane width). `RefCell` so the
/// `&App` render pass can populate it on a miss (mirrors `last_scroll_maxes`).
/// Repopulated on miss and cleared whenever the active tab or rendered content
/// changes, so the per-frame render loop never re-parses unchanged bodies.
pub markdown_cache: std::cell::RefCell<HashMap<(u64, u16), Vec<Line<'static>>>>,
```

```rust
/// Deterministic content hash for the markdown cache key. Same text always
/// hashes to the same u64, so cache hits are content-stable across frames.
fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}
```

**Route every render site through one cached helper.** In
`crates/makina/src/ui.rs`, replace the **5** direct `markup::render_markdown`
calls (lines 1047, 1834, 2152, 2481, 2535) with a single helper that owns the
check/store logic, so all call sites share one cache and one invalidation point.
The helper takes `&App` (it must — `render` is `&App`) and writes through the
`RefCell`, so `render`'s signature is unchanged:

```rust
/// Cached front door to `markup::render_markdown`: returns the cached lines for
/// (text, width) on a hit, otherwise renders once and stores the result through
/// the `RefCell` cache. Takes `&App` so `render` stays immutable; output is
/// byte-identical to a direct `render_markdown` call, just faster.
fn render_markdown_cached(app: &App, text: &str, base: Style, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let key = (hash_text(text), width);
    if let Some(lines) = app.markdown_cache.borrow().get(&key) {
        return lines.clone();
    }
    let lines = markup::render_markdown(text, base, width, theme);
    app.markdown_cache.borrow_mut().insert(key, lines.clone());
    lines
}
```

Invalidation is conservative and is the **only** `&mut self` use of the cache:
`self.markdown_cache.borrow_mut().clear()` runs from `App::update` when the
active tab changes or the task/response content updates. Finer-grained
invalidation (tracking which hashes are still in scope) is deferred (see SCOPE).

**Properties that make this safe:**

- The theme fallback is additive: it only changes the missing-role path, so the
  `themes_define_every_role_and_ansi_entry` invariant and every built-in theme
  keep returning their declared colors unchanged.
- The cache is transparent to callers: the key is deterministic
  (`hash_text(text)` plus pane `width`) and the helper returns byte-identical
  output to `render_markdown`, so a hit and a miss produce the same lines.
- The cache uses interior mutability (`RefCell`), so `render` keeps its `&App`
  signature and the "no mutable state" module contract holds at the API level —
  the same pattern `last_scroll_maxes` already uses. A streaming response is a
  cache miss every frame (its text+cursor change), so it costs the same as today
  and the streaming re-parse is **not** claimed eliminated.
- Invalidation clears the whole cache on tab or content change, so a stale entry
  can never outlive the text that produced it — correctness is preserved even
  though the invalidation is coarse.
- The theme fallback leaves `Theme::get` total: missing roles return
  `Color::Reset` (never another role lookup), so the fallback path cannot recurse
  or loop.

## 0002 — Key-Feedback-Sidebar-And-Provider-Editor

Today the accordion keys `s` / `a` / `t` / `z` toggle sections only when a plan
tab is active and the main pane is focused
(`crates/makina/src/event.rs:1369–1396`); outside that context they fall through
to `AppEvent::Tick`, a silent no-op with no feedback, and the accordion footer
help (`crates/makina/src/ui.rs:2035`) only renders *inside* a plan tab, so the
keys are undiscoverable elsewhere. Separately, the body layout is a hardcoded
30%/70% split (`crates/makina/src/ui.rs:90`) with no user control and no
minimum-size guard: on a 40-column terminal the sidebar is 12 cols — too narrow
for labels like `makina/0034-test-plan` — and a 20×5 terminal renders
overlapping, collapsed panes with no graceful message.

**Edits:**

**Emit a status message instead of a silent no-op.** In
`crates/makina/src/event.rs:1376–1396`, replace the `AppEvent::Tick` fallbacks
for `a` / `t` / `z` with a transient status message so the keypress is
acknowledged (`s` keeps its primary `StartRun` binding):

```rust
// Outside a plan tab the accordion keys have nothing to toggle; tell the user
// the key was received rather than dropping it into a silent Tick.
'a' | 't' | 'z' => AppEvent::StatusMessage(
    "Accordion toggle not available here — open a plan tab.".to_string(),
),
```

The `StatusMessage(String)` variant on `AppEvent` (`crates/makina/src/app.rs`)
and its `App::update` handler already exist for other transient messages; this
task reuses them and adds the variant only if a grep confirms it is absent.

**Make the sidebar width user-adjustable with a bounded range.** Add a session
preference to `App` (`crates/makina/src/app.rs`), initialized to 30, and two
events that step it by 2% within `[10, 50]` so neither pane can be starved:

```rust
/// User's current sidebar width as a percentage of the body. Adjusted by
/// ResizeSidebarLeft/Right and clamped to [10, 50] so neither pane collapses.
pub sidebar_width_percent: u16,
```

```rust
// Step the sidebar by 2% per keystroke, clamped so the sidebar stays in
// [10, 50]% and the main pane always keeps at least half the body.
AppEvent::ResizeSidebarLeft => {
    self.sidebar_width_percent = self.sidebar_width_percent.saturating_sub(2).max(10);
    true
}
AppEvent::ResizeSidebarRight => {
    self.sidebar_width_percent = (self.sidebar_width_percent + 2).min(50);
    true
}
```

Shift+Left / Shift+Right map to these events in `translate_key`
(`crates/makina/src/event.rs:1246`; the arm is `KeyCode::Left if
key.modifiers.contains(KeyModifiers::SHIFT)` — the parameter is `key`, not
`event`). The two SHIFT-guarded arms must sit **after** the ALT-guarded arms
(`event.rs:1412–1413`) and **before** the bare `KeyCode::Right`/`KeyCode::Left`
arms (`event.rs:1422`/`1427`), or clippy `-D warnings` flags them unreachable.
The body split at `crates/makina/src/ui.rs:88–91` reads the preference instead
of the literal:

```rust
// Body split is driven by the user's saved sidebar width, not a fixed 30/70.
let body = Layout::default()
    .direction(Direction::Horizontal)
    .constraints([
        Constraint::Percentage(app.sidebar_width_percent),
        Constraint::Percentage(100 - app.sidebar_width_percent),
    ])
    .split(body_area);
```

**Guard against terminals too small to render.** As the **first statement** in
`render` (`crates/makina/src/ui.rs:60`), before any layout split or widget,
short-circuit to a centered fallback message (drawn via `centered_rect`,
`ui.rs:3267`) when the frame is below the usable threshold, so cramped panes
never overlap. The threshold must sit **below** the smallest full-`render` test
size — the smallest is `make_terminal(80, 10)` (`accordion_scrollbar_renders_when_tall`,
`ui.rs:3672`, drawing the top-level `render` at `ui.rs:3717`) — so `MIN_W = 40`,
`MIN_H = 10` keeps 80×10 rendering normally (`10 < 10` is false). The
`render_plan_accordion_pane_*` "very small terminal" tests near `ui.rs:8068`/`8133`
call `render_plan_accordion_pane` directly (not the top-level `render`), so they
are unaffected.

```rust
// First statement in render(), before any layout. Below this the normal panes
// overlap and are unusable; draw a single readable centered message and return.
const MIN_W: u16 = 40;
const MIN_H: u16 = 10;
if area.width < MIN_W || area.height < MIN_H {
    // centered "Terminal too small (min 40×10) — resize to continue."
    let popup = centered_rect(/* .. */, area);
    // .. render the message into `popup` ..
    return;
}
```

**Properties that make this safe:**

- The status message reuses the existing `status_message` path, so it is cleared
  opportunistically on the next significant event (e.g. the `RunLoaded` handler
  at `app.rs:2314–2319` clears it unless it contains "fail"/"error") — there is
  no timer, and only one message is held at a time, so it cannot accumulate or
  clutter the bar. The string is kept short because the status-bar trailer clips
  on narrow terminals (`ui.rs:795–798`).
- The sidebar width is clamped to `[10, 50]` at every mutation, so the user can
  never allocate 0% or 100% and starve a pane; the split therefore always
  produces two non-degenerate columns.
- The small-terminal guard is the first statement in `render` and triggers when
  *either* bound is below the threshold (`width < MIN_W || height < MIN_H`),
  returning before any pane is drawn, so the fallback message is always the only
  thing on screen and always fits. The threshold sits below the smallest
  full-`render` test (80×10), so no existing render test regresses.
- The `s` key keeps its `StartRun` binding and the sidebar's existing arrow-key
  tree navigation is untouched (only Shift+Left/Right are added), so no existing
  keybinding regresses.

## 0003 — Provider-Editor-Rename-Or-Implement

Today the provider editor modal (`crates/makina/src/event.rs:1300–1309`,
`crates/makina/src/app.rs:2422–2437`, `crates/makina/src/ui.rs:2677–2800`) is
titled "Configure Providers & Roles", but it is read-only:
`ProviderEditorUp` / `ProviderEditorDown` only move a `selection_index`, there
is no key to edit a provider or a role assignment, and the only action is Enter,
which commits the *unchanged* config back to disk. The "Configure" title
overpromises an edit capability the modal does not have.

**Edits:**

This plan takes **Option A — rename to an honest read-only label** (see SCOPE
for why; the edit path is deferred to a future plan). Change the modal title in
`crates/makina/src/ui.rs:2689` and clarify the limitation in the footer:

```rust
// The modal only views config — say so. The edit path is a future plan.
let title = " View Providers & Roles ";
```

```rust
// Footer note makes the read-only contract explicit and points at the source
// of truth, so users know where to make changes.
let footer = "(Read-only; edits via .makina/config.toml)";
```

Annotate the struct so the deferral is recorded at the definition site, above
`ProviderEditor` (`crates/makina/src/app.rs:561`):

```rust
/// A read-only view of the current providers and role assignments.
/// Edit paths (add/remove provider, reassign roles, change model/effort) are
/// deferred to a future plan.
pub struct ProviderEditor { /* .. */ }
```

**Properties that make this safe:**

- The rename is purely cosmetic — no event variant, mutation path, or commit
  logic changes — so there is no behavior to regress; `ProviderEditorUp/Down`
  and the Enter-commit path are byte-for-byte unchanged.
- The footer note is rendered into the existing modal footer area, so it adds a
  line of text without altering the modal's layout contract or focus handling.
- The view remains useful: users still see the live config and now know,
  inline, that edits are made in `.makina/config.toml` — the label finally
  matches the capability.

## Test strategy

- **0001 (theme + cache).** `test_theme_get_missing_role_returns_default`
  constructs a `Theme` with an incomplete `colors` map, calls `get` on a missing
  role, and asserts the result is `Color::Reset` with no panic, while the
  existing `themes_define_every_role_and_ansi_entry` stays green.
  `test_markdown_cache_hits_on_same_text_and_width` calls `render_markdown_cached`
  twice with identical text and width and asserts the second call is a hit
  (`app.markdown_cache.borrow().len()` stays 1 — no growth beyond the first
  insert), and that invalidation clears the cache on tab/content change.
- **0002 (feedback + sidebar).**
  `test_accordion_key_outside_plan_tab_emits_status` builds an app with no plan
  tab active, dispatches `a`, and asserts the result is
  `AppEvent::StatusMessage` with the expected text.
  `test_sidebar_resize_left_clamps_to_min` and
  `test_sidebar_resize_right_clamps_to_max` dispatch the resize events ten times
  and assert the width clamps to 10% and 50% respectively.
  `test_small_terminal_renders_fallback_message` calls the top-level `render`
  into a 20×5 area and asserts only the "terminal too small" message is drawn,
  not the normal layout; the pre-existing 80×10 full-`render` test
  (`accordion_scrollbar_renders_when_tall`) must still pass.
- **0003 (provider editor).** `test_provider_editor_title_is_view_not_configure`
  opens the provider editor modal, renders a frame, and asserts the title
  contains "View" and not "Configure"; existing provider-editor tests stay green.
- All tests keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.

## Interaction with prior work

- **0036 / 0037 (TUI polish).** This plan lands the short-term completeness
  follow-ups the glm-5.2 review left after the 0036/0037 polish work: it reuses
  the existing `Theme` / `ThemeRole` machinery (adding only the missing-role
  fallback), the existing accordion key map and `AppEvent::StatusMessage`
  transient-message path, and the existing provider-editor modal — extending,
  not replacing, each.
- **0020 (markdown rendering hardening).** 0001's cache wraps
  `markup::render_markdown` unchanged: the helper is a pure front door over the
  already-hardened parser, so plan 0020's rendering invariants (code blocks,
  lists, links, width-respecting wrap) carry over verbatim and no new parser is
  introduced.
- **Provider editor edit path.** 0003 deliberately renames rather than
  implements; the add/remove-provider, reassign-role, and change-model/effort
  edit path is reserved for a separate future plan (see SCOPE), so this plan adds
  no new event variants or mutation handlers to the editor.
