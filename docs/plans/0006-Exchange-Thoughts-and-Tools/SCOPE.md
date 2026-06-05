# Scope — Plan 0006

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

The Exchange pane (the live "prompt-answer-stream" view added in plan 0003, task 30) is the primary place users watch an agent work. Today it only ever shows:

- `PromptSent` entries (the full prompt text the orchestrator sent to Developer or Reviewer).
- Accumulating `ResponseChunk` entries that become the final visible assistant message (the `agent_message_chunk` text).

ACP (the protocol Makina uses for real agents) provides a much richer stream of `session/update` notifications during a single turn:

- `agent_thought_chunk` — chunks of the model's internal reasoning / "thought process".
- `tool_call` — the agent has initiated a tool invocation (with `toolCallId`, human `title`, `kind`, `status`, `locations`, `content`, `raw_input`...).
- `tool_call_update` — incremental status and result updates as the tool runs (status transitions pending → in_progress → completed/failed, plus emitted content/diffs).

See the official schema (`SessionUpdate` in agent-client-protocol-schema) and the protocol docs on "Agent Reports Output" and "Tool Calls".

In the current code these are explicitly dropped:

- `protocol::SessionUpdate` already has `AgentThoughtChunk` but everything else (including tool updates) collapses to `Other`.
- `client::PromptStream::chunk_text` (and the poll loop) only extracts `AgentMessageChunk`; the comment literally says "Other update kinds (thoughts, tool calls, …) return `None` and are skipped."
- Consequently `AcpResponseChunk`, `core::backend::ResponseEvent`, `api::ExchangeEvent`, the actor emission sites, `ExchangeEntry`/`ExchangeLog`, and `exchange_entry_lines` only know about prompts + final response text.

The result is that the pane hides exactly the "model's thought process" and "tools called" that users want to see while an agent is working. Permission-gated tools surface in audit logs and the approval UI, but normal (auto-approved or post-approval) tool progress and all thoughts are invisible in the primary observability surface.

This plan wires the already-parsed (or easily parsable) ACP data all the way to the Exchange pane so the pane becomes a true transcript of the full exchange — prompts, thoughts, tool activity with live status, and final answer — while preserving the existing "final visible answer text" contract for the Developer/Reviewer actors.

## In scope

Exactly the work items described in [TASKS.md](TASKS.md) (sections 0025–0028 and their leaf tasks):

