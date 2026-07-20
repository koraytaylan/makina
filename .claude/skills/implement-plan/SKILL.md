---
name: implement-plan
description: Execute a committed canonical per-task plan using one lease-owning Rust plan-contract session and dependency-aware worker agents.
---

# implement-plan

Launch the repository workflow; do not parse plan files or perform coordinator Git/status operations yourself.

- Preferred: `Workflow({ name: "implement-plan", args: { plan: "<selector>", ... } })`
- Fallback: `Workflow({ scriptPath: ".claude/workflows/implement-plan.js", args: { plan: "<selector>", ... } })`

`plan` is required and identifies the canonical plan directory. Optional arguments are `dryRun`, `maxParallel` (default `4`), `sequential`, `maxReviewIters` (default `3`), finalization mode, and developer/reviewer model overrides.

The workflow owns only dependency-ready scheduling and host agent calls. One authenticated, reconnectable Rust contract session loads the typed DAG, holds the repository lease, creates private worktrees, validates both candidate-diff checkpoints, and owns registration, claim, blocker/retry/disposition, A/B landing, resume, and P/F/C finalization. Every agent call is bracketed by the exact contract-issued `BeginWorker`/`EndWorker` handle. Workers never edit coordinator status paths or mutate Git topology.

Ambiguous refs, worktrees, evidence, or worker lifetime block safely and remain available for recovery. `dryRun` is read-only. The workflow never pushes; retained Stage/Manual finalization is reported explicitly.
