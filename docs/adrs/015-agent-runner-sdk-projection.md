# ADR-015: AgentRunner — narrow SDK projection, append-mode system prompt, structural query injection

## Status

Accepted (2026-04-26).

## Context

Issue #11 (Wave 3) ships `src/daemon/agent-runner.ts`, the supervisor's only entry point into the
Claude Agent SDK. The runner wraps `query()` from `@anthropic-ai/claude-agent-sdk`, streams
`SDKMessage`s as `TaskEvent`s onto the event bus, and persists the SDK-assigned `sessionId` so the
supervisor can carry model context across CI / review / rebase iterations.

Three load-bearing design questions came up during implementation that warrant pinning down before
the supervisor (#12), the stabilize loop (Wave 4), or any future agent-side work depends on the
runner's exact contract:

1. **What shape does the runner expose to the rest of the daemon?** The SDK's `SDKMessage` is a wide
   discriminated union (28+ variants — `assistant`, `user`, `result`, `system/init`,
   `tool_progress`, `task_started`, …) backed by a multi-megabyte native CLI binary per platform.
   The supervisor doesn't need most of that surface; the TUI definitely doesn't.
2. **How does the runner inject the conventional-commits commit-message contract** (ADR-009) into
   the agent's behavior without rewriting the SDK's preset system prompt?
3. **How do tests drive the runner without spawning the real `claude` CLI subprocess?** Wave 2's
   `tests/helpers/mock_agent_runner.ts` already addresses the supervisor side; this ADR is about the
   runner module itself.

The W1 contract in `src/types.ts` froze the `AgentRunner` interface at
`runAgent(...) → AsyncIterable<AgentRunnerMessage>` where `AgentRunnerMessage = { role, text }`.
That is a deliberately narrow projection of `SDKMessage`. This ADR captures the rationale for the
projection plus two implementation decisions made on top of it. Note: issue scoping (the
`issueNumber` the supervisor passes) is **not** part of the frozen `AgentRunner` contract; the
runner exposes it on a widened `RunAgentArgs` and treats it as optional so the implementation
remains assignable to the W1 interface (see "System-prompt addendum" below for the degraded
rendering when `issueNumber` is absent).

## Decision

### 1. Narrow SDK projection: `AgentRunnerMessage = { role, text }`

The runner adapts the SDK's `SDKMessage` into the W1 `AgentRunnerMessage` shape and **never lets the
SDK type leak past its own module boundary**. Concretely:

- `assistant` messages: concatenate every `text` block; emit `tool_use` blocks as a single line of
  the form `[tool <name>] <json input>`. The `text` projection is what the TUI's transcript shows;
  the `role` is `"assistant"`.
- `user` messages: concatenate every `text` block. The `role` is `"user"`.
- `result` messages: take the final `result` string. The `role` is `"system"` (the supervisor never
  branches on `result`; it wants the text on the bus and the session id captured).
- `tool_use` standalone messages: `role` `"tool-use"`, `text` rendered the same way as the embedded
  tool block.
- `tool_result` standalone messages: `role` `"tool-result"`.
- Everything else (`system/init`, `tool_progress`, `task_started`, `task_updated`, …): `role`
  `"system"`, `text` the SDK type wrapped in square brackets (`[task_progress]`). The discriminant
  alone is more useful than nothing, and the supervisor never branches on these variants today.

The projection is implemented inside the agent-runner module so a future SDK release that adds new
`SDKMessage` variants is absorbed by extending `mapSdkTypeToRole` and `renderSdkMessageText` locally
— no change to `AgentRunnerMessage`, no propagation to the supervisor, the persistence layer, or the
TUI. **The W1 contract surface is the seam; the SDK is hidden behind it.**

The truncation budget on the bus side is `AGENT_MESSAGE_TEXT_TRUNCATION_CODE_UNITS = 8192`. Long
tool inputs and long outputs would otherwise balloon every subscriber's queue depth, run subscribers
up against `EVENT_BUS_DEFAULT_BUFFER_SIZE = 256` more often, and bloat persistence snapshots that
include recent events. Eight kilobytes is generous for the supervisor's prose-and-summary workload
while still bounding pathological cases. Truncated text is suffixed with `…` so consumers can tell
rendering from a hard cap.

### Alternatives considered

- **Pass `SDKMessage` through verbatim.** Rejected. Two concrete costs: the supervisor and the TUI
  pull in the SDK's full type definitions (multi-thousand lines of d.ts) just to discriminate on
  `type`; new SDK variants land as breaking type changes for every consumer rather than being
  absorbed locally.
- **Per-variant projections** (e.g. `AgentToolUseMessage`, `AgentResultMessage`). Rejected. The
  consumers don't branch on the variant — they render the text and log the role. A finer projection
  is solving a problem we don't have, and it makes the bus-event payload's discriminator harder to
  type.

### 2. System-prompt addendum via the SDK's `append` field

The supervisor passes the GitHub issue number; the runner builds an addendum that instructs the
agent to author commits in `<type>(#<issueNumber>): <subject>` Conventional Commits format (ADR-009)
and forwards it via `Options.systemPrompt = { type: 'preset', preset: 'claude_code',
append: ... }`.
The exact wording is unit-tested by snapshot in `tests/unit/agent_runner_test.ts` so silent drift is
caught on every PR.

The decision rationale:

- **`append` over `customSystemPrompt`.** The SDK's `customSystemPrompt` (now
  `systemPrompt: string
  | string[]`) replaces the default Claude Code prompt entirely. We want the
  model to retain every piece of CLI-shipped guidance (tool-use protocol, Bash safety, file-edit
  conventions); we are only adding one constraint. `append` does exactly that and keeps the static
  prefix prompt-cacheable across runs.
