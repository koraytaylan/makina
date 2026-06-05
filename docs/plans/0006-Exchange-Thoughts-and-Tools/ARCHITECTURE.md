# Architecture — Plan 0006 (deltas)

> Deltas required to surface `agent_thought_chunk`, `tool_call`, and `tool_call_update` through the existing Exchange pane machinery. Grounded against `develop` immediately after the 0007 renumbering of the prior ingestion-hardening plan (and the state of the investigation session that produced the request).

The overall shape is "extend the side-channel path that was deliberately truncated in 0003 for the 'prompt-answer-stream' feature."

## Current (post-0003) data flow for a turn (the part we will extend)

```
ACP agent (stdio)
  └─ JSON-RPC "session/update" notifications
       └─ transport reader task (always forwards every SessionNotificationParams)
            └─ PromptStream (in client.rs)
                 ├─ only AgentMessageChunk → Text
                 └─ everything else (AgentThoughtChunk, tool_call*, ...) → dropped (continue;)
                      └─ AcpResponseChunk::Text / TurnComplete only
                           └─ acp backend run_turn
                                └─ ResponseEvent::TextChunk / TurnComplete only
                                     └─ actor (developer/reviewer) loop
                                          ├─ TextChunk → sink(AgentExchange{ ResponseChunk }) + append to local output str
                                          └─ TurnComplete → sink( ... TurnComplete )
                                               └─ TUI App::update
                                                    └─ ExchangeLog (only prompts + response text entries)
                                                         └─ ui::exchange_entry_lines + render_exchange_pane
```

Thoughts and tool progress never leave the ACP client's notification receiver.

## Target shape (after 0006)

The same path now carries the extra kinds, while the "final answer text" branch for actors stays exactly as before.

Key extensions (new or widened types):

- `makina_acp::protocol::SessionUpdate` gains explicit variants before the `Other` arm:
  ```rust
  ToolCall(OurToolCall),
  ToolCallUpdate(OurToolCallUpdate),
  ```
  (AgentThoughtChunk already existed; its comment is updated.)

- `AcpResponseChunk` (public) becomes:
  ```rust
  pub enum AcpResponseChunk {
      Text(String),
      Thought(String),
      ToolCall { id: String, title: String, kind: Option<String>, status: String, ... },
      ToolCallUpdate { id: String, status: Option<String>, ... /* deltas */ },
      TurnComplete(StopReason),
  }
  ```
  The `PromptStream` poll now yields the new kinds when it sees the corresponding `SessionUpdate`. `chunk_text` is renamed/generalised or kept as an internal helper only for the Text case; the public stream now surfaces everything.

- `makina_core::backend::ResponseEvent` gains parallel variants (docs clarify they are *observability only*):
  ```rust
  ThoughtChunk { text: String },
  ToolCall { id: ..., title: ..., ... },
  ToolCallUpdate { id: ..., ... },
  ```
  The `ResponseStream` contract is updated in prose only: "zero or more Text/Thought/Tool* events, exactly one TurnComplete at the end."

- `makina_core::api::ExchangeEvent` (the view-level mirror) gains:
  ```rust
  ThoughtChunk { text: String },
  ToolCall { ... },
  ToolCallUpdate { ... },
  ```
  (The comment "mirrors ResponseEvent + outgoing prompt" is kept accurate.)

- In `developer.rs:218` and `reviewer.rs` (the `while let Some(item) = events.next()`):
  ```rust
  Ok(ResponseEvent::ThoughtChunk { text }) => { sink(AgentExchange { event: ExchangeEvent::ThoughtChunk { text }, ... }); }
  Ok(ResponseEvent::ToolCall { .. }) => { sink(... ToolCall ...); }
  // similarly for Update
  Ok(ResponseEvent::TextChunk { text }) => { sink(ResponseChunk); output.push_str...; }
  ```
  The local `output` String (used for the commit message etc.) receives *only* TextChunk contents.

- `makina/src/app.rs`:
  - `ExchangeEntry` is generalised (or a new internal `ExchangeContent` enum is introduced) so a single entry can be:
    - Prompt (existing)
    - Response { text, complete } (existing)
    - Thought { text }
    - Tool { id, title, kind, status, content_lines: Vec<String> or accumulated text }
  - `ExchangeLog` gains `append_thought(role, text)` (accumulate like chunks) and `apply_tool_event(...)` (create on ToolCall, mutate status/content on Update by matching id).
  - The match on `AgentExchange` grows the three new arms.

