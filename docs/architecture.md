# Architecture

The contracts in `packages/core/src/types.ts`, `packages/core/src/ipc/protocol.ts`, and
`packages/core/src/config/schema.ts` are the foundation; the daemon, TUI, and the stabilize loop
implement them. Every component is wired in production by `packages/cli/main.ts daemon` (see
ADR-024).

## Process model

Two cooperating processes communicate over a Unix domain socket using a length-prefixed framed JSON
wire format (`<decimal-length>\n<utf8-json>\n`, see `packages/core/src/ipc/codec.ts`):

- **`makina daemon`** — long-running supervisor. Owns agents, worktrees, GitHub App auth, polling,
  and persisted state. Survives TUI exit; restarts replay persisted task projections.
- **`makina`** (TUI) — Ink-based React app. Connects to the daemon over the socket; auto-spawns the
  daemon when not running.

`packages/cli/main.ts` is an argv-dispatch shell: `daemon`, `setup`, or default → TUI.

## Daemon internals

| Component       | File                                           | Purpose                                                     |
| --------------- | ---------------------------------------------- | ----------------------------------------------------------- |
| TaskSupervisor  | `packages/core/src/daemon/supervisor.ts`       | Per-issue state machine (see ADR-016).                      |
| Stabilize loop  | `packages/core/src/daemon/supervisor.ts`       | Rebase → CI → conversations sub-phases after each push.     |
| AgentRunner     | `packages/core/src/daemon/agent-runner.ts`     | Wraps `@anthropic-ai/claude-agent-sdk` (see ADR-015).       |
| WorktreeManager | `packages/core/src/daemon/worktree-manager.ts` | Bare clone + per-task worktrees (see ADR-007).              |
| GitHubClient    | `packages/core/src/github/client.ts`           | High-level methods over `@octokit/core` (see ADR-011).      |
| Poller          | `packages/core/src/daemon/poller.ts`           | Per-task polling with backoff (see ADR-017).                |
| Persistence     | `packages/core/src/daemon/persistence.ts`      | Atomic JSON state store (see ADR-014).                      |
| EventBus        | `packages/core/src/daemon/event-bus.ts`        | In-process pub/sub (see ADR-012).                           |
| Daemon server   | `packages/core/src/daemon/server.ts`           | Unix socket listener + dispatch (see ADR-013).              |
| Handlers        | `packages/core/src/daemon/handlers.ts`         | IPC `command`/`prompt` routing onto the supervisor surface. |
| Runtime wiring  | `packages/cli/main.ts` (`wireDaemonRuntime`)   | Boot-time DI for all of the above (see ADR-024).            |

## TUI

| Component                           | File                                           |
| ----------------------------------- | ---------------------------------------------- |
| `App`                               | `packages/cli/src/tui/App.tsx`                 |
| `Header` / `MainPane` / `StatusBar` | `packages/cli/src/tui/components/`             |
| `CommandPalette` / `TaskSwitcher`   | `packages/cli/src/tui/components/`             |
| `useFocusedTask`                    | `packages/cli/src/tui/hooks/useFocusedTask.ts` |
| Slash-command parser                | `packages/cli/src/tui/slash-command-parser.ts` |
| Keybindings parser                  | `packages/cli/src/tui/keybindings.ts`          |

### Slash commands

The command palette parses leading-`/` lines through `packages/cli/src/tui/slash-command-parser.ts`
and dispatches the resulting `CommandPayload` over the daemon socket. The parser only validates
shape; per-command behaviour lives in `packages/core/src/daemon/handlers.ts` (which routes `command`
envelopes onto `TaskSupervisor` methods).

| Command                                             | Behaviour                               |
| --------------------------------------------------- | --------------------------------------- |
| `/issue <number> [--repo <owner/name>] [--merge=…]` | Start a new task.                       |
| `/repo {add\|default\|list}`                        | Manage registered repositories.         |
| `/status`                                           | Print every in-flight task's state.     |
| `/switch <task-id>`                                 | Focus the listed task.                  |
| `/cancel <task-id>`                                 | Cancel a non-terminal task.             |
| `/retry <task-id>`                                  | Re-enter a `NEEDS_HUMAN` task.          |
| `/merge <task-id>`                                  | Force the merge of `READY_TO_MERGE`.    |
| `/logs <task-id>`                                   | Open the task scrollback.               |
| `/quit`                                             | Exit the TUI; the daemon keeps running. |
| `/daemon stop`                                      | Stop the daemon process.                |
| `/help [command]`                                   | List commands or describe one.          |

