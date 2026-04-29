# Lifecycle

The supervisor (`packages/core/src/daemon/supervisor.ts`) walks every issue through a per-task FSM.
The three stabilize sub-phases — rebase, CI, and conversations — are all wired end-to-end and
bounded by `MAX_TASK_ITERATIONS` (default 8): exhaustion in any phase escalates the task to
`NEEDS_HUMAN`.

Every transition the supervisor performs follows the **persist → emit → act** ordering documented in
[ADR-016](adrs/016-supervisor-persist-then-emit-then-act.md): the new state is durably on disk
_before_ it appears on the event bus, and the side effect attached to the destination state runs
only after both. This keeps a daemon crash mid-transition replayable on the next boot.

The TaskSupervisor walks each issue through this state graph:

```
INIT
  → CLONING_WORKTREE
  → DRAFTING                 (initial agent run on the issue body)
  → COMMITTING
  → PUSHING
  → PR_OPEN                  (Copilot requested as reviewer)
  → STABILIZING ↻            (loop until stable)
  → READY_TO_MERGE
  → MERGED | NEEDS_HUMAN | FAILED
```

## The stabilize loop

After every push, the supervisor runs three phases in order. If any phase produces a new commit, the
loop restarts at phase 1 against the just-pushed HEAD. The loop exits cleanly to `READY_TO_MERGE`
only when none of the phases needs to do work AND a configurable settling window has elapsed.

1. **Rebase** — fetch the base branch and `git rebase`. On conflict, dispatch a focused agent run
   with the conflict diff (see [ADR-018](adrs/018-stabilize-rebase-conflict-loop.md)). Bounded by
   `maxIterationsPerTask`. Unresolvable → `NEEDS_HUMAN`.
2. **CI** — poll combined commit status + check runs. On red, fetch failing-job logs trimmed to
   `STABILIZE_CI_LOG_BUDGET_BYTES` (see
   [ADR-019](adrs/019-stabilize-ci-log-budget-and-trim-policy.md) and
   [ADR-022](adrs/022-stabilize-ci-zip-extraction.md) for the ZIP-extraction policy), dispatch the
   agent. Restart at phase 1 after the fix is pushed.
3. **Conversations** — fetch new review comments and review summaries since `lastReviewAt` (see
   [ADR-023](adrs/023-conversations-watermark-monotonicity.md) for the monotonic-watermark guarantee
   and [ADR-020](adrs/020-graphql-via-octokit-core.md) for the GraphQL transport). If any, group
   them and dispatch the agent. After the fix is pushed, resolve threads via GraphQL
   `resolveReviewThread` and re-request Copilot review. Restart at phase 1.

## Merge

Once a task reaches `READY_TO_MERGE`, the supervisor branches on `mergeMode`:

- `squash` and `rebase` auto-merge through `GitHubClient.mergePullRequest`. On success the task
  lands in `MERGED`. On failure, [ADR-021](adrs/021-merge-modes-failure-classification.md)
  classifies the error: HTTP `405` / `409` from the merge endpoint mean the PR is genuinely not
  mergeable (conflicts, base-branch protection, stale head SHA) and escalate the task to
  `NEEDS_HUMAN`; every other failure is transient and lands the task in `FAILED`.
- `manual` parks at `READY_TO_MERGE` without calling the GitHub API. An operator unblocks the task
  by issuing `/merge <task-id>` from the TUI; the daemon dispatches into the supervisor's
  `mergeReadyTask` entry point, which re-uses the same merge step (so failure classification and
  cleanup behave identically). `/merge` rejects with a precise `ack { ok: false, error }` if the
  target task is unknown or not currently at `READY_TO_MERGE`.

After a successful merge, the worktree is removed via `WorktreeManager.removeWorktree` unless
`lifecycle.preserveWorktreeOnMerge` is true. Cleanup failures are logged but never unwind the
`MERGED` transition — `MERGED` is the durable success state.
