# Architecture — Plan 0020 (deltas)

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). This plan touches only the `makina` (TUI) crate —
> `crates/makina/src/markup.rs` and `crates/makina/src/ui.rs`.

## Current shape (what exists)

- **Renderer** (`crates/makina/src/markup.rs`): `pub fn render_markdown(text:
  &str, base: Style, _width: u16) -> Vec<Line<'static>>` parses with
  `pulldown_cmark::Parser::new_ext` under `Options::ENABLE_TABLES |
  ENABLE_STRIKETHROUGH`. It pre-strips ANSI via `strip_ansi` (which delegates to
  `render_ansi`) when the text contains `\x1b`. The event loop handles
  `Tag::Heading`/`Strong`/`Emphasis`/`CodeBlock`, `Event::Code`, `Event::Text`,
  `HardBreak`/`SoftBreak`, blockquotes, and tables; `finalize_line` flushes the
  current `Vec<Span>` into a `Line`. **`_width` is ignored** (note the leading
  underscore), `Tag::Item` / `Tag::List` arms are no-ops, and links / images /
  rules fall through the trailing `_ => {}` arm.
- **Call sites** (`crates/makina/src/ui.rs`, `exchange_entry_lines`): the
  `ExchangeContent::Response { text, complete }` arm calls
  `crate::markup::render_markdown(text, base_style, 80)` (the **hardcoded 80**)
  and then appends a streaming cursor `▌` when `!complete`. The
  `ExchangeContent::Thought { text }` arm calls the same with a `DarkGray` base
  and then indents every returned line by two spaces.
- **Pane width source** (`ui.rs`, `render_exchange_pane`): the bordered
  `Block::default().borders(Borders::TOP)` yields `let inner = block.inner(area)`;
  `inner.width`/`inner.height` are the real content dimensions. The collected
  `lines` are drawn via `Paragraph::new(lines).wrap(Wrap { trim: false })
  .scroll((scroll_offset, 0))` — so a *final* soft-wrap already happens at the
  pane width, but it is structure-blind (it does not re-indent wrapped code/list
  continuation lines).
- **Streaming accumulation** (`crates/makina/src/app.rs`): `ExchangeLog::
  append_chunk(role, chunk)` pushes onto the last incomplete
  `ExchangeContent::Response { text, complete }`’s `text` (`text.push_str`),
  flipping `complete` on `TurnComplete`. Every render reparses the whole grown
  `text` — so `render_markdown` is repeatedly handed *prefixes* of a document.
- **pulldown-cmark 0.12.2 API** (confirmed): `Tag::CodeBlock(CodeBlockKind)` with
  `CodeBlockKind::{Fenced(CowStr), Indented}`; `Tag::List(Option<u64>)` (ordered
  ⇒ `Some(first_number)`, bullet ⇒ `None`); `Tag::Item` / `TagEnd::Item`;
  `Tag::Link { dest_url, title, .. }` / `TagEnd::Link`; `Event::Rule`;
  `Event::HardBreak` / `Event::SoftBreak`.

## 0063 — Harden the markdown renderer

Edits in `crates/makina/src/markup.rs` and the two call sites in
`crates/makina/src/ui.rs`.

### Thread the real width

- In `ui.rs`, change the helper signature to
  `fn exchange_entry_lines(entry: &ExchangeEntry, app: &App, width: u16) ->
  Vec<Line<'static>>` and pass `width` into both `render_markdown` calls instead
  of `80`. In `render_exchange_pane`, compute the width from the existing `inner`
  and pass it down:

  ```rust
  // render_exchange_pane: `inner` already = block.inner(area)
  let content_width = inner.width;
  for entry in &log.entries {
      lines.extend(exchange_entry_lines(entry, app, content_width));
  }
  ```

  Update the three test call sites (`exchange_entry_lines(&entry, &app)` etc.) to
  pass an explicit width. The `Thought` arm threads the same `width` (its two-
  space indent is applied *after* `render_markdown` as today, so wrapping to
  `width` then indenting is acceptable — note it in the task).

- In `markup.rs`, drop the underscore: `pub fn render_markdown(text: &str, base:
  Style, width: u16) -> Vec<Line<'static>>`. Add a small word-wrap helper that
  splits a styled run of text to fit `width` (respecting a per-block indent), and
  apply it where `Event::Text` content is flushed into lines. Treat `width == 0`
  as "no wrap" (defensive: tiny/closed panes) so we never divide by or loop on a
  zero budget.

  ```rust
  /// Greedily wrap `text` into chunks no wider than `width` (in columns),
  /// breaking on spaces; `width == 0` disables wrapping (returns one chunk).
  fn wrap_words(text: &str, width: u16) -> Vec<String> { /* … */ }
  ```

### Code blocks as preserved blocks

- Track an `in_code_block: bool` (or an enum block context) toggled by
  `Event::Start(Tag::CodeBlock(_))` / `Event::End(TagEnd::CodeBlock)`. While set,
  route `Event::Text` to a buffer split on `'\n'` so **each source line becomes
  its own `Line`**, styled dim (and not word-wrapped — code keeps its columns),
  with a small left indent so it reads as a block:

  ```rust
  Event::Start(Tag::CodeBlock(_)) => { in_code_block = true; flush(&mut spans, &mut out); }
  Event::Text(t) if in_code_block => {
      for raw in t.split_inclusive('\n') {
          let line = raw.trim_end_matches('\n').to_string();
          out.push(Line::from(Span::styled(
              format!("  {line}"),
              base.add_modifier(Modifier::DIM),
          )));
      }
  }
  Event::End(TagEnd::CodeBlock) => { in_code_block = false; }
  ```

  This handles both `CodeBlockKind::Fenced` and `CodeBlockKind::Indented`
  identically (a four-space-indented block parses as `Indented`).

