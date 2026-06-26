# XAgent Plan 0039 — UX-Completeness-Hardening

This plan addresses five short-term UX completeness gaps from the glm-5.2 review: (1) **Theme robustness** — `Theme::get` panics on missing roles; add a fallback to `Color::Reset` (the terminal default) and a `tracing::warn!` to enable future custom themes. (2) **Markdown caching** — `render_markdown` re-parses plan-accordion bodies every frame (250ms); cache parsed lines by `(text_hash, width)` in an interior-mutable `RefCell` cache on `App` to eliminate O(n) re-parses for static bodies and complete responses (mid-stream responses stay uncached). (3) **Sidebar resizing** — body layout is fixed 30%/70%; add keyboard-driven width adjustment (Shift+Left/Right) and a minimum-size guard to handle small terminals gracefully. (4) **Key feedback on overloaded keys** — `a`/`t`/`z` keys outside plan tabs are silent no-ops; emit a transient status message "not available here" so users know the keys tried to act. (5) **Provider editor implementation** — the modal is labeled "Configure Providers & Roles" but `ProviderEditorUp/Down` only navigate `selection_index`; there is no edit path, only a read-only view; rename it to "View Providers & Roles" or implement editing (add/remove provider, reassign role). All changes preserve gate-command compliance and land the TUI polish plan 0036/0037 begun.

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

## 0001 — Theme-Robustness-And-Markdown-Caching

### theme-fallback — Theme::get Fallback on Missing Role

