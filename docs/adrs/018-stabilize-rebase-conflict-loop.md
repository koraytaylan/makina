# ADR-018: Stabilize-rebase phase — fetch, rebase, agent-resolve loop

## Status

Accepted (2026-04-26).

## Context

Issue #15 (Wave 4) ships the rebase sub-phase of the stabilize loop. After every push, the
supervisor must reconcile the per-task feature branch with the base branch so the open PR stays
mergeable while sibling tasks are landing. ADR-006 settled the polling-vs-webhooks question for
downstream phases (CI and conversations); the rebase phase is push-driven and synchronous: each call
follows a single push and resolves with one of three observable outcomes (clean, conflict, fatal git
error).

The implementation has to satisfy four constraints simultaneously:

1. **Bounded.** Conflict resolution dispatches an agent that may itself re-introduce conflicts; an
   unbounded loop on a perpetually-conflicting rebase would spin the supervisor forever. The Wave 1
   `MAX_TASK_ITERATIONS` cap is the natural ceiling.
2. **Worktree-preserving.** When the rebase exhausts its iteration budget, the operator inspects the
   worktree to finish the resolution by hand. Tearing down the worktree on `NEEDS_HUMAN` would erase
   the in-progress edits the agent attempted.
3. **Deterministic for tests.** Every collaborator that reaches outside the module — `git`, the
   filesystem, the agent runner — is injected so unit tests stay millisecond-fast and never spawn
   subprocesses. The integration test in `tests/integration/supervisor_end_to_end_test.ts` exercises
   the production `Deno.Command` path against a real `file://` source repo so the default bindings
   are not stale.
4. **Bare-clone friendly.** The Wave 2 `WorktreeManagerImpl` configures per-repo bare clones without
   `remote.origin.fetch`. A naive `git fetch origin <baseBranch>` populates `FETCH_HEAD` but does
   not write `refs/remotes/origin/<baseBranch>`, so a follow-up `git rebase origin/<baseBranch>`
   fails with "fatal: invalid upstream". The phase has to make the remote-tracking ref explicit.

Wave 3's #12 brief left `runStabilizing` in `src/daemon/supervisor.ts` as three back-to-back stubs
(REBASE → CI → CONVERSATIONS) that each emitted the matching `STABILIZING(<phase>)` self-transition
and exited to `READY_TO_MERGE`. The integration test asserts the observable timeline of those
events; #15 must preserve that timeline so the sibling Wave 4 issues (#16, #17) can replace the CI
and CONVERSATIONS stubs in their own PRs without contention.

## Decision

The rebase phase lives in a new `src/daemon/stabilize.ts` module exporting one entry point,
`runRebasePhase(opts)`. The supervisor's `runStabilizing` helper publishes the `STABILIZING(REBASE)`
self-transition, calls `runRebasePhase`, and on a clean result advances through the still-stub CI
and CONVERSATIONS sub-phases to `READY_TO_MERGE`.

### Module split rationale

