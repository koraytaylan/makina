# Makina Plan 0009 — Exchange-Pane Fidelity

Make the Exchange pane a faithful, readable, chronological transcript: fix the
response/thought/tool ordering bug, render full Markdown + ANSI, compact tool
paths to the repo root, restore native text selection, and add a working
spinner.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas. Every change is in the `makina` binary crate.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must also keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints against `develop`; locate every site by the named
  symbol (grep), since earlier tasks shift lines.
- When a task says "add a test named X", the test's name must appear literally
  in "Done when" and use `#[test]` / `#[tokio::test]` matching the file's style.

---

## 0029 — Chronological response segmentation

### exchange-segment-response-chunks — Stop coalescing response chunks across thoughts/tools

Today `ExchangeLog::append_chunk` (`crates/makina/src/app.rs`) scans **backward
past** `Thought`/`Tool` entries to merge every response chunk into one `Response`
entry anchored at the first chunk. A `chunk → thought → tool → chunk` turn
therefore renders the whole response first and the interleaved reasoning/tools
after it. We fix this by coalescing only with the **immediately preceding** open
response, so the response splits into chronological segments.

**Steps:**

1. Open `crates/makina/src/app.rs` and find `impl ExchangeLog`.

2. Add a small helper that finalises a trailing open response:

   ```rust
   /// Mark the last entry complete if it is a still-open response.
   /// Called before pushing any new entry so only the tail segment streams.
   fn finalize_trailing_response(&mut self) {
       if let Some(last) = self.entries.last_mut()
           && let ExchangeContent::Response { complete, .. } = &mut last.content
       {
           *complete = true;
       }
   }
   ```

3. Replace the body of `append_chunk` so it coalesces with the **last** entry
   only, finalising any previous open response when a new segment starts:

   ```rust
   pub fn append_chunk(&mut self, role: AgentRole, chunk: String) {
       if let Some(last) = self.entries.last_mut()
           && last.role == role
           && let ExchangeContent::Response { text, complete } = &mut last.content
           && !*complete
       {
           text.push_str(&chunk);
           return;
       }
       // A thought/tool (or a different role) intervened: close the old segment
       // and start a new one so order is preserved.
       self.finalize_trailing_response();
       self.push(ExchangeEntry {
           role,
           content: ExchangeContent::Response { text: chunk, complete: false },
       });
   }
   ```

4. At the **start** of `append_thought` and `start_tool`, call
   `self.finalize_trailing_response();` (before their existing coalesce/push
   logic) so a response segment that precedes a thought/tool is closed and stops
   showing the streaming cursor.

5. Leave `complete_turn` as-is (it already scans back past trailing Thought/Tool
   to finalise the last open response — now a no-op in the common case, a
   safety net when a thought/tool trails the final chunk).

6. Add a test in the `mod tests` block:

   ```rust
   #[test]
   fn exchange_log_segments_response_around_thought_and_tool() {
       let mut log = ExchangeLog::default();
       log.append_chunk(AgentRole::Developer, "Let me ".into());
       log.append_thought(AgentRole::Developer, "checking…".into());
       log.start_tool(AgentRole::Developer, "t1".into(), "read".into(), None, "completed".into());
       log.append_chunk(AgentRole::Developer, "do X.".into());
       let kinds: Vec<_> = log.entries.iter().map(|e| match &e.content {
           ExchangeContent::Response { .. } => "resp",
           ExchangeContent::Thought { .. }  => "thought",
           ExchangeContent::Tool { .. }     => "tool",
           ExchangeContent::Prompt { .. }   => "prompt",
       }).collect();
       assert_eq!(kinds, ["resp", "thought", "tool", "resp"]);
       // First segment is closed; only the tail is open until complete_turn.
       assert!(matches!(log.entries[0].content, ExchangeContent::Response { complete: true, .. }));
       log.complete_turn();
       assert!(log.entries.iter().all(|e| e.complete()));
   }
   ```

