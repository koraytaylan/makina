# Architecture

> Wave 0 stub. Implemented progressively across Waves 2–4.

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

NDJSON envelopes `{ id, type, payload }` with zod schemas in `src/ipc/protocol.ts` (W1). Client →
Daemon: `subscribe`, `unsubscribe`, `command`, `prompt`, `ping`. Daemon → Client: `event`, `ack`,
`pong`.