`Theme::get` (`crates/makina/src/theme.rs:48–53`) calls `unwrap_or_else(|| panic!("theme {:?} missing role {:?}", self.name, role))` when a role is not found in the `colors` map. Today all built-in themes define every `ThemeRole`, guarded by `themes_define_every_role_and_ansi_entry` test. But the architecture invites custom themes (semantic roles, persisted name in config), and a half-built custom theme would crash the app at render time, leaving users unable to recover without deleting the broken config file. The panic is harsh for a UI that should be resilient to user error. The fallback is `Color::Reset` (the terminal's default) — **not** another role like `Foreground`, because a `Foreground` fallback would recurse through `get(Foreground)` and could infinite-loop if `Foreground` is itself the missing role.

**Steps:**

1. In `crates/makina/src/theme.rs:48–53`, replace the `unwrap_or_else(|| panic!(...))` with `unwrap_or_else(|| { tracing::warn!("theme {} missing role {:?}, using fallback", self.name, role); Color::Reset })`. This returns a neutral fallback and logs a warning so the user can diagnose the issue. Use `tracing::warn!` (fully qualified, no import needed) — the `makina` crate has **no** `log` dependency; it logs via `tracing` (`tracing`/`tracing-subscriber`/`tracing-appender` in `Cargo.toml`). `Color` is already in scope at `theme.rs:1`.
2. Add a unit test `test_theme_get_missing_role_returns_default` in `crates/makina/src/theme.rs` that constructs a `Theme` with an incomplete `colors` map, calls `get` on a missing role, and asserts the result is `Color::Reset` (not a panic).
3. Verify the test passes and that the gate commands remain green.

- **Depends on:** —
- **Done when:** Calling `theme.get(missing_role)` returns `Color::Reset` and emits a `tracing::warn!` (no panic). The test `test_theme_get_missing_role_returns_default` passes. All existing theme tests remain green. cargo test/clippy/fmt green.

---

### markdown-cache — Markdown Parsing Cache by (text_hash, width)

`render_markdown` (`crates/makina/src/markup.rs:76–337`) performs a full `pulldown_cmark` parse and render on every call. There are **exactly 5** production call sites in `ui.rs`: line 1047 in `render_plan_task_pane` (`ui.rs:959`, the task-preview body), line 1834 in `render_task_entry_pane` (`ui.rs:1771`, the task entry text), line 2152 in `render_accordion_section` (`ui.rs:2102`, the accordion section body), and lines 2481 and 2535 in `render_exchange_pane` (`ui.rs:1528`, the streaming response body and the verbose-mode thought body). For large plan-accordion bodies, this is an O(n) re-parse per 250ms tick. Caching by `(text_hash, width)` trades O(1) hash computation for O(n) parsing.

**The render path is `&App`-only and must stay that way.** `render` is declared `pub fn render(app: &App, frame: &mut Frame)` (`ui.rs:60`) and the module doc (`ui.rs:3–5`) states it "holds **no mutable state**". So the cache helper **cannot** take `&mut App`. Use **interior mutability**, exactly as the codebase already does for render-path caches: the `&App` render pass already writes `last_scroll_maxes: RefCell<HashMap<...>>` each frame (field declared at `app.rs:1182`; written via `app.last_scroll_maxes.borrow_mut().insert(...)` at `ui.rs:1059–1061` and other sites). Mirror that pattern.

**Scope of the win:** the cache helps the **static** bodies (the plan-accordion / task-preview / task-entry sites at `ui.rs:1047`, `1834`, `2152`) and **complete** responses. While a response is **streaming** (`ExchangeContent::Response` with `complete == false`), the text grows every frame and a streaming cursor is appended (`ui.rs:2489`), so `hash_text(text)` changes every frame and the `(hash, width)` key never hits mid-stream — the per-frame re-parse at `ui.rs:2481` is **not** eliminated for in-flight streams. Do **not** claim the streaming O(n) re-parse is eliminated; cache mid-stream is a miss every frame (acceptable — same cost as today), and the win lands once the response is complete and on the static accordion bodies.

**Steps:**

1. In `crates/makina/src/app.rs`, add a new field to `App` for the markdown cache using interior mutability so the `&App` render pass can populate it: `pub markdown_cache: std::cell::RefCell<std::collections::HashMap<(u64, u16), Vec<Line<'static>>>>` (`Line` is `ratatui::text::Line`). Mirror the existing `last_scroll_maxes: std::cell::RefCell<std::collections::HashMap<...>>` field at `app.rs:1182`. Initialize it to `RefCell::new(HashMap::new())` in `App::new`.
2. In `crates/makina/src/app.rs`, add a helper function `fn hash_text(text: &str) -> u64` that computes a hash of the input text. Use `std::collections::hash_map::DefaultHasher`: `use std::hash::{Hash, Hasher}; let mut hasher = DefaultHasher::new(); text.hash(&mut hasher); hasher.finish()`.
3. In `crates/makina/src/ui.rs`, add a single cached helper with an `&App` signature (do **not** change `render`'s signature):

   ```rust
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

   Then route the **exactly 5** production call sites (lines 1047, 1834, 2152, 2481, 2535) through it: replace each direct `crate::markup::render_markdown(text, base, width, &app.active_theme)` with `render_markdown_cached(app, text, base, width, &app.active_theme)`. Output is byte-identical to a direct call.
4. Add cache invalidation in `App::update` (`&mut self`) **only** for invalidation — when the active tab changes or the task/response content changes, clear the cache with `self.markdown_cache.borrow_mut().clear()`. (Invalidation is the only place `&mut self` touches the cache; the read/insert in step 3 is the interior-mutability path.) This is conservative (clears everything) but correct; finer-grained invalidation is an optimization.
5. Add a unit test `test_markdown_cache_hits_on_same_text_and_width` that calls `render_markdown_cached` twice with identical text and width, asserts the second call returns the cached result without re-parsing (check by inspecting `app.markdown_cache.borrow().len()` — it stays 1 after the second call).
6. Verify the test passes and that the gate commands remain green.

- **Depends on:** —
- **Done when:** Calling `render_markdown_cached` twice with identical text and width returns the cached result (second call avoids parsing; `markdown_cache.borrow().len()` does not grow). `render`'s signature stays `&App` (the helper takes `&App` and writes via `RefCell::borrow_mut`). The cache is invalidated (cleared) when the active tab changes or content changes. The win is scoped to static accordion/task bodies and complete responses; mid-stream responses remain uncached (no false claim of eliminating the streaming re-parse). Test `test_markdown_cache_hits_on_same_text_and_width` passes. All existing markdown and rendering tests remain green. cargo test/clippy/fmt green.

---

## 0002 — Key-Feedback-Sidebar-And-Provider-Editor

### key-feedback-status — Status Message Feedback on Overloaded Keys

`s`/`a`/`t`/`z` toggle accordion sections only when a plan tab is active and the main pane is focused (`crates/makina/src/event.rs:1369–1396`). Outside a plan tab, they emit `Tick` (a silent no-op) with no feedback. The accordion footer help (`ui.rs:2035`) only appears *inside* a plan tab, so the keys are undiscoverable elsewhere. A user pressing `a` on a non-plan tab gets no message and may assume the app is unresponsive. The user experience is worse than desktop apps where a button action fails with "this control is not available here."

**Steps:**

1. In `crates/makina/src/app.rs`, add a new `AppEvent` variant if it does not already exist: `StatusMessage(String)`. This event is already used elsewhere in the code to emit transient messages; confirm it exists by grepping for `AppEvent::StatusMessage`. If it does not exist, add it.
2. In `crates/makina/src/event.rs:1376–1396`, replace the `AppEvent::Tick` fallbacks for `a`, `t`, and `z` with `AppEvent::StatusMessage("Accordion toggle not available here — open a plan tab.".to_string())`. (Keep `s` returning `AppEvent::StartRun` since that is the primary binding.)
3. Verify the `StatusMessage` handler in `App::update` (should already exist) queues the message for display in the status bar.
4. Add a unit test `test_accordion_key_outside_plan_tab_emits_status` that: (1) constructs an app with no plan tab active, (2) dispatches a key press for `a`, (3) asserts the result is `AppEvent::StatusMessage` with the expected text.
5. Verify the test passes and that the gate commands remain green.

- **Depends on:** markdown-cache
- **Done when:** Pressing `a`/`t`/`z` outside a plan tab emits a `StatusMessage` event that displays "Accordion toggle not available here" in the status bar. (The message is **not** on a timer — `status_message` is cleared opportunistically on the next significant event, e.g. the `RunLoaded` handler at `app.rs:2314–2319` clears it unless it contains "fail"/"error"; do not describe it as "2–3 seconds".) Keep the status string short — the status bar's trailer is clipped on narrow terminals (`ui.rs:795–798`). The test `test_accordion_key_outside_plan_tab_emits_status` passes. All existing event tests remain green. cargo test/clippy/fmt green.

---

### sidebar-resize — Resizable Sidebar with Minimum-Width Guard

The body layout is hardcoded 30%/70% (`crates/makina/src/ui.rs:90`). On a 40-column terminal the sidebar is 12 cols — too narrow for run labels like `makina/0034-test-plan`. There is no minimum-size guard; a 20×5 terminal renders overlapping/collapsed panes with no graceful degradation. Users expect to resize the sidebar (like a desktop app) and get a warning if the terminal is too small.

**Steps:**

1. In `crates/makina/src/app.rs`, add a new field to `App`: `pub sidebar_width_percent: u16`. Initialize it to 30 in `App::new`. This holds the user's current sidebar width preference as a percentage.
2. In `crates/makina/src/event.rs`, add two new SHIFT-guarded key arms in the `translate_key` function (`event.rs:1246`; the relevant arms are at `~event.rs:1412–1430`). The `translate_key` parameter is named **`key`** (`fn translate_key(key: KeyEvent, ...)` at `event.rs:1247`), so use **`key.modifiers`** (not `event.modifiers`): `KeyCode::Left if key.modifiers.contains(KeyModifiers::SHIFT)` → `AppEvent::ResizeSidebarLeft`, and `KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT)` → `AppEvent::ResizeSidebarRight`.

   **Match-arm order matters (clippy `-D warnings` rejects unreachable arms).** The Normal keymap already has ALT-guarded `KeyCode::Left`/`KeyCode::Right` arms at `event.rs:1412–1413` and **bare** (no-modifier-guard) `KeyCode::Right` (`event.rs:1422`) and `KeyCode::Left` (`event.rs:1427`) arms. A SHIFT-guarded `KeyCode::Left`/`KeyCode::Right` arm placed **after** the bare arms is unreachable and clippy will reject it. Place the two new SHIFT-guarded arms **immediately after the ALT-guarded arms at `event.rs:1412–1413` and before the bare `Up`/`Down`/`Right`/`Left` arms**, so guard precedence reads ALT → SHIFT → bare.
3. In `crates/makina/src/app.rs`, add two new `AppEvent` variants: `ResizeSidebarLeft` and `ResizeSidebarRight`. In `App::update`, handle them: `AppEvent::ResizeSidebarLeft => { self.sidebar_width_percent = self.sidebar_width_percent.saturating_sub(2).max(10); true }, AppEvent::ResizeSidebarRight => { self.sidebar_width_percent = (self.sidebar_width_percent + 2).min(50); true }`. This keeps the width between 10% and 50%.
4. In `crates/makina/src/ui.rs:88–91`, replace the hardcoded split with: `let body = Layout::default().direction(Direction::Horizontal).constraints([Constraint::Percentage(app.sidebar_width_percent), Constraint::Percentage(100 - app.sidebar_width_percent)]).split(body_area);`. This applies the user's chosen width.
5. Add a small-terminal guard as the **very first statement** in `render` (`ui.rs:60`), **before any layout split or widget render**: define `const MIN_W: u16` / `const MIN_H: u16`, and `if area.width < MIN_W || area.height < MIN_H { render a centered "Terminal too small" message via centered_rect (ui.rs:3267) and return; }`. **Do not invent the threshold** — audit the smallest `TestBackend` sizes the existing tests feed to the **top-level `render`** (the smallest full-`render` test is `make_terminal(80, 10)`, e.g. `accordion_scrollbar_renders_when_tall` at `ui.rs:3672`, which draws `render(&app, f)` at `ui.rs:3717`) and set `MIN_W`/`MIN_H` strictly **below** those (e.g. `MIN_W = 40`, `MIN_H = 10` keeps 80×10 rendering normally since `10 < 10` is false) — **or** update any affected test that would now hit the fallback. Note: the `render_plan_accordion_pane_*` "very small terminal" tests near `ui.rs:8068`/`8133` call `render_plan_accordion_pane` **directly** (not the top-level `render`), so they are unaffected by this guard; do not touch them.
6. Add unit tests: `test_sidebar_resize_left_clamps_to_min` (dispatch `ResizeSidebarLeft` 10 times, assert width is clamped to 10%). `test_sidebar_resize_right_clamps_to_max` (dispatch `ResizeSidebarRight` 10 times, assert width is clamped to 50%). `test_small_terminal_renders_fallback_message` (call the top-level `render` into an area below `MIN_W`×`MIN_H`, e.g. 20×5, and assert the "terminal too small" message is drawn and no normal sidebar/layout is). Also re-run the pre-existing small-terminal tests (the 80×10 full-`render` test above and the `render_plan_accordion_pane_*` direct tests) and confirm they still pass.
7. Verify the tests pass and that the gate commands remain green.

- **Depends on:** markdown-cache, key-feedback-status
- **Done when:** Pressing Shift+Left/Right decrements/increments sidebar width by 2% (clamped to 10–50%). The sidebar width persists for the running App session (in-memory; not saved to config) and is applied on each render. When `area.width < MIN_W || area.height < MIN_H`, the **first** statement in `render` short-circuits to a centered "Terminal too small" message via `centered_rect` and returns before any layout split. Tests `test_sidebar_resize_left_clamps_to_min`, `test_sidebar_resize_right_clamps_to_max`, `test_small_terminal_renders_fallback_message` pass, **and** the pre-existing small-terminal render tests (the 80×10 full-`render` test and the `render_plan_accordion_pane_*` direct tests) still pass. All existing layout and render tests remain green. cargo test/clippy/fmt green.

---

## 0003 — Provider-Editor-Rename-Or-Implement

### provider-editor-rename — Rename Provider Editor to "View Providers & Roles" (Read-Only)

The provider editor modal is titled "Configure Providers & Roles" but it is read-only: `ProviderEditorUp/Down` only navigate `selection_index`, there is no key to edit a provider or role, and the only action is Enter (commit, writing unchanged config). It overpromises functionality. The honest choice is to rename it to "View Providers & Roles" and clarify that edits are made via the config file. An edit-path implementation (`ProviderEditorEdit` mode, field-level mutations, commit on Enter) is deferred to a separate plan.

**Steps:**

1. In `crates/makina/src/ui.rs:2689`, change the title from `" Configure Providers & Roles "` to `" View Providers & Roles "`.
2. In the footer of `render_provider_editor` (after the current footer content, around line 2800+), add a help line: `"(Read-only; edits via .makina/config.toml)"`. This clarifies the limitation inline.
3. Add a comment in `crates/makina/src/app.rs` above the `ProviderEditor` struct definition (line 561) explaining that this is the read-only view and that implementing an edit path is deferred: `/// A read-only view of the current providers and role assignments.\n/// Edit paths (add/remove provider, reassign roles, change model/effort) are deferred to a future plan.`
4. Verify the renamed modal renders correctly and that the help text is visible.
5. Add a unit test `test_provider_editor_title_is_view_not_configure` that opens the provider editor modal, renders a frame, and asserts the title contains "View" (not "Configure").

- **Depends on:** markdown-cache, key-feedback-status, sidebar-resize
- **Done when:** The provider editor modal is titled "View Providers & Roles" (not "Configure"). A footer line clarifies "(Read-only; edits via .makina/config.toml)". The test `test_provider_editor_title_is_view_not_configure` passes. All existing provider-editor tests remain green. cargo test/clippy/fmt green.

---

**End of plan 0039 TASKS.** When every "Done when" bullet is green, five
short-term UX completeness fixes land: theme robustness (graceful fallback on
missing role instead of a render-time panic); markdown caching (parsed lines
keyed by `(text_hash, width)` in a `RefCell` cache, eliminating O(n) re-parses
of static accordion bodies and complete responses); a
resizable sidebar with a small-terminal guard; transient status-message feedback
when overloaded accordion keys are pressed outside a plan tab; and an honest
provider-editor title that reflects its read-only nature — all with the gate
commands green.
