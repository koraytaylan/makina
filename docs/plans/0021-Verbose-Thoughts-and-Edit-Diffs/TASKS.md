# Makina Plan 0021 — Verbose Mode: Thoughts & Edit Diffs

Make the content pane reveal *process* detail. First **capture** the tool/edit
content the ACP layer already parses but currently drops (so
`ExchangeContent::Tool.content` stops being empty), then add a **verbose toggle**
(`Ctrl+O`) that reveals full model thoughts and the captured edit diffs on demand.

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

## 0065 — Capture tool/edit content from ACP

### capture-tool-edit-content — Plumb the added/updated content through

Thread the tool's content (the edit diff / added+updated lines for file-edit
tools; `rawInput` text otherwise) from the parsed `ToolCall.extra` map all the
way to `ExchangeContent::Tool.content`, as an `Option<String>` at every hop.

**Steps:**

1. **Extract (makina-acp).** In `crates/makina-acp/src/client.rs`, add a free
   `fn tool_detail(extra: &std::collections::HashMap<String, serde_json::Value>)
   -> Option<String>` that prefers the `content` blocks' text. Handle the
   **file-edit diff block** that `gemini --acp` actually sends —
   `{ "type": "diff", "path", "oldText", "newText" }` (see
   `crates/makina-acp/src/protocol.rs:956` and
   `crates/makina-acp/tests/real_cli.rs`) — surfacing its `newText` (ideally as
   an `oldText`→`newText` diff) so edit content shows; also handle the nested
   text shape `[{ "type": "content", "content": { "type": "text", "text": … } }]`
   and a bare `{ "type": "text", "text": … }`. Fall back to a compact
   `rawInput` rendering; return `None` when neither yields text. Add
   `detail: Option<String>` to the `ToolCall` and `ToolCallUpdate` variants of
   `enum AcpResponseChunk` and populate them from `tool_detail(&tc.extra)` /
   `tool_detail(&u.extra)` in `fn classify_update`. Update the
   `AcpResponseChunk` doc comment that claims richer payloads are "intentionally
   not carried here". (`AcpResponseChunk` keeps `#[derive(… PartialEq, Eq)]` —
   `Option<String>` is `Eq`.)

2. **Carry on `ResponseEvent`.** In `crates/makina-core/src/backend.rs`, add
   `detail: Option<String>` to `ResponseEvent::ToolCall` and
   `ResponseEvent::ToolCallUpdate`. In `crates/makina-acp/src/backend.rs`, the
   bridge `match` (`Ok(AcpResponseChunk::ToolCall { … })` / `ToolCallUpdate`)
   forwards `detail` into the matching `ResponseEvent` variant. The `..`-pattern
   `matches!` sites in that file need no change.

3. **Carry on `api::ExchangeEvent`.** In `crates/makina-core/src/api.rs`, add
   `content: Option<String>` to `ExchangeEvent::ToolCall` and
   `ExchangeEvent::ToolCallUpdate`. In `crates/makina-core/src/actors/
   developer.rs`, the response-stream drain loop maps
   `ResponseEvent::ToolCall { …, detail }` → `ExchangeEvent::ToolCall { …,
   content: detail }` and likewise for the update arm.