The stabilize sub-phases are wide enough that growing them inside `supervisor.ts` would make the FSM
hard to read; pulling each into its own module keeps the supervisor's switch statement focused on
state transitions and lets the rebase, CI, and conversations phases evolve independently. Wave 4's
sibling issues (#16, #17) extend the same pattern by adding `runCiPhase` and `runConversationsPhase`
exports to the same module.

### Cadence

The rebase phase is **synchronous against a single push**, not poller-driven. The poller (ADR-017)
governs the CI and conversations phases; the rebase phase runs once per stabilize entry and either
resolves to `kind: "clean"` (advance to CI) or `kind: "needs-human"` (transition to `NEEDS_HUMAN`).
Fatal git errors throw `StabilizeRebaseError` and the supervisor lands the task in `FAILED`.

The phase does not retry the initial fetch on transient failures; the supervisor's higher-level
state machine is responsible for the next attempt (a fresh task `start`, in Wave 4's model). Adding
a `Poller`-style retry inside the rebase phase would conflate two different cadences (the
push-synchronous rebase vs. the poll-driven CI/conversations phases) and split the rate-limit
honoring across two layers.

### Conflict-resolution loop

When `git rebase` exits non-zero, the phase iterates up to `maxIterations` (default
`MAX_TASK_ITERATIONS`) times:

1. List unmerged files via `git diff --name-only --diff-filter=U`. Empty list with a non-zero rebase
   exit code is fatal — typical when the rebase aborted mid-flight and the worktree is clean.
2. Build the conflict-context prompt (issue number, base branch, file list, conflict-marker preview
   per file capped at `STABILIZE_REBASE_CONFLICT_FILE_PREVIEW_CHARS`).
3. Dispatch the agent runner with the prompt, threading the SDK session id forward across iterations
   so the model carries context.
4. Stage every edit with `git add -A`. A non-zero exit here is fatal — the worktree is in a broken
   state the agent cannot recover from.
5. Continue the rebase via `git rebase --continue`. Three outcomes:
   - Exit 0 → resolve with `kind: "clean", iterations: <n>`.
   - Exit non-zero with non-empty conflict set → loop again.
   - Exit non-zero with empty conflict set → fatal (a non-conflict rebase failure).

On exhaustion the phase captures the final conflict set, attempts `git rebase --abort` so a
follow-up `git status` is sane, and resolves with `kind: "needs-human"`. The abort is best-effort:
its failure is logged but does not mask the `NEEDS_HUMAN` signal — the worktree is preserved either
way.

### Bare-clone-aware fetch

The phase fetches with an explicit refspec, `<baseBranch>:refs/remotes/origin/<baseBranch>`, and
rebases against `refs/remotes/origin/<baseBranch>`. The explicit refspec writes the remote-tracking
ref even on a bare clone configured without `remote.origin.fetch` (which is how the Wave 2 worktree
manager sets up per-repo bare clones via `git clone --bare`). Without the explicit refspec, the
fetch updates only `FETCH_HEAD` and the rebase fails with "fatal: invalid upstream
'origin/<baseBranch>'".

### Domain error

Non-conflict failures (auth, missing refspec, malformed worktree) surface as `StabilizeRebaseError`
carrying an `operation` tag (`"fetch"`, `"rebase-start"`, `"rebase-continue"`, `"add"`,
`"diff-conflicts"`, `"agent-resolve"`, `"validate"`). The supervisor catches the error and
transitions the task to `FAILED` with `terminalReason` derived from the message and operation.

The narrow taxonomy mirrors ADR-017's `PollerError` shape: a small set of operations is enough for
the supervisor to render a precise event without unpacking the cause chain, and consumers reading
persistence (the TUI's recovery view, an operator running `jq`) see the same labels the supervisor
log lines emit.

### Injected collaborators

Three collaborators are injected through the options bag:

- `gitInvoker: (args, { cwd }) => Promise<{exitCode, stdout, stderr}>` — defaults to a
  `Deno.Command("git", …)`-backed implementation. Tests pass a scripted double.
- `conflictFileReader: (path) => Promise<string>` — defaults to `Deno.readTextFile`. Tests pass a
  scripted reader.
- `agentRunner: AgentRunner` — production passes the daemon's real `AgentRunnerImpl`; tests pass the
  `MockAgentRunner` from `tests/helpers/mock_agent_runner.ts`.

Per Lesson #3 from the Wave 3 brief, no `Deno.Command`, `Deno.readTextFile`, or other process global
is invoked outside the default-factory bindings at the bottom of `stabilize.ts`.

### Cross-platform paths

The phase normalises conflict-file paths emitted by `git diff` (forward-slash separators on every
host) to the host separator before joining them with the worktree path. The branch on
`Deno.build.os` is conservative — Wave 1 deferred Windows support per ADR-008 — but keeps the rebase
phase forward-compatible without pulling a heavier abstraction.

## Consequences

**Positive:**

- Sub-phase logic lives in its own module, so the supervisor's FSM stays a clean switch statement.
  Sibling Wave 4 issues (CI, conversations) extend the pattern with their own exports.
- Conflict resolution is bounded by the same `MAX_TASK_ITERATIONS` constant the rest of the daemon
  uses; no new knob.
- Worktree preservation on `NEEDS_HUMAN` makes the operator's recovery path obvious — they `cd` into
  the worktree, finish the resolution by hand, and either `git rebase --continue` themselves or use
  the TUI's retry path.
- The injected collaborators keep unit tests millisecond-fast (no real `git`, no real filesystem)
  and the integration test exercises the production `Deno.Command` path end-to-end.

**Negative:**

- The phase does not retry transient git failures (network blips during fetch). The supervisor's
  higher-level state machine handles retries via fresh `start` calls, but a flaky network during
  rebase will land the task in `FAILED` rather than retrying. Mitigated by the Wave 4 plan to
  surface `FAILED` tasks for operator-driven retry; if the failure-rate becomes a real signal we
  would add a small fixed-attempt retry inside `runRebasePhase` (no new ADR needed since the options
  bag is the only contract surface).
- The conflict-context prompt embeds full file content (capped at 16 KiB per file) rather than
  parsing out the conflict markers and surrounding lines. The agent has the full context, but for
  very wide diffs the prompt grows linearly in conflicting-file count. Acceptable for the current
  workload; a smarter trimmer is a future optimisation.

## Alternatives considered

- **Rebase against `FETCH_HEAD`.** Rejected: `FETCH_HEAD` is a single ref shared across every fetch,
  so two concurrent rebases on different worktrees of the same bare clone would race on its
  contents. The explicit `refs/remotes/origin/<baseBranch>` refspec gives each fetch a stable target
  and is the canonical "remote-tracking" idiom.
- **Use the worktree manager's `runGit` directly.** Rejected: `runGit` redacts URL credentials and
  throws on non-zero exits, both of which the rebase phase has to bypass. Re-exporting it would bake
  the worktree manager's invariants into a layer that needs a different policy. The `gitInvoker`
  shim is six lines and keeps the contracts decoupled.
- **Inline the rebase phase in `supervisor.ts`.** Rejected: the supervisor is already the longest
  module in `src/daemon/`. Adding the rebase loop, the conflict-prompt builder, and the git-invoker
  default would push it past the readability threshold. Splitting also lets sibling Wave 4 issues
  edit `stabilize.ts` without touching the supervisor's FSM.
