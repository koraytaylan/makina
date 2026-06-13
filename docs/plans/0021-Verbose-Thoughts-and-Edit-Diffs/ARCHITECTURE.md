# Architecture — Plan 0021

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). This plan touches `makina-acp` (ACP → chunk mapping),
> `makina-core` (`ResponseEvent`, `api::ExchangeEvent`, the developer drain
> loop), and the `makina` TUI (`app.rs` log + `App`, `ui.rs` render, `event.rs`
> keymap).

## Current shape (what exists)

- **ACP parse → `extra`** (`crates/makina-acp/src/protocol.rs`): `SessionUpdate`
  (`enum`) has `ToolCall(ToolCall)` and `ToolCallUpdate(ToolCallUpdate)` arms
  (`ToolCallUpdate` is a `pub type … = ToolCall`). `struct ToolCall` names
  `tool_call_id`/`status`/`title`/`kind` and **flattens everything else** into
  `pub extra: std::collections::HashMap<String, serde_json::Value>`. A unit test
  (`session_update_parses_tool_call_update_and_preserves_extra`) proves
  `extra` retains `content`, `rawInput`, and unknown keys; the permission-flow
  test proves `content` and `locations` survive too. The ACP `content` block
  shape observed on the wire is
  `[{ "type": "content", "content": { "type": "text", "text": "…" } }]`.
- **Classify → `AcpResponseChunk`** (`crates/makina-acp/src/client.rs`):
  `fn classify_update(update: SessionUpdate) -> Option<AcpResponseChunk>` maps
  `SessionUpdate::ToolCall`/`ToolCallUpdate` into the `AcpResponseChunk::ToolCall`
  /`ToolCallUpdate` variants. Those variants (the `enum AcpResponseChunk`) carry
  only `id`/`title`/`kind`/`status` — the doc comment states richer payloads are
  *intentionally not carried here*. **This is where `extra` is dropped today.**
- **Bridge → `ResponseEvent`** (`crates/makina-acp/src/backend.rs`, the
  `while let Some(item) = stream.next()` loop): maps `AcpResponseChunk::ToolCall`
  /`ToolCallUpdate` 1:1 to `makina_core::backend::ResponseEvent::ToolCall`
  /`ToolCallUpdate`.
- **`ResponseEvent`** (`crates/makina-core/src/backend.rs`, `enum ResponseEvent`):
  `ToolCall { id, title, kind, status }` and `ToolCallUpdate { id, status, title }`.
- **Drain loop → `api::ExchangeEvent`** (`crates/makina-core/src/actors/
  developer.rs`, the response-stream `match` over `Some(Ok(ResponseEvent::…))`):
  `ResponseEvent::ToolCall`/`ToolCallUpdate` are wrapped into
  `api::ExchangeEvent::ToolCall`/`ToolCallUpdate` and pushed through `msg.sink`.
- **`api::ExchangeEvent`** (`crates/makina-core/src/api.rs`, `enum ExchangeEvent`):
  `ToolCall { id, title, kind, status }`, `ToolCallUpdate { id, status, title }`.
- **Exchange log** (`crates/makina/src/app.rs`): `enum ExchangeContent` has
  `Tool { id, title, kind, status, content: String }`; the `content` doc comment
  notes *"the live event path carries no content text yet, so this stays empty
  there."* `fn apply_exchange_event` routes `ExchangeEvent::ToolCall` →
  `ExchangeLog::start_tool` and `ToolCallUpdate` → `ExchangeLog::update_tool`;
  both leave `content` as `String::new()`.
- **Render** (`crates/makina/src/ui.rs`): `fn exchange_entry_lines(entry, app)`
  has a `ExchangeContent::Thought` arm (`💭 … thought` header + dimmed markdown)
  and a `ExchangeContent::Tool` arm (`⚙ {title} [{status}]` header, then
  `for text_line in content.lines() { lines.push(diff_overlaid_content_line(
  text_line)); }`). `fn diff_overlaid_content_line(text_line: &str) -> Line`
  already styles a diff line. The caller is `render_exchange_pane`'s
  `for entry in &log.entries { lines.extend(exchange_entry_lines(entry, app)); }`.
