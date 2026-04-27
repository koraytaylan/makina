# ADR-021 — Merge-mode failure classification and `/merge` re-entry surface

## Status

Accepted (Wave 4, issue #18).

## Context

Wave 3 (#12) shipped the supervisor skeleton, including a stub at the `READY_TO_MERGE` step that
performed an unconditional auto-merge or parked the task for `manual` mode. Wave 4 needs to fill in
the real merge step:

1. **Three merge modes.** `squash` and `rebase` auto-merge through `GitHubClient.mergePullRequest`;
   `manual` waits for an operator-issued `/merge <task-id>` slash command.
2. **Cleanup.** Configuration `lifecycle.preserveWorktreeOnMerge` controls whether the per-task
   worktree is removed after a successful merge.
3. **Failure handling.** `mergePullRequest` can fail for two structurally different reasons:
   - The PR is genuinely **not mergeable** (conflicts, base-branch protection, stale head SHA). The
     code is fine; an operator must unblock the merge by hand. Escalating the task to `FAILED` would
     lose the work.
   - The API call faulted for **transient** reasons (network glitch, 5xx). A follow-up retry is
     reasonable; for now the task lands in `FAILED` so the operator decides whether to re-issue
     `/merge` or restart the supervisor for that pair.

The `/merge` slash command parser already exists on `develop` (W3 #14); this ADR records the
daemon-side wiring choice the merge-modes branch is making.

## Decision

### 1. Merge runs through one shared step, regardless of entry point.

`runMergeStep(task, mode)` is the single function that:

- Validates `task.prNumber` is present.
- Refuses to call the GitHub API with `mode === "manual"` (defensive — only `squash` / `rebase` are
  valid `merge_method` values).
- Calls `GitHubClient.mergePullRequest(repo, prNumber, mode)`.
- Classifies any thrown error via `classifyMergeError()`.
- Performs cleanup per `preserveWorktreeOnMerge`.

Both the auto-merge path (the FSM's `READY_TO_MERGE` driver) and the manual-merge path
(`mergeReadyTask`, wired to `/merge`) call into `runMergeStep`. This guarantees identical behaviour
for cleanup, event-bus emission, and failure classification regardless of which trigger started the
merge.

### 2. Failure classification is keyed on HTTP status, not error message text.

`classifyMergeError(error)` extracts the `status` field from the thrown error (matching
`@octokit/request-error`'s shape) and:

- Treats `405` and `409` as **`not-mergeable`** → escalates to `NEEDS_HUMAN`. The constant
  `MERGE_NOT_MERGEABLE_HTTP_STATUSES = [405, 409]` is exported so unit tests assert against the same
  source of truth as production.
- Treats every other error (no status, 4xx other than the above, 5xx) as **`transient`** → escalates
  to `FAILED`.

We chose status-based classification rather than parsing GitHub's free- text response messages
because:

- GitHub localises and rephrases error messages without notice; a string match is fragile.
- The `405` response for "Pull Request is not mergeable" is a long-standing API contract documented
  under the [merge a PR endpoint](https://docs.github.com/en/rest/pulls/pulls#merge-a-pull-request).
- The classifier is open for extension: future statuses (e.g. a future `412 Precondition Failed` for
  stricter base-branch checks) flip a single constant rather than a regex.

### 3. `mergeReadyTask` is the synchronous-failure entry point for `/merge`.

The slash-command dispatcher needs to reject misuse with a precise
`ack { ok: false, error: "..." }`. Four cases must be distinguished before any state transition is
attempted:

- **Unknown task id** — `kind: "unknown-task"`.
- **Task is not at `READY_TO_MERGE`** — `kind: "not-ready-to-merge"`.
- **Concurrent `/merge` for the same task** — `kind: "merge-in-flight"`.
- **Caller passed `overrideMode === "manual"`** — `kind: "merge-precondition"`. `manual` is not a
  GitHub merge strategy; rejecting it at the entry point keeps a caller bug from landing the task in
  `FAILED` via `runMergeStep`'s defensive check.

All four cases throw a `SupervisorError` carrying a `kind` discriminator the daemon's command
handler reads. FSM-internal failures (the merge itself faulting after the preconditions pass) do
**not** throw — they land the task in `MERGED`, `NEEDS_HUMAN`, or `FAILED`. The handler inspects the
returned task: `MERGED` becomes `ack { ok: true }`; `NEEDS_HUMAN` and `FAILED` become
`ack { ok: false, error: "...final state: <STATE>: <terminalReason>" }` so clients can distinguish
"merged" from "escalate-to-human" via the ack alone (in addition to the `state-changed` event
stream).

### 4. `manual` substitutes `squash` when reaching the GitHub API.

GitHub's `merge_method` accepts `merge`, `squash`, `rebase` only — `manual` is a makina-side concept
meaning "wait for /merge". When `mergeReadyTask` fires for a task whose own `mergeMode` is `manual`,
the supervisor substitutes `"squash"` as the API strategy unless the caller passes `overrideMode`.
The override itself is constrained: passing `overrideMode === "manual"` is rejected synchronously
(see §3) since substituting `manual` for `manual` would be a no-op tautology. Operators can request
a specific strategy by extending `/merge <task-id>` with a future flag (out of scope for this ADR);
the supervisor surface already accepts the override.

### 5. Cleanup failures never unwind the merge.

`MERGED` is the durable success transition. If `removeWorktree` rejects after a successful API merge
(e.g. the workspace is read-only), the supervisor logs a warning and leaves the task at `MERGED`.
The worktree path is recoverable via `task.worktreePath` for manual cleanup; reverting the FSM to a
non-terminal state would lie about what happened on GitHub's side.

## Consequences

- The merge step has a single error budget: every observable failure becomes a deterministic FSM
  transition with a specific `terminalReason` prefix (`"merged"` / `"merge (not-mergeable): …"` /
  `"merge: …"`). Operators can grep on those.
- `mergeReadyTask` is exported on `TaskSupervisorImpl` but **not** on the W1 `TaskSupervisor`
  contract — the daemon command dispatcher imports the wider implementation interface, while
  consumers that only need the cross-wave surface (TUI, persistence) stay decoupled.
- Adding a new "non-mergeable" HTTP status is a one-line change in
  `MERGE_NOT_MERGEABLE_HTTP_STATUSES`. No FSM logic moves.
- The `/merge` command's daemon wiring lives in the test fixture for this branch only; a follow-up
  wave that constructs the supervisor inside `main.ts daemon` is responsible for promoting the
  wiring to production. The classifier and the supervisor surface are the load-bearing contracts
  that survive that wiring change.