4. **Store (makina TUI).** In `crates/makina/src/app.rs`, add a
   `content: Option<String>` parameter to `ExchangeLog::start_tool` (new entry:
   `content.unwrap_or_default()`; existing entry: overwrite only on
   `Some(non-empty)`) and to `ExchangeLog::update_tool` (overwrite the matched
   entry's `content` on `Some(non-empty)`). In `fn apply_exchange_event`, pass
   `content.clone()` from the `ExchangeEvent::ToolCall`/`ToolCallUpdate` arms.
   Update existing `start_tool`/`update_tool` call sites/tests for the new param.

5. Add tests:

   ```rust
   /* makina-acp/src/client.rs (or protocol/client test module):
      tool_detail_extracts_content_blocks — a SessionUpdate::ToolCallUpdate whose
      `extra["content"]` is the wire-shaped array maps through classify_update to
      AcpResponseChunk::ToolCallUpdate { detail: Some(s), .. } where s contains the
      inner text; an update with no content and no rawInput => detail == None.
      tool_detail_extracts_edit_diff — a SessionUpdate whose `extra["content"]` is
      the file-edit diff-block array
      `[{ "type": "diff", "path", "oldText", "newText" }]` yields a detail that is
      Some and contains the `newText`. */
   /* makina/src/app.rs:
      tool_content_populated_from_event — apply_exchange_event with
      ExchangeEvent::ToolCall { content: Some("+added line"), .. } then a matching
      ToolCallUpdate yields an ExchangeContent::Tool whose content == "+added line";
      content_none_leaves_tool_content_empty — a content:None event leaves
      ExchangeContent::Tool.content == "" and does not panic. */
   ```

- **Depends on:** —
- **Done when:** `tool_detail` extracts edit-content (and `rawInput` fallback)
  from `extra`; `detail`/`content` flow through `AcpResponseChunk` →
  `ResponseEvent` → `api::ExchangeEvent` → `ExchangeContent::Tool.content`; a tool
  call carrying content populates `ExchangeContent::Tool.content`, a content-less
  one stays empty with no panic; the new tests pass; cargo test/clippy/fmt green.

---

## 0066 — Verbose toggle + render

### verbose-mode-toggle-and-render — Show thoughts + edit diffs on demand

Add an `App.verbose_mode` toggle (`Ctrl+O`), advertise it in the status bar, and
branch the exchange render so verbose mode reveals full thoughts and the captured
tool/edit content while compact mode stays tight.

**Steps:**

1. In `crates/makina/src/app.rs`, add `pub verbose_mode: bool` to `App`,
   initialised `false` in the `App::new` struct literal (`App::with_config`
   delegates to `Self::new(...)` and has no struct literal of its own).
   Add `AppEvent::ToggleVerbose` and handle it in `App::update` like
   `ToggleErrorPane`: `self.verbose_mode = !self.verbose_mode; true`.

2. In `crates/makina/src/event.rs`, in the **normal** keymap `match key.code`,
   add `KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) =>
   AppEvent::ToggleVerbose` **before** the existing
   `KeyCode::Char('o') | KeyCode::Char('O') => AppEvent::OpenBrowser` arm so the
   modifier guard wins. Leave all other bindings unchanged.

3. In `crates/makina/src/ui.rs`, extend the status-bar hint `Paragraph` to
   advertise the key and current state — append `[^O] verbose:{on|off}` to the
   leading hint span (derive `on`/`off` from `app.verbose_mode`). Keep the
   existing `[o] open` / `[v] view` / `[L] log` / `[?] doctor` hints.

4. In `crates/makina/src/ui.rs` `fn exchange_entry_lines(entry, app)` (it already
   takes `app`): in the `ExchangeContent::Thought` arm, always render the
   `💭 … thought` header but render the dimmed markdown body **only when
   `app.verbose_mode`**; in the `ExchangeContent::Tool` arm, always render the
   `⚙ {title} [{status}]` header but render `content.lines()` through
   `diff_overlaid_content_line` **only when `app.verbose_mode`**. Leave the
   `Prompt`/`Response` arms unconditional. Thoughts stay clearly distinguished
   (the `💭` header) in both modes.

5. Add tests:

   ```rust
   /* makina/src/ui.rs:
      verbose_off_hides_thought_and_tool_content — fixture log with a Thought and a
      Tool whose content is non-empty; render with verbose_mode=false; assert the
      💭/⚙ headers are present but the thought body text and the tool content lines
      are ABSENT from the buffer.
      verbose_on_shows_thought_and_tool_content — same fixture, verbose_mode=true;
      assert the thought body and the tool content lines ARE present.
      status_bar_advertises_verbose_key — render; assert the screen contains the
      "[^O] verbose" hint. */
   /* makina/src/app.rs (optional):
      toggle_verbose_flips_flag — App::update(AppEvent::ToggleVerbose) flips
      verbose_mode true↔false. */
   ```

- **Depends on:** capture-tool-edit-content
- **Done when:** `Ctrl+O` toggles `App.verbose_mode` (no collision with the
  `o`/`O` browser key); the status bar advertises `[^O] verbose`; in compact mode
  the exchange pane shows response + concise tool/thought headers, and in verbose
  mode it additionally renders full thoughts and the captured tool/edit content via
  `diff_overlaid_content_line`; the new tests pass; cargo test/clippy/fmt green.

---

**End of plan 0021 TASKS.** When every "Done when" bullet is green, the content
pane stops dropping the edit/tool content it already parses — `ExchangeContent::
Tool.content` carries the added/updated lines — and `Ctrl+O` flips between a tight
compact view and a verbose view that reveals exactly what the agent thought and
what each tool changed.
