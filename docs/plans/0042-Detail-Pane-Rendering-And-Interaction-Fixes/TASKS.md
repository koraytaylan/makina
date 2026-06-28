# XAgent Plan 0042 — Detail-Pane Rendering And Interaction Fixes

This plan executes six interconnected workstreams to resolve user-reported defects in the detail pane (ratatui-based TUI). WS1 fixes unfocused tab text visibility by using a true surface background distinct from the foreground text. WS2 implements proper code-block rendering: a dedicated `CodeBlockBg` theme role, a cached `syntect`/`two-face` syntax highlighter with language detection, full-width background bands, and elimination of the spurious blank line cmark emits after every source line. WS3 corrects scrollbar thumb position/size math to accurately track scroll offset at the end of content. WS4 makes the 'v' dependency-view cycle give immediate visible feedback from any context — the binding and state cycle already work and are tested; the effect was simply invisible outside the run/exchange pane — and documents it in the help overlay. WS5 restores start/pause/cancel by adding always-available `Ctrl+S`/`Ctrl+P`/`Ctrl+C` run-control aliases and syncing the selected run for plan tabs (today 's' on a focused plan tab only toggles the accordion, and a plan tab never selects its run). WS6 adopts an accordion-style layout for task detail tabs with collapsible Scope and Execution sections, mirroring the plan-accordion pattern; with run-control moved to Ctrl-aliases, plain `s`/`z` are free to toggle the task accordion. All work keeps `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check` passing.

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

## 0001 — Tab-Styling-And-Visibility

### fix-unfocused-tab-background-color — Fix Unfocused Tab Background to Border or Secondary Surface Color

