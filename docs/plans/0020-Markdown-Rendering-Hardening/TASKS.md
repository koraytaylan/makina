# Makina Plan 0020 — Markdown Rendering Hardening & Tests

Model responses currently read as raw continuous blobs. Markdown rendering
*already exists* (`crate::markup::render_markdown`, pulldown-cmark + `ansi_to_tui`,
applied to `Response` and `Thought` entries in `ui.rs`), so this plan **hardens**
it — thread the real pane width through (today the call sites pass a hardcoded
`80` and the `width` arg is unused), render code blocks / lists / links / rules
properly, keep mid-stream parsing panic-free — and adds the thorough,
locked-in test coverage the user asked for.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0063 — Harden the markdown renderer

### harden-markdown-rendering — Real width, code blocks, lists, links, wrapping

Make `render_markdown` honour its `width` argument and pass the real
exchange-pane inner width from the call sites instead of `80`; render fenced and
indented code blocks as preserved dim blocks; render ordered + unordered +
nested lists with markers and indent; render links (text + dim URL) and the
thematic break (`---`); keep tables/strikethrough working; and ensure a partial
mid-stream Markdown string renders without panic.

**Steps:**

1. In `crates/makina/src/markup.rs`, rename the parameter from `_width` to
   `width` in `pub fn render_markdown(text: &str, base: Style, width: u16)`. Add
   a private `fn wrap_words(text: &str, width: u16) -> Vec<String>` that greedily
   breaks `text` on spaces into chunks no wider than `width` columns; treat
   `width == 0` as "no wrap" (return the text as a single chunk). Apply it when
   flushing plain `Event::Text` content into lines so a long paragraph folds to
   `width` (the existing outer `Paragraph` `Wrap` in `render_exchange_pane`
   remains as a final fold).

2. **Code blocks.** Track an `in_code_block: bool` toggled by
   `Event::Start(Tag::CodeBlock(_))` (flush pending spans, set true) and
   `Event::End(TagEnd::CodeBlock)` (set false). While set, route `Event::Text`
   to emit **one `Line` per source line** (split on `'\n'`), each styled
   `base.add_modifier(Modifier::DIM)` with a two-space indent and **no
   word-wrap** — so a multi-line snippet keeps its line breaks. This covers both
   `CodeBlockKind::Fenced` and `CodeBlockKind::Indented`.

3. **Lists.** Maintain a `Vec<ListCtx>` (`struct ListCtx { ordered: bool, next:
   u64 }`). On `Event::Start(Tag::List(start))` push `{ ordered: start.is_some(),
   next: start.unwrap_or(1) }`; on `Event::End(TagEnd::List(_))` pop (guard the
   pop). On `Event::Start(Tag::Item)` prepend a marker span: `"- "` for bullets,
   `"{n}. "` for ordered (consume and increment `next`), preceded by
   `"  ".repeat(depth)` where `depth = list_stack.len().saturating_sub(1)`; flush
   the item line on `Event::End(TagEnd::Item)` (keep the existing flush there).

4. **Links.** On `Event::Start(Tag::Link { dest_url, .. })` stash
   `link_url = Some(dest_url.to_string())`; the link text arrives as ordinary
   `Event::Text`. On `Event::End(TagEnd::Link)`, if the captured URL adds
   information beyond the visible link text, append a dim ` ({url})` span. Remove
   links from the `_ => {}` catch-all’s silent drop.

5. **Thematic break.** Handle `Event::Rule` by flushing pending spans and pushing
   a dim rule `Line` of `'─'` repeated to `width` (or a short fixed run when
   `width == 0`).

6. **Mid-stream safety.** Ensure every state pop/take (`list_stack`, `link_url`,
   `in_code_block`) is `Option`-safe so a truncated document (unclosed fence,
   half-open `[link`, unterminated list) renders what it has; the trailing
   `if !spans.is_empty()` flush at the end of `render_markdown` still runs. No new
   `unwrap()` on parser state.

7. In `crates/makina/src/ui.rs`, change `fn exchange_entry_lines(entry:
   &ExchangeEntry, app: &App)` to take a `width: u16` and pass it into **both**
   `crate::markup::render_markdown(text, base_style, …)` calls (the `Response`
   and `Thought` arms) in place of the literal `80`. In `render_exchange_pane`,
   compute `let content_width = inner.width;` (from the existing `let inner =
   block.inner(area)`) and pass it at the `exchange_entry_lines(entry, app)` call
   in the `Some(log)` loop. Update the in-test call sites
   (`exchange_entry_lines(&entry, &app)`, the thought/tool ones, and the late
   tool one) to pass an explicit width.

