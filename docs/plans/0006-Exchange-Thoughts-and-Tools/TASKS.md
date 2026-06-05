# Makina Plan 0006 — Exchange Thoughts and Tools

Live visibility of the agent's internal reasoning (`agent_thought_chunk`) and tool invocations (`tool_call` / `tool_call_update`) inside the Exchange pane, alongside the existing prompts and final response text. Extends the prompt-answer-stream machinery from plan 0003 without changing the "final answer text" contract consumed by the Developer/Reviewer actors.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md) for the type/flow deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also used for its branch `task/{id}` and worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only; the Planner adds further dependency edges automatically for tasks that touch the same files or areas.
- **Done when** is the verifiable acceptance check used by gates and the Reviewer. Every task must also keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check` green.
- Line numbers below are grounded against `develop` (post 0006 renumber) and are hints only; locate every site by the named symbol (grep), since earlier tasks shift lines.
- When a task says "add a test that asserts X", the test must be a `#[tokio::test]` (or `#[test]`) whose name appears literally in the "Done when" list, and the assertions must be written exactly as described (use the same helper style already present in the file).
- When a task says "a grep check must pass", include the exact `grep -n '...' path` line in the "Done when" and make sure it matches after your edit.

---

## 0025 — ACP protocol and PromptStream surface

### acp-protocol-add-tool-variants — Model ToolCall and ToolCallUpdate in SessionUpdate

In `crates/makina-acp/src/protocol.rs`, extend the `SessionUpdate` enum (currently at protocol.rs:438) with explicit variants before the `#[serde(other)] Other` arm:

```rust
ToolCall(OurToolCall),
ToolCallUpdate(OurToolCallUpdate),
```

Introduce minimal supporting structs (placed near the existing permission `ToolCall`):

- `OurToolCall` (or just reuse/extend the name `ToolCall` in this module) with at minimum `tool_call_id: String`, `title: String`, `kind: Option<String>`, `status: Option<String>`, plus `#[serde(flatten)] extra: HashMap<...>` for all the other schema fields (content, locations, raw_input, ...). Mirror the style used by the permission `ToolCall` at protocol.rs:516.
- `OurToolCallUpdate` similarly: `tool_call_id`, `#[serde(flatten)] fields: ToolCallUpdateFields` (or a simple struct with optional status/title/etc.), plus meta/extra.

Update the module docs at the top of the file (the paragraph that lists `agent_message_chunk`, `agent_thought_chunk`, …).

Add (or expand) a unit test that round-trips a realistic `tool_call_update` payload (including extra fields) and that an unknown future update kind still lands in `Other`.

- **Done when:** `cargo test -p makina-acp --test protocol` (or the module tests) passes; a new test `session_update_parses_tool_call_update_and_preserves_extra` (or similar) exists and passes; `grep -n 'ToolCall' crates/makina-acp/src/protocol.rs` shows the new variants; the existing `session_update_ignores_unknown_update_kinds` test still passes for a brand-new discriminator.

### acp-client-rich-chunks — Extend AcpResponseChunk and PromptStream to emit thoughts + tools

In `crates/makina-acp/src/client.rs`:

- Change `AcpResponseChunk` (around client.rs:182) to:
  ```rust
  pub enum AcpResponseChunk {
      Text(String),
      Thought(String),
      ToolCall { id: String, title: String, kind: Option<String>, status: String, /* + minimal other display fields */ },
      ToolCallUpdate { id: String, status: Option<String>, title: Option<String>, /* ... */ },
      TurnComplete(StopReason),
  }
  ```
  Update the doc comment that currently says "yields zero or more Text items ... followed by TurnComplete".

- Rename or supplement `fn chunk_text` (client.rs:658) — keep a private `extract_text` for the message case; add a `classify_update(SessionUpdate) -> Option<AcpResponseChunk>` (or equivalent) that returns the appropriate rich variant for Thought / ToolCall / ToolCallUpdate / Text.

- In the `poll_next` implementation (the loop that does `transport.notifications_mut().poll_recv` and the buffered post-response drain), yield the new rich items instead of silently `continue;`ing. The existing buffered logic must be extended to handle the non-Text kinds (or simply re-poll — the important thing is they are not lost).

- Update `lib.rs` re-exports and the crate-level docs that mention `AcpResponseChunk` if they describe the old shape.

- Rename/adapt the test `non_text_updates_are_ignored_during_turn` (mock_exchange.rs) to something like `non_message_updates_are_emitted_as_rich_chunks`. In the test, after the turn, assert that the richer drain (or a side collection) saw at least one Thought and a ToolCall+Update pair that the mock injected via the existing `leading_noise_updates` or a new `inject_thoughts_and_tools` behaviour flag on `MockBehavior`.

- **Done when:** the renamed test (and a new `thought_and_tool_events_are_delivered` test) passes and explicitly asserts the new variants appear in arrival order interleaved with text; `grep -n 'Thought\|ToolCall' crates/makina-acp/src/client.rs` finds the new arms; all existing acp client tests (including real_cli when run) that only care about Text+TurnComplete continue to pass; `cargo test -p makina-acp` is green.

