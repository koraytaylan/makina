# ADR-007: One git worktree per in-flight task

## Status

Accepted (2026-04-26).

## Context

The product runs many issues concurrently. Each task needs an isolated working directory because the
agent runs file edits, commits, and rebases. Three options:

1. **Single shared checkout, switch branches** — lightest, no parallelism, prone to "wait, which
   branch am I on" bugs.
2. **Fresh `git clone` per task** — strongest isolation, heavy on disk and network.
3. **One bare clone per repo + per-task worktrees on their own branches** — middle ground.

## Decision

Per repo, maintain one bare clone at `<workspace>/repos/<owner>__<name>.git`. Per task, create a
worktree at `<workspace>/worktrees/<owner>__<name>/issue-<n>` on a fresh branch `makina/issue-<n>`.
The WorktreeManager owns lifecycle.

## Consequences

**Positive:**

- True parallelism: agents in different worktrees never share a working directory.
- Lightweight: a worktree is essentially a checked-out file tree pointing at the shared object
  store.
- `git fetch` once per repo (in the bare clone) refreshes objects for every worktree.

**Negative:**

- Worktrees must be removed cleanly with `git worktree remove`; an interrupted daemon can leak
  metadata. Recovery: `git worktree prune` on daemon start.
- The worktree directory layout becomes part of the user-visible workspace; `docs/configuration.md`
  documents it so users can locate a worktree on `NEEDS_HUMAN`.