- **Depends on:** —
- **Done when:** `exchange_log_segments_response_around_thought_and_tool` passes;
  `grep -n 'finalize_trailing_response' crates/makina/src/app.rs` matches; all
  existing exchange-log tests still pass; cargo test/clippy/fmt green.

---

## 0030 — Full Markdown and ANSI rendering

### add-markup-renderer — A Markdown + ANSI → ratatui module

Agent responses contain Markdown and sometimes ANSI. We add a focused render
module backed by maintained crates instead of extending the four-code
`ansi.rs`.

**Steps:**

1. Add dependencies to `crates/makina/Cargo.toml` under `[dependencies]`:

   ```toml
   pulldown-cmark = { version = "0.12", default-features = false }
   ansi-to-tui    = "7"
   ```

   (Add `comrak = "0.29"` only if a GFM-table test needs its parser; prefer
   pulldown-cmark's `Options::ENABLE_TABLES` first.)

2. Create `crates/makina/src/markup.rs` and declare `mod markup;` in
   `crates/makina/src/main.rs` (next to the other `mod` lines).

3. Implement two entry points. `render_markdown` walks `pulldown_cmark::Parser`
   events into ratatui lines; representative skeleton (handle these event kinds,
   extend as the tests require):

   ```rust
   use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
   use ratatui::style::{Modifier, Style};
   use ratatui::text::{Line, Span};

   /// Render CommonMark + GFM tables to styled lines, wrapped to `width`.
   pub fn render_markdown(text: &str, base: Style, _width: u16) -> Vec<Line<'static>> {
       let mut opts = Options::empty();
       opts.insert(Options::ENABLE_TABLES);
       opts.insert(Options::ENABLE_STRIKETHROUGH);
       let mut out: Vec<Line<'static>> = Vec::new();
       let mut spans: Vec<Span<'static>> = Vec::new();
       let mut style = base;
       for ev in Parser::new_ext(text, opts) {
           match ev {
               Event::Start(Tag::Heading { level, .. }) => {
                   style = base.add_modifier(Modifier::BOLD); // colour by `level`
               }
               Event::Start(Tag::Strong) => style = style.add_modifier(Modifier::BOLD),
               Event::Start(Tag::Emphasis) => style = style.add_modifier(Modifier::ITALIC),
               Event::Code(t) => spans.push(Span::styled(t.to_string(),
                   base.add_modifier(Modifier::DIM | Modifier::REVERSED))),
               Event::Text(t) => spans.push(Span::styled(t.to_string(), style)),
               Event::End(TagEnd::Heading(_)) | Event::End(TagEnd::Paragraph)
               | Event::HardBreak | Event::SoftBreak => {
                   out.push(Line::from(std::mem::take(&mut spans)));
                   style = base;
               }
               // … list items (indent + marker), blockquote (dim), code blocks
               //   (indented, ANSI-aware via render_ansi), tables (aligned cells)
               _ => {}
           }
       }
       if !spans.is_empty() { out.push(Line::from(spans)); }
       out
   }

   /// Render a string that may contain ANSI SGR (16/256/RGB, bold/italic/underline).
   pub fn render_ansi(text: &str) -> Vec<Line<'static>> {
       use ansi_to_tui::IntoText;
       text.into_text().map(|t| t.lines).unwrap_or_else(|_|
           text.lines().map(|l| Line::from(l.to_string())).collect())
   }
   ```

4. Add tests in `markup.rs`:

   ```rust
   #[test]
   fn markup_renders_heading_bold_list_and_code() { /* assert styled spans, no literal '#'/'*' */ }
   #[test]
   fn markup_renders_gfm_table() { /* a 2x2 table renders >= 2 lines with cell text */ }
   #[test]
   fn markup_renders_ansi_256_color() {
       let lines = super::render_ansi("\x1b[38;5;208mhi\x1b[0m");
       assert!(lines.iter().any(|l| l.spans.iter().any(|s| s.content == "hi")));
   }
   ```

- **Depends on:** —
- **Done when:** the three `markup_*` tests pass; `grep -n 'pub fn render_markdown\|pub fn render_ansi' crates/makina/src/markup.rs` matches; cargo test/clippy/fmt green.

### wire-markup-into-exchange-pane — Render response/thought bodies through markup

**Steps:**

1. Open `crates/makina/src/ui.rs` and find `exchange_entry_lines`.

2. In the **Response** and **Thought** arms, replace the per-line
   `diff_overlaid_content_line` loop with:

   ```rust
   lines.extend(crate::markup::render_markdown(text, base_style, inner_width));
   ```

   passing the role-coloured `base_style` and the pane inner width (thread the
   width into `exchange_entry_lines`, or render at a sensible default and let the
   paragraph wrap).

3. Leave the **Tool** arm on the existing diff-aware path
   (`diff_overlaid_content_line`) so edit diffs keep `+`/`-`/`@@` colouring; if
   tool content carries ANSI, route it through `crate::markup::render_ansi`.

4. Keep the **Prompt** arm rendering plain text (our own text, not Markdown).

5. Add a render test `exchange_render_markdown_and_ansi` modelled on the existing
   `exchange_render_styles_ansi_and_diff…`: build a Response entry whose text has
   a heading, a bullet, and an ANSI colour; render a small area; assert the
   heading text appears styled and no literal escape bytes remain in the buffer.

- **Depends on:** add-markup-renderer
- **Done when:** `exchange_render_markdown_and_ansi` passes; `grep -n 'render_markdown' crates/makina/src/ui.rs` matches; existing exchange render tests pass; cargo green.

---

## 0031 — Compact tool paths to the repo root

### compact-tool-paths-to-repo-root — Show repo-relative paths in tool entries

Tool titles are passed through verbatim, so an absolute worktree path is shown in
full. Thread the repo root into `App` and rewrite such paths.

**Steps:**

1. In `crates/makina/src/app.rs`, add `pub repo_root: std::path::PathBuf,` to the
   `App` struct and a `repo_root` parameter to `App::new`; set the field in the
   constructor.

2. In `crates/makina/src/main.rs`, pass the already-computed `repo_root` (the
   `std::env::current_dir()` value) into `App::new(api, initial_runs, repo_root)`.

3. Add a helper in `crates/makina/src/markup.rs` (or a small `paths` module):

   ```rust
   use std::path::Path;
   /// Rewrite absolute paths under the repo root (incl. the worktrees dir) to a
   /// compact repo-relative form. Non-matching text is returned unchanged.
   pub fn compact_paths(s: &str, repo_root: &Path) -> String {
       let root = repo_root.to_string_lossy();
       let wt = format!("{root}/.makina/worktrees/");
       let mut out = s.to_string();
       // Strip "<root>/.makina/worktrees/<slug>--<id>/" → "" (worktree-relative).
       if let Some(i) = out.find(&*wt) {
           if let Some(rel_start) = out[i + wt.len()..].find('/') {
               let cut = i + wt.len() + rel_start + 1;
               out.replace_range(i..cut, "");
           }
       }
       out.replace(&format!("{root}/"), "")
   }
   ```

4. In `crates/makina/src/ui.rs`, apply `compact_paths(title, &app.repo_root)` (and
   to tool content paths) in the Tool arm of `exchange_entry_lines`. Thread
   `&app.repo_root` (or just the `PathBuf`) into the render path that builds tool
   lines.

5. Update every `App::new(...)` call site, including the test constructors (grep
   `App::new(`), to pass a repo root (`std::path::PathBuf::from(".")` in tests).

6. Add a test `tool_title_compacted_to_repo_root` that renders a Tool entry whose
   title contains `<repo>/.makina/worktrees/plan--t1/src/main.rs` and asserts the
   rendered line contains `src/main.rs` and not the worktree prefix.

- **Depends on:** —
- **Done when:** `tool_title_compacted_to_repo_root` passes; `grep -n 'compact_paths' crates/makina/src/ui.rs` matches; `cargo test -p makina` compiles (all `App::new` call sites updated) and is green; clippy/fmt green.

---

## 0032 — Restore native text selection

### remove-mouse-capture — Stop capturing the mouse so the terminal can select text

Nothing in the TUI consumes mouse events, but `EnableMouseCapture` prevents the
terminal's native selection/copy.

**Steps:**

1. Open `crates/makina/src/tui.rs`.

2. In `init`, remove `EnableMouseCapture` from the `execute!(stdout, …)` block
   (keep `EnterAlternateScreen` and `cursor::Hide`).

3. In `restore`, remove the matching `DisableMouseCapture`.

4. Remove the now-unused `EnableMouseCapture` / `DisableMouseCapture` imports
   (and, if it triggers an unused-import warning, the `MouseEventKind` import in
   `event.rs`).

- **Depends on:** —
- **Done when:** `grep -n 'MouseCapture' crates/makina/src/tui.rs` returns nothing;
  `cargo build -p makina` is warning-free; clippy/fmt green; manual check: text
  can be selected/copied in the running TUI.

---

## 0033 — Working spinner

### add-working-spinner — Animate a throbber while an agent is working

The event loop already ticks every 250 ms (`event.rs`); drive a frame counter
off it and render a braille spinner.

**Steps:**

1. In `crates/makina/src/app.rs`, add `pub tick: u64,` to `App`, initialise it to
   `0` in `App::new`, and increment it in the `AppEvent::Tick` arm of
   `App::update` (currently a no-op):

   ```rust
   AppEvent::Tick => {
       self.tick = self.tick.wrapping_add(1);
       true
   }
   ```

2. Add a spinner helper (in `app.rs` or `ui.rs`):

   ```rust
   pub const SPINNER: [&str; 10] = ["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"];
   pub fn spinner_frame(tick: u64) -> &'static str { SPINNER[(tick as usize) % SPINNER.len()] }
   ```

3. In `crates/makina/src/ui.rs` task table rows, for tasks whose state is
   `InProgress` or `InReview`, prefix the state cell with
   `spinner_frame(app.tick)` and a space.

4. In `render_exchange_pane`, when the selected task's `ExchangeLog` has a
   trailing incomplete `Response` segment, show `spinner_frame(app.tick)` in the
   pane title.

5. Add tests:

   ```rust
   #[test]
   fn spinner_frame_advances_on_tick() {
       assert_ne!(super::spinner_frame(0), super::spinner_frame(1));
   }
   ```

   and a render test `spinner_shown_for_in_progress_task` asserting a spinner
   glyph appears in the rendered table for an `InProgress` task.

- **Depends on:** —
- **Done when:** `spinner_frame_advances_on_tick` and `spinner_shown_for_in_progress_task` pass; `grep -n 'spinner_frame' crates/makina/src` matches in app/ui; cargo test/clippy/fmt green.

---

## Cross-cutting verification

### plan-0009-acceptance — End-to-end pane fidelity check

Add one render test (in `crates/makina/src/ui.rs` tests or `makina/tests/`) that
builds a single task's `ExchangeLog` with: a prompt, a response chunk, a thought,
a tool (completed), a second response chunk containing Markdown (`**bold**`) and
an ANSI colour, and a `complete_turn`. Render the exchange pane and assert:

- entry order is prompt → response → thought → tool → response (segmentation);
- the second response shows styled bold text and the ANSI colour (no literal
  `**` or escape bytes);
- a tool title with a worktree-absolute path renders repo-relative.

- **Depends on:** exchange-segment-response-chunks, wire-markup-into-exchange-pane, compact-tool-paths-to-repo-root
- **Done when:** the test (name containing `pane_fidelity` or `plan_0009`) passes
  and would have failed before 0029–0031; cargo test/clippy/fmt green.

---

**End of plan 0009 TASKS.** When every "Done when" bullet is green, the Exchange
pane shows the turn in true order, renders Markdown and colour, keeps tool paths
short, allows native selection, and spins while the agent works.