### Lists with markers and indent

- Maintain a small stack of list contexts so nesting indents correctly. On
  `Event::Start(Tag::List(start))` push `{ ordered: start.is_some(), next:
  start.unwrap_or(1) }`; on `Event::End(TagEnd::List(_))` pop. On
  `Event::Start(Tag::Item)`, emit the marker prefix for the current depth —
  `"- "` for bullets, `"{n}. "` for ordered (incrementing the context’s
  counter) — preceded by `depth * 2` spaces of indent; flush on
  `Event::End(TagEnd::Item)`:

  ```rust
  struct ListCtx { ordered: bool, next: u64 }
  // on Start(Item):
  let depth = list_stack.len().saturating_sub(1);
  let marker = match list_stack.last_mut() {
      Some(c) if c.ordered => { let n = c.next; c.next += 1; format!("{n}. ") }
      _ => "- ".to_string(),
  };
  spans.push(Span::styled(format!("{}{marker}", "  ".repeat(depth)), base));
  ```

  Nested lists work because the parser emits a fresh `Start(List)` inside the
  parent `Item` before that item ends.

### Links and thematic breaks

- Replace the `_ => {}` catch-all’s handling of links: on
  `Event::Start(Tag::Link { dest_url, .. })` stash `dest_url`; the link’s text
  arrives as ordinary `Event::Text` (rendered with the active style); on
  `Event::End(TagEnd::Link)`, if the captured URL differs from the rendered link
  text, append a dim ` (url)` span:

  ```rust
  Event::Start(Tag::Link { dest_url, .. }) => { link_url = Some(dest_url.to_string()); }
  Event::End(TagEnd::Link) => {
      if let Some(url) = link_url.take() {
          // append dim "(url)" when it adds information beyond the link text
          spans.push(Span::styled(format!(" ({url})"), base.add_modifier(Modifier::DIM)));
      }
  }
  ```

- Render `Event::Rule` as a horizontal rule line built from `'─'` repeated to
  `width` (or a fixed short run when `width == 0`), styled dim, flushing any
  pending spans first.

### Partial / mid-stream safety

- The whole loop already operates on whatever prefix `text` is; the additions
  above must not assume balanced tags. Guard every "pop"/"take" (list stack,
  `link_url`, `in_code_block`) so a truncated document — an unclosed code fence,
  a half-open `[link`, a list with no closing — simply renders what it has and
  the trailing-spans flush at the end of `render_markdown` still runs. No
  `unwrap()` on parser state; `list_stack.last_mut()`/`pop()` are `Option`-safe.

## 0064 — Comprehensive markdown tests

Edits add a dedicated test module; no production behaviour changes beyond what
0063 introduces. Tests live in `markup.rs` (unit-level, asserting `Line`/`Span`
structure and `Style`) and may add a render-buffer test in `ui.rs` for the
width-threading.

- **Mixed real-world document.** Feed a document that exercises headings, a
  fenced code block, an ordered and an unordered list (one nested), a GFM table,
  a link, bold/italic/strikethrough, inline code, a blockquote, and a `---`
  rule. Assert: the heading text is present and **bold**; the code block’s lines
  are preserved (`fn main` and its `}` are on *separate* lines, both `DIM`); list
  markers (`- `, `1. `) appear with the right indent; the link text is present
  and its URL appears dim; the table’s header and a cell are present; the rule
  line is a run of `─`; no literal Markdown markers (`# `, `**`, ```` ``` ````)
  leak into the visible text.
- **Streamed prefixes never panic.** Take one such document, and for every
  prefix length `0..=doc.len()` (byte-boundary-safe) call `render_markdown(prefix,
  base, width)` and assert it returns without panicking and yields a `Vec<Line>`
  (possibly empty). This simulates `append_chunk` accumulation and pins the
  mid-stream guarantee.
- **ANSI response is stripped then rendered.** A response containing SGR codes
  mixed with Markdown renders with no `\x1b` bytes in any span (reuses the
  existing `strip_ansi` path; assert both the de-ANSI’d word and a heading
  survive, as `markup_renders_markdown_with_ansi` does today, but now also that
  width-wrapping applied).
- **Wrapping respects pane width.** Render a long single-paragraph string at a
  small `width` (e.g. 20) and assert **every** produced `Line`’s displayed width
  is `<= width` (sum of span `content` char widths), proving `render_markdown`
  itself wraps rather than relying on the outer `Paragraph`.

## Test strategy

- 0063’s logic is pure and synchronous — assert directly on the returned
  `Vec<Line<'static>>` (text content via `line.spans.iter().map(|s|
  s.content.as_ref())`, styles via `span.style`), as the existing
  `markup::tests` already do. No terminal/`Buffer` needed except the optional
  `ui.rs` width-threading render test.
- For the streamed-prefix test, iterate prefixes on `char_indices()` (or guard
  with `text.is_char_boundary(i)`) so we never slice mid-codepoint.
- Width assertions use character count of each span’s `content`; with
  `Wrap` removed from the equation inside `render_markdown`, the returned lines
  are already folded.

`cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt
--check` stay green.

## Interaction with prior plans

- Independent of the unmerged plans 0016 (sidebar tree: `App::focused_node()`,
  `TreeNode`, `tree_cursor`, `tree_move`, `tree_toggle_expand`, `collapsed_runs`;
  note `enum Panel { Sidebar, Main }` already exists at `app.rs:435`, pre-dating
  0016, and this plan does not touch it) and 0017 (task retry) — this plan
  touches only the exchange-pane
  render path and the `markup` module, neither of which 0016/0017 modify. If
  0016 has already removed the main-panel task table when this plan runs, the
  exchange pane is simply taller; the width-threading reads `inner.width`
  regardless, so there is no ordering constraint.