Today the supervisor wires `/issue`, `/merge`, and `/status`; the remaining commands are routed by
the parser and handlers but the supervisor surface for `/cancel`, `/retry`, `/logs`, `/switch`,
`/repo`, `/help`, `/quit`, and `/daemon` returns a deterministic
`ack { ok: false, error: "not yet
implemented" }`. Wiring those onto supervisor methods is tracked
as v0.2.0 work.

Default overlay toggles: `Ctrl+P` (palette), `Ctrl+G` (switcher); both are configurable via
`tui.keybindings` in `config.json`. The chord parser in `packages/cli/src/tui/keybindings.ts`
accepts `<modifier>+<key>` strings (`ctrl+p`, `ctrl+shift+tab`) and matches Ink's `useInput` flag
bag uniformly across macOS and Linux.

## IPC protocol

Length-prefixed framed envelopes `{ id, type, payload }` with zod schemas in
`packages/core/src/ipc/protocol.ts`. Each frame is `<decimal-length>\n<utf8-json>\n`; the trailing
newline keeps the wire format human-readable for ad-hoc tooling. The framer lives in
`packages/core/src/ipc/codec.ts` and rejects malformed frames (oversize, partial, schema-mismatched,
non-UTF8) with a typed `IpcCodecError`. Client → Daemon: `subscribe`, `unsubscribe`, `command`,
`prompt`, `ping`. Daemon → Client: `event`, `ack`, `pong`.

Consumers do not import zod directly: `packages/core/src/ipc/protocol.ts` exposes typed interfaces
and a `parseEnvelope(raw): ParseEnvelopeResult` function; the same idiom in
`packages/core/src/config/schema.ts` exposes `parseConfig`. This keeps the public API zod-free so
`deno doc --lint` reflects the contract, not the validator implementation.

## TUI client

`packages/cli/src/tui/ipc-client.ts` exports a `DaemonClient` interface plus a `SocketDaemonClient`
that opens a Unix-domain socket via `Deno.connect({ transport: "unix" })`, encodes outgoing
envelopes through `packages/core/src/ipc/codec.ts`, and decodes pushed envelopes back into typed
values. Reply correlation is by envelope id; pushed `event` envelopes fan out through
`subscribeEvents`. The interface surface mirrors
`packages/core/tests/helpers/in_memory_daemon_client.ts` so consumers can swap the real client for
the in-memory double in tests with no other change.

`packages/cli/src/tui/hooks/useDaemonConnection.ts` wraps the client in a React hook that exposes
`{ status, lastError, send, subscribe, connect, disconnect }` and walks the lifecycle
`idle → connecting → connected → disconnected | error`. The hook is transport-agnostic: a client
without `connect`/`close` methods (the in-memory double) starts directly in `connected`.

### Ink-on-Deno feasibility verdict (issue #10)

Ink 5.2 renders cleanly under Deno 2.7 with `npm:ink`/`npm:react` specifiers. Yoga's WASM layer
loads, the default `process.stdout`/`process.stdin` adapters provided by Deno's Node compatibility
shim work as Ink expects, and `signal-exit`'s 13 signal listeners are released on `unmount()`. The
ADR-001 risk is closed in favor of the original Deno-native plan; ADR-010 (Node-side Ink subprocess
fallback) was not invoked. The single test-side caveat is that the Deno test sanitizer is strict
about signal-listener leaks during interleaved Ink renders, so the App-level snapshot tests opt out
of `sanitizeOps`/`sanitizeResources` (component-level tests keep both on).

## End-to-end test suite

`tests/e2e/` is gated by `MAKINA_E2E=1` and a small family of `MAKINA_E2E_*` environment variables
naming the sandbox repo, GitHub App credentials, and per-scenario issue numbers (see
`tests/e2e/_e2e_harness.ts`). When the gate is off the tests register cleanly and skip with a
single-line note, so `deno task ci` continues to run on every push without a sandbox dependency.
When the gate is on the harness spawns `packages/cli/main.ts daemon` against a synthetic `HOME`,
drives the supervisor through real GitHub, and observes the FSM via the wildcard event subscription.

Three scenarios cover the verification matrix:

| Scenario         | File                                        | Asserts                                                                 |
| ---------------- | ------------------------------------------- | ----------------------------------------------------------------------- |
| Happy path       | `tests/e2e/happy_path_test.ts`              | `MERGED` without operator intervention.                                 |
| CI-fail recovery | `tests/e2e/ci_fail_recovery_test.ts`        | `STABILIZING/CI` runs at least once, ≥ 2 iterations, lands in `MERGED`. |
| Review-comment   | `tests/e2e/review_comment_recovery_test.ts` | `STABILIZING/CONVERSATIONS` runs at least once, lands in `MERGED`.      |
