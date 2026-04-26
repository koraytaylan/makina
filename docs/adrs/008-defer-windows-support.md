# ADR-008: Defer Windows support

## Status

Accepted (2026-04-26).

## Context

The daemon-TUI IPC uses Unix domain sockets, which Windows has supported only since 2018 and which
still surface oddities through Node/Deno's net module. The git-worktree path layout uses POSIX-style
separators. The `which claude` discovery and the `~/Library/Application Support` path conventions
are POSIX-leaning.

We could abstract the transport (named pipes on Windows), normalize paths, and add a Windows runner
to CI. The cost is real: ~2–3 days of work plus a permanent ongoing test surface.

## Decision

v1 ships macOS and Linux only. Windows is explicitly out of scope. The release pipeline builds for
`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, and
`aarch64-unknown-linux-gnu`.

## Consequences

**Positive:**

- Tighter scope; less code surface; fewer moving CI targets.
- Concrete IPC primitives without a transport-abstraction layer.

**Negative:**

- Windows users cannot run `makina` natively. WSL2 works (it is Linux for our purposes); document
  this in the README if users ask.
- Re-opening Windows support later means a transport abstraction PR plus matrix expansion in CI and
  `release.yml`. Tractable; not free.
