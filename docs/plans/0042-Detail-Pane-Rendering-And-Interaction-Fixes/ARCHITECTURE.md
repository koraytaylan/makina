# Architecture — Plan 0042 (deltas)

> The concrete deltas. This plan touches `crates/makina/src/ui.rs`,
> `crates/makina/src/markup.rs`, `crates/makina/src/event.rs`,
> `crates/makina/src/app.rs`, `crates/makina/src/theme.rs`, a new
> `crates/makina/src/syntax.rs`, and `crates/makina/Cargo.toml`.
> Line numbers are hints; locate by symbol.

## 0001 — Tab-Styling-And-Visibility

Today `render_tab_bar()` in `crates/makina/src/ui.rs:946–1009` renders each tab
chip using a conditional style (lines 988–998): active tabs use `Accent`
background with `Background` foreground and BOLD, while unfocused tabs use `Dim`
background with `Foreground` foreground. The semantic mismatch — `Dim` is defined
as a secondary *text* color, not a surface background — causes poor contrast in
light themes where `Dim` (#828E9F) is too close in value to the text color
(#5C6166).

**Edits:**

**Change unfocused tab background to Border (a true secondary surface).** Replace
line 996 (`.bg(app.active_theme.get(crate::theme::ThemeRole::Dim))`) with
`Border`, a defined UI-line color that sits between Background and Foreground. The
foreground stays `Foreground` for legibility.

```rust
// Unfocused tab: distinct from focused via a secondary surface + normal weight
Style::default()
    .bg(app.active_theme.get(crate::theme::ThemeRole::Border))
    .fg(app.active_theme.get(crate::theme::ThemeRole::Foreground))
```

**Properties that make this safe:**

- The change is style-only: one constant in one conditional; no logic or state.
- Active tabs retain their inverted style (Accent bg + Background fg + BOLD), so
  the focused tab remains visually dominant.
- All themes define both `Border` and `Foreground`, so the new style compiles and
  renders across all three (ayu_dark, ayu_mirage, ayu_light).

## 0002 — Code-Block-Rendering-With-Syntax

Three independent defects in the code-block path of `render_markdown()`
(`crates/makina/src/markup.rs:127–182`):

1. **Blank line after every source line.** The `in_code_block` branch of
   `Event::Text` (lines 150–159) does `for line in text_str.split('\n') { out.push(...) }`.
   pulldown-cmark emits one `Event::Text` per source line *including* the trailing
   `'\n'`, so `split('\n')` returns a trailing empty element that becomes a blank
   `Line` after every code line — exactly the user's "unnecessary blank lines
   between every line."
2. **No token coloring.** Every character uses the single `ThemeRole::CodeBlock`
   foreground (lines 153–156); there is no per-token highlighting.
3. **No visible band.** The background is `ThemeRole::Background` (line 155) — the
   same color as the surrounding pane — and only spans the glyph cells, so the
   block neither stands out nor fills the viewport width.

This workstream is split into three tasks (a self-contained island — nothing else
in the plan depends on it).

**Edit A — add a `CodeBlockBg` theme role (`crates/makina/src/theme.rs`).** Add a
`CodeBlockBg` variant to `ThemeRole` (after `CodeBlock`, ~line 17), include it in
the role list/exhaustiveness array (~line 22–32), and give it a distinct surface
color per theme — a hair off `Background`: ayu_dark `#161B24`, ayu_mirage
`#272D38`, ayu_light `#EEF1F4`. Mirror any per-role value test (~line 212–238).

**Edit B — add a cached syntect highlighter (`crates/makina/src/syntax.rs`,
`Cargo.toml`).** Add `two-face` via `cargo add two-face` (it bundles syntect's
syntax + theme assets and re-exports `syntect`). Cache the `SyntaxSet`/`ThemeSet`
in `OnceLock`s so assets load once. Expose:

```rust
// crates/makina/src/syntax.rs
use std::sync::OnceLock;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

pub fn highlight_code_line(
    line: &str,
    lang: Option<&str>,
    theme: &crate::theme::Theme,
) -> Vec<Span<'static>> {
    // 1. resolve syntax: SyntaxSet::find_syntax_by_token(lang) else plain text
    // 2. pick a bundled syntect theme by light/dark (luminance of Background)
    // 3. HighlightLines::highlight_line -> map syntect fg -> Color::Rgb spans
    //    (NO background here; the render task paints the band)
    // 4. on error/unknown language: one monochrome Span fg = ThemeRole::CodeBlock
    todo!()
}
```

**Edit C — fix splitting, wire highlighting, paint a full-width band
(`crates/makina/src/markup.rs`).** Capture the fence language, emit exactly one
`Line` per source line, color via `highlight_code_line`, and set a full-width
`CodeBlockBg` band:

```rust
Event::Start(Tag::CodeBlock(kind)) => {
    if !spans.is_empty() { out.push(finalize_line(std::mem::take(&mut spans))); }
    in_code_block = true;
    code_lang = match kind {
        pulldown_cmark::CodeBlockKind::Fenced(info) if !info.is_empty() => Some(info.to_string()),
        _ => None,
    };
}
// inside Event::Text, in_code_block branch:
for line in trimmed_lines(&text_str) {           // skip the trailing empty element
    let mut spans = vec![Span::raw("  ")];        // keep the indent
    spans.extend(crate::syntax::highlight_code_line(line, code_lang.as_deref(), theme));
    out.push(
        Line::from(spans).style(Style::default().bg(theme.get(crate::theme::ThemeRole::CodeBlockBg))),
    );
}
// pad each pushed code line to `width` so the band reaches the right edge.
Event::End(TagEnd::CodeBlock) => { in_code_block = false; code_lang = None; style = base; }
```

**Properties that make this safe:**

- Highlighting is best-effort: unknown languages / highlight errors fall back to a
  single monochrome `CodeBlock`-colored span; rendering never panics.
- `OnceLock` caching keeps the expensive syntax/theme load off the render path
  after the first code block.
- One-`Line`-per-source-line plus full-width band are rendering properties only;
  no layout/state change beyond the new field and theme role.

## 0003 — Scrollbar-Position-Accuracy

Today `crates/makina/src/ui.rs:2148` (and the sibling at line 1131) use
`ScrollbarState::new(total_rendered_rows as usize).position(scroll_offset)`,
treating `total_rendered_rows` as the content height. But `ScrollbarState::new()`
expects the *maximum* scroll value (`content_height - viewport_height`), not the
total content height. When `scroll_offset == scroll_max` the thumb should sit at
the bottom; instead it floats, because the widget interprets the position against
the wrong denominator.

For example, 100 content rows in a 20-row viewport gives `scroll_max = 80`. At
offset 80 the thumb should reach the bottom, but `ScrollbarState::new(100)` reads
80 as 80% of 100 and places the thumb mid-track.

**Edits:**

**Use `scroll_max` (not `total_rendered_rows`) as the content size.** Replace
`ScrollbarState::new(total_rendered_rows as usize)` with
`ScrollbarState::new(scroll_max as usize)` at line 2148 and the sibling site
(line 1131):

```rust
// Old (incorrect):
let mut scrollbar_state = ScrollbarState::new(total_rendered_rows as usize)
    .position(scroll_offset as usize);
// New (correct):
let mut scrollbar_state = ScrollbarState::new(scroll_max as usize)
    .position(scroll_offset as usize);
```

`scroll_max = total_rendered_rows.saturating_sub(content_area.height)` (e.g. line
2147) is already correct; add it at any site that lacks it. The `scroll_max == 0`
guard (content ≤ viewport) keeps the thumb empty/hidden.

**Properties that make this safe:**

- Aligns with the ratatui `ScrollbarState::new()` contract (argument = max scroll).
- The position is still clamped within `[0, scroll_max]`, so no out-of-bounds.
- Logic-free: only the state initialization changes.

## 0004 — Cycle-Views-Key-Binding

The 'v' binding is NOT dead. `crates/makina/src/event.rs:1311` maps `'v'`/`'V'`
to `AppEvent::CycleDependencyView`; the handler at
`crates/makina/src/app.rs:2128–2135` cycles `self.dependency_view`
`Off → List → Tree → Timeline → Off` (proven by `app.rs:4128–4136`); and the
status bar shows `view: …` (`crates/makina/src/ui.rs:821–826`). The reason users
see nothing: the dependency sub-pane is only carved and drawn inside the
run/exchange render branch (`crates/makina/src/ui.rs:711–720`), and
`render_dependency_view` (`ui.rs:1148–1205`) only shows content for a focused task
WITH dependencies (else "No task focused." / "No dependencies."). From a plan tab,
a task tab, or with no task focused, 'v' has no obvious visible effect — so the fix
is feedback + documentation, not a no-op help line.

**Edits:**

**Emit immediate status feedback on cycle.** In the `AppEvent::CycleDependencyView`
handler (`app.rs:2128`), after updating the mode, set a status message:

```rust
AppEvent::CycleDependencyView => {
    self.dependency_view = self.dependency_view.next();   // existing cycle
    let label = match self.dependency_view { /* off|list|tree|timeline */ };
    self.set_status(format!("Dependency view: {label}")); // use the existing status mechanism
}
```

**Make the empty state self-explanatory** in `render_dependency_view`
(`ui.rs:1148–1205`): replace "No task focused." with "Select a task to see its
dependencies — v cycles the view".

**Document 'v' in the help overlay** (`render_help_overlay`, `ui.rs:3334`), near
the other view/pane toggles (e.g. the 'e' error pane):

```rust
Line::from(vec![
    Span::raw("v  "),
    Span::styled(
        "Cycle dependency view (off → list → tree → timeline)",
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
    ),
])
```

**Properties that make this safe:**

- Binding + cycle are already tested (`event.rs:1743`, `app.rs:4128`); we add
  feedback, not new dispatch logic.
- A status message is observable from every context, so 'v' always "does
  something," directly answering the user report.
- No new events or API calls.

## 0005 — Plan-Execution-Controls

Two concrete defects make start "completely broken" for an opened plan:

1. **`s` is shadowed on a plan tab.** `crates/makina/src/event.rs:1332–1338`
   binds `'s'` to `ToggleAccordionSection(Scope)` when
   `focused_panel == Panel::Main && plan_tab_active`, with `StartRun` only in the
   `else` (sidebar) branch — so on a focused plan tab there is NO key that starts
   the run. WS6 will also overload `'s'` for task tabs.
2. **A plan tab never selects its run.** `sync_selected_run_to_active_tab`
   (`crates/makina/src/app.rs:1483–1502`) early-returns unless the active tab is a
   `TabContent::Task`, so for a plan tab `selected_run` stays unset and
   `run_control` (`event.rs:947–964`) hits `app.selected_run() == None` → returns
   "No run selected".

The clean fix decouples run-control from the overloaded letter keys.

**Edit A — always-available Ctrl run-control aliases (`event.rs`).** Add, BEFORE
the accordion-overloaded plain-letter arms (mirroring the `Ctrl+O` guard at line
1318):

```rust
KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => AppEvent::StartRun,
KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => AppEvent::PauseRun,
KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => AppEvent::CancelRun,
```

These dispatch run control from any context (plan tab, task tab, sidebar).

**Edit B — select the run for a plan tab (`app.rs`).** Extend
`sync_selected_run_to_active_tab` to handle a `TabContent::Plan { plan_slug }`:

```rust
match self.tabs.open_tabs.get(active) {
    Some(TabContent::Task { plan_slug, task_id }) => { /* existing */ }
    Some(TabContent::Plan { plan_slug }) => {
        if let Some(idx) = self.runs.iter().position(|r|
            makina_core::orchestrator::plan_slug(&r.task_list_path) == *plan_slug)
        { self.selected_run = Some(idx); }
    }
    _ => {}
}
```

Call it after tab activation / focus changes in `App::update()` so `selected_run`
is fresh before a control key dispatches.

**Edit C — actionable no-run message (`event.rs:952`).** Replace "No run selected"
with "No run selected — open a plan or task tab, or pick a run in the sidebar". If
an opened plan has no run in `self.runs`, surface a clear message rather than
silently no-op'ing (confirm whether "start" maps to an existing `RunId` or must
create a run first).

**Properties that make this safe:**

- Ctrl aliases are additive and modifier-guarded; they do not change the existing
  plain-letter behavior, so they cannot regress the accordion toggles.
- The plan-tab sync only sets `selected_run` when a matching run exists; otherwise
  state is unchanged and the user gets an explanatory message.
- `run_control()` itself is unchanged and already tested.

## 0006 — Task-Detail-Accordion-Structure

Today `render_task_entry_pane()` at `crates/makina/src/ui.rs:1831–1913` renders
task metadata and the markdown body as a flat sequence. The plan accordion UI
(`render_accordion_section()` at line 2163) is a proven pattern: collapsible
sections, focused-section highlighting, keyboard navigation. A task tab adopts it
with two sections — "Scope" (static `entry_text`) and "Execution" (live activity:
exchanges + progress). Because WS5 moved run-control to `Ctrl+S`/`Ctrl+P`/`Ctrl+C`,
plain `'s'`/`'z'` are now free to toggle the task accordion.

**Edits:**

**Add task accordion state to `App` (`app.rs`).**

```rust
// In App struct (~line 1090):
pub task_accordion_expanded: HashMap<TaskId, HashSet<AccordionSection>>,
// In App::new():
task_accordion_expanded: HashMap::new(),
// Extend AccordionSection (app.rs:1027) with an Execution variant:
pub enum AccordionSection { Scope, Architecture, Tasks, Status, Execution }
```

**Refactor `render_task_entry_pane()` into two accordion sections.** Render
metadata, then Scope (task `entry_text` as markdown) and Execution (summarize the
task's `exchange_logs` / progress for this `(RunId, TaskId)`, with a "No execution
yet — start the run (Ctrl+S)" empty state), each via `render_accordion_section()`.

**Wire `s`/`z` to the task accordion (`event.rs`, `app.rs`).** Add
`AppEvent::ToggleTaskAccordionSection(AccordionSection)` (near
`ToggleAccordionSection`, `app.rs:909`) and route plain letters by tab kind:

```rust
KeyCode::Char('s') | KeyCode::Char('S') => {
    if focused_panel == Panel::Main && plan_tab_active {
        AppEvent::ToggleAccordionSection(AccordionSection::Scope)
    } else if focused_panel == Panel::Main && task_tab_active {
        AppEvent::ToggleTaskAccordionSection(AccordionSection::Scope)
    } else {
        AppEvent::StartRun // sidebar fallback; Ctrl+S is the primary start path
    }
}
// 'z' similarly toggles Execution for task tabs.
```

Handle `ToggleTaskAccordionSection` in `App::update()` by toggling the section in
`self.task_accordion_expanded` for the active task id.

**Properties that make this safe:**

- `render_accordion_section()` is already proven and tested for plan details;
  task details reuse it — no new expand/collapse, navigation, or scrolling logic.
- Run-control start no longer collides with the accordion toggle (Ctrl+S, WS5), so
  overloading plain `'s'`/`'z'` for the task accordion cannot re-break start.
- Accordion state is in-memory `App` state; no persistence concerns.
- The refactor is backward-compatible: a task tab renders both sections (expanded
  by default) so existing users see no regression.