8. Add tests in `markup.rs` (one per construct, plus wrapping):

   ```rust
   #[test]
   fn renders_heading_bold_italic_strikethrough_and_inline_code() { /* "# H\n\n**b** *i* ~~s~~ `c`" => H bold; b/i/s/c present; no literal "# " "**" "~~" "`" markers leak */ }
   #[test]
   fn renders_fenced_code_block_preserving_line_breaks() { /* "```\nfn main() {\n    body\n}\n```" => >=3 DIM lines, "fn main() {" and "}" on SEPARATE lines */ }
   #[test]
   fn renders_indented_code_block() { /* a 4-space-indented block => DIM lines, breaks preserved */ }
   #[test]
   fn renders_ordered_unordered_and_nested_lists() { /* "1. a\n2. b\n\n- x\n  - y" => "1. ", "2. ", "- " markers; nested "y" indented deeper than "x" */ }
   #[test]
   fn renders_blockquote() { /* "> quoted" => "quoted" present, DIM */ }
   #[test]
   fn renders_link_text_and_dim_url() { /* "[docs](https://x.y)" => "docs" present; "https://x.y" present and DIM */ }
   #[test]
   fn renders_thematic_break_as_rule() { /* "a\n\n---\n\nb" => a line that is all '─' */ }
   #[test]
   fn soft_and_hard_breaks_split_lines() { /* "l1\nl2" (soft) and "l1  \nl2" (hard) each yield 2 lines */ }
   #[test]
   fn wraps_long_paragraph_to_small_width() { /* one long word-rich paragraph at width 20 => every returned Line's char width <= 20 */ }
   ```

- **Depends on:** —
- **Done when:** the new tests pass and the existing `markup::tests`
  (`markup_renders_heading_bold_list_and_code`, `markup_renders_gfm_table`,
  `markup_renders_ansi_256_color`, `markup_renders_markdown_with_ansi`) still
  pass; `render_markdown` wraps to `width` (no longer ignores it); both `ui.rs`
  call sites pass the real `inner.width` instead of `80`; code blocks preserve
  line breaks as DIM lines; ordered/unordered/nested lists show markers and
  indent; links render text + dim URL; `---` renders a rule line;
  a partial mid-stream string does not panic; cargo test/clippy/fmt green.

---

## 0064 — Comprehensive markdown tests

### markdown-render-tests — Thorough, locked-in coverage

Add the dedicated, thorough test module the user explicitly asked for: a mixed
real-world document whose `Line`/`Span` structure and styles are asserted; a
streamed case parsed at increasing prefixes that never panics or regresses; an
ANSI-containing response that is stripped then rendered; and a wrapping case
proving the renderer respects the pane width.

**Steps:**

1. In `crates/makina/src/markup.rs`, add a `mixed_document_renders_all_constructs`
   test: build one document exercising a heading, a fenced code block, an ordered
   list, an unordered (with a nested) list, a GFM table, a link, bold/italic/
   strikethrough, inline code, a blockquote, and a `---` rule. Render with a
   sensible `width` and assert the concrete structure: heading text present and
   BOLD; the code block’s `fn`-line and its closing `}` on *separate* DIM lines;
   `1. ` / `- ` markers present with the nested item indented deeper; the link
   text present and its URL DIM; the table header and a cell present; a line that
   is all `─` for the rule; and **no** literal Markdown markers (`# `, `**`,
   ```` ``` ````, `~~`) in the visible text.

2. Add `streamed_prefixes_never_panic`: take that document and for each prefix
   (iterate `char_indices()` / `is_char_boundary`-guarded so no slice splits a
   codepoint) call `render_markdown(prefix, base, width)`; assert each call
   returns a `Vec<Line>` without panicking. This simulates the
   `ExchangeLog::append_chunk` accumulation that reparses growing prefixes.

3. Add `ansi_response_is_stripped_and_rendered`: a response mixing SGR codes with
   Markdown (e.g. a heading plus `\x1b[38;5;208m…\x1b[0m` body) renders with no
   `\x1b` byte and no literal `[38;5;208m` in any span, and both the heading and
   the de-ANSI’d word survive (extends the existing
   `markup_renders_markdown_with_ansi` expectation).

4. Add `wrapping_respects_pane_width`: render a long single-paragraph string at
   `width = 20` and assert every produced `Line`’s displayed width (sum of its
   spans’ `content` char counts) is `<= 20`, proving `render_markdown` itself
   wraps. Optionally add a sibling render-buffer test in `ui.rs` that drives
   `render_exchange_pane` with a narrow pane and asserts the real `inner.width`
   reached the renderer (no over-wide line in the buffer).

5. Test stubs:

   ```rust
   #[test]
   fn mixed_document_renders_all_constructs() { /* heading+code+lists+table+link+blockquote+rule => assert Line/Span structure & styles per step 1; no literal markers leak */ }
   #[test]
   fn streamed_prefixes_never_panic() { /* for every char-boundary prefix of the mixed doc: render_markdown returns Vec<Line> without panic */ }
   #[test]
   fn ansi_response_is_stripped_and_rendered() { /* markdown + SGR => no "\x1b"/"[38;5;208m" in any span; heading + body word survive */ }
   #[test]
   fn wrapping_respects_pane_width() { /* long paragraph at width 20 => every Line char-width <= 20 */ }
   ```

- **Depends on:** harden-markdown-rendering
- **Done when:** the four named tests pass; the mixed document’s rendered
  structure and styles are asserted (not just non-empty output); every prefix of
  a streamed response renders without panic; an ANSI-containing response is
  stripped and rendered with no escape bytes; the renderer wraps to a small pane
  width; the existing `markup::tests` remain green; cargo test/clippy/fmt green.

---

**End of plan 0020 TASKS.** When every "Done when" bullet is green, agent
responses render as structured Markdown — headings, preserved code blocks,
marked-up lists, links, and rules wrapped to the real pane width — instead of raw
continuous blobs, and a thorough test suite locks every construct (and the
mid-stream streaming case) in place.
