# Current Status and Safety Boundaries

> **Status:** Maintained overview for `develop`.
> **Last reviewed:** 2026-07-15.

Makina is an MVP with a working plan → develop → gate → review → merge loop.
The first real-agent trial is preserved in
[`trial/trial-findings.md`](trial/trial-findings.md), but its gap list is
historical: runtime persistence, permission handling, audit logging, recovery,
and the broader TUI have all evolved since that trial.

## Implemented capabilities

- Dependency-aware concurrent task scheduling with explicit task states.
- Per-task Git branches and worktrees, deterministic gates, agent review, and
  plan-branch integration.
- Atomic task-graph persistence, crash recovery, run metadata, transcripts,
  logs, and permission audit records.
- ACP provider/role configuration, termination caps, retries, reset, and live
  TUI observability.
- Multi-folder discovery with project-qualified plan/run identity. Execution
  dependencies are selected from the plan's repository rather than the folder
  Makina happened to launch from.
- Single-flight run ownership: opening the same canonical project/plan is
  idempotent and only one scheduler generation may own it at a time.

## Safety boundary

Makina's `WorktreePolicy` is a permission gateway, not an operating-system
sandbox. It validates structured ACP path metadata against the assigned
worktree, denies missing or malformed location metadata, rejects lexical and
symlink escapes, selects only one-shot approval, and records the decision.

The external ACP process still inherits the host process environment and may
have network or process access granted by the operating system. Gate commands
are also project-authored shell commands. Run untrusted agents or repositories
inside an OS/container sandbox when host-level containment is required.

## Operational limitations

- Real-agent integration tests require an installed, authenticated ACP CLI and
  remain opt-in; deterministic fake-backend coverage runs by default.
- A clean, isolated Cargo target directory is recommended when testing across
  temporary Git worktrees, because Rust test binaries can embed their build-time
  source root.
- Runtime logs and worktrees are transient. Task-graph artifacts and project
  configuration are the reviewable state intended for version control.

## Sources of truth

- Normative behavior: [`spec/`](spec/)
- Current implementation review and remediation source:
  [`reviews/2026-07-14-codex.md`](reviews/2026-07-14-codex.md)
- Plan history and shipped work: [`plans/STATUS.md`](plans/STATUS.md)
- Historical first trial: [`trial/trial-findings.md`](trial/trial-findings.md)
