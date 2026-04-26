# Architecture

> Wave 1 contracts in place; daemon and TUI bodies implemented progressively across Waves 2–4.

## Process model

Two cooperating processes communicate over a Unix domain socket using NDJSON:

- **`makina daemon`** — long-running supervisor. Owns agents, worktrees, GitHub App auth, polling,
  and persisted state. Survives TUI exit. Lands in Waves 2–4.
- **`makina`** (TUI) — Ink-based React app. Connects to the daemon over the socket; auto-spawns the
  daemon when not running. Lands in Wave 2.

`main.ts` is an argv-dispatch shell: `daemon`, `setup`, or default → TUI.

## Daemon internals

| Component       | File                             | Wave | Purpose                                       |
| --------------- | -------------------------------- | ---- | --------------------------------------------- |
| TaskSupervisor  | `src/daemon/supervisor.ts`       | W3   | Per-issue state machine.                      |
| Stabilize loop  | `src/daemon/stabilize.ts`        | W4   | Rebase → CI → conversations after every push. |
| AgentRunner     | `src/daemon/agent-runner.ts`     | W3   | Wraps `@anthropic-ai/claude-agent-sdk`.       |
| WorktreeManager | `src/daemon/worktree-manager.ts` | W2   | Bare clone + per-task worktrees.              |
| GitHubClient    | `src/github/client.ts`           | W2   | High-level methods over `@octokit/core`.      |
| Poller          | `src/daemon/poller.ts`           | W3   | Per-task polling with backoff.                |
| Persistence     | `src/daemon/persistence.ts`      | W2   | Atomic JSON state store.                      |
| EventBus        | `src/daemon/event-bus.ts`        | W2   | In-process pub/sub.                           |
| Daemon server   | `src/daemon/server.ts`           | W2   | Unix socket listener + dispatch.              |

## TUI

| Component                           | File                  | Wave |
| ----------------------------------- | --------------------- | ---- |
| `App`                               | `src/tui/App.tsx`     | W2   |
| `Header` / `MainPane` / `StatusBar` | `src/tui/components/` | W2   |
| `CommandPalette` / `TaskSwitcher`   | `src/tui/components/` | W3   |

## IPC protocol

Length-prefixed framed envelopes `{ id, type, payload }` with zod schemas in `src/ipc/protocol.ts`
(Wave 1). Each frame is `<decimal-length>\n<utf8-json>\n`; the trailing newline keeps the wire
format human-readable for ad-hoc tooling. The framer lives in `src/ipc/codec.ts` and rejects
malformed frames (oversize, partial, schema-mismatched, non-UTF8) with a typed `IpcCodecError`.
Client → Daemon: `subscribe`, `unsubscribe`, `command`, `prompt`, `ping`. Daemon → Client: `event`,
`ack`, `pong`.

Consumers do not import zod directly: `src/ipc/protocol.ts` exposes typed interfaces and a
`parseEnvelope(raw): ParseEnvelopeResult` function; the same idiom in `src/config/schema.ts` exposes
`parseConfig`. This keeps the public API zod-free so `deno doc --lint` reflects the contract, not
the validator implementation.

## TUI client (W2)

`src/tui/ipc-client.ts` exports a `DaemonClient` interface plus a `SocketDaemonClient` that opens a
Unix-domain socket via `Deno.connect({ transport: "unix" })`, encodes outgoing envelopes through
`src/ipc/codec.ts`, and decodes pushed envelopes back into typed values. Reply correlation is by
envelope id; pushed `event` envelopes fan out through `subscribeEvents`. The interface surface
mirrors `tests/helpers/in_memory_daemon_client.ts` so consumers can swap the real client for the
in-memory double in tests with no other change.

`src/tui/hooks/useDaemonConnection.ts` wraps the client in a React hook that exposes
`{ status, lastError, send, subscribe, connect, disconnect }` and walks the lifecycle
`idle → connecting → connected → disconnected | error`. The hook is transport-agnostic: a client
without `connect`/`close` methods (the in-memory double) starts directly in `connected`.

The Wave-2 shell is intentionally read-only. Wave 3 wires the command palette and task switcher;
Wave 3 also turns the focused-task id into a real selection driven by the task-list component.

### Ink-on-Deno feasibility verdict (issue #10)

Ink 5.2 renders cleanly under Deno 2.7 with `npm:ink`/`npm:react` specifiers. Yoga's WASM layer
loads, the default `process.stdout`/`process.stdin` adapters provided by Deno's Node compatibility
shim work as Ink expects, and `signal-exit`'s 13 signal listeners are released on `unmount()`. The
ADR-001 risk is closed in favor of the original Deno-native plan; ADR-010 (Node-side Ink subprocess
fallback) was not invoked. The single test-side caveat is that the Deno test sanitizer is strict
about signal-listener leaks during interleaved Ink renders, so the App-level snapshot tests opt out
of `sanitizeOps`/`sanitizeResources` (component-level tests keep both on).
