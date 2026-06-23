# Architecture — Plan 0032 (deltas)

> The concrete deltas. This plan touches
> `crates/makina-core/src/orchestrator.rs`, `crates/makina/src/app.rs`,
> `crates/makina/src/ui.rs`, `crates/makina/src/event.rs`, and the integration
> suite under `crates/makina/tests/`.
> Line numbers are hints; locate by symbol.

## 0001 — Tab-Based Plan Rendering

Today the plan detail pane is driven by `app.plan_detail: Option<usize>`
(`crates/makina/src/app.rs:1012`), a singleton pointer into `discovered_plans`.
`render_plan_detail` (`crates/makina/src/ui.rs:1293`) renders one plan at a time
into the main content area when `plan_detail.is_some()`, so viewing a second plan
means losing the first.

**Edits:**

**Define plan-tab identity.** The existing `TabContent` enum
(`crates/makina/src/app.rs:876–881`, from plan 0031) already carries a
`Plan { plan_slug: String }` variant. A plan tab is identified by its slug — the
discovery key — so we reuse it as-is. `TabState::open_tab`
(`crates/makina/src/app.rs:901`) already dedups by `TabContent` equality, so
opening a plan that is already open *focuses* its existing tab (sets `active_tab`)
rather than creating a duplicate — the requested "open if closed, focus if open"
behavior, with closing reserved for `Ctrl+W`:

```rust
/// Content displayed in a tab in the main pane (plan 0031).
pub enum TabContent {
    Task { plan_slug: String, task_id: TaskId },
    /// A discovered plan, keyed by plan slug — reused by 0032 for plan tabs.
    Plan { plan_slug: String },
}
```

**Route plan node selection to tab open.** In `crates/makina/src/app.rs`, where
`focused_node()` returns `TreeNode::Plan { plan_idx }` and Enter is pressed,
dispatch an `OpenTab` instead of setting `plan_detail`. The existing
`handle-tab-events-in-update` handler from 0031 already processes `OpenTab`, so no
new event plumbing is added:

```rust
TreeNode::Plan { plan_idx } => {
    // Reuse 0031's tab infrastructure: open (or switch to) the plan tab.
    let slug = self.discovered_plans[plan_idx].slug.clone();
    return Some(AppEvent::OpenTab(TabContent::Plan { plan_slug: slug }));
}
```

**Remove the `plan_detail` singleton.** Delete the
`plan_detail: Option<usize>` field from `App` (`crates/makina/src/app.rs:1012`)
and every site that sets or clears it (around lines 1267, 1276, 1284, 1417). The
main content pane now consults `app.tabs.active_tab` to know which tab, if any, is
showing a plan.

**Render the active plan tab in the main pane.** In `crates/makina/src/ui.rs`,
where the content pane decides what to draw (around lines 240–392), replace the
`if app.plan_detail.is_some() { render_plan_detail(...) }` check with an
active-tab lookup that resolves the slug back to a `discovered_plans` entry:

```rust
if let Some(active_idx) = app.tabs.active_tab {
    if let Some(TabContent::Plan { plan_slug }) = app.tabs.open_tabs.get(active_idx) {
        // Resolve slug -> entry, then render the accordion pane (WS 0002).
        if let Some(plan) = app.discovered_plans.iter().find(|p| p.slug == *plan_slug) {
            render_plan_accordion_pane(app, plan, frame, content_area);
            return;
        }
    }
}
// Normal run/task rendering follows, unchanged.
```

**Properties that make this safe:**

- Tab identity is already defined in plan 0031; 0032 reuses it for plans instead
  of ignoring plan tabs — no new tab mechanism is introduced.
- The event routing is a straightforward substitution: a `plan_detail`
  assignment becomes an `OpenTab` dispatch into the existing handler.
- The render path keeps its structure; only the source of the plan changes, from
  a singleton index to an active-tab slug lookup.
- Backward compat: opening one plan at a time degenerates to a single-tab pane,
  so there is no new UI to learn for the common case.

## 0002 — Accordion-Section Layout for Plan Metadata