- **Keymap / status bar**: `crates/makina/src/event.rs` normal keymap binds
  `o`/`O` → `OpenBrowser`, `v`/`V` → `CycleDependencyView`
  (the arm is `KeyCode::Char('v') | KeyCode::Char('V')`), etc.; `Ctrl+O` is unbound.
  `crates/makina/src/ui.rs` status-bar `Paragraph` lists
  `" [o] open  [s/p/c] start/pause/cancel  [Tab] panel  [v] view  [L] log  [?] doctor  "`.
  `crates/makina/src/app.rs` `enum AppEvent` + `fn update(&mut self, event:
  AppEvent) -> bool` handle the toggle-style events (`ToggleErrorPane`,
  `CycleDependencyView`).

## 0065 — Capture tool/edit content from ACP

The content already lives in `ToolCall.extra`; the job is to pull it out once and
thread an `Option<String>` through every hop to `ExchangeContent::Tool.content`.

### a) Extract a content string in `makina-acp`

In `crates/makina-acp/src/client.rs`, add a free helper that turns a parsed
`ToolCall`'s `extra` map into a best-effort human-readable string:

```rust
/// Best-effort extraction of a tool call's displayable detail from its open
/// `extra` map. Prefers the `content` blocks' text — including the **file-edit
/// diff block** that `gemini --acp` actually sends (the diff / added+updated
/// lines for edit tools); falls back to a compact `rawInput` rendering for
/// non-edit tools. Returns `None` when neither is present or yields no text.
fn tool_detail(extra: &std::collections::HashMap<String, serde_json::Value>) -> Option<String> {
    // `content`: array of content blocks. Handle three observed shapes:
    //   1. file-edit diff block — the real `gemini --acp` payload, see
    //      crates/makina-acp/src/protocol.rs:956 + crates/makina-acp/tests/real_cli.rs:
    //      `{ "type": "diff", "path", "oldText", "newText" }`. Surface `newText`
    //      (ideally as an old→new diff) so the edit's added/updated lines show.
    //   2. nested text — `{ "type": "content", "content": { "type": "text", "text": … } }`.
    //   3. bare text — `{ "type": "text", "text": … }` (also tolerate `{ "text": … }`).
    if let Some(serde_json::Value::Array(blocks)) = extra.get("content") {
        let mut out = String::new();
        for b in blocks {
            // if `b.type == "diff"`: render its `newText` (and ideally an
            //   `oldText`→`newText` diff); else dig `b.content.text`, else `b.text`
            /* … push text + '\n' … */
        }
        if !out.trim_end().is_empty() {
            return Some(out.trim_end().to_string());
        }
    }
    // Fallback: `rawInput` rendered compactly (one key:value per line, or the
    // raw JSON string when it is not an object).
    if let Some(raw) = extra.get("rawInput") {
        /* … return Some(compact_raw_input(raw)) when non-empty … */
    }
    None
}
```

Extend `enum AcpResponseChunk`'s `ToolCall` and `ToolCallUpdate` variants with
`detail: Option<String>` and populate them in `classify_update`:

```rust
SessionUpdate::ToolCall(tc) => Some(AcpResponseChunk::ToolCall {
    detail: tool_detail(&tc.extra),
    id: tc.tool_call_id,
    title: tc.title.unwrap_or_default(),
    kind: tc.kind,
    status: tc.status.unwrap_or_else(|| "pending".to_string()),
}),
SessionUpdate::ToolCallUpdate(u) => Some(AcpResponseChunk::ToolCallUpdate {
    detail: tool_detail(&u.extra),
    id: u.tool_call_id,
    status: u.status,
    title: u.title,
}),
```