- **Issue number formatted into the addendum, not into `extraArgs`.** The runner is responsible for
  the conventional-commits contract; the supervisor passes the number through `runAgent`'s
  `issueNumber` argument and never has to hand-format the prompt itself.
- **`issueNumber` is _optional_ on `RunAgentArgs`.** The W1 `AgentRunner` interface in
  `src/types.ts` does not include `issueNumber`; making the field required on `RunAgentArgs` would
  break the assignability `AgentRunnerImpl extends AgentRunner` claims. So the field is optional and
  the runner degrades gracefully: when omitted, `buildSystemPromptAddendum()` drops the `(#<n>)`
  scope and renders the format as `<type>: <subject>`. The supervisor (the only production caller)
  always passes the issue number; the optional shape exists so a generic `AgentRunner` consumer
  cannot accidentally produce a `#undefined` literal in the prompt.
- **Snapshot-tested wording.** A small text drift (a different word, a missing example) is the kind
  of regression that doesn't fail the build but fails the agent at commit time. The snapshot is the
  cheapest line of defense.

### 3. Structural query injection for tests; lazy SDK import in production

The factory accepts an `opts.query` parameter typed as `SdkQueryFunction`, a structural projection
of the SDK's real `query` signature. Tests pass a scripted stub that yields a controlled stream of
`SdkMessageProjection` values; production code leaves `opts.query` undefined and the runner _lazily
imports_ `@anthropic-ai/claude-agent-sdk` on first use, memoizing the import.

The rationale:

- **No global mutation in tests** (Wave 2 lessons learned #3). We could have wrapped the SDK in a
  module-scope binding and reassigned it in tests; that's the kind of cross-test contamination the
  W2 review rounds rejected. Constructor injection keeps the substitute strictly scoped.
- **Lazy import.** The SDK's npm package brings a per-platform native binary
  (`claude-agent-sdk-darwin-arm64`, `…-linux-x64-musl`, …). Eagerly importing it on module load
  would extend the daemon's startup time and load native code on every test that touches
  `src/daemon/agent-runner.ts` even if it doesn't reach the SDK code path. Lazy + memoized cuts the
  cost.
- **Executable discovery is also injectable.** `opts.resolveExecutable` defaults to a thin
  `Deno.Command("which", [name])` wrapper; tests pass a stub. Same rationale: no `Deno.Command`
  override, no race with sibling tests, no platform-specific harness setup.

### Alternatives considered

- **Wrap `query` in a module-scope mutable**
  (`let queryImpl = realQuery; export function
  setQueryForTests(...)`). Rejected. Lessons learned
  #3: global mutation in tests caused 8 review rounds across W2.
- **Subprocess-mock the `claude` binary directly.** Rejected. The SDK's process management is
  intricate (stdin/stdout framing, MCP, hooks, lifecycle messages); a mock binary that satisfies
  every check would be more code than the runner itself, and it would not exercise the unit's real
  responsibility (adapting the SDK projection onto the bus). The integration test already covers the
  real-binary path behind `AGENT_RUNNER_REAL_TEST=1`.

## Consequences

- The supervisor and TUI consume `AgentRunnerMessage` exclusively. Adding new SDK variants is a
  one-file change inside the runner.
- The agent's commit format is a snapshot-tested invariant. Any future change to the wording (e.g.
  expanding the type list or relaxing the subject limit) is a deliberate, reviewable diff.
- The runner is testable without spawning a subprocess. The unit test suite in
  `tests/unit/agent_runner_test.ts` runs in milliseconds and exercises every adapter branch.
- The integration test in `tests/integration/agent_runner_real_test.ts` is opt-in via
  `AGENT_RUNNER_REAL_TEST=1`. CI does not run it by default; an operator who installs the `claude`
  CLI on their host can flip the gate to validate the production path.
- A future SDK release that renames `query()` or breaks the `Options` shape will fail the lazy
  import or the structural type check at first use. The runner surfaces this through
  `AgentRunnerError.operation = "loadSdk"` so the diagnostic is precise.

## References

- `src/daemon/agent-runner.ts` — implementation.
- `src/types.ts` — frozen `AgentRunner` and `AgentRunnerMessage` contracts.
- `tests/unit/agent_runner_test.ts` — adapter branches, snapshot, error paths.
- `tests/integration/agent_runner_real_test.ts` — gated end-to-end run.
- ADR-009 (`docs/adrs/009-conventional-commits-and-git-cliff.md`) — commit format the addendum
  enforces.
- ADR-012 (`docs/adrs/012-event-bus-backpressure-and-sync-callbacks.md`) — backpressure model the
  runner publishes against.
