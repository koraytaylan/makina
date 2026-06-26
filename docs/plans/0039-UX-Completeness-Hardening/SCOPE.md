# Scope — Plan 0039

> Harden UX completeness with theme robustness, markdown caching, sidebar resizing, key feedback on silent no-ops, and provider-editor implementation.

## Why this plan

**1. Theme::get panics on missing role, crashing at render time.** `Theme::get` (`crates/makina/src/theme.rs:48–53`) calls `unwrap_or_else(|| panic!(...))` if a role is absent. Today, all built-in themes define every `ThemeRole`, guarded by `themes_define_every_role_and_ansi_entry` test (`theme.rs:245`). But the architecture invites custom themes (semantic roles, persisted name in config), and a half-built custom theme would crash the app at render time instead of degrading gracefully. Users cannot recover from a missing role in a config file. **Fix:** return `Color::Reset` (the terminal default — **not** another role like `Foreground`, which would recurse through `get(Foreground)` and could loop) and emit a `tracing::warn!`, so the app stays responsive and the user can fix the config.

**2. Markdown re-parsed every frame, causing O(n) re-parses on every tick.** `render_markdown` (`crates/makina/src/markup.rs:76–337`) is a full `pulldown_cmark` parse and render, called from 5 production sites in `ui.rs` (1047 in `render_plan_task_pane`, 1834 in `render_task_entry_pane`, 2152 in `render_accordion_section`, 2481/2535 in `render_exchange_pane`). For large plan-accordion bodies this is O(n) per 250ms tick. **Fix:** cache parsed lines keyed by `(text_hash, width)` in an interior-mutable `RefCell` cache on `App` (so the `&App` render pass stays immutable, mirroring `last_scroll_maxes` at `app.rs:1182`), invalidated on tab/content change. The cache helps the static accordion/task bodies and **complete** responses; a **streaming** response changes its text every frame (cursor appended at `ui.rs:2489`), so it is a cache miss every frame and its re-parse is not eliminated. Trades O(1) hash computation for O(n) parsing.

**3. Sidebar is fixed 30%/70% split with no user control and no min-size guard.** `ui.rs:90` splits the body with hardcoded `Constraint::Percentage(30)` / `Constraint::Percentage(70)`. On a 40-column terminal the sidebar is 12 cols — unusable for labels like `makina/0034-test-plan`. No minimum-size guard; a 20×5 terminal renders overlapping/collapsed panes with no graceful "terminal too small" message. **Fix:** make sidebar width user-adjustable (e.g., Shift+Left/Right) and add a min-size fallback screen, so small terminals degrade predictably.

**4. Overloaded accordion keys s/a/t/z are silent no-ops outside plan tabs.** `s`/`a`/`t`/`z` toggle accordion sections only when a plan tab is active and the main pane is focused (`crates/makina/src/event.rs:1369–1396`). Outside a plan tab, they emit `Tick` (a silent no-op) with no feedback to the user. The accordion footer help (`ui.rs:2035`) only appears *inside* a plan tab, so the keys are undiscoverable elsewhere. A user on a non-plan tab pressing `a` gets nothing and no message, assuming the app is unresponsive. **Fix:** emit a transient "not available here" status message on the no-op so users know the keys tried to act and understand the context dependency.

**5. Provider editor is read-only but titled "Configure" — overpromises.** The provider editor modal (`crates/makina/src/event.rs:1300–1309`, `crates/makina/src/app.rs:2422–2437`) is titled "Configure Providers & Roles" but `ProviderEditorUp`/`Down` only move a `selection_index`; there is no key to actually edit a provider or role assignment (no Enter-to-edit, no field editing). The only action is Enter = commit, which writes the unchanged config back. It is effectively a read-only viewer. **Fix:** either rename to "View Providers & Roles" (honest) or implement the edit path (add/remove provider, reassign role, change model/effort). The current state misleads users into expecting edit capability.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0001–0003):

