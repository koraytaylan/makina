# Scope — Plan 0020

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Model responses read as **raw continuous blobs**. The user filed feature #3 on
exactly this: the exchange pane shows an agent's answer with little visual
structure — headings, code, and lists run together — and there are almost no
tests pinning the rendering down.

The important nuance: **Markdown rendering already exists.** `crate::markup::
render_markdown` (pulldown-cmark + `ansi_to_tui`) is already applied to both the
`Response` and `Thought` exchange entries in `exchange_entry_lines` (`ui.rs`).
So this is **not greenfield** — it is a *hardening* pass on an existing renderer
plus the thorough test coverage the user asked for. Concretely, three gaps:

1. **Width is hardcoded.** `render_markdown(text, base, width)` takes a `width`
   argument, but the two call sites in `exchange_entry_lines` pass a literal
   `80` (`ui.rs`), ignoring the real exchange-pane width. The `width` parameter
   is even named `_width` inside `render_markdown` — it is **completely
   unused**. (Soft-wrapping today happens only at the `Paragraph::new(lines)
   .wrap(Wrap { trim: false })` layer in `render_exchange_pane`, which wraps to
   the pane but knows nothing about Markdown block structure such as code-block
   or list indentation.)
2. **Several constructs render thin or wrong.** Fenced/indented **code blocks**
   are not preserved as a styled block (the `Tag::CodeBlock` arm only nudges the
   style and the inner text loses its line breaks); **ordered/nested lists** emit
   no marker or indent (`Tag::Item` is a no-op); **links** fall into the `_ =>`
   catch-all so the link text renders but the URL is dropped; the **thematic break**
   (`---`, `Event::Rule`) is silently swallowed.
3. **Mid-stream Markdown is reparsed every frame.** A streamed response is
   accumulated chunk-by-chunk into one `String` (`ExchangeLog::append_chunk` →
   `ExchangeContent::Response { text, complete }`) and `render_markdown` is
   re-run on the growing prefix on every render. A partial document (an unclosed
   code fence, a half-written table row) must **never panic or mangle**, and we
   have no test proving it.

This plan threads the real pane width into the renderer, makes code blocks,
lists, links, and rules render as recognisable structure, and locks the whole
thing down with a thorough, per-construct test suite.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0063–0064):

- **0063 — Harden the markdown renderer.** In `markup.rs`, actually *use* the
  `width` argument (wrap text to it), and at the `ui.rs` call sites pass the real
  exchange-pane inner width instead of `80` (threaded through
  `render_exchange_pane` → `exchange_entry_lines` → `render_markdown`). Render
  fenced + indented **code blocks** as a preserved, dim block (line breaks kept,
  monospace feel); **ordered + unordered + nested lists** with correct
  markers/indent; **links** (render the link text, optionally dim the URL); the
  **thematic break** as a rule line. Keep tables/strikethrough working. Ensure a
  partial mid-stream string renders without panic.
- **0064 — Comprehensive markdown tests.** The thorough, locked-in coverage the
  user asked for: a mixed real-world document (headings + code + lists + table +
  links) asserting the resulting `Line`/`Span` structure and styles; a streamed
  case parsing the same response at *increasing prefixes* that never
  panics/regresses; an ANSI-containing response stripped then rendered; and a
  wrapping case proving the renderer respects a small pane width.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| `width` arg is ignored (`_width`); call sites pass hardcoded `80` | `0063` |
| Code blocks not preserved as a styled block; line breaks lost | `0063` |
| Ordered/nested lists emit no marker or indent | `0063` |
| Link URL dropped (text renders, but the `_ =>` arm drops the URL); thematic break swallowed | `0063` |
| Mid-stream partial Markdown is reparsed each frame, untested | `0063`, `0064` |
| No thorough per-construct / real-document / streamed test suite | `0064` |

## Locked decisions

- **Harden, don't replace.** Keep `pulldown_cmark::Parser::new_ext` with
  `Options::ENABLE_TABLES | ENABLE_STRIKETHROUGH` and the existing event loop and
  ANSI pre-strip (`strip_ansi`) in `render_markdown`. We extend the match arms;
  we do not swap the parser or pull in a new dependency.
- **Width comes from the pane, computed once.** `render_exchange_pane` already
  derives `inner` from the bordered block; its `inner.width` is the single source
  of truth. Thread it through `exchange_entry_lines` and into `render_markdown`.
  `render_markdown` wraps **content text** to that width (accounting for list /
  code-block indent) so blocks no longer run off as one logical line; the
  outer `Paragraph` `Wrap` stays as a belt-and-braces final fold.
- **Code blocks are preserved blocks.** Inside a `Tag::CodeBlock(_)` (both
  `CodeBlockKind::Fenced` and `Indented`), emit one `Line` per source line of the
  block text with a dim style and a small indent, so a multi-line snippet keeps
  its line breaks and reads as code — not collapsed into one paragraph.
- **Links render their text; URL is a quiet suffix.** A `Tag::Link { dest_url,
  .. }` renders its child text normally; the URL is appended dim in parentheses
  only when it differs from the visible text. No clickable behaviour.
- **No data-model or wire change.** `ExchangeContent::Response { text, complete }`
  and the chunk-accumulation path (`append_chunk`) are unchanged; this is a
  pure render-side plan in the `makina` (TUI) crate.
- **Tests are the deliverable, not a side effect.** 0064 is a dedicated,
  thorough module the user explicitly asked for; it asserts concrete
  `Line`/`Span` structure and styles, not just "non-empty output".

## Out of scope

- Syntax highlighting of code blocks by language (the fence info string is
  read but not used to colourise tokens).
- Clickable / openable links or any mouse-hit-testing of rendered URLs.
- Caching the parsed Markdown across frames (we keep reparsing the accumulated
  `String`; we only guarantee it is panic-free and correct).
- Image rendering (`Tag::Image`) beyond showing its alt text.
- Re-flowing the surrounding panes, the 30/70 split, or the prompt/tool arms of
  `exchange_entry_lines` (the Tool arm keeps its existing ANSI + diff overlay).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