## 0026 — Core ResponseEvent and ExchangeEvent

### core-backend-response-event-extensions — Add side-channel variants to ResponseEvent

In `crates/makina-core/src/backend.rs`:

- Extend `ResponseEvent` (backend.rs:133) with:
  ```rust
  ThoughtChunk { text: String },
  ToolCall { id: String, title: String, kind: Option<String>, status: String, /* ... */ },
  ToolCallUpdate { id: String, /* delta fields */ },
  ```
  Keep `TextChunk` and `TurnComplete` exactly as before.

- Update the trait docs for `ResponseStream` / `AgentSession::prompt` to state that implementers *may* emit the new variants at any point before the final `TurnComplete`; consumers that only want the final answer text should ignore them (or only accumulate `TextChunk`).

- Update the `NoopBackend` (in `backend/noop.rs`) so that at least one of its test scenarios (or a new `emit_thought_and_tool` helper) can produce the richer events when requested. Existing tests that hard-code `vec![TextChunk..., TurnComplete]` continue to work.

- Update the adapter in `crates/makina-acp/src/backend.rs` (`run_turn`, around line 445) to map the new `AcpResponseChunk` variants to the new `ResponseEvent` variants.

- **Done when:** `grep -n 'ThoughtChunk\|ToolCall' crates/makina-core/src/backend.rs` finds the variants; a new unit test in `backend.rs` (or `noop.rs`) constructs a stream containing Thought + Tool events and a test helper drains only the Text ones and still gets the expected answer; `cargo test -p makina-core` (the backend tests) pass.

### core-api-exchange-event — Add the view-level ExchangeEvent variants

In `crates/makina-core/src/api.rs`:

- Extend `ExchangeEvent` (api.rs:426) with the three new variants (mirror the shapes from ResponseEvent but keep the "view level" comments):
  ```rust
  ThoughtChunk { text: String },
  ToolCall { id: String, ... },
  ToolCallUpdate { id: String, ... },
  ```
- Update the doc for `AgentExchange` and the "mirrors `ResponseEvent`" comment.

- Update every place in the same file (and in `orchestrator.rs` tests) that constructs or matches `ExchangeEvent` for the old three variants; the new variants must be handled (usually by ignoring in test match arms that only care about prompts/responses).

- **Done when:** the module compiles and its own tests (`cargo test -p makina-core --test ...` or the api tests) pass; `grep -n 'ThoughtChunk' crates/makina-core/src/api.rs` shows the addition; no `unreachable!` or `_ => panic` on the new kinds in test code.

## 0027 — Forwarding in the actor layer

### actor-forward-rich-exchange-events — Emit Thought and Tool exchange events from developer/reviewer

In both `crates/makina-core/src/actors/developer.rs` and `reviewer.rs` (the almost-identical prompt loops around developer.rs:218 and reviewer.rs:194):

- In the `while let Some(item) = events.next().await` match:
  - On `ResponseEvent::ThoughtChunk { text }` (and the Tool* ones) do:
    ```rust
    (msg.sink)(api::Event::AgentExchange {
        run: msg.run,
        task: task_id.clone(),
        role: api::AgentRole::Developer, // or Reviewer
        event: api::ExchangeEvent::ThoughtChunk { text: text.clone() },
    });
    ```
  - The existing `TextChunk` arm stays exactly as-is (still appends to `output`).
  - `TurnComplete` arm unchanged.

- The local `output: String` must *never* receive thought or tool text.

- Add a small unit-test-style comment or a `#[cfg(test)]` helper that a future orchestrator test can use to observe the richer events.

- **Done when:** the two files compile; a grep for the new `ExchangeEvent::ThoughtChunk` (or `ToolCall`) in the actors directory finds the forwarding code; existing actor tests that only assert on the collected `output` string or on `TurnComplete` still pass; `cargo test -p makina-core` (the roles/actor tests) is green.

## 0028 — TUI ExchangeLog and rendering

### tui-exchange-entry-generalisation — Make ExchangeEntry able to represent thoughts and tools

In `crates/makina/src/app.rs`:

- Either evolve `ExchangeEntry` (app.rs:56) or introduce a small `ExchangeContent` enum that `ExchangeEntry` holds. The entry still carries `role` and `complete` (for responses). New kinds:
  - `Thought { text: String }`
  - `Tool { id: String, title: String, kind: Option<String>, status: String, content: String /* accumulated or last content */ }`

- Add methods on `ExchangeLog`:
  - `append_thought(&mut self, role: AgentRole, text: String)` — behaves like `append_chunk` but for a thought "burst" (accumulate into last open thought for the role, or start a new one).
  - `apply_tool_event(...)` (or two methods `start_tool` / `update_tool`) that finds by `id` (or inserts) and mutates status + appends/replaces content.