Note `AcpResponseChunk` derives `PartialEq, Eq` — keep it; `Option<String>` is
`Eq`. Update the doc comment that claims richer payloads are "intentionally not
carried here".

### b) Carry `detail` on `ResponseEvent`

In `crates/makina-core/src/backend.rs`, add `detail: Option<String>` to
`ResponseEvent::ToolCall` and `ResponseEvent::ToolCallUpdate`. In
`crates/makina-acp/src/backend.rs`, the bridge `match` forwards it:

```rust
Ok(AcpResponseChunk::ToolCall { id, title, kind, status, detail })
    => Ok(ResponseEvent::ToolCall { id, title, kind, status, detail }),
Ok(AcpResponseChunk::ToolCallUpdate { id, status, title, detail })
    => Ok(ResponseEvent::ToolCallUpdate { id, status, title, detail }),
```

(The other `match`/`matches!` sites in `backend.rs` that pattern `ResponseEvent::
ToolCall { .. }` / `ToolCallUpdate { .. }` with `..` need no change.)

### c) Carry content on `api::ExchangeEvent`

In `crates/makina-core/src/api.rs`, add `content: Option<String>` to
`ExchangeEvent::ToolCall` and `ExchangeEvent::ToolCallUpdate`.

In `crates/makina-core/src/actors/developer.rs`, the drain loop forwards it:

```rust
Some(Ok(ResponseEvent::ToolCall { id, title, kind, status, detail })) => {
    (msg.sink)(api::Event::AgentExchange {
        run: msg.run, task: task_id.clone(), role: api::AgentRole::Developer,
        event: api::ExchangeEvent::ToolCall { id, title, kind, status, content: detail },
    });
}
Some(Ok(ResponseEvent::ToolCallUpdate { id, status, title, detail })) => {
    (msg.sink)(api::Event::AgentExchange {
        run: msg.run, task: task_id.clone(), role: api::AgentRole::Developer,
        event: api::ExchangeEvent::ToolCallUpdate { id, status, title, content: detail },
    });
}
```

### d) Store into `ExchangeContent::Tool.content`

In `crates/makina/src/app.rs`:

- Give `ExchangeLog::start_tool` a `content: Option<String>` parameter and use it
  for the new entry (`content: content.unwrap_or_default()`); when the entry
  already exists (the upsert branch), set its `content` only if the incoming
  `Some` is non-empty so a later content-less update never blanks a captured diff.
- Give `ExchangeLog::update_tool` a `content: Option<String>` parameter and, when
  `Some(non-empty)`, overwrite the matched entry's `content` (append-or-replace —
  replace is fine for the MVP; the agent re-sends the full block).
- In `fn apply_exchange_event`, pass `content.clone()` from the `ExchangeEvent::
  ToolCall`/`ToolCallUpdate` arms into `start_tool`/`update_tool`.

This is the single reducer used by both the live path and plan-0010 replay, so
persisted transcripts that carry the new `content` field render their diffs after
a restart with no extra work.

## 0066 — Verbose toggle + render

### a) `App.verbose_mode` + toggle event

In `crates/makina/src/app.rs`:

- Add `pub verbose_mode: bool` to `App`; initialise `false` in the `App::new`
  struct literal (`App::with_config` delegates to `Self::new(...)` and has no
  struct literal of its own, so it needs no change).
- Add `AppEvent::ToggleVerbose` and handle it in `App::update` exactly like
  `ToggleErrorPane`:

  ```rust
  AppEvent::ToggleVerbose => {
      self.verbose_mode = !self.verbose_mode;
      true
  }
  ```

In `crates/makina/src/event.rs`, in the **normal** keymap (the final `else`
branch), bind `Ctrl+O` before the plain-`o` arm so the modifier wins:

```rust
KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL)
    => AppEvent::ToggleVerbose,
// existing:
KeyCode::Char('o') | KeyCode::Char('O') => AppEvent::OpenBrowser,
```