- `ui.rs`:
  - `exchange_entry_lines(&ExchangeEntry)` grows arms for Thought and Tool.
  - Thought: role-coloured header ("💭 Developer thought" / "💭 Reviewer thought"), then indented text (lighter or dim style).
  - Tool: "⚙ <title> [<status>]" header (status colour: yellow pending, cyan in_progress, green completed, red failed), then the content lines (diff styling still applies for edit tools).
  - The render loop in `render_exchange_pane` is unchanged (it just walks the now-richer log).

## Example interleaving (what a real turn may produce)

```
PromptSent: "Implement foo..."
  Thought: "I need to understand the existing Bar trait first."
  Thought: "I'll use the read_file tool on src/bar.rs"
  ToolCall: id="read_1", title="Reading src/bar.rs", kind="read", status="pending"
  ToolCallUpdate: id="read_1", status="in_progress"
  ToolCallUpdate: id="read_1", status="completed", content=[ {text: "pub trait Bar..."} ]
  Thought: "Now I'll edit the implementation with write_file."
  ToolCall: id="write_42", title="Writing src/bar.rs", kind="edit", status="pending"
  ToolCallUpdate: id="write_42", status="in_progress"
  ... (permission may be requested here via separate RPC if policy requires)
  ToolCallUpdate: id="write_42", status="completed", content=[diff...]
  Thought: "Tests should pass; running cargo test..."
  ToolCall: id="test_7", ...
  ...
  (final) ResponseChunk chunks for the user-visible summary
TurnComplete
```

All of the above (except the final response text) used to be invisible in the pane.

## Type minimality & compatibility notes

- We do **not** take a dependency on `agent-client-protocol-schema`. We continue the existing hand-maintained minimal mirror (exactly as we do for `RequestPermissionParams` / `ToolCall` used by the policy path). The structs capture the stable identity + display fields + a `HashMap` flatten for future-proofing.
- `ContentChunk` (already present) is reused for thoughts.
- For tool content we capture enough text/diff lines for rendering; full `ToolCallContent` enum can be widened later.
- All new variants are added at the *end* of enums before any `#[serde(other)]` or in a non-breaking way for our internal serde (the public API of `AcpResponseChunk` etc. is crate-internal-ish for the makina binary; tests are updated in the same PR).

## Test strategy deltas

- The existing `non_text_updates_are_ignored_during_turn` (mock_exchange.rs) is renamed or repurposed to `non_message_updates_are_captured_as_exchange_events` and now asserts that thoughts + tool lifecycle *do* appear when you drain with the richer API (or inspect the side log).
- New positive tests in `app.rs` (next to the existing `exchange_log_prompt_and_chunk_accumulation` etc.) feed synthetic `AgentExchange { ThoughtChunk }` and `ToolCall`/`ToolCallUpdate` events and assert the log contains the right kinds and that tool updates mutate the same entry.
- A render test exercises the new `exchange_entry_lines` arms (similar to `exchange_render_styles_ansi_and_diff...`).
- The acp `backend_trait` and `mock` tests continue to work because they only care about Text + TurnComplete for the answer.
- NoopBackend can optionally emit a thought + a tool pair in one of its richer test scenarios so that higher-level orchestrator tests can exercise the full path if desired.

## Interaction with prior plans

- 0003 (TUI hardening, prompt-answer-stream): this is a pure extension of the Exchange pane that 0003 delivered. No change to the scroll, error-pane badge, focus filtering, or dependency sub-pane logic.
- 0002 (governance): tool calls that require permission still go through the `session/request_permission` + `WorktreePolicy` + `AuditEntry` path. The `tool_call` notification may appear in the exchange *in addition to* (or instead of) an interactive prompt, depending on policy. The two surfaces remain complementary.
- 0004/0007 (ingestion): unrelated; this plan touches none of the interpreter/validator/report paths.
- The "Exchange" title badge for errors (the `(N errors)` thing) continues to work.

## Future work (explicitly not in this plan)

- Persisted exchange transcripts (so you can see thoughts/tools for a finished run after restarting the TUI).
- First-class support for Plan updates, terminal sessions, etc.
- Collapsible thought blocks or a "show thoughts only" filter.
- Emitting usage/cost information that some agents attach to thoughts or tool results.

This change keeps the "maximalist core, thin shell" and "thin TUI that only renders events" principles: all the intelligence of deciding what a thought vs. a tool update means stays in the ACP client + core event types; the TUI layer just has a couple more match arms and a slightly richer entry type.

See SCOPE.md for the "why" and in/out boundaries. See TASKS.md for the concrete, verifiable work items.