- Extend the `AgentExchange` match arm (app.rs:833) with the three new `ExchangeEvent` arms that call the new log helpers.

- Update every existing unit test that builds `ExchangeEntry` or asserts on `exchange_logs` (there are many in the `mod tests` block starting ~app.rs:1820). Most can stay as "response" entries; add a few new tests that feed Thought and Tool events.

- Add at least two new public tests whose names appear in "Done when":
  - `exchange_log_captures_thoughts_and_tool_updates` — feeds a Prompt, several Thoughts, a ToolCall, two ToolCallUpdates for the same id, and a Response; asserts the log has the right number/kinds of entries and that the tool entry has the final status.
  - `exchange_log_tool_update_mutates_in_place` — specifically asserts that two updates for id "tc-1" result in only one tool entry whose status/content reflects the last update.

- Keep `EXCHANGE_LOG_CAP` behaviour (oldest eviction) for the richer entries too.

- **Done when:** the two new tests (plus any supporting ones) are present with the exact names above and pass; `cargo test -p makina --test ...` (or the app tests) is green; `grep -n 'Thought\|Tool' crates/makina/src/app.rs` shows the new variants and helpers.

### tui-exchange-render-new-kinds — Render thoughts and tools distinctly in the pane

In `crates/makina/src/ui.rs`:

- Extend `exchange_entry_lines` (ui.rs:939) with arms for the new entry kinds (after the existing `if entry.is_prompt { ... } else { ... }`).
  - For Thought: push a bold role-coloured header e.g. "💭 Developer thought" (green for dev, yellow for reviewer) then indented lines of the text (use `Color::DarkGray` or a lighter variant).
  - For Tool: header "⚙ <title> [<status>]" (choose fg colour by status: DarkGray pending, Cyan in_progress, Green completed, Red failed); then the content lines (re-use the existing `parse_ansi` + `diff_line_style` loop so edit diffs look good).
- A blank separator line after every entry (existing behaviour) is fine.

- Add (or extend) a render test, e.g. `exchange_render_shows_thought_and_tool_entries` (modeled on `exchange_render_styles_ansi_and_diff_no_literal_escape` or `render_exchange_pane_shows_prompt_and_concatenated_answer`). Construct an `ExchangeEntry` (or the new content) for a thought and for a completed tool with a diff line, render a small area, and assert the expected header text and at least one content line appears in the buffer.

- If the title logic in `render_exchange_pane` (ui.rs:719) needs a tiny tweak for the new volume of entries, do it (e.g. keep the error-count badge logic).

- **Done when:** the new render test with the literal name above passes and inspects the rendered cells/lines for the distinctive headers; `cargo test -p makina` (ui tests) green; running the TUI manually (or via the existing e2e harness) with a mock that produces thoughts/tools shows them in the Exchange pane with the expected styling and no literal escape codes.

## Cross-cutting / verification tasks

### update-all-downstream-matches — Make every match on the extended enums handle the new variants

Sweep the workspace (especially `makina`, `makina-core`, and acp tests) for `match` / `if let` on `AcpResponseChunk`, `ResponseEvent`, `ExchangeEvent`, and `SessionUpdate`. Add `_ => {}` or explicit arms (usually "ignore for this test" or "forward") so nothing is a non-exhaustive error after the enum changes.

Pay special attention to:
- `orchestrator.rs` tests that build fake event streams.
- `placeholder.rs`, `e2e.rs`, error_pane_wire.rs etc. in the makina crate.
- All the "drain" helpers in `makina-core/tests/common` and acp tests.

- **Done when:** `grep -n 'AcpResponseChunk\|ResponseEvent::\|ExchangeEvent::' --include='*.rs' crates/ | xargs grep -L 'Thought\|ToolCall' | ...` (or simply a full `cargo check -p makina -p makina-core -p makina-acp --tests` succeeds with no "non-exhaustive" or "unreachable" warnings related to the new variants).

### plan-0005-acceptance — End-to-end observability test using the real mock path

Add (or expand) one test that goes through the full stack:

- A `MockBehavior` that emits 1–2 thought chunks, a tool_call, two tool_call_updates, and normal text.
- Drive it via `AcpBackend` (or the in-memory transport) → real `Developer` actor path (or the gated orchestrator test helper) → `CoreApi` event stream → a `App` that consumes the `AgentExchange` events.
- Assert on the final `app.exchange_logs` for the task that the thoughts and the tool (with final status) are present.

This can live in `makina/tests/e2e.rs` or a new `exchange_observability.rs` next to the existing exchange tests.

- **Done when:** the test (with a name containing `exchange_thoughts_and_tools` or `full_turn_with_thoughts_and_tools`) passes and is listed in the "Done when" of this task; the test would have failed before the changes in 0025–0028.

**End of plan 0006 TASKS.** When every "Done when" bullet is green (plus the usual cargo test/clippy/fmt), the Exchange pane shows the model's thought process and the tools the agent calls, exactly as requested.
