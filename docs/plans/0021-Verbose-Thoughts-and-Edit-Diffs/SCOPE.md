# Scope — Plan 0021

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

The content pane shows the conversation and a one-line header per tool call, but
it does not show enough *process* detail to follow what an agent is actually
doing. Two gaps in particular:

1. **Edit/tool content is never captured.** The exchange log models a tool entry
   as `ExchangeContent::Tool { id, title, kind, status, content: String }`
   (`app.rs`), and the render arm already knows how to draw `content.lines()`
   through a diff-aware styler (`ui.rs` `diff_overlaid_content_line`). But the
   `content` field is **always empty**: the ACP → exchange pipeline drops the
   tool's content blocks. `AcpResponseChunk::ToolCall`/`ToolCallUpdate`
   (`client.rs`) carry only `id`/`title`/`kind`/`status` — their doc comment says
   *"richer per-tool payloads (raw input, content, …) are intentionally not
   carried here"* — even though the parsed `ToolCall.extra` map (`protocol.rs`)
   **does** retain the `content`/`rawInput`/`locations` keys. So the answer to
   *"what was added/updated?"* is sitting in `extra` and being thrown away before
   it reaches `ExchangeContent::Tool.content`.

2. **No way to expand detail on demand.** CLI agents offer a verbose/expand mode
   (e.g. `ctrl+o`) that reveals reasoning and edit diffs. Makina renders thoughts
   (`ui.rs` `ExchangeContent::Thought` arm) and the tool header unconditionally,
   but there is no toggle to switch between a **compact** view (response + concise
   tool/thought headers) and a **verbose** view (full thoughts + the captured
   edit/tool content), and the status bar advertises no such key.

This plan **plumbs the captured tool/edit content through to the exchange log**
and adds a **verbose toggle** so the user can reveal model thoughts and exactly
what each tool added/updated.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0065–0066):

- **0065 — Capture tool/edit content from ACP.** Extract the tool's content (for
  file-edit tools: the added/updated diff lines; for others: `rawInput`/output
  text) from the parsed `ToolCall.extra` map in `makina-acp`, carry it on
  `AcpResponseChunk` → `ResponseEvent::ToolCall`/`ToolCallUpdate`, through a new
  content field on the `api::ExchangeEvent` tool variants
  (`developer.rs` mapping), and store it into `ExchangeContent::Tool.content` via
  `start_tool`/`update_tool` (`app.rs`).
- **0066 — Verbose toggle + render.** Add `App.verbose_mode: bool` and a key
  (`Ctrl+O`) to toggle it (`event.rs`), advertised in the status bar. In the
  exchange render (`ui.rs` `exchange_entry_lines`): **compact** mode shows the
  response plus concise tool/thought headers; **verbose** mode additionally
  renders full thoughts and the captured tool/edit content (via the existing
  `diff_overlaid_content_line`), so the user sees exactly what was added/updated.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| `ExchangeContent::Tool.content` is always empty; ACP `content`/`rawInput`/`locations` dropped before the log | `0065` |
| No verbose/expand toggle; thoughts + edit diffs cannot be revealed on demand; status bar advertises no key | `0066` |

## Locked decisions

- **Capture is additive and lossless-tolerant.** The new content field is
  `Option<String>` end-to-end; a tool call with no extractable content leaves
  `ExchangeContent::Tool.content` empty (its current behaviour). No panic, no
  change for tools that carry nothing.
- **Extract from `extra`, do not re-model the ACP schema.** `ToolCall.extra`
  already retains `content`/`rawInput`/`locations` verbatim (`protocol.rs`). 0065
  reads those keys out of the `serde_json::Value` map rather than adding typed
  `content`-block structs — the schema is open and agent-specific, and the goal is
  a human-readable string for the diff renderer, not a faithful typed mirror.
- **Edit content first, raw input as fallback.** For file-edit tools the value of
  interest is the diff / added+updated lines (the `content` blocks' text); for
  other tools fall back to a compact `rawInput` rendering. A single helper picks
  the best available text so the render arm stays unchanged.
- **Reuse the existing diff styler.** Captured content renders through the
  already-present `diff_overlaid_content_line` (`ui.rs`) — no new diff-styling
  code, no new badge vocabulary.
- **Verbose is a pure view toggle.** `verbose_mode` lives entirely on `App`; it
  changes *which lines render*, never what is captured or stored. Default is
  **compact** (off) so existing screenshots/tests of the compact view hold.
- **`Ctrl+O` for the toggle.** It does not collide with any normal-mode binding
  (`o`/`O` open the file browser; `Ctrl+O` is unbound — `event.rs`) and matches
  the CLI-agent convention the user referenced.

## Out of scope

- Persisting `verbose_mode` across restarts.
- Capturing tool content for the reviewer turn differently from the developer
  turn (the same `ExchangeEvent` path serves both).
- A typed mirror of the ACP tool-call `content` block schema (image/audio/resource
  blocks); only text is extracted, everything else is ignored as today.
- Syntax highlighting beyond the existing diff-line overlay.
- Any change to how exchanges are persisted/replayed (plan 0010) beyond the new
  `content` already flowing through `apply_exchange_event`.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
