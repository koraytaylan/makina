# Architecture — Plan 0009 (deltas)

> Concrete changes to make the Exchange pane a faithful, readable transcript.
> Line numbers are hints against `develop`; locate every site by the named
> symbol.

The pane is fed by `Event::AgentExchange` → `App::update` → `ExchangeLog`
(`crates/makina/src/app.rs`) → `ui::exchange_entry_lines` /
`render_exchange_pane` (`crates/makina/src/ui.rs`). All five deltas live in the
`makina` binary crate; none touches `makina-core` or `makina-acp`.

## 0029 — Chronological response segmentation

### The bug

`ExchangeLog::append_chunk` (app.rs:178) currently walks entries in reverse and
*skips over* `Thought`/`Tool` entries to find the still-open `Response` of the
same role:

```rust
for entry in self.entries.iter_mut().rev() {
    match &mut entry.content {
        ExchangeContent::Response { text, complete }
            if entry.role == role && !*complete => { text.push_str(&chunk); return; }
        ExchangeContent::Thought { .. } | ExchangeContent::Tool { .. } => continue, // ← bug
        _ => break,
    }
}
```

For the turn `chunk("Let me ") · thought · tool · chunk("do X.")` the entries end
up `[Prompt, Response("Let me do X."), Thought, Tool]` — the response is one
block and the interleaved thought/tool sort after it.

### The fix

Coalesce only with the **immediately preceding** entry when it is an open
`Response` of the same role; otherwise push a new segment:

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
    self.push(ExchangeEntry { role, content: ExchangeContent::Response { text: chunk, complete: false } });
}
```

So the same turn yields `[Prompt, Response("Let me "), Thought, Tool,
Response("do X.")]` — true chronological order, no render change required.

Supporting tweaks:

- **`complete_turn`** (app.rs:211): keep finalising the last open response, but
  since multiple open segments can now exist, also mark *all* prior open
  `Response` segments of that turn complete (an intervening thought/tool means
  the earlier segment is finished). Simplest: when starting a new segment (or a
  thought/tool), mark the previous open `Response` of the same role complete, so
  at most the trailing segment streams a cursor.
- **Streaming cursor** in `exchange_entry_lines` already renders only on
  incomplete responses; with the above only the last segment shows it.

Data model is unchanged (`Vec<ExchangeEntry>` in arrival order); only insertion
logic changes. `append_thought`/`start_tool`/`update_tool` are untouched.

```
before:  Prompt │ Response("Let me do X.") │ 💭 Thought │ ⚙ Tool
after:   Prompt │ Response("Let me ") │ 💭 Thought │ ⚙ Tool │ Response("do X.")
```

## 0030 — Full Markdown + full ANSI rendering

Today response/thought lines go through `diff_overlaid_content_line` (ui.rs:952)
which calls the four-code `parse_ansi` (ansi.rs) and overlays diff colours.

Introduce a render module (e.g. `crates/makina/src/markup.rs`) with:

- `render_markdown(text: &str, base: Style, width: u16) -> Vec<Line<'static>>`
  — parse CommonMark + GFM with `pulldown-cmark` (add `comrak` only if tables
  need its renderer), mapping block/inline events to ratatui `Line`/`Span`:
  headings (bold + colour by level), emphasis/strong (italic/bold), bullet and
  ordered lists (indent + marker), blockquotes (indent + dim), fenced code
  (boxed/indented, monospace style, ANSI-aware), inline code (reverse/dim),
  tables (aligned columns within `width`).
- ANSI handling delegates to `ansi-to-tui` (`Text::from_ansi`/`into_text`) so
  16/256/RGB + bold/italic/underline all work; `ansi.rs` is replaced by this
  path (kept only if a test still needs the old helper).

Wiring in `exchange_entry_lines` (ui.rs:982–1095):

- **Response** and **Thought** bodies → `render_markdown`.
- **Tool** content keeps the diff-aware path (edit diffs must still colour
  `+`/`-`/`@@`); ANSI inside tool content still goes through `ansi-to-tui`.