- **0001 — Theme-Robustness-And-Markdown-Caching.** Add `Theme::get` fallback on missing role (return `Color::Reset`, log warning); cache markdown parsing keyed by `(text_hash, width)` in `App`, invalidate on text/width change.
- **0002 — Key-Feedback-Sidebar-And-Provider-Editor.** Emit transient "not available here" status message on overloaded accordion keys (s/a/t/z) outside plan tabs; make sidebar resizable (Shift+Left/Right) with minimum-width guard and graceful degradation on small terminals.
- **0003 — Provider-Editor-Rename-Or-Implement.** Rename the provider editor modal from "Configure Providers & Roles" to "View Providers & Roles" (read-only) OR implement the edit path (add/remove provider, reassign role, change model/effort). Document the choice and rationale in ARCHITECTURE.md.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Theme::get panics on missing role, crashing at render time instead of degrading gracefully for custom themes. | `0001` |
| Markdown re-parsed every frame (250ms tick + per exchange event); no caching, O(n) re-parses on large plans/streams. | `0001` |
| Sidebar is fixed 30%/70% with no user control and no min-size guard; small terminals render unusable/collapsed panes. | `0002` |
| Overloaded accordion keys s/a/t/z are silent no-ops outside plan tabs, leaving the app feeling unresponsive. | `0002` |
| Provider editor is read-only but titled "Configure", overpromising an edit path that does not exist. | `0003` |

## Locked decisions

- **Theme fallback returns Color::Reset (not Foreground).** When `Theme::get` encounters a missing role, it returns `Color::Reset` (a safe, neutral default that delegates to the terminal's current foreground) rather than `Foreground` (which itself requires a theme lookup and could fail for custom themes). `Color::Reset` is more robust. A warning is logged so the user knows a role was missing.
- **Markdown cache is transparent to the caller.** The cache is hidden behind a `render_markdown_cached` helper, so call sites do not need to change their logic — they call the same function signature and get the same output, but faster. Cache invalidation is conservative (clear on tab change or content update) to maintain correctness.
- **Sidebar width is adjusted by 2% per keystroke, not free-form input.** Shift+Left/Right decrement/increment by 2% (not 1% per frame or arbitrary percentage) to keep the adjustment granular but not too slow, and to avoid the complexity of an input mode. Users can reach any target width by pressing the key multiple times, bounded to 10–50%.
- **Small-terminal guard uses hard thresholds (40×10).** Terminals smaller than 40 columns × 10 lines render a centered "Terminal too small" message instead of the normal layout. This is a conservative threshold: below it, normal panes are too cramped to be useful. The threshold is hard-coded for simplicity; making it configurable is future work.
- **Provider editor is renamed (Option A), not reimplemented (Option B).** Plan 0039 addresses the misleading title by renaming to "View Providers & Roles", clarifying the read-only nature in the footer. Implementing the edit path (add/remove provider, reassign role, change model/effort) is deferred to a separate plan, as it requires new event logic, mutation handlers, and more extensive testing.

## Out of scope

- Provider editor edit path implementation. Implementing field-level editing (add/remove provider, reassign role, change model/effort) is a separate plan (000N). This plan only renames the modal to clarify current read-only behavior.
- Custom keybindings for sidebar resize. Sidebar resize uses hard-coded Shift+Left/Right. Customizable keybindings are deferred to a separate plan.
- Markdown cache granularity beyond `(text_hash, width)`. The cache key is `(text_hash, width)`. Finer-grained cache invalidation (e.g. per-line or per-section) is an optimization deferred to future work.
- Terminal resize detection and dynamic minimum thresholds. The small-terminal guard uses a fixed 40×10 threshold. Dynamic thresholds (e.g. based on pane content size) are future work.
- Status message persistence and history. Key-feedback status messages reuse the existing `status_message` slot, which is cleared opportunistically on the next significant event (e.g. `RunLoaded`, `app.rs:2314–2319`) rather than on a timer. Building a persistent message log is a separate feature.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