Today `PlanEntry` (`crates/makina-core/src/orchestrator.rs:212–224`) holds only
`slug`, `dir`, `has_tasks`, and `tasks` (the parsed task preview). The plan
detail pane (`render_plan_detail`, `crates/makina/src/ui.rs:1293–1377`) renders a
title, the directory, and the task list — there is no SCOPE, ARCHITECTURE, or
STATUS content, and no way to focus on one section while hiding the rest.

**Edits:**

**Extend `PlanEntry` to cache spec content.** Add three optional fields to the
struct at `crates/makina-core/src/orchestrator.rs:212`, populated at discovery
time so the render thread never does IO:

```rust
/// SCOPE.md content (cached at discovery time; None if unreadable or absent).
pub scope_text: Option<String>,
/// ARCHITECTURE.md content (cached at discovery time; None if unreadable or absent).
pub architecture_text: Option<String>,
/// STATUS.md content (cached at discovery time; None if unreadable or absent).
pub status_text: Option<String>,
```

In `discover_plans` (around line 354), read the three files non-blocking before
constructing the entry, converting any IO error to `None`:

```rust
// Best-effort reads: a missing or unreadable spec file degrades to None.
let scope_text = tokio::fs::read_to_string(dir.join("SCOPE.md")).await.ok();
let architecture_text = tokio::fs::read_to_string(dir.join("ARCHITECTURE.md")).await.ok();
let status_text = tokio::fs::read_to_string(dir.join("STATUS.md")).await.ok();
```

**Define the accordion-section enum and per-tab state.** In
`crates/makina/src/app.rs` (before `App`, around line 940), add the section
identifier and the expand-state map. The set holds *expanded* sections; absence
means collapsed, so all four default to collapsed:

```rust
/// Accordion section identifier for plan tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccordionSection {
    Scope,
    Architecture,
    Tasks,
    Status,
}

/// Accordion expand/collapse state for plan tabs.
/// Keyed by plan slug; the set contains the sections that are expanded.
/// Sections not in the set are collapsed.
pub accordion_state: HashMap<String, HashSet<AccordionSection>>,
```

`App::new()` (around line 1390) initializes the field to `HashMap::new()`;
`HashMap` / `HashSet` are already imported for `collapsed_runs` / `collapsed_plans`.

**Add the toggle event.** Extend `AppEvent` (`crates/makina/src/app.rs:595`) with
a single section-toggle variant, handled in `App::update()` against the active
plan tab:

```rust
/// Toggle the accordion section for the active plan tab.
/// A no-op when the active tab is not a plan tab.
ToggleAccordionSection(AccordionSection),
```

The handler resolves the active tab's slug, then flips the section in that slug's
set — insert if absent, remove if present:

```rust
AppEvent::ToggleAccordionSection(section) => {
    if let Some(active_idx) = self.tabs.active_tab {
        if let Some(TabContent::Plan { plan_slug }) = self.tabs.open_tabs.get(active_idx) {
            let slug = plan_slug.clone();
            let sections = self.accordion_state.entry(slug).or_default();
            // Toggle: collapse if expanded, expand if collapsed.
            if !sections.remove(&section) {
                sections.insert(section);
            }
        }
    }
}
```

**Render the accordion sections.** Add `render_plan_accordion_pane` to
`crates/makina/src/ui.rs` (after `render_plan_detail`, around line 1378). It reads
the plan's expand set, then emits each section as a one-line header plus, when
expanded, its indented content; a small `render_accordion_section` helper owns the
`[+]` / `[-]` marker and content lines:

```rust
/// Render a single accordion section header (with expand/collapse marker)
/// followed by its content lines when the section is expanded.
fn render_accordion_section(
    title: &str,
    section: AccordionSection,
    expanded: &HashSet<AccordionSection>,
    content: &str,
) -> Vec<Line> {
    let is_open = expanded.contains(&section);
    let marker = if is_open { "[-]" } else { "[+]" }; // editor disclosure vocabulary
    // header line: yellow marker + cyan title; content (indented) only when open.
    // Missing content arrives as "(no SCOPE.md)" from the caller — never an error.
    /* push header; if is_open, push wrapped, indented content.lines() */
}
```

The pane scrolls and clamps so the header row and footer stay visible even when a
section is taller than the area; missing content renders the dim
`"(no SCOPE.md)"` / `"(no ARCHITECTURE.md)"` / `"(no STATUS.md)"` placeholder the
caller passes in.