- **Prompt** text stays plain (it is our own text, not model Markdown).

`Cargo.toml` (makina crate) gains `pulldown-cmark` (+ optional `comrak`) and
`ansi-to-tui`. Wrapping/width: pass the pane inner width so tables and code
boxes lay out correctly; long lines wrap as today.

## 0031 — Repo-root-relative tool paths

`App` does not know the repo root today. Thread it in:

- `main.rs:52` already computes `repo_root`; pass it to `App::new` and store
  `repo_root: PathBuf` on `App` (app.rs:496–586).
- Add `fn compact_path(s: &str, repo_root: &Path) -> String` (in `markup.rs` or
  a small `paths` helper): replace occurrences of `repo_root` **and** the
  `repo_root/.makina/worktrees/{slug}--{id}/` prefix with `""` (or `./`),
  yielding `src/main.rs`. Only absolute paths under those roots are rewritten;
  everything else is verbatim.
- Apply in the Tool arm of `exchange_entry_lines` to `title` (and to any path
  text in tool content). The worktree slug/id are not stored per task today; the
  generic `.makina/worktrees/…/` prefix strip handles it without needing them.

## 0032 — Restore text selection

In `crates/makina/src/tui.rs`:

- `init` (tui.rs:57–64): drop `EnableMouseCapture` from the `execute!` block.
- `restore` (tui.rs:84): drop the matching `DisableMouseCapture`.

Raw mode + alternate screen remain; with capture off the terminal performs
native selection/copy. `MouseEventKind` is imported but never matched in
`event.rs`, so no event-loop change is needed (optionally drop the unused
import).

## 0033 — Working spinner

- Add `tick: u64` (frame counter) to `App` (app.rs:496–586); increment it in the
  `AppEvent::Tick` arm (app.rs:819, currently a no-op).
- `const SPINNER: [&str; 10] = ["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"];`
  frame = `SPINNER[(self.tick as usize) % SPINNER.len()]`.
- Render:
  - **Task table** (ui.rs task rows): for `TaskState::InProgress`/`InReview`
    prefix the state cell with the current frame.
  - **Exchange header** (`render_exchange_pane`, ui.rs:719): show the frame in
    the title while the selected task has an incomplete (streaming) `Response`
    segment.

The 250 ms tick (event.rs:69) gives ~4 fps — smooth enough for a throbber and
already the redraw cadence.

## Test strategy

- `append_chunk` segmentation: `exchange_log_segments_response_around_thought_and_tool`
  feeds `chunk · thought · tool · chunk` and asserts the entry order is
  `Response, Thought, Tool, Response` (two response entries).
- Markdown render: `exchange_render_markdown_headings_lists_code_table` asserts
  styled spans for a heading, a list item, a fenced block, and a table row, with
  no literal `#`/`*`/backticks leaking as plain text where they should be styled.
- ANSI render: extend/replace `exchange_render_styles_ansi_and_diff…` to assert
  256/RGB + underline survive and no literal escape bytes remain.
- Path compaction: `tool_title_is_repo_relative` asserts a worktree-absolute path
  renders as `src/…`.
- Selection: grep-style check that `EnableMouseCapture` is absent from `tui.rs`.
- Spinner: `spinner_frame_advances_on_tick` asserts the frame index changes after
  N ticks; a render test asserts a frame glyph appears for an in-progress task.

All changes keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
`cargo fmt --check` green.

## Interaction with prior plans

- **0003 / 0006**: pure extension of the pane those plans built — same event
  plumbing, same `ExchangeLog`/`exchange_entry_lines` seams.
- **0010** depends on this plan: the segmentation (0029) and the markup renderer
  (0030) define the exchange model that replay must reproduce, so a loaded run
  renders identically to a live one.

## Future work (not in this plan)

- Collapsible thoughts / pane filtering.
- Mouse scroll (re-enabling capture behind a modifier so selection survives).
- Syntax highlighting inside fenced code blocks.