(`Ctrl+C` is already special-cased above the `match`; `Ctrl+O` is otherwise
unbound, so no collision.)

### b) Advertise it in the status bar

In `crates/makina/src/ui.rs`, extend the status-bar hint string to advertise the
key and reflect current state, e.g. append `[^O] verbose:{on|off}` to the leading
hint span:

```rust
let verbose_state = if app.verbose_mode { "on" } else { "off" };
// "… [v] view  [^O] verbose:{verbose_state}  [L] log  [?] doctor  "
```

Keep the existing `[o] open` / `[v] view` / `[L] log` / `[?] doctor` hints.

### c) Branch the render on `verbose_mode`

In `crates/makina/src/ui.rs` `fn exchange_entry_lines(entry, app)` — it already
receives `app`, so it can read `app.verbose_mode`:

- **`ExchangeContent::Thought` arm.** In **compact** mode, render only the
  `💭 … thought` header (a concise one-line marker that reasoning happened); in
  **verbose** mode, additionally render the dimmed markdown body (the current
  behaviour). This keeps thoughts clearly distinguished from the answer in both
  modes while keeping the compact view tight.
- **`ExchangeContent::Tool` arm.** Always render the
  `⚙ {title} [{status}]` header. Render the captured `content.lines()` through
  `diff_overlaid_content_line` **only when `app.verbose_mode`** (and `content` is
  non-empty). In compact mode the body is suppressed; in verbose mode the user
  sees exactly what was added/updated.
- Leave the `Prompt`/`Response` arms unconditional — the answer is always shown.

Sketch:

```rust
ExchangeContent::Thought { text } => {
    lines.push(/* 💭 … thought header */);
    if app.verbose_mode {
        /* … existing dimmed markdown body, 2-space indented … */
    }
}
ExchangeContent::Tool { title, status, content, .. } => {
    lines.push(/* ⚙ {title} [{status}] header */);
    if app.verbose_mode {
        for text_line in content.lines() {
            lines.push(diff_overlaid_content_line(text_line));
        }
    }
}
```

## Testing notes

- **0065 mapping (acp):** feed `classify_update` a `SessionUpdate::ToolCallUpdate`
  whose `extra` carries the wire-shaped `content` array; assert the produced
  `AcpResponseChunk::ToolCallUpdate.detail` is `Some` and contains the inner text.
  A tool with empty/absent `content` and no `rawInput` yields `detail == None`.
  A dedicated `tool_detail_extracts_edit_diff` feeds a `SessionUpdate` whose
  `extra["content"]` is the file-edit diff-block array
  (`[{ "type": "diff", "path", "oldText", "newText" }]`) and asserts the extracted
  detail is `Some` and contains the `newText`.
- **0065 reducer (app):** drive `apply_exchange_event` with an
  `ExchangeEvent::ToolCall { content: Some("…added…"), .. }` followed by an update;
  assert the matching `ExchangeContent::Tool.content` holds the text. A
  `content: None` event leaves `content` empty and does not panic.
- **0066 render:** build a fixture log with a thought and a tool entry that has
  non-empty `content`; render to a `Buffer` with `verbose_mode = false` and assert
  the thought body / tool content lines are **absent** (headers present); flip
  `verbose_mode = true` and assert they are **present**. A second test asserts the
  status bar contains the `[^O] verbose` hint.
- `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` stay green at every task boundary.

## Interaction with prior plans

- Builds on plan 0016's sidebar tree (`App.focused_node()`, `tree_cursor`, the
  `Panel` enum) and plan 0017's retry, both *introduced by plan 0016/0017* and not
  yet merged: this plan adds the `Ctrl+O`/`AppEvent::ToggleVerbose` binding and the
  `verbose_mode` field alongside them without touching tree navigation.
- Reuses plan 0010's `apply_exchange_event` reducer so the new `content` persists
  and replays through the existing transcript path with no new persistence code.
