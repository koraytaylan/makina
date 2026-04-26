# ADR-002: Headless daemon + thin TUI client

## Status

Accepted (2026-04-26).

## Context

A task that drives a PR end-to-end runs for hours: agent edits, commits, push, CI wait (minutes),
Copilot review wait (minutes), comment addressal, more CI, then merge. If this work lives in the TUI
process, `Ctrl+C` kills it. Three options:

1. **In-process** — agents and polling live in the TUI. Simplest. Fragile to TUI exit.
2. **Headless daemon + TUI client** — agents and polling live in a long-running daemon. The TUI is a
   connector.
3. **Per-task subprocess supervisor** — each task is its own forked process. More isolation, more
   orchestration.

## Decision

A long-running `makina daemon` process owns agents, worktrees, GitHub auth, polling, and persisted
state. The Ink-based `makina` TUI is a client that connects over a Unix domain socket using NDJSON.
The TUI auto-spawns the daemon if it is not already running. Quitting the TUI does not stop the
daemon.

## Consequences

**Positive:**

- Reattachable: a user can `/quit`, run other commands, then relaunch `makina` and pick up exactly
  where they left off.
- Crash isolation: a TUI rendering bug cannot lose hours of in-flight work.
- Simpler TUI code: no business logic, just rendering and IPC.

**Negative:**

- Two processes to reason about; an extra IPC layer to maintain.
- Daemon lifecycle (start, stop, restart, log, crash recovery) is real work — addressed by atomic
  JSON persistence and explicit `/daemon stop`.
- macOS/Linux only for v1 (Windows would require a named-pipe abstraction; see ADR-008).