The unfocused tab style at `crates/makina/src/ui.rs:994–997` applies `Dim` background (a text-color semantic) with `Foreground` text, creating poor contrast — in ayu_light the `Dim` color (#828E9F, grayish-blue) is too close in value to the foreground text (#5C6166, dark gray), so the label is barely legible. The active tab uses `Accent` background with `Background` foreground, which is clear and distinct — the unfocused tab must achieve similar legibility with a different visual weight.

**Steps:**

1. In `crates/makina/src/ui.rs`, locate the `render_tab_bar()` function (around line 946–1009).
2. Find the conditional style assignment (around line 988–998) that handles active vs. unfocused tab rendering.
3. Replace the unfocused background at line 996: `.bg(app.active_theme.get(crate::theme::ThemeRole::Dim))` with `.bg(app.active_theme.get(crate::theme::ThemeRole::Border))`.
4. Verify the full unfocused style now reads `Style::default().bg(Border).fg(Foreground)` (no `Dim` background) and stays visually distinct from the active tab (Accent bg + Background fg + BOLD).
5. Run `cargo test` and `cargo fmt --check` to ensure no regressions in existing tests and formatting consistency.

- **Depends on:** —
- **Done when:** The unfocused tab background is `Border` (or another secondary-surface color distinct from both `Background` and `Dim`); the text is still `Foreground` for legibility; the active tab remains visually dominant; cargo test, clippy, and fmt all pass green.

---

## 0002 — Code-Block-Rendering-With-Syntax

### add-codeblock-background-theme-role — Add a Dedicated CodeBlock Background Theme Role

Code blocks today set their background to `ThemeRole::Background` (`crates/makina/src/markup.rs:136, 144, 155`) — the SAME color as the surrounding pane — so there is no visible code-block band at all. To render a distinct, full-width band (later tasks) the theme needs a dedicated surface color. Add a `CodeBlockBg` role so every theme variant supplies a code-block band color slightly offset from `Background`.

**Steps:**

1. In `crates/makina/src/theme.rs`, add a `CodeBlockBg` variant to the `ThemeRole` enum (just after `CodeBlock`, around line 17).
2. Add `ThemeRole::CodeBlockBg` to the role list / exhaustiveness array (around line 22–32) so any round-trip or coverage test still passes.
3. Insert a color for `CodeBlockBg` in all three theme builders: ayu_dark (around line 71–81; e.g. a hair lighter than `#0D1017`, such as `#161B24`), ayu_mirage (around line 111–121; e.g. `#272D38`), and ayu_light (around line 151–161; e.g. `#EEF1F4`, a faint gray distinct from the `#F8F9FA` Background).
4. If a unit test asserts each role's value per theme (around line 212–238), add `CodeBlockBg` assertions mirroring the existing `CodeBlock` ones.
5. Run `cargo test` (theme tests) and `cargo fmt --check`.

- **Depends on:** —
- **Done when:** `ThemeRole::CodeBlockBg` exists and resolves to a distinct, non-`Background` surface color in all three themes; the theme coverage test passes; cargo test, clippy, and fmt all pass green.

---

### add-syntect-code-highlighter — Add a Cached syntect/two-face Code Highlighter Helper

Code blocks currently render every character in the single `ThemeRole::CodeBlock` color (`crates/makina/src/markup.rs:153–156`) — no per-token coloring ("the code is not colored at all"). Add a real highlighter backed by `syntect`, pulled in via the `two-face` crate which bundles syntect's syntax + theme assets so no runtime asset files are needed. The helper classifies tokens for the fenced language and returns ratatui spans; building the syntax/theme sets is expensive, so cache them once for the process.

**Steps:**

1. Add the `two-face` crate (run `cargo add two-face` to pin the current version; it re-exports `syntect`) — or `syntect` directly — to `crates/makina/Cargo.toml` `[dependencies]`. Prefer the pure-Rust `fancy-regex` engine over `onig` to avoid a C build dependency.
2. Create `crates/makina/src/syntax.rs` and declare `mod syntax;` next to `mod markup;` (in `main.rs`/`lib.rs`). Hold the `SyntaxSet` and `ThemeSet` in `std::sync::OnceLock`s (loaded from `two_face::syntax::extra_newlines()` and `two_face::theme::extra()`) so the assets load at most once.
3. Implement `pub fn highlight_code_line(line: &str, lang: Option<&str>, theme: &crate::theme::Theme) -> Vec<ratatui::text::Span<'static>>`. Resolve the syntax via `SyntaxSet::find_syntax_by_token(lang)` (the fence info string) with a fallback to `find_syntax_plain_text()`. Pick a bundled syntect theme by light/dark — derive light vs. dark from the luminance of `theme.get(ThemeRole::Background)`.
4. Run `syntect::easy::HighlightLines::highlight_line(line, &syntax_set)`, then map each `(syntect::highlighting::Style, &str)` to `Span::styled(text.to_string(), Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b)))`. Do NOT set a background here (the render task applies the full-width band). On any error or unknown language, return a single `Span::styled(line.to_string(), Style::default().fg(theme.get(ThemeRole::CodeBlock)))` (monochrome fallback).
5. Add unit tests in `syntax.rs`: highlighting `let x = 1;` as `rust` yields more than one span (tokens colored distinctly), and an unknown language (or `None`) yields a single monochrome span.
6. Run `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`.

- **Depends on:** —
- **Done when:** `crate::syntax::highlight_code_line()` exists, caches its syntax/theme sets in `OnceLock`s, returns multiple distinctly-colored spans for a known language, and falls back to a single monochrome span for unknown languages or errors; the new dependency builds; cargo test, clippy, and fmt all pass green.

---

### fix-codeblock-line-splitting-and-full-width-band — Fix Code-Block Blank Lines, Wire Highlighting, and Render a Full-Width Band

Three defects live in the code-block path of `render_markdown()` (`crates/makina/src/markup.rs:127–182`): (a) the `in_code_block` branch of `Event::Text` splits on `'\n'` (line 157) and pushes a `Line` for every element — but pulldown-cmark emits one `Event::Text` per source line WITH a trailing `'\n'`, so `split('\n')` yields a trailing empty element that becomes a blank line after EVERY code line (the user's "unnecessary blank lines between every line"); (b) tokens are uncolored; (c) the background is `Background` (== pane) and only spans glyph cells. Fix all three.

**Steps:**

1. Capture the fence language: change `Event::Start(Tag::CodeBlock(_))` (line 127) to bind the kind, and when it is `CodeBlockKind::Fenced(info)`, store `code_lang = Some(info.to_string())` in an `Option<String>` declared alongside `in_code_block` (around line 99). Reset `code_lang = None` in the `Event::End(TagEnd::CodeBlock)` arm (line 180).
2. Stop emitting a `Line` for the empty trailing split element in the `in_code_block` branch (lines 150–159). Either iterate `text_str.split('\n')` and skip the final empty element, or accumulate the block's text and split once at block end. Each real source line must map to EXACTLY ONE `Line`.
3. For each code source line, call `crate::syntax::highlight_code_line(line, code_lang.as_deref(), theme)` to get foreground-colored spans, prefixed with the existing two-space indent span.
4. Apply the full-width band: build the line and set its background to `theme.get(ThemeRole::CodeBlockBg)` for the whole row — e.g. `Line::from(spans).style(Style::default().bg(theme.get(ThemeRole::CodeBlockBg)))` — and pad the line to the render `width` (trailing spaces) so ratatui fills the band to the right edge (mirror how other full-width rows pad).
5. Verify no blank line appears between consecutive code lines; if cmark still emits a single trailing empty line, strip exactly that one at `Event::End(TagEnd::CodeBlock)`.
6. Update/extend markdown unit tests: a fenced ` ```rust ` block of N source lines renders exactly N code `Line`s (no interleaved blanks), each carrying the `CodeBlockBg` background; an unknown-language block renders monochrome but still banded.
7. Run `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`.

- **Depends on:** add-codeblock-background-theme-role, add-syntect-code-highlighter
- **Done when:** fenced code blocks render exactly one `Line` per source line (no spurious blank lines), each line carries syntax-highlighted foreground spans (monochrome fallback for unknown languages), and a `CodeBlockBg` background band spans the full viewport width; cargo test, clippy, and fmt all pass green.

---

## 0003 — Scrollbar-Position-Accuracy

### fix-scrollbar-state-initialization — Fix Scrollbar State Initialization to Use scroll_max Instead of total_rendered_rows

The scrollbar widget at `crates/makina/src/ui.rs:2148` (and the sibling site at line 1131) initializes `ScrollbarState::new(total_rendered_rows as usize)`, treating the total row count as the content size. ratatui's `ScrollbarState::new()` expects the *maximum* scroll offset (i.e. `content_height - viewport_height`), not the total content height. This mismatch places the thumb incorrectly — most visibly, when scrolled to the very end the thumb floats mid-track and the bar looks as if more content remains below. Use `scroll_max` instead.

**Steps:**

1. In `crates/makina/src/ui.rs`, find all `ScrollbarState::new(total_rendered_rows as usize)` sites — at minimum line 1131 (exchange pane) and line 2148 (accordion pane).
2. At line 2148, replace `ScrollbarState::new(total_rendered_rows as usize)` with `ScrollbarState::new(scroll_max as usize)`.
3. At line 1131 (and any other matching site), apply the same fix: use `scroll_max` instead of `total_rendered_rows`. Confirm each site computes `scroll_max = total_rendered_rows.saturating_sub(content_area.height)` (e.g. line 2147 for the accordion); add the computation if a site lacks it.
4. Confirm the `scroll_max == 0` guard still behaves (content shorter than the viewport hides/empties the thumb) — `ScrollbarState::new(0)` with `.position(0)` is correct.
5. Run `cargo test` and manually scroll long content to the very end; the thumb should reach the bottom of the track, not float mid-track.

- **Depends on:** fix-unfocused-tab-background-color
- **Done when:** all `ScrollbarState` initializations on scrollable panes use `scroll_max` (maximum scroll offset) rather than `total_rendered_rows`; the thumb accurately reflects scroll position at all offsets, including the end of content; cargo test, clippy, and fmt all pass green.

---

## 0004 — Cycle-Views-Key-Binding

### verify-and-document-cycle-dependency-view-binding — Make 'v' Cycle Give Immediate Visible Feedback and Document It

The 'v' binding is NOT dead. `crates/makina/src/event.rs:1311` maps `'v'`/`'V'` to `AppEvent::CycleDependencyView`; the handler at `crates/makina/src/app.rs:2128–2135` cycles `Off → List → Tree → Timeline → Off` (proven by the test at `app.rs:4128–4136`); and a `view: …` status label updates (`crates/makina/src/ui.rs:821–826`). The reason users see "nothing happen": the dependency sub-pane is only carved and drawn inside the run/exchange render branch (`crates/makina/src/ui.rs:711–720`), and `render_dependency_view` (`ui.rs:1148–1205`) only shows content for a focused task WITH dependencies (otherwise "No task focused." / "No dependencies."). From a plan tab, a task tab, or with no task focused, pressing 'v' produces no obvious change. The fix is immediate, context-independent feedback plus documentation — NOT merely a help line.

**Steps:**

1. In `crates/makina/src/app.rs`, in the `AppEvent::CycleDependencyView` handler (line 2128), after updating `self.dependency_view`, push an immediate status message such as `format!("Dependency view: {}", label)` (label = off/list/tree/timeline). Use whatever status mechanism other handlers use (grep for how `StatusMessage` / a `status` field is set) so feedback is visible from ANY focus context.
2. In `render_dependency_view` (`crates/makina/src/ui.rs:1148–1205`), make the empty state self-explanatory: when no task is focused, render "Select a task to see its dependencies — v cycles the view" instead of a bare "No task focused.".
3. In `render_help_overlay` (`crates/makina/src/ui.rs:3334`), add a keybinding row "v — Cycle dependency view (off → list → tree → timeline)" styled with `ThemeRole::Dim`, near the other view/pane toggles (e.g. the 'e' error-pane entry).
4. Confirm whether the dependency sub-pane should also be visible while a task tab is focused; if the carve at `ui.rs:711` is unreachable in that render path, either extend it to that path or rely on the step-1 status feedback. At minimum, pressing 'v' must observably do something everywhere.
5. Add/extend a test asserting `CycleDependencyView` yields a non-empty status message (alongside the existing cycle test at `app.rs:4128`). Run `cargo test`, clippy, and fmt.

- **Depends on:** fix-unfocused-tab-background-color, fix-scrollbar-state-initialization
- **Done when:** pressing 'v' produces immediate visible feedback (a "Dependency view: …" status message) from any focus context, the empty dependency-view state is self-explanatory, and the 'v' binding is listed in the help overlay; the existing cycle behavior is preserved; cargo test, clippy, and fmt all pass green.

---

## 0005 — Plan-Execution-Controls

### restore-run-control-dispatch-from-detail-pane — Restore Start/Pause/Cancel via Ctrl Aliases and Plan-Tab Run Sync

Two concrete defects make start "completely broken" for an opened plan. (1) When a plan tab is focused in the main pane, `crates/makina/src/event.rs:1332–1338` binds `'s'` to `ToggleAccordionSection(Scope)`, so `StartRun` is UNREACHABLE there — and WS6 will overload `'s'` for task tabs too. (2) `sync_selected_run_to_active_tab` (`crates/makina/src/app.rs:1483–1502`) only syncs `selected_run` for a `TabContent::Task` tab and early-returns for a `TabContent::Plan` tab, so `run_control` (`event.rs:947–964`) finds `selected_run() == None` and returns "No run selected". Fix both: add always-available Ctrl-modified run-control keys, and select the run for plan tabs.

**Steps:**

1. In `crates/makina/src/event.rs` key handling, add Ctrl aliases BEFORE the accordion-overloaded plain-letter arms (around line 1311–1338) so the modifier guard wins (mirror the existing `Ctrl+O` → `ToggleVerbose` at line 1318): `KeyCode::Char('s') if mods.contains(CONTROL) => AppEvent::StartRun`, `Char('p') if CONTROL => AppEvent::PauseRun`, `Char('c') if CONTROL => AppEvent::CancelRun`. These dispatch run control in ANY context.
2. In `crates/makina/src/app.rs`, extend `sync_selected_run_to_active_tab` (line 1483) to also handle a `TabContent::Plan { plan_slug }` active tab: set `selected_run` to the index of the run whose `makina_core::orchestrator::plan_slug(&run.task_list_path)` equals `plan_slug` (no task match required). Keep the existing `TabContent::Task` arm.
3. Ensure `sync_selected_run_to_active_tab()` runs after tab activation / focus changes so `selected_run` is fresh before a control key dispatches. Grep `App::update()` for the tab/focus handlers (`OpenTab` / `ActivateTab` / `NextTab` / `PrevTab` / `FocusNext` / `FocusPrev`) and call it there if it is not already invoked.
4. Improve the no-run message in `run_control` (`event.rs:952`) from "No run selected" to an actionable "No run selected — open a plan or task tab, or pick a run in the sidebar".
5. If an opened plan tab corresponds to no entry in `self.runs` (the plan was never ingested into a run), do NOT assume a run exists: surface a clear status message describing how to start work rather than silently no-op'ing. Confirm what "start work for a plan" maps to (an existing `RunId` vs. creating a run) before wiring the dispatch.
6. Add a test: with a plan tab active, `sync_selected_run_to_active_tab` sets `selected_run` to that plan's run, and `resolve_io_for_test(StartRun)` issues a `Command::StartRun` (mirror the existing StartRun tests at `event.rs:2238–2309`). Run `cargo test`, clippy, fmt.

- **Depends on:** verify-and-document-cycle-dependency-view-binding
- **Done when:** `Ctrl+S`/`Ctrl+P`/`Ctrl+C` start/pause/cancel the active plan's (or task's) run from the detail pane in any focus context; `sync_selected_run_to_active_tab` resolves `selected_run` for plan tabs; a missing run surfaces an actionable message instead of silently failing; cargo test, clippy, and fmt all pass green.

---

## 0006 — Task-Detail-Accordion-Structure

### add-accordion-state-for-task-details — Add Accordion State Management for Task Detail Tabs

To support accordion-style rendering in task tabs, the `App` struct needs to track which accordion sections (Scope, Execution) are expanded for each task — analogous to the plan accordion state for plan tabs. The state is initialized when a task tab opens and updated when accordion toggles dispatch.

**Steps:**

1. In `crates/makina/src/app.rs`, locate the `App` struct definition (around line 1030–1140).
2. Add a field (around line 1090): `pub task_accordion_expanded: HashMap<TaskId, HashSet<AccordionSection>>`, mapping each task id to its set of expanded sections.
3. In `App::new()`, initialize this field to `HashMap::new()`.
4. Extend the `AccordionSection` enum (`app.rs:1027`) to include an `Execution` variant (it already has `Scope`); keep it `#[derive(... Copy, PartialEq, Eq, Hash)]`.
5. Run `cargo build` to ensure the new field and variant compile with no type errors.

- **Depends on:** restore-run-control-dispatch-from-detail-pane
- **Done when:** the `App` struct has a `task_accordion_expanded: HashMap<TaskId, HashSet<AccordionSection>>` field initialized in `new()`; `AccordionSection` includes `Scope` and `Execution` variants; cargo build (and test/clippy/fmt) pass green.

---

### refactor-task-entry-pane-to-accordion-sections — Refactor render_task_entry_pane to Use Accordion-Section Structure

`render_task_entry_pane()` at `crates/makina/src/ui.rs:1831–1913` renders task metadata and the markdown body as a flat sequence. To adopt the accordion pattern, render the metadata, then two accordion sections: "Scope" (the task's static description) and "Execution" (live activity — exchanges and progress). This reuses the proven `render_accordion_section()` helper (`ui.rs:2163–2220`), which handles expand/collapse, focused-section highlighting, and content rendering.

**Steps:**

1. In `crates/makina/src/ui.rs`, locate `render_task_entry_pane()` (around line 1831–1913).
2. Render task metadata at the top (id, title, state badge, dependencies, metrics) as before, but build the body as two accordion sections instead of a single markdown blob.
3. Section 1 — "Scope": the task's `entry_text` rendered as markdown. Call `render_accordion_section()` with `AccordionSection::Scope` and the task's expanded state from `app.task_accordion_expanded.get(task_id)`.
4. Section 2 — "Execution": populate it with the task's live activity so it is not empty — summarize the task's exchanges (`app.exchange_logs` / `selected_exchange_log`) and progress/state for this `(RunId, TaskId)`, falling back to "No execution yet — start the run (Ctrl+S)" when there is none. Render it via `render_accordion_section()` with `AccordionSection::Execution`.
5. Collect all lines (metadata + both accordion sections) and render as a `Paragraph` with `Wrap { trim: false }`.
6. Run `cargo test` and manually open a task tab to verify the accordion renders with collapsible Scope and Execution sections, Execution showing activity once a run is started.

- **Depends on:** add-accordion-state-for-task-details, fix-unfocused-tab-background-color, fix-scrollbar-state-initialization, verify-and-document-cycle-dependency-view-binding
- **Done when:** `render_task_entry_pane()` renders two accordion sections (Scope and Execution) with expand/collapse toggles; Scope holds the task's static description; Execution shows live activity (or a clear empty state) for the task; the function reuses `render_accordion_section()`; cargo test, clippy, and fmt all pass green.

---

### wire-accordion-toggles-for-task-tabs — Wire Accordion Toggle Keybinds (s/z) to Task Accordion State

The accordion-toggle keybinds at `crates/makina/src/event.rs:1332–1371` currently apply only to plan tabs. Extend them so a task tab routes `'s'` and `'z'` to toggle its Scope and Execution sections. Because WS5 moved run-control to `Ctrl+S`/`Ctrl+P`/`Ctrl+C`, overloading plain `'s'`/`'z'` for the task accordion no longer blocks starting a run.

**Steps:**

1. In `crates/makina/src/app.rs`, add an `AppEvent::ToggleTaskAccordionSection(AccordionSection)` variant (around the existing `ToggleAccordionSection` at `app.rs:909`).
2. In `crates/makina/src/event.rs`, update the `'s'` arm (line 1332–1338): keep `plan_tab_active` → `ToggleAccordionSection(Scope)`; add a task-tab branch → `ToggleTaskAccordionSection(Scope)` (detect a task tab via the active `TabContent::Task`); otherwise fall through. Run-control start is now `Ctrl+S` (WS5), so the plain-`'s'` `StartRun` fallback may remain for the sidebar context but is no longer the primary start path.
3. Update the `'z'` arm (line 1361–1371) similarly to toggle `Execution` for task tabs.
4. In `App::update()`, handle `AppEvent::ToggleTaskAccordionSection(section)` by toggling that section in `self.task_accordion_expanded` for the active task id.
5. Run `cargo test` and manually toggle a task tab's Scope (`s`) and Execution (`z`) sections; confirm `Ctrl+S` still starts the run from the same tab.

- **Depends on:** refactor-task-entry-pane-to-accordion-sections
- **Done when:** `'s'` and `'z'` toggle a task tab's Scope and Execution sections when a task tab is active; the state persists in `app.task_accordion_expanded`; the keybinds still route to the plan accordion when a plan tab is active; `Ctrl+S` still starts the run; cargo test, clippy, and fmt all pass green.

---

**End of plan 0042 TASKS.** When every "Done when" bullet is green, the six
detail-pane defects are resolved and the task-detail accordion enhancement is in
place: unfocused tabs are legible, code blocks render with syntect-backed syntax
highlighting and full-width `CodeBlockBg` bands with no spurious blank lines, the
scrollbar thumb tracks position accurately, the 'v' dependency-view cycle gives
immediate feedback and is documented, start/pause/cancel dispatch from the detail
pane via `Ctrl+S`/`Ctrl+P`/`Ctrl+C` with the plan's run correctly selected, and
task tabs adopt the proven accordion structure with collapsible Scope and
Execution sections.
