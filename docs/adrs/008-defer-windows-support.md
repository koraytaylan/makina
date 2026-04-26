# ADR-008: Defer Windows and x86_64 macOS support

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

v1 ships **Apple Silicon macOS** and **Linux** only. Windows and Intel macOS are explicitly out of
scope. The release pipeline builds for `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, and
`aarch64-unknown-linux-gnu`.

Apple has shipped only Apple Silicon Macs since 2020; the Intel macOS user base shrinks every
quarter, and the GitHub-hosted macos-13 (Intel) runner pool is scarce enough that queue times
exceeded the entire rest of the release workflow. Cross-compiling from a macos-14 (ARM) runner is
technically possible but adds an artifact whose audience is small and shrinking. WSL2 covers Windows
users.

## Consequences

**Positive:**

- Tighter scope; less code surface; fewer moving CI targets.
- Concrete IPC primitives without a transport-abstraction layer.

**Negative:**

- Windows users cannot run `makina` natively. WSL2 works (it is Linux for our purposes); document
  this in the README if users ask.
- Intel-Mac users are not supported. The cohort is small (Apple has been ARM-only since 2020) and
  Rosetta 2 does not help here because the binary is statically linked to a target-specific Deno
  runtime.
- Re-opening either platform later means a transport abstraction PR (Windows) and/or a matrix
  expansion in CI and `release.yml` (either). Tractable; not free.
