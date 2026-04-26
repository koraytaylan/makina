# ADR-001: Deno runtime with `npm:` specifiers

## Status

Accepted (2026-04-26).

## Context

The implementation engine (`@anthropic-ai/claude-agent-sdk`) and the TUI library (`ink`) are both
npm packages. Ink in particular targets Node.js officially; Deno support is unsupported but
available through Deno's `npm:` specifier compatibility layer. We had three runtime choices:

1. **Bun** — Node-compatible, fast, batteries-included, runs Ink natively.
2. **Deno + `npm:` specifiers** — official choice; risk in Yoga (WASM) and raw-mode TTY.
3. **Node + tsx** — most proven path for Ink.

## Decision

Build on Deno 2.7+ using `npm:` specifiers for Ink, React, Octokit, the Claude Agent SDK, and zod.
Use JSR `@std/*` for everything where it offers parity.

## Consequences

**Positive:**

- A single first-party toolchain (formatter, linter, test runner, type checker, lockfile, doc
  generator, compile-to-binary) without assembling an npm bestiary.
- Permissioned execution model fits a tool that spawns subprocesses and writes outside its
  workspace.
- The Claude Agent SDK officially supports Deno via `executable: 'deno'`.

**Negative:**

- Ink-on-Deno is unsupported. Phase-1 spike treats this as a feasibility gate; if Yoga or `node:tty`
  raw-mode fails, ADR-010 (TBD if invoked) defines the Node-side Ink subprocess fallback.
- Some npm packages have optional native deps that Deno ignores; for the Claude Agent SDK we
  sidestep this by pointing `pathToClaudeCodeExecutable` at the user's globally-installed `claude`
  binary.

**Trade-off accepted:** the Ink-on-Deno risk in exchange for tooling simplicity. The mitigation is
upfront verification, not late-stage scrambling.