**Wire the keybindings.** In `crates/makina/src/event.rs`, map the toggle keys —
but only when the main pane is focused and the active tab is a plan tab — to the
new event:

```rust
// Accordion toggles apply only to a focused plan tab; other modes pass through.
if app.focused_panel == Panel::Main
    && matches!(active_tab_content(app), Some(TabContent::Plan { .. }))
{
    match key.code {
        KeyCode::Char('s') | KeyCode::Char('S') =>
            return Some(AppEvent::ToggleAccordionSection(AccordionSection::Scope)),
        KeyCode::Char('a') | KeyCode::Char('A') =>
            return Some(AppEvent::ToggleAccordionSection(AccordionSection::Architecture)),
        KeyCode::Char('t') | KeyCode::Char('T') =>
            return Some(AppEvent::ToggleAccordionSection(AccordionSection::Tasks)),
        KeyCode::Char('z') | KeyCode::Char('Z') =>
            return Some(AppEvent::ToggleAccordionSection(AccordionSection::Status)),
        _ => {}
    }
}
```

**Properties that make this safe:**

- Reading SCOPE / ARCHITECTURE / STATUS happens at discovery time, off the render
  path; any failed read degrades to `None`, and the section renders a placeholder
  rather than erroring.
- Accordion state is ephemeral (in-memory only); it resets when a plan tab closes
  and reopens, so there is no persistence format to maintain.
- The render logic is additive: a section absent from the expand set collapses to
  a one-line header, so an unused feature shows nothing new — no regression on the
  old single-plan view.
- The toggle event is a no-op unless the active tab is a plan tab, so the keys
  stay inert in run/task contexts.

## 0003 — Tab Navigation and Keybindings

Today the plan detail pane renders a static footer
(`crates/makina/src/ui.rs:1365–1368`):
`"[Enter] close   [→] expand in tree   [o] open a task-list to run"`. With tabs
and accordion sections, more bindings apply and the footer must advertise them.

**Edits:**

**Map tab-navigation keys.** In `crates/makina/src/event.rs`, bind tab cycling
and close to the `AppEvent` variants plan 0031 already handles — this plan only
wires the keys, it adds no new event handling:

```rust
match (key.modifiers, key.code) {
    // Next / previous tab — standard editor vocabulary.
    (KeyModifiers::ALT, KeyCode::Right) => return Some(AppEvent::NextTab),
    (KeyModifiers::ALT, KeyCode::Left)  => return Some(AppEvent::PrevTab),
    // Close the active tab.
    (KeyModifiers::CONTROL, KeyCode::Char('w')) => return Some(AppEvent::CloseTab),
    _ => {}
}
```

**Update the footer help text.** `render_plan_accordion_pane` (WS 0002) emits a
dynamic footer that lists the accordion and tab bindings; it appears only when a
plan tab is active:

```rust
lines.push(Line::from(Span::styled(
    "  [s] scope  [a] arch  [t] tasks  [z] status  [◄] [►] tabs  [Ctrl+W] close",
    Style::default().fg(Color::DarkGray),
)));
```

**Resolve conflicts.** Audit existing bindings in `crates/makina/src/event.rs` to
confirm `Alt+Left` / `Alt+Right` and `Ctrl+W` are unbound. If any collide, fall
back to `Page Up` / `Page Down` for cycling (or `q` for close) and record the
substitution in SCOPE's decision section.

**Properties that make this safe:**

- All bindings target `AppEvent` variants that already exist from plan 0031, so
  no new event handling is introduced — only key mapping.
- The footer renders solely from the plan accordion pane, so it never appears in
  run/task modes.
- The chosen keys (`Alt+Left/Right` = cycle, `Ctrl+W` = close) match modern
  editors, minimizing what a user must learn.

## 0004 — Integration and Polish

Today there is no multi-tab plan viewing, so there is no coverage for tab state
surviving re-discovery or accordion state persisting per-tab, and nothing clamps
plan tabs when `discovered_plans` changes underneath them.

**Edits:**

