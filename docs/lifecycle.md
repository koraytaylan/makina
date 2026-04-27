# Lifecycle

> Wave 3 ships the supervisor skeleton (#12); the three `STABILIZING` sub-phases are stubs that
> immediately fall through to `READY_TO_MERGE`. Wave 4 fills them in.

Every transition the supervisor performs follows the **persist → emit → act** ordering documented in
[ADR-015](adrs/015-supervisor-persist-then-emit-then-act.md): the new state is durably on disk
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
   with the conflict diff. Bounded by `maxIterationsPerTask`. Unresolvable → `NEEDS_HUMAN`.
2. **CI** — poll combined commit status + check runs. On red, fetch failing-job logs trimmed to a
   configurable byte budget, dispatch the agent. Restart at phase 1 after the fix is pushed.
3. **Conversations** — fetch new review comments and review summaries since `lastReviewAt`. If any,
   group them and dispatch the agent. After the fix is pushed, resolve threads via GraphQL
   `resolveReviewThread` and re-request Copilot review. Restart at phase 1.

## Merge

When `mergeMode` is `squash` or `rebase`, the supervisor performs the merge via the GitHub API once
stable. `manual` stops at `READY_TO_MERGE` and waits for `/merge`.