- **0025 — ACP protocol & client stream surface.** Add explicit `ToolCall(...)` and `ToolCallUpdate(...)` variants (with minimal supporting structs using the same `#[serde(flatten)] extra` pattern already used for permission `ToolCall`) to `SessionUpdate` so they no longer vanish into `Other`. Extend `AcpResponseChunk` (and its doc) plus the `PromptStream` implementation to yield `Thought(String)`, `ToolCall {..}`, and `ToolCallUpdate {..}` items in addition to `Text`/`TurnComplete`. Keep the text-only extraction path working so the "answer" accumulator contract is unchanged. Update the "non_text_updates_are_ignored" test (and mock behaviour) to also assert that thoughts and tool events are now *delivered* (they just aren't turned into the final text response).

- **0026 — Core backend + api event types.** Add the corresponding side-channel variants to `makina_core::backend::ResponseEvent` (`ThoughtChunk { text }`, `ToolCall {..}`, `ToolCallUpdate {..}`) and to `makina_core::api::ExchangeEvent` (parallel shapes). Update docs to make the contract explicit: `TextChunk` + `TurnComplete` are the final assistant message; the new variants are live observability only. Update the thin adapter in `makina-acp/src/backend.rs`.

- **0027 — Actor forwarding.** In `developer.rs` and `reviewer.rs` (the loops that consume `ResponseStream` and emit `AgentExchange` events), forward the new `ResponseEvent` kinds as the corresponding `ExchangeEvent` kinds. Text chunks continue to be concatenated into the local `output` string used for commit messages etc.; thoughts and tools are emitted as exchange events but never added to that string.

- **0028 — TUI state + rendering.** Generalise `ExchangeEntry` (and `ExchangeLog` helpers) so it can represent thoughts (accumulating text, role-coloured) and tools (structured by `id`, with mutable status + content lines). Add `append_thought` / `upsert_tool` (or equivalent) and update the `AgentExchange` match arm in `App::update`. Extend `exchange_entry_lines` (and its ANSI/diff helpers if needed) to render the new kinds distinctly (e.g. "💭 Developer thought", "⚙ <title> [in_progress]", indented content). Add render tests. Minor title or badge polish if the pane now regularly contains more than just prompts+answers.

All changes keep `cargo test`, clippy, and fmt green. New cross-cutting acceptance tests (mock-injected thoughts + full tool lifecycle) live next to the existing exchange tests in `makina/src/app.rs` and the acp mock tests.

## Origin → workstream mapping

| Gap / request | Addressed by |
|---|---|
| `AgentThoughtChunk` already on the wire and deserialized but never shown in Exchange | 0025 + 0026 + 0027 + 0028 |
| `tool_call` / `tool_call_update` notifications arrive (seen in mocks + real_cli probes) but are dropped before the pane | 0025 + 0026 + 0027 + 0028 |
| ExchangeEntry / render only understand prompt vs. response text | 0028 |
| Need to keep the "final answer text only" contract for actors that consume the stream | 0025 (separate yield kinds) + 0027 (only Text goes into output) |
| Tests explicitly assert that non-text is ignored (they must now assert it is captured for exchange) | 0025 (update test + add positive delivery tests) |

## Locked decisions

Settled during the investigation + this plan's design:

- **Thoughts and tool events are side-channel only.** They never contribute to the concatenated response string that `developer`/`reviewer` collect and later use for commit messages or feedback to the other role. Only `TextChunk` / `ResponseChunk` do.
- **Tool state is upsert-by-id in the log.** A `ToolCall` creates (or initialises) an entry; subsequent `ToolCallUpdate`s for the same `tool_call_id` mutate the status/content in place. The rendered pane therefore always shows the *current* view of each tool rather than an append-only history of every micro-update. (A full transcript of every notification can still be obtained from the per-run log files if needed.)
- **No new public contract breakage for backend implementers.** Adding variants to `ResponseEvent` is acceptable because the only two implementations are the in-tree `NoopBackend` (tests) and the ACP one; all existing stream-draining test helpers already use exhaustive or wildcard matches that we will update.
- **Rendering style reuses existing machinery.** Thoughts get a distinct role-coloured header + indented lighter text (similar to responses but with "thought" label and perhaps dim styling). Tool entries get a tool icon + title + `[status]` and then their content lines (diff-aware ANSI still applies). No collapsible UI yet.
- **Ephemeral only (for this plan).** The richer exchange contents live only in the in-memory `HashMap<TaskId, ExchangeLog>` in the TUI `App`. They are not written into the `.makina/tasks/{slug}.json` artifacts or any new per-turn transcript file. (That would be a follow-up.)
- **Start narrow.** We model only the fields needed for useful display (id, title, kind, status, textual content summary). Full `raw_input` / `raw_output` / complex `ToolCallContent` variants can be added later without changing the event emission path.

## Out of scope — deferred to FUTURE or a later plan

- Persisting the full exchange (including thoughts and tool transcripts) for completed runs so you can reopen the TUI and still see the reasoning.
- Support for the other `SessionUpdate` kinds (`Plan`, `CurrentModeUpdate`, `AvailableCommandsUpdate`, ...).
- Richer tool content rendering (embedded terminals, multi-part diffs, images, etc.).
- Any UI for expanding/collapsing thoughts or filtering the exchange pane.
- Cost, token, or timing annotations on thoughts/tool steps.
- Changes to how *permission requests* themselves are presented (they remain the audit + interactive gate path).
- Backwards compatibility shims or feature flags; this is additive.
- Any alteration to the core `TaskListInterpreter`, supervisor FSM, or worktree commit flows.

This plan is deliberately focused on making the Exchange pane reflect what the ACP protocol and real agents already provide for observability of reasoning and tool use. It re-uses the event plumbing built for the original prompt-answer-stream work (0003) rather than inventing a second channel.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete type deltas, data-flow diagram, and example event interleavings. See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