**Clamp plan tabs on re-discovery.** In `crates/makina/src/app.rs`, the
`AppEvent::PlansDiscovered` handler updates `self.discovered_plans` and may drop
or reorder entries. Add a `close_tabs_for_missing_plans` pass that closes any plan
tab whose slug no longer exists, removing in reverse index order so earlier
indices stay valid (the active-tab clamp is `TabState::close_tab`'s existing
responsibility from 0031):

```rust
/// Close plan tabs whose slug is no longer in `discovered_plans`.
fn close_tabs_for_missing_plans(&mut self) {
    let valid: HashSet<_> = self.discovered_plans.iter().map(|p| p.slug.clone()).collect();
    let stale: Vec<usize> = self.tabs.open_tabs.iter().enumerate()
        .filter_map(|(i, t)| match t {
            TabContent::Plan { plan_slug } if !valid.contains(plan_slug) => Some(i),
            _ => None,
        })
        .collect();
    // Reverse order so closing one tab does not shift the next index to close.
    for i in stale.into_iter().rev() {
        self.tabs.close_tab(i);
    }
}
```

**Cover accordion state per-tab.** Add a test in `crates/makina/tests/ui_tests.rs`
that opens two plan tabs, expands different sections in each, switches between
them, and asserts each slug's expand set is preserved independently — proving the
slug-keyed `accordion_state` survives tab switches.

**Cover rendering with long content.** Add a render test in the same suite that
builds a `PlanEntry` with all four sections populated (a long SCOPE), expands them
all, draws the frame, and asserts the screen shows `[-] SCOPE`, `[-] TASKS`, a
`GATED` marker, and that drawing does not panic — exercising wrap and scroll
clamping.

**Document the bindings.** The dynamic footer from WS 0003 is the in-app
reference; no separate help pane is added.

**Properties that make this safe:**

- Tab-state clamping mirrors the run-tab behavior from plan 0031, so the pattern
  is established rather than invented here.
- Accordion rendering is best-effort: a section with no content shows a
  placeholder, never a panic.
- The integration tests assert both that the render path does not panic and that
  per-tab state survives discovery cycles, locking in the invariants above.

## Test strategy

- **0001 (tabs).** A unit test navigates the `tree_cursor` to a plan node,
  simulates Enter, and asserts an `OpenTab(TabContent::Plan { .. })` is dispatched
  and the plan becomes the active tab; the old `plan_detail` field is gone.
- **0002 (accordion).** A unit test toggles a section and asserts the slug's
  expand set inserts when absent and removes when present, and that the toggle is
  a no-op when the active tab is a task tab. A render test expands all four
  sections and asserts the drawn pane shows each header's `[-]` marker with
  content (and `(no SCOPE.md)` for a missing file). An `orchestrator` unit test
  asserts a plan dir with all three spec files readable populates `scope_text` /
  `architecture_text` / `status_text`, and a dir with missing files yields `None`.
- **0003 (keybindings).** An integration test opens a plan tab, presses `s`, and
  asserts `accordion_state` reflects the toggle; pressing the same key with a task
  tab active (or the sidebar focused) has no effect.
- **0004 (integration).** `accordion_sections_persist_per_tab` opens two plan
  tabs, expands different sections, switches tabs, and asserts each tab's state is
  preserved. `plan_tabs_render_accordion_sections_without_panic` renders a fully
  expanded plan into a wide terminal and asserts the section headers, task ids,
  and `GATED` marker appear without panic. A re-discovery test opens a plan tab,
  drops that plan from `discovered_plans`, and asserts the tab is closed and the
  active index stays valid.
- All tests keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.

## Interaction with prior work

- **0027 / discovery.** Reuses `App::discovered_plans` and the startup
  `discover_plans` pass as the plan-content source; 0002 only extends `PlanEntry`
  with cached spec text at the same discovery point — no new discovery mechanism.
- **0031 / tabs + unified tree.** Builds directly on 0031's tab infrastructure:
  the `TabContent::Plan` variant, `TabState`, and the `OpenTab` / `CloseTab` /
  `NextTab` / `PrevTab` events are all reused. This plan adds accordion sections
  and SCOPE/ARCHITECTURE/STATUS rendering on top; it introduces no separate
  plan-tab system, and tab-index clamping on re-discovery mirrors 0031's run-tab
  clamp pattern. Retiring `plan_detail` completes 0031's tree-to-tab routing for
  plan nodes.
