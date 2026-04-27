/**
 * daemon/supervisor.ts — per-task state machine that walks an issue
 * end-to-end through the lifecycle defined in `docs/lifecycle.md`.
 *
 * Wave 3 (#12) ships the skeleton: the supervisor drives a task through
 * `INIT → CLONING_WORKTREE → DRAFTING → COMMITTING → PUSHING → PR_OPEN
 *  → STABILIZING → READY_TO_MERGE → MERGED | NEEDS_HUMAN | FAILED`.
 * Wave 4 fills in the `STABILIZING` sub-phases:
 *
 *  - **CI** (#16, this PR) — polls `getCombinedStatus` + `listCheckRuns`
 *    at the configured cadence; on red, fetches failing-job logs trimmed
 *    to {@link STABILIZE_CI_LOG_BUDGET_BYTES}, dispatches the agent, and
 *    restarts polling on the post-fix head SHA. Bounded by
 *    {@link MAX_TASK_ITERATIONS} → `NEEDS_HUMAN` on exhaustion.
 *  - **REBASE** (#15) and **CONVERSATIONS** (#17) — still stubs that
 *    publish their `state-changed` event so observers see the expected
 *    timeline, then yield back to the loop. Sibling agents replace them
 *    in their own PRs.
 *
 * **Invariants the FSM upholds.**
 *
 *  - **Persist-then-emit-then-act.** Every transition first writes the
 *    new {@link Task} record through {@link Persistence.save}, then
 *    publishes a `state-changed` {@link TaskEvent} on the bus, and only
 *    then performs the transition's observable side effect (creating a
 *    worktree, opening a PR, requesting reviewers, merging, etc.). A
 *    crash *between* persistence and the side effect means the next
 *    daemon boot sees the task in the new state and re-issues the side
 *    effect — observers never see a side effect for a state the store
 *    does not know about. (The TUI subscriber listens to the event bus
 *    directly; persistence is what the daemon reloads from on restart.)
 *
 *  - **Branded ids only.** Task ids, repo names, issue numbers, and
 *    PR numbers all flow through `makeTaskId`, `makeRepoFullName`,
 *    `makeIssueNumber`. No primitive cast is allowed. The id-mint
 *    helper {@link mintTaskId} runs `makeTaskId` so the runtime brand
 *    invariant is enforced.
 *
 *  - **Copilot reviewer requested on `PR_OPEN`.** Immediately after the
 *    transition into `PR_OPEN` is persisted and emitted, the supervisor
 *    calls `requestReviewers([COPILOT_REVIEWER_LOGIN])`. Failure of that
 *    call escalates the task to `FAILED` with the reason recorded — the
 *    supervisor never silently proceeds without a reviewer.
 *
 *  - **One task per (repo, issue) pair.** `start()` rejects if there is
 *    already a non-terminal task for the same `(repo, issueNumber)`. A
 *    terminal task (`MERGED`, `NEEDS_HUMAN`, `FAILED`) does not block a
 *    subsequent start — operators retry by spinning up a fresh task.
 *
 * **What this skeleton does not do.** Polling, real stabilize phases,
 * conflict-driven agent re-invocation, and the agent re-iterations on
 * red CI all land in Wave 4. The interface surface here is the same:
 * `start()` returns the final state, every transition appears on the
 * event bus, and persistence replay reconstructs the task identically
 * regardless of which wave fills in the body.
 *
 * @module
 */

import { getLogger } from "@std/log";

import {
  HEX_BYTE_WIDTH_CHARACTERS,
  HEX_RADIX,
  MAX_TASK_ITERATIONS,
  POLL_INTERVAL_MILLISECONDS,
  STABILIZE_CI_LOG_BUDGET_BYTES,
  STABILIZE_CI_MAX_CONSECUTIVE_FETCHER_ERRORS,
  TASK_ID_RANDOM_SUFFIX_BYTES,
  TASK_ID_RANDOM_SUFFIX_LENGTH_CHARACTERS,
} from "../constants.ts";
import {
  type AgentRunner,
  type EventBus,
  type GitHubClient,
  type IssueNumber,
  makeTaskId,
  type MergeMode,
  type Persistence,
  type PullRequestDetails,
  type RepoFullName,
  type StabilizePhase,
  type Task,
  type TaskEvent,
  type TaskId,
  type TaskState,
} from "../types.ts";
import type { CheckRunSummary, StabilizeGitHubClient } from "../github/client.ts";
import { createPoller, PollerError, type PollerImpl } from "./poller.ts";
import type { WorktreeManagerImpl } from "./worktree-manager.ts";

/**
 * GitHub login the supervisor requests as reviewer the moment a PR is
 * opened. Hard-coded by the spec (`gh issue view 12`); centralised so
 * the test asserts against the same constant the production code uses.
 */
export const COPILOT_REVIEWER_LOGIN = "Copilot";

/**
 * Base branch name used when the supervisor creates a pull request.
 *
 * The skeleton always opens PRs against `main` — Wave 4 will take the
 * task's tracked branch from configuration once the loader exposes it.
 * Centralised rather than inlined so the constant is visible to tests
 * and future code review.
 */
export const DEFAULT_BASE_BRANCH = "main";

/**
 * Narrow logger surface used by the supervisor. The `@std/log` `Logger`
 * class satisfies this shape (its `warn` accepts a string), but the
 * narrower interface lets tests inject a recording double without
 * matching the SDK's full overload signature.
 */
export interface SupervisorLogger {
  /** Emit an informational line. */
  info(message: string): void;
  /** Emit a warning. */
  warn(message: string): void;
}

/**
 * Wall-clock source. The supervisor takes ISO-8601 timestamps for
 * `Task.createdAtIso` / `updatedAtIso` and `TaskEvent.atIso` from
 * `clock.nowIso()`. Default is `() => new Date().toISOString()`; tests
 * inject a deterministic clock so persistence-replay assertions can
 * compare records by deep equality.
 */
export interface SupervisorClock {
  /**
   * Return the current wall-clock time as an ISO-8601 string.
   *
   * @returns The current instant rendered with `Date.prototype.toISOString`.
   */
  nowIso(): string;
}

/**
 * Random-suffix source used by {@link mintTaskId}.
 *
 * The default reads from `crypto.getRandomValues`; tests inject a
 * deterministic generator so persistence-replay assertions can compare
 * records verbatim.
 */
export interface SupervisorRandomSource {
  /**
   * Fill `bytes` with cryptographically-random (or test-deterministic)
   * values, in place. The default implementation calls
   * `crypto.getRandomValues`.
   *
   * @param bytes Buffer to populate. The supervisor mints task ids by
   *   hex-encoding the post-fill contents.
   */
  fillRandomBytes(bytes: Uint8Array): void;
}

/**
 * Construction-time wiring for {@link createTaskSupervisor}.
 *
 * Every collaborator is injected — the supervisor performs **no** module
 * imports of `Deno.*` syscalls beyond what the constructor already pulls
 * in transitively. That is what lets the integration test stand up a
 * real {@link WorktreeManagerImpl}, a real {@link Persistence}, and a
 * real {@link EventBus} without any global mutation while still using
 * the in-memory {@link InMemoryGitHubClient} double for the GitHub side.
 */
export interface TaskSupervisorOptions {
  /** GitHub client used to open PRs, request reviewers, and merge. */
  readonly githubClient: GitHubClient;
  /** Worktree manager owning the per-task git checkouts. */
  readonly worktreeManager: WorktreeManagerImpl;
  /** Persistence used to durably record every state transition. */
  readonly persistence: Persistence;
  /** Event bus used to publish every transition and log line. */
  readonly eventBus: EventBus;
  /** Agent runner used during the `DRAFTING` phase. */
  readonly agentRunner: AgentRunner;
  /**
   * Clone URL builder. Wave 3's daemon will eventually inject a function
   * that mints a per-installation HTTPS URL with a short-lived token; the
   * supervisor receives the resolved URL whenever it asks. Centralising
   * the lookup here keeps the supervisor agnostic of token rotation.
   *
   * Defaults to `https://github.com/<repo>.git` so test setups that
   * point a {@link WorktreeManagerImpl} at a `file://` source can pass
   * the URL through directly.
   */
  readonly cloneUrlFor?: (repo: RepoFullName) => string;
  /**
   * Optional clock override. Production passes `undefined`; tests pass
   * a deterministic clock so persisted records compare verbatim across
   * a save/load round trip.
   */
  readonly clock?: SupervisorClock;
  /**
   * Optional logger override. Defaults to the `@std/log` default-namespace
   * logger adapted to the {@link SupervisorLogger} surface.
   */
  readonly logger?: SupervisorLogger;
  /**
   * Optional random-suffix override used by {@link mintTaskId}. Tests
   * inject a deterministic generator so persistence replay can assert
   * the full {@link Task} shape; production leaves this unset and uses
   * `crypto.getRandomValues`.
   */
  readonly randomSource?: SupervisorRandomSource;
  /**
   * Optional poller used by the stabilize loop's CI phase to space its
   * `getCombinedStatus` / `listCheckRuns` reads. Defaults to a fresh
   * {@link createPoller} instance with the standard cadence-and-backoff
   * defaults; tests inject a recording poller (or a poller backed by a
   * synthetic clock) so cadence assertions are deterministic.
   *
   * The supervisor binds to the wider {@link PollerImpl} surface so the
   * CI phase can pass an `onError` callback (the W1 {@link Poller}
   * contract is intentionally narrower; {@link PollerImpl} extends it
   * additively per ADR-017).
   */
  readonly poller?: PollerImpl;
  /**
   * Optional poll cadence for the stabilize-CI loop, in milliseconds.
   * Defaults to {@link POLL_INTERVAL_MILLISECONDS}. Tests pin this to a
   * small value so a `green-on-first-poll` timeline does not consume
   * real wall-clock time even with a real poller.
   */
  readonly pollIntervalMilliseconds?: number;
  /**
   * Optional upper bound on agent iterations spent inside the stabilize
   * loop's CI fix loop before the supervisor escalates to
   * {@link "NEEDS_HUMAN"}. Defaults to {@link MAX_TASK_ITERATIONS}.
   * The bound is shared across stabilize sub-phases — the rebase phase
   * and the conversations phase debit the same budget — so a task that
   * already burned half its iterations on a rebase gets the remainder
   * for the CI fix loop. The supervisor reads `task.iterationCount` from
   * persistence so the budget survives a daemon restart.
   */
  readonly maxIterations?: number;
  /**
   * Optional upper bound on the per-failing-job log excerpt the CI phase
   * forwards to the agent prompt, in bytes. Defaults to
   * {@link STABILIZE_CI_LOG_BUDGET_BYTES} (100 KB).
   */
  readonly ciLogBudgetBytes?: number;
  /**
   * Optional resolver for the head SHA of the per-task branch. Returns
   * the SHA the CI phase should poll. Called once on entry to the CI
   * phase and once after each agent fix iteration so the supervisor
   * polls CI for the **new** head commit. Defaults to a stub that
   * returns the SHA captured from the PR-open response and never
   * advances; production wires this to a `git rev-parse` against the
   * worktree.
   *
   * @param task The task whose branch head should be resolved.
   * @returns The current head SHA of `task.branchName`.
   */
  readonly resolveHeadSha?: (task: Task) => Promise<string>;
}

/**
 * Public arg shape for {@link TaskSupervisorImpl.start}.
 *
 * Aliased here so callers can write
 * `supervisor.start({ repo, issueNumber, mergeMode })` without
 * memorising the long parameter list each time.
 */
export interface StartTaskArgs {
  /** Target repository. */
  readonly repo: RepoFullName;
  /** Issue number within `repo`. */
  readonly issueNumber: IssueNumber;
  /**
   * Merge strategy to use once the task reaches `READY_TO_MERGE`.
   * Defaults to `"squash"`.
   */
  readonly mergeMode?: MergeMode;
  /**
   * Anthropic model id for agent runs. Defaults to
   * {@link DEFAULT_AGENT_MODEL}.
   */
  readonly model?: string;
}

/**
 * Default Anthropic model id used by `start()` when the caller omits
 * `model`. Aligns with the Wave 1 sample in `src/types.ts`.
 */
export const DEFAULT_AGENT_MODEL = "claude-sonnet-4-6";

/**
 * Wider supervisor surface returned by {@link createTaskSupervisor}.
 *
 * Extends the W1 {@link TaskSupervisor} contract with `start(args)`
 * that returns the *final* state — the W1 surface is `startTask` which
 * returns only the {@link TaskId}. The integration test drives the
 * happy path via `start()` because the assertion target is the merged
 * task, not the id; the wider surface stays out of the cross-wave
 * contract file so consumers (TUI, daemon server) are not coupled to
 * it.
 */
export interface TaskSupervisorImpl {
  /**
   * Start a new task and walk it through the FSM until it reaches a
   * terminal state. Resolves with the final {@link Task} record once
   * the merge (or escalation) completes.
   *
   * @param args Task descriptor: repo, issue, merge mode, and model.
   * @returns The final persisted task record, including its terminal
   *   `state` and `terminalReason`.
   * @throws SupervisorError when the FSM cannot make progress *and*
   *   the failure is not catchable inside the FSM itself (e.g. a
   *   double-start for the same `(repo, issueNumber)`). FSM-internal
   *   failures (worktree, PR open, merge) are absorbed into a `FAILED`
   *   transition rather than surfaced as throws.
   */
  start(args: StartTaskArgs): Promise<Task>;

  /**
   * Snapshot every task the supervisor knows about, in start order.
   *
   * @returns Defensive copy of the in-memory task table.
   */
  listTasks(): readonly Task[];

  /**
   * Look up a single task by id, if known.
   *
   * @param taskId The task identifier to look up.
   * @returns The current in-memory record, or `undefined` if no task
   *   with that id exists.
   */
  getTask(taskId: TaskId): Task | undefined;
}

/**
 * Domain error class thrown by the supervisor for caller-visible
 * failures (double-start, unknown task, brand violation). FSM-internal
 * failures (worktree creation, PR open, merge) are *not* thrown — they
 * land the task in `FAILED` and the caller observes the terminal
 * record's `terminalReason`.
 *
 * The class wraps the underlying exception via `cause` so log readers
 * can recover the original stack.
 *
 * @example
 * ```ts
 * try {
 *   await supervisor.start({ repo, issueNumber });
 * } catch (error) {
 *   if (error instanceof SupervisorError) {
 *     console.error(error.message);
 *   } else {
 *     throw error;
 *   }
 * }
 * ```
 *
 * @throws by {@link TaskSupervisorImpl.start} when the FSM rejects the
 *   call before any state transition is persisted.
 */
export class SupervisorError extends Error {
  /** Discriminator visible in stack traces and `error.name === ...` checks. */
  override readonly name = "SupervisorError";

  /**
   * Build a supervisor error.
   *
   * @param message Human-readable description.
   * @param options Optional standard `cause` carrying the underlying
   *   exception.
   */
  constructor(message: string, options?: { readonly cause?: unknown }) {
    super(message, options);
  }
}

/**
 * Construct a {@link TaskSupervisorImpl} with the given collaborators.
 *
 * The factory does **no** IO; it returns an object whose `start()`
 * walks the FSM the first time it is called. Multiple supervisors over
 * the same `(persistence, eventBus, worktreeManager)` would race on
 * persistence and would not coordinate task-id minting; the daemon
 * constructs exactly one per process.
 *
 * @param opts Collaborators and overrides. See {@link TaskSupervisorOptions}.
 * @returns A {@link TaskSupervisorImpl} ready to drive tasks.
 *
 * @example
 * ```ts
 * import { createTaskSupervisor } from "./supervisor.ts";
 *
 * const supervisor = createTaskSupervisor({
 *   githubClient,
 *   worktreeManager,
 *   persistence,
 *   eventBus,
 *   agentRunner,
 * });
 * const task = await supervisor.start({
 *   repo: makeRepoFullName("owner/name"),
 *   issueNumber: makeIssueNumber(42),
 * });
 * console.log(task.state); // "MERGED" on the happy path
 * ```
 */
export function createTaskSupervisor(opts: TaskSupervisorOptions): TaskSupervisorImpl {
  const clock: SupervisorClock = opts.clock ?? defaultClock();
  const logger: SupervisorLogger = opts.logger ?? defaultLogger();
  const random: SupervisorRandomSource = opts.randomSource ?? defaultRandomSource();
  const cloneUrlFor = opts.cloneUrlFor ??
    ((repo: RepoFullName) => `https://github.com/${repo}.git`);
  const poller: PollerImpl = opts.poller ?? createPoller();
  const pollIntervalMilliseconds = opts.pollIntervalMilliseconds ??
    POLL_INTERVAL_MILLISECONDS;
  const maxIterations = opts.maxIterations ?? MAX_TASK_ITERATIONS;
  const ciLogBudgetBytes = opts.ciLogBudgetBytes ?? STABILIZE_CI_LOG_BUDGET_BYTES;

  /**
   * In-memory task table. Wave 3's daemon hydrates this on boot via
   * `persistence.loadAll()` (Wave 4 wires the loader); for the
   * supervisor skeleton the table is populated by `start()` and read
   * back by `listTasks()` and `getTask()` so test assertions can
   * inspect mid-flight transitions.
   */
  const tasks = new Map<TaskId, Task>();

  /**
   * Per-task head SHA captured from the {@link PullRequestDetails}
   * returned by `createPullRequest`. The stabilize-CI phase polls this
   * SHA until CI flips green; after each agent fix iteration the
   * resolver below re-reads the head SHA so we poll for the new commit.
   *
   * Not persisted: a daemon restart re-resolves the head SHA on the way
   * back into the CI phase via `resolveHeadSha`. Keeping this off-disk
   * keeps the {@link Task} record stable for cross-wave consumers.
   */
  const headShasByTaskId = new Map<TaskId, string>();

  const resolveHeadSha = opts.resolveHeadSha ?? defaultResolveHeadSha;

  function defaultResolveHeadSha(task: Task): Promise<string> {
    const sha = headShasByTaskId.get(task.id);
    if (sha === undefined) {
      return Promise.reject(
        new Error(
          `supervisor: no head SHA captured for task ${task.id}; ` +
            "production wiring must inject `resolveHeadSha` before stabilize CI runs.",
        ),
      );
    }
    return Promise.resolve(sha);
  }

  /**
   * Run `mutate(prev)` to produce the next task record, persist it,
   * and publish a `state-changed` event covering the transition. The
   * persist-then-emit ordering is the load-bearing invariant
   * documented at the top of the module: subscribers (and persistence
   * replay) only ever see a state the store has accepted.
   *
   * @param prev The previous task record.
   * @param next The desired next state. The helper computes the
   *   `Task` shape from `prev` plus the supplied overrides; `state`
   *   is required and `updatedAtIso` is set automatically from the
   *   clock.
   * @param reason Optional human-readable rationale recorded on the
   *   `state-changed` event.
   * @returns The persisted next task record.
   */
  async function transition(
    prev: Task,
    next: {
      readonly state: TaskState;
      readonly stabilizePhase?: StabilizePhase;
      readonly prNumber?: IssueNumber;
      readonly branchName?: string;
      readonly worktreePath?: string;
      readonly sessionId?: string;
      readonly iterationCount?: number;
      readonly terminalReason?: string;
    },
    reason?: string,
  ): Promise<Task> {
    const updated = applyTransition(prev, next, clock.nowIso());
    await opts.persistence.save(updated);
    tasks.set(updated.id, updated);
    publish(opts.eventBus, {
      taskId: updated.id,
      atIso: clock.nowIso(),
      kind: "state-changed",
      data: stateChangedPayload(prev.state, updated, reason),
    });
    return updated;
  }

  async function start(args: StartTaskArgs): Promise<Task> {
    rejectDuplicateTask(tasks, args.repo, args.issueNumber);
    const taskId = mintTaskId(clock, random);
    const nowIso = clock.nowIso();
    const initial: Task = {
      id: taskId,
      repo: args.repo,
      issueNumber: args.issueNumber,
      state: "INIT",
      mergeMode: args.mergeMode ?? "squash",
      model: args.model ?? DEFAULT_AGENT_MODEL,
      iterationCount: 0,
      createdAtIso: nowIso,
      updatedAtIso: nowIso,
    };
    // Register the in-memory entry synchronously, before any `await`,
    // so a concurrent `start()` for the same `(repo, issueNumber)`
    // sees the in-flight task and the duplicate guard fires. The
    // store-on-disk write follows on the next line; persistence-replay
    // and the FSM are both consistent because the only way to read
    // the disk is through `persistence.loadAll()` (Wave 4 hydrates the
    // table on boot) and the only way to read the table is through
    // `listTasks()`, both of which observe the same record.
    tasks.set(taskId, initial);
    try {
      await opts.persistence.save(initial);
    } catch (saveError) {
      // Persistence failure on the very first save is unrecoverable —
      // the FSM cannot proceed without a durable record. Roll back the
      // in-memory registration so a follow-up `start()` for the same
      // pair is not blocked by a phantom entry.
      tasks.delete(taskId);
      throw new SupervisorError(
        `failed to persist initial task record for ${args.repo}#${args.issueNumber}`,
        { cause: saveError },
      );
    }
    publish(opts.eventBus, {
      taskId,
      atIso: clock.nowIso(),
      kind: "state-changed",
      data: { fromState: "INIT", toState: "INIT", reason: "task created" },
    });
    return await drive(initial);
  }

  /**
   * Walk the FSM from `task`'s current state to a terminal one.
   *
   * Each phase is a method on the helper object; the dispatch switch
   * picks the next phase by looking at `task.state`. Failures are
   * absorbed into a `FAILED` transition (with the original error on
   * `cause` and a copy of its message in `terminalReason`).
   */
  async function drive(task: Task): Promise<Task> {
    let current = task;
    while (true) {
      switch (current.state) {
        case "INIT": {
          current = await transition(
            current,
            { state: "CLONING_WORKTREE" },
            "starting worktree clone",
          );
          break;
        }
        case "CLONING_WORKTREE": {
          current = await runCloningWorktree(current);
          if (current.state === "FAILED") return current;
          break;
        }
        case "DRAFTING": {
          current = await runDrafting(current);
          break;
        }
        case "COMMITTING": {
          current = await transition(
            current,
            { state: "PUSHING" },
            "ready to push",
          );
          break;
        }
        case "PUSHING": {
          current = await runPushing(current);
          if (current.state === "FAILED") return current;
          break;
        }
        case "PR_OPEN": {
          current = await runPrOpenFollowup(current);
          if (current.state === "FAILED") return current;
          break;
        }
        case "STABILIZING": {
          current = await runStabilizing(current);
          break;
        }
        case "READY_TO_MERGE": {
          current = await runReadyToMerge(current);
          return current;
        }
        case "MERGED":
        case "NEEDS_HUMAN":
        case "FAILED":
          return current;
      }
    }
  }

  async function runCloningWorktree(task: Task): Promise<Task> {
    try {
      await opts.worktreeManager.ensureBareClone(task.repo, cloneUrlFor(task.repo));
      const path = await opts.worktreeManager.createWorktreeForIssue(
        task.repo,
        task.issueNumber,
      );
      opts.worktreeManager.registerTaskId(task.id, path);
      return await transition(
        task,
        {
          state: "DRAFTING",
          worktreePath: path,
          branchName: branchNameFor(task.issueNumber),
        },
        "worktree ready",
      );
    } catch (error) {
      logger.warn(
        `supervisor: worktree clone failed for ${task.repo}#${task.issueNumber}: ${
          errorMessage(error)
        }`,
      );
      return await fail(task, error, "worktree-clone");
    }
  }

  async function runDrafting(task: Task): Promise<Task> {
    if (task.worktreePath === undefined) {
      return await fail(
        task,
        new Error("DRAFTING entered without a worktree path"),
        "drafting-precondition",
      );
    }
    try {
      const issue = await opts.githubClient.getIssue(task.repo, task.issueNumber);
      const prompt = buildInitialDraftPrompt(issue.title, issue.body);
      let lastSessionId: string | undefined;
      for await (
        const message of opts.agentRunner.runAgent({
          taskId: task.id,
          worktreePath: task.worktreePath,
          prompt,
          model: task.model,
        })
      ) {
        publish(opts.eventBus, {
          taskId: task.id,
          atIso: clock.nowIso(),
          kind: "agent-message",
          data: { role: message.role, text: message.text },
        });
        if ((message as { sessionId?: string }).sessionId !== undefined) {
          lastSessionId = (message as { sessionId?: string }).sessionId;
        }
      }
      const next = await transition(
        task,
        lastSessionId === undefined
          ? { state: "COMMITTING", iterationCount: task.iterationCount + 1 }
          : {
            state: "COMMITTING",
            iterationCount: task.iterationCount + 1,
            sessionId: lastSessionId,
          },
        "draft complete",
      );
      return next;
    } catch (error) {
      logger.warn(
        `supervisor: drafting failed for task ${task.id}: ${errorMessage(error)}`,
      );
      return await fail(task, error, "drafting");
    }
  }

  async function runPushing(task: Task): Promise<Task> {
    // Wave 3 skeleton: the daemon does not perform a real git push from
    // the worktree. We treat the PR opening as the observable side
    // effect that completes the PUSHING → PR_OPEN transition; Wave 4
    // wires the actual push. The PR creation runs *before* the
    // transition so the persisted record carries `prNumber` from the
    // moment it enters `PR_OPEN` — observers (and persistence replay)
    // never see a `PR_OPEN` task with `prNumber === undefined`.
    if (task.branchName === undefined) {
      return await fail(
        task,
        new Error("PUSHING entered without a branch name"),
        "push-precondition",
      );
    }
    let pullRequest: PullRequestDetails;
    try {
      pullRequest = await opts.githubClient.createPullRequest(task.repo, {
        headRef: task.branchName,
        baseRef: DEFAULT_BASE_BRANCH,
        title: prTitleFor(task.issueNumber),
        body: prBodyFor(task.issueNumber),
      });
    } catch (error) {
      logger.warn(
        `supervisor: createPullRequest failed for task ${task.id}: ${errorMessage(error)}`,
      );
      return await fail(task, error, "create-pull-request");
    }
    // Capture the head SHA so the stabilize-CI phase can poll the right
    // commit. The map is not persisted: a daemon restart re-enters the
    // FSM at `PR_OPEN` and the production wiring's `resolveHeadSha`
    // (defaulting to `git rev-parse HEAD` against the worktree) will
    // re-source the SHA on the next CI tick.
    headShasByTaskId.set(task.id, pullRequest.headSha);
    return await transition(
      task,
      { state: "PR_OPEN", prNumber: pullRequest.number },
      `PR #${pullRequest.number} opened`,
    );
  }

  /**
   * Side effect that follows a `PR_OPEN` transition: request the
   * Copilot reviewer, then enter the stabilize loop. Kept on its own
   * pass through `drive()` so the `PR_OPEN` event lands on the bus
   * (and persistence) *before* the reviewer call fires — the brief
   * requires Copilot be requested "immediately on PR open", which we
   * read as "after the `PR_OPEN` state is observable, before any
   * subsequent transition".
   */
  async function runPrOpenFollowup(task: Task): Promise<Task> {
    if (task.prNumber === undefined) {
      return await fail(
        task,
        new Error("PR_OPEN entered without a PR number"),
        "pr-open-precondition",
      );
    }
    try {
      await opts.githubClient.requestReviewers(
        task.repo,
        task.prNumber,
        [COPILOT_REVIEWER_LOGIN],
      );
    } catch (error) {
      logger.warn(
        `supervisor: requestReviewers failed for task ${task.id} PR #${task.prNumber}: ${
          errorMessage(error)
        }`,
      );
      return await fail(task, error, "request-reviewers");
    }
    return await transition(
      task,
      { state: "STABILIZING", stabilizePhase: "REBASE" },
      "reviewer requested; entering stabilize loop",
    );
  }

  async function runStabilizing(task: Task): Promise<Task> {
    // Wave 4 splits the stabilize loop into three phases that run in
    // strict order: rebase → CI → conversations. The CI phase is the
    // only one fully implemented in this PR (issue #16). The rebase
    // (#15) and conversations (#17) phases stay as stubs so the
    // event-bus timeline is observable — the supervisor publishes the
    // `STABILIZING / <phase>` transition for each, and the next phase
    // takes over without doing real work yet.
    let current = await runStabilizeRebase(task);
    if (isTerminal(current.state)) return current;
    current = await runStabilizeCi(current);
    if (isTerminal(current.state)) return current;
    current = await runStabilizeConversations(current);
    if (isTerminal(current.state)) return current;
    return await transition(
      current,
      { state: "READY_TO_MERGE" },
      "stabilize loop settled",
    );
  }

  /**
   * Stub body for the rebase sub-phase (#15 replaces this). Records the
   * phase transition on the bus so observers see the expected timeline,
   * then yields to the next phase.
   */
  async function runStabilizeRebase(task: Task): Promise<Task> {
    return await transition(
      task,
      { state: "STABILIZING", stabilizePhase: "REBASE" },
      "stabilize phase REBASE (stub)",
    );
  }

  /**
   * Stub body for the conversations sub-phase (#17 replaces this).
   * Records the phase transition on the bus so observers see the
   * expected timeline, then yields back to {@link runStabilizing}.
   */
  async function runStabilizeConversations(task: Task): Promise<Task> {
    return await transition(
      task,
      { state: "STABILIZING", stabilizePhase: "CONVERSATIONS" },
      "stabilize phase CONVERSATIONS (stub)",
    );
  }

  /**
   * Stabilize-loop CI sub-phase (#16).
   *
   * Polls `getCombinedStatus` + `listCheckRuns` for the per-task head
   * SHA at {@link pollIntervalMilliseconds} cadence:
   *
   *  - **Pending**: keep polling. Each poll publishes a `github-call`
   *    event so observers (the TUI, integration tests) see the cadence.
   *  - **Green** (`success`): phase complete; return.
   *  - **Red** (`failure` / `error`): collect the failing job ids,
   *    fetch their logs trimmed to {@link ciLogBudgetBytes} (line
   *    boundary preferred, byte boundary fallback), dispatch the agent
   *    with the failing-job summary, advance `iterationCount`, restart
   *    polling on the new head SHA. Bounded by {@link maxIterations}; on
   *    exhaustion the task transitions to {@link "NEEDS_HUMAN"}.
   */
  async function runStabilizeCi(task: Task): Promise<Task> {
    let current = await transition(
      task,
      { state: "STABILIZING", stabilizePhase: "CI" },
      "stabilize phase CI",
    );
    if (current.worktreePath === undefined) {
      return await fail(
        current,
        new Error("STABILIZING/CI entered without a worktree path"),
        "ci-precondition",
      );
    }
    const stabilizeClient = asStabilizeClient(opts.githubClient);
    if (stabilizeClient === undefined) {
      return await fail(
        current,
        new Error(
          "STABILIZING/CI requires a StabilizeGitHubClient " +
            "(listCheckRuns / getCheckRunLogs / listReviews / listReviewComments); " +
            "supervisor was constructed with a plain GitHubClient",
        ),
        "ci-precondition",
      );
    }

    while (true) {
      let headSha: string;
      try {
        headSha = await resolveHeadSha(current);
      } catch (error) {
        logger.warn(
          `supervisor: resolveHeadSha failed for task ${current.id}: ${errorMessage(error)}`,
        );
        return await fail(current, error, "ci-resolve-head");
      }

      const outcome = await pollCiOnce(
        current,
        stabilizeClient,
        headSha,
      );
      if (outcome.kind === "fatal") {
        return await fail(current, outcome.error, "ci-poll");
      }
      if (outcome.kind === "green") {
        return current;
      }

      // Red: budget check before we spin the agent.
      if (current.iterationCount >= maxIterations) {
        publish(opts.eventBus, {
          taskId: current.id,
          atIso: clock.nowIso(),
          kind: "log",
          data: {
            level: "error",
            message:
              `stabilize CI exhausted ${maxIterations} agent iterations on perpetually-red CI`,
          },
        });
        return await transition(
          current,
          {
            state: "NEEDS_HUMAN",
            terminalReason: `ci-perpetually-red: agent iteration cap (${maxIterations}) exhausted`,
          },
          "stabilize CI exhausted iteration budget",
        );
      }

      try {
        current = await dispatchCiAgent(
          current,
          stabilizeClient,
          headSha,
          outcome.failingChecks,
        );
      } catch (error) {
        logger.warn(
          `supervisor: stabilize CI agent dispatch failed for task ${current.id}: ${
            errorMessage(error)
          }`,
        );
        return await fail(current, error, "ci-agent");
      }
      // Loop: re-resolve the head SHA (now pointing at the agent's fix
      // commit) and poll again.
    }
  }

  /**
   * Drive a single CI polling episode for `headSha` to completion.
   *
   * Returns once the combined status crosses out of `pending`. The
   * poller spaces successful ticks by `pollIntervalMilliseconds`;
   * each tick publishes a `github-call` event so observers see the
   * cadence. The function resolves with one of:
   *
   *  - `{ kind: "green" }` — `state === "success"`.
   *  - `{ kind: "red", failingChecks }` — `state === "failure"` or
   *    `"error"`.
   *  - `{ kind: "fatal", error }` — the fetcher hit a non-retryable
   *    error (a {@link PollerError} of kind `"fatal"` or any synchronous
   *    throw the poller cannot absorb).
   */
  async function pollCiOnce(
    task: Task,
    client: StabilizeGitHubClient,
    headSha: string,
  ): Promise<TerminalCiPollOutcome> {
    return await new Promise<TerminalCiPollOutcome>((resolve) => {
      let settled = false;
      let consecutiveErrors = 0;
      // `poller.poll(...)` validates `intervalMilliseconds` synchronously
      // (see `poller.ts:411-415` — non-finite values throw `RangeError`).
      // Without this guard the throw would bypass the `{ kind: "fatal" }`
      // outcome path and bubble out of the surrounding `runStabilizeCi`,
      // skipping the FAILED transition that downstream observers (the
      // TUI, integration tests) depend on. Catch and resolve as fatal
      // instead so every exit from `pollCiOnce` flows through the same
      // discriminated outcome shape.
      let handle: { cancel(): void };
      try {
        handle = poller.poll<CiPollOutcome>({
          taskId: task.id,
          intervalMilliseconds: pollIntervalMilliseconds,
          fetcher: async (): Promise<CiPollOutcome> => {
            // Each GitHub call is wrapped so a rejection still emits a
            // `github-call` event before the error propagates to the
            // poller's `onError`. Without this the TUI sees nothing
            // when the fetcher rejects (network outage, 404 for an
            // unknown SHA, transient 5xx on the way back from
            // GitHub) — operators couldn't see *which* request failed
            // even though the polling cadence was happening.
            const statusEndpoint = "GET /repos/{owner}/{repo}/commits/{ref}/status";
            let status;
            try {
              status = await client.getCombinedStatus(task.repo, headSha);
            } catch (error) {
              publish(opts.eventBus, {
                taskId: task.id,
                atIso: clock.nowIso(),
                kind: "github-call",
                data: {
                  method: "GET",
                  endpoint: statusEndpoint,
                  error: errorMessage(error),
                },
              });
              throw error;
            }
            publish(opts.eventBus, {
              taskId: task.id,
              atIso: clock.nowIso(),
              kind: "github-call",
              data: {
                method: "GET",
                endpoint: statusEndpoint,
              },
            });
            if (status.state === "pending") {
              return { kind: "pending" };
            }
            if (status.state === "success") {
              return { kind: "green" };
            }
            // failure / error: read the per-check breakdown so we can
            // surface failing job ids to the caller.
            const checkRunsEndpoint = "GET /repos/{owner}/{repo}/commits/{ref}/check-runs";
            let checkRuns;
            try {
              checkRuns = await client.listCheckRuns(task.repo, headSha);
            } catch (error) {
              publish(opts.eventBus, {
                taskId: task.id,
                atIso: clock.nowIso(),
                kind: "github-call",
                data: {
                  method: "GET",
                  endpoint: checkRunsEndpoint,
                  error: errorMessage(error),
                },
              });
              throw error;
            }
            publish(opts.eventBus, {
              taskId: task.id,
              atIso: clock.nowIso(),
              kind: "github-call",
              data: {
                method: "GET",
                endpoint: checkRunsEndpoint,
              },
            });
            return {
              kind: "red",
              failingChecks: checkRuns.filter(isFailedCheckRun),
            };
          },
          onResult: (result) => {
            if (settled) return;
            // Any successful fetcher resolution clears the error budget —
            // the poller's own bookkeeping does the same on its
            // exponential-backoff series, but we mirror it here so the
            // CI-level threshold below tracks *consecutive* fetcher
            // failures, not lifetime ones.
            consecutiveErrors = 0;
            if (result.kind === "pending") {
              return; // keep polling
            }
            if (result.kind === "fatal") {
              // The fetcher itself does not produce fatal outcomes (those
              // arrive through `onError`); the discriminant exists so the
              // caller can surface a uniform shape.
              return;
            }
            settled = true;
            handle.cancel();
            resolve(result);
          },
          onError: (error: unknown) => {
            if (settled) return;
            if (error instanceof PollerError && error.kind === "fatal") {
              settled = true;
              handle.cancel();
              resolve({ kind: "fatal", error });
              return;
            }
            // `PollerError(kind: "rate-limited")` is a normal control-flow
            // signal: the fetcher is telling the poller "skip ahead, don't
            // retry-storm". The poller itself does **not** count it as a
            // failure for backoff bookkeeping (see `poller.ts:553-577`).
            // Mirror that here: counting rate-limited tickets toward the
            // CI-level fatal threshold would let a sustained rate-limit
            // (which is recoverable) trip `STABILIZE_CI_MAX_CONSECUTIVE_
            // FETCHER_ERRORS` even though no fetcher error actually
            // occurred. Reset the counter — a rate-limited tick is a
            // success-shaped event, not a failure.
            if (error instanceof PollerError && error.kind === "rate-limited") {
              consecutiveErrors = 0;
              return;
            }
            // Transient errors stay inside the poll loop — the poller's
            // exponential backoff handles them. We keep an additional
            // CI-level counter that bails out if the fetcher rejects
            // `STABILIZE_CI_MAX_CONSECUTIVE_FETCHER_ERRORS` times in a
            // row; without it, an oblivious test (or a real outage that
            // flips every retry into a rejection) would loop forever even
            // after the poller's backoff saturates at the upper bound.
            consecutiveErrors += 1;
            if (consecutiveErrors >= STABILIZE_CI_MAX_CONSECUTIVE_FETCHER_ERRORS) {
              settled = true;
              handle.cancel();
              resolve({ kind: "fatal", error });
            }
          },
        });
      } catch (error) {
        // Synchronous throw from `poller.poll(...)` — currently only the
        // `intervalMilliseconds` finite-number guard. Surface it through
        // the same fatal outcome shape the async paths use so the caller
        // (`runStabilizeCi`) transitions the task to FAILED rather than
        // letting the error escape upward.
        resolve({ kind: "fatal", error });
        return;
      }
    });
  }

  /**
   * Build the agent prompt from `failingChecks`, run the agent, and
   * return the post-iteration {@link Task} record. The transition into
   * the agent run uses the task's existing state (`STABILIZING / CI`);
   * we only bump `iterationCount` so the budget check at the top of the
   * outer loop can detect exhaustion.
   */
  async function dispatchCiAgent(
    task: Task,
    client: StabilizeGitHubClient,
    headSha: string,
    failingChecks: readonly CheckRunSummary[],
  ): Promise<Task> {
    if (task.worktreePath === undefined) {
      throw new Error("dispatchCiAgent: task has no worktreePath");
    }
    const logsByJob = new Map<number, string>();
    const logsEndpoint = "GET /repos/{owner}/{repo}/check-runs/{check_run_id}/logs";
    for (const check of failingChecks) {
      try {
        const bytes = await client.getCheckRunLogs(task.repo, check.id);
        publish(opts.eventBus, {
          taskId: task.id,
          atIso: clock.nowIso(),
          kind: "github-call",
          data: {
            method: "GET",
            endpoint: logsEndpoint,
          },
        });
        logsByJob.set(
          check.id,
          // Thread the budget through to the ZIP extractor so
          // decompression itself stops at `ciLogBudgetBytes` rather
          // than first materializing the full uncompressed archive
          // (which can be tens of MB) and then trimming. The
          // subsequent `trimLogToBudget` still caps the rendered
          // prompt — including the marker and line-boundary tail
          // logic — but it now operates on an at-most-budget input.
          trimLogToBudget(
            await decodeCheckRunLogs(bytes, ciLogBudgetBytes),
            ciLogBudgetBytes,
          ),
        );
      } catch (error) {
        // Don't abort the whole CI iteration when a single log fetch
        // fails — surface the failure on the bus (both as a
        // `github-call` event with the request's `error` field set,
        // so observability tools see *which* request failed, and as
        // a human-readable warn-level `log` event for the TUI
        // scrollback) and replace the excerpt with a placeholder so
        // the agent still has the job name and url.
        publish(opts.eventBus, {
          taskId: task.id,
          atIso: clock.nowIso(),
          kind: "github-call",
          data: {
            method: "GET",
            endpoint: logsEndpoint,
            error: errorMessage(error),
          },
        });
        publish(opts.eventBus, {
          taskId: task.id,
          atIso: clock.nowIso(),
          kind: "log",
          data: {
            level: "warn",
            message: `stabilize CI: log fetch failed for check ${check.id} (${check.name}): ${
              errorMessage(error)
            }`,
          },
        });
        logsByJob.set(check.id, `[log fetch failed: ${errorMessage(error)}]`);
      }
    }

    const prompt = buildCiAgentPrompt({
      issueNumber: task.issueNumber,
      headSha,
      failingChecks,
      logsByJob,
    });

    let lastSessionId: string | undefined = task.sessionId;
    const runArgs: {
      taskId: TaskId;
      worktreePath: string;
      prompt: string;
      model: string;
      sessionId?: string;
    } = task.sessionId === undefined
      ? {
        taskId: task.id,
        worktreePath: task.worktreePath,
        prompt,
        model: task.model,
      }
      : {
        taskId: task.id,
        worktreePath: task.worktreePath,
        prompt,
        model: task.model,
        sessionId: task.sessionId,
      };
    for await (const message of opts.agentRunner.runAgent(runArgs)) {
      publish(opts.eventBus, {
        taskId: task.id,
        atIso: clock.nowIso(),
        kind: "agent-message",
        data: { role: message.role, text: message.text },
      });
      const candidate = (message as { sessionId?: string }).sessionId;
      if (candidate !== undefined) {
        lastSessionId = candidate;
      }
    }

    return await transition(
      task,
      lastSessionId === undefined
        ? {
          state: "STABILIZING",
          stabilizePhase: "CI",
          iterationCount: task.iterationCount + 1,
        }
        : {
          state: "STABILIZING",
          stabilizePhase: "CI",
          iterationCount: task.iterationCount + 1,
          sessionId: lastSessionId,
        },
      `stabilize CI agent iteration ${task.iterationCount + 1}`,
    );
  }

  async function runReadyToMerge(task: Task): Promise<Task> {
    if (task.mergeMode === "manual") {
      // Manual merge mode: stay in READY_TO_MERGE; an operator takes
      // over from the GitHub UI. The integration test exercises
      // `squash`, but the manual branch keeps the FSM honest.
      return task;
    }
    if (task.prNumber === undefined) {
      return await fail(
        task,
        new Error("READY_TO_MERGE entered without a PR number"),
        "merge-precondition",
      );
    }
    try {
      await opts.githubClient.mergePullRequest(
        task.repo,
        task.prNumber,
        task.mergeMode,
      );
    } catch (error) {
      logger.warn(
        `supervisor: mergePullRequest failed for task ${task.id}: ${errorMessage(error)}`,
      );
      return await fail(task, error, "merge");
    }
    return await transition(
      task,
      { state: "MERGED", terminalReason: "merged" },
      "PR merged",
    );
  }

  /**
   * Drive `task` to `FAILED`, recording `error` on the task's
   * `terminalReason` and the bus's `error` event. The transition is
   * still persist-then-emit so a daemon restart sees the failure.
   */
  async function fail(task: Task, error: unknown, source: string): Promise<Task> {
    const message = errorMessage(error);
    publish(opts.eventBus, {
      taskId: task.id,
      atIso: clock.nowIso(),
      kind: "error",
      data: { message: `${source}: ${message}` },
    });
    return await transition(
      task,
      { state: "FAILED", terminalReason: `${source}: ${message}` },
      `failure during ${source}`,
    );
  }

  return {
    start,
    listTasks(): readonly Task[] {
      return [...tasks.values()];
    },
    getTask(taskId: TaskId): Task | undefined {
      return tasks.get(taskId);
    },
  };
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/**
 * Combine `prev` with the partial `next` to produce a fully-typed
 * {@link Task} record carrying the new `state` and an updated
 * timestamp. Optional fields supplied on `next` override the
 * corresponding `prev` value; fields *not* supplied retain their prior
 * value so transitions are minimally destructive.
 *
 * The `exactOptionalPropertyTypes` compiler flag forbids re-spreading
 * `prev` with an `undefined` override (`{ ...prev, prNumber: undefined }`
 * does not erase `prNumber`). We therefore project each optional field
 * explicitly, picking the override when supplied and otherwise the
 * previous value.
 */
function applyTransition(
  prev: Task,
  next: {
    readonly state: TaskState;
    readonly stabilizePhase?: StabilizePhase;
    readonly prNumber?: IssueNumber;
    readonly branchName?: string;
    readonly worktreePath?: string;
    readonly sessionId?: string;
    readonly iterationCount?: number;
    readonly terminalReason?: string;
  },
  nowIso: string,
): Task {
  const baseRecord: Task = {
    id: prev.id,
    repo: prev.repo,
    issueNumber: prev.issueNumber,
    state: next.state,
    mergeMode: prev.mergeMode,
    model: prev.model,
    iterationCount: next.iterationCount ?? prev.iterationCount,
    createdAtIso: prev.createdAtIso,
    updatedAtIso: nowIso,
  };
  // `STABILIZING` is the only state that carries a `stabilizePhase`;
  // every other state strips it so the on-disk record reflects the
  // FSM's invariant.
  const withPhase: Task = next.state === "STABILIZING"
    ? (next.stabilizePhase !== undefined
      ? { ...baseRecord, stabilizePhase: next.stabilizePhase }
      : (prev.stabilizePhase !== undefined
        ? { ...baseRecord, stabilizePhase: prev.stabilizePhase }
        : baseRecord))
    : baseRecord;
  const withPr: Task = (next.prNumber ?? prev.prNumber) !== undefined
    ? { ...withPhase, prNumber: (next.prNumber ?? prev.prNumber) as IssueNumber }
    : withPhase;
  const withBranch: Task = (next.branchName ?? prev.branchName) !== undefined
    ? { ...withPr, branchName: next.branchName ?? prev.branchName as string }
    : withPr;
  const withWorktree: Task = (next.worktreePath ?? prev.worktreePath) !== undefined
    ? { ...withBranch, worktreePath: next.worktreePath ?? prev.worktreePath as string }
    : withBranch;
  const withSession: Task = (next.sessionId ?? prev.sessionId) !== undefined
    ? { ...withWorktree, sessionId: next.sessionId ?? prev.sessionId as string }
    : withWorktree;
  const withTerminal: Task = next.terminalReason !== undefined
    ? { ...withSession, terminalReason: next.terminalReason }
    : (prev.terminalReason !== undefined && isTerminal(next.state)
      ? { ...withSession, terminalReason: prev.terminalReason }
      : withSession);
  return withTerminal;
}

/**
 * Build the {@link StateChangedPayload} for a transition.
 *
 * `stabilizePhase` is only meaningful when the destination state is
 * `STABILIZING`; we omit it otherwise to match the contract in
 * `src/types.ts`.
 */
function stateChangedPayload(
  fromState: TaskState,
  next: Task,
  reason?: string,
): {
  readonly fromState: TaskState;
  readonly toState: TaskState;
  readonly stabilizePhase?: StabilizePhase;
  readonly reason?: string;
} {
  const base: {
    fromState: TaskState;
    toState: TaskState;
    stabilizePhase?: StabilizePhase;
    reason?: string;
  } = { fromState, toState: next.state };
  if (next.state === "STABILIZING" && next.stabilizePhase !== undefined) {
    base.stabilizePhase = next.stabilizePhase;
  }
  if (reason !== undefined) {
    base.reason = reason;
  }
  return base;
}

function isTerminal(state: TaskState): boolean {
  return state === "MERGED" || state === "NEEDS_HUMAN" || state === "FAILED";
}

/**
 * Reject a duplicate {@link TaskSupervisorImpl.start} call for a
 * `(repo, issueNumber)` pair already tracked by a non-terminal task.
 *
 * Throws {@link SupervisorError} so callers see a deterministic failure
 * before any state is persisted.
 */
function rejectDuplicateTask(
  tasks: ReadonlyMap<TaskId, Task>,
  repo: RepoFullName,
  issueNumber: IssueNumber,
): void {
  for (const task of tasks.values()) {
    if (task.repo !== repo || task.issueNumber !== issueNumber) {
      continue;
    }
    if (!isTerminal(task.state)) {
      throw new SupervisorError(
        `task already in flight for ${repo}#${issueNumber} (taskId=${task.id}, state=${task.state})`,
      );
    }
  }
}

/**
 * Mint a fresh {@link TaskId} of the form `task_<isoDate>_<rand6>`.
 *
 * The ISO date prefix keeps logs sortable; the random suffix avoids
 * collisions when the supervisor mints two ids inside the same wall
 * second. The string is run through `makeTaskId` so the brand
 * invariant fires on the way out — Lesson #1 from the W3 brief.
 *
 * Exported for unit-testing alongside the supervisor; production code
 * uses it via {@link TaskSupervisorImpl.start}.
 *
 * @param clock Wall-clock source.
 * @param random Random-byte source for the suffix.
 * @returns A branded {@link TaskId}.
 */
export function mintTaskId(
  clock: SupervisorClock,
  random: SupervisorRandomSource,
): TaskId {
  const isoDate = clock.nowIso();
  const safeIso = isoDate.replace(/[:.]/g, "-");
  const bytes = new Uint8Array(TASK_ID_RANDOM_SUFFIX_BYTES);
  random.fillRandomBytes(bytes);
  const hex = Array.from(
    bytes,
    (byte) => byte.toString(HEX_RADIX).padStart(HEX_BYTE_WIDTH_CHARACTERS, "0"),
  )
    .join("")
    .slice(0, TASK_ID_RANDOM_SUFFIX_LENGTH_CHARACTERS);
  return makeTaskId(`task_${safeIso}_${hex}`);
}

/**
 * Branch name minted by the supervisor for a given issue.
 *
 * The supervisor always uses `makina/issue-<n>` to match
 * {@link WorktreeManagerImpl.createWorktreeForIssue}'s branch-naming
 * scheme. Centralised so the integration test can compare against the
 * same constant the worktree manager uses.
 *
 * @param issueNumber The issue number to format.
 * @returns The branch name.
 */
export function branchNameFor(issueNumber: IssueNumber): string {
  return `makina/issue-${issueNumber}`;
}

/**
 * PR title for a freshly-drafted issue. Wave 3 keeps it minimal; Wave
 * 4 enriches it from the agent's commit messages.
 *
 * @param issueNumber The issue number to format.
 * @returns The PR title.
 */
export function prTitleFor(issueNumber: IssueNumber): string {
  return `feat(#${issueNumber}): draft from supervisor`;
}

/**
 * PR body for a freshly-drafted issue. Wave 3 keeps it minimal; Wave
 * 4 enriches it with the failing-CI excerpt and the conversations log.
 *
 * @param issueNumber The issue number to format.
 * @returns The PR body.
 */
export function prBodyFor(issueNumber: IssueNumber): string {
  return `Drafted by makina supervisor for #${issueNumber}.`;
}

/**
 * Construct the initial agent-run prompt from the issue payload.
 *
 * @param title Issue title.
 * @param body Issue body (markdown).
 * @returns The prompt the agent runner will receive.
 */
export function buildInitialDraftPrompt(title: string, body: string): string {
  return `# ${title}\n\n${body}`;
}

/**
 * Publish `event` on `bus`, swallowing any throw so a misbehaving
 * subscriber never aborts the FSM. The W2 event-bus contract already
 * isolates handlers, but we keep the defensive try/catch here in case
 * a future bus drops that guarantee.
 */
function publish(bus: EventBus, event: TaskEvent): void {
  try {
    bus.publish(event);
  } catch {
    // Bus failures are not load-bearing — the supervisor's authoritative
    // state lives in persistence, and we already logged the original
    // condition above.
  }
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

function defaultClock(): SupervisorClock {
  return { nowIso: () => new Date().toISOString() };
}

function defaultLogger(): SupervisorLogger {
  const inner = getLogger();
  return {
    info(message: string): void {
      inner.info(message);
    },
    warn(message: string): void {
      inner.warn(message);
    },
  };
}

function defaultRandomSource(): SupervisorRandomSource {
  return {
    fillRandomBytes(bytes: Uint8Array): void {
      crypto.getRandomValues(bytes);
    },
  };
}

// ---------------------------------------------------------------------------
// Stabilize CI helpers
// ---------------------------------------------------------------------------

/**
 * Discriminated union returned by a single tick of the stabilize-CI
 * fetcher. `pending` keeps the poll loop running; the other variants
 * settle the {@link pollCiOnce} promise.
 */
export type CiPollOutcome =
  | { readonly kind: "pending" }
  | { readonly kind: "green" }
  | {
    readonly kind: "red";
    /** Failing per-check entries collected from `listCheckRuns`. */
    readonly failingChecks: readonly CheckRunSummary[];
  }
  | {
    readonly kind: "fatal";
    /** The fatal error that exited the poller. */
    readonly error: unknown;
  };

/**
 * Subset of {@link CiPollOutcome} that {@link pollCiOnce} resolves
 * with: `pending` is filtered out internally because the poll loop
 * continues on pending ticks rather than returning to the caller.
 */
export type TerminalCiPollOutcome = Exclude<CiPollOutcome, { kind: "pending" }>;

/**
 * Narrow `client` to the {@link StabilizeGitHubClient} extension if it
 * implements the four stabilize-phase methods (`listCheckRuns`,
 * `getCheckRunLogs`, `listReviews`, `listReviewComments`), otherwise
 * return `undefined`.
 *
 * The supervisor's W1 contract is the narrower {@link GitHubClient};
 * the stabilize phases need the additive surface. We feature-detect
 * rather than re-typing the constructor so consumers passing the
 * production {@link GitHubClientImpl} (which implements both surfaces)
 * see the wider methods without rewiring, and tests passing the
 * {@link InMemoryGitHubClient} double get the same behavior.
 */
function asStabilizeClient(
  client: GitHubClient,
): StabilizeGitHubClient | undefined {
  const candidate = client as Partial<StabilizeGitHubClient>;
  if (
    typeof candidate.listCheckRuns === "function" &&
    typeof candidate.getCheckRunLogs === "function" &&
    typeof candidate.listReviews === "function" &&
    typeof candidate.listReviewComments === "function"
  ) {
    return client as StabilizeGitHubClient;
  }
  return undefined;
}

/**
 * Predicate selecting CI check runs the supervisor must escalate to the
 * agent: completed runs whose `conclusion` is one of the failure-class
 * values. Skipped, neutral, and stale runs are not failures; queued and
 * in-progress runs are not yet completed and would have kept the
 * combined-status `pending`.
 */
function isFailedCheckRun(check: CheckRunSummary): boolean {
  if (check.status !== "completed") return false;
  return (
    check.conclusion === "failure" ||
    check.conclusion === "timed_out" ||
    check.conclusion === "action_required" ||
    check.conclusion === "cancelled"
  );
}

/**
 * Decode `bytes` as UTF-8, replacing malformed sequences with U+FFFD.
 *
 * Used for already-extracted text content. The lossy decode keeps the
 * supervisor robust against pathological inputs (terminal escapes,
 * cropped surrogate pairs) without requiring a streaming UTF-8
 * validator.
 *
 * @param bytes Raw text bytes.
 * @returns UTF-8 string with malformed sequences replaced.
 */
function decodeUtf8Lossy(bytes: Uint8Array): string {
  return new TextDecoder("utf-8", { fatal: false }).decode(bytes);
}

/** First two bytes of every ZIP signature: `P` `K`. */
const ZIP_MAGIC_P = 0x50;
const ZIP_MAGIC_K = 0x4b;
/** End-of-central-directory record size, excluding the variable comment. */
const EOCD_FIXED_SIZE = 22;
/** Maximum size of the optional EOCD comment, per the ZIP spec. */
const EOCD_MAX_COMMENT = 0xffff;
/** Central-directory file-header signature (little-endian `PK\x01\x02`). */
const CDFH_SIGNATURE = 0x02014b50;
/** End-of-central-directory record signature (little-endian `PK\x05\x06`). */
const EOCD_SIGNATURE = 0x06054b50;

/**
 * Detect a ZIP container by its `PK` magic prefix. The full signature
 * test is deferred to {@link extractZipEntriesAsText}; this is a fast
 * pre-check that lets us pass already-extracted text through unchanged
 * (the test doubles in `tests/helpers/in_memory_github_client.ts` queue
 * raw text bytes rather than building real ZIPs).
 */
function looksLikeZip(bytes: Uint8Array): boolean {
  return bytes.length >= 4 && bytes[0] === ZIP_MAGIC_P && bytes[1] === ZIP_MAGIC_K;
}

/**
 * Decode a check-run logs payload to readable text.
 *
 * `GitHubClientImpl.getCheckRunLogs` returns the raw `application/zip`
 * bytes the GitHub API hands back (see ADR-022). This function:
 *
 *  1. Inspects the magic prefix. Non-ZIP byte streams (the in-memory
 *     test double's pre-extracted text) are decoded as UTF-8 directly,
 *     preserving the existing test contract.
 *  2. For real ZIP responses, parses the central directory, decodes
 *     each entry through `DecompressionStream("deflate-raw")` (or
 *     copies through verbatim for `STORED` entries), and joins
 *     them with a `--- <path> ---` separator so the agent prompt sees
 *     readable per-step output instead of binary gibberish.
 *  3. Falls back to a UTF-8 decode of the raw bytes if the parse fails
 *     mid-way — better to surface partial garbage with a warning than to
 *     starve the agent of context entirely.
 *
 * Memory safety: GitHub's check-run log archives can unpack to tens of
 * MB. To avoid transient large allocations (and an outright OOM on a
 * pathological run), this function threads `maxBytes` (when provided)
 * through to {@link extractZipEntriesAsText}. Decompression of each
 * entry stops as soon as the running total reaches `maxBytes`, and
 * subsequent entries are skipped. The caller's downstream
 * {@link trimLogToBudget} step still caps the *prompt* size, but the
 * cap here means we never hold the full unpacked archive in memory.
 *
 * The implementation intentionally avoids adding a new npm dependency
 * (see ADR-004): the parser only handles the `STORED` and `DEFLATE`
 * compression methods GitHub actually uses, and reads the central
 * directory rather than streaming local file headers so we don't have
 * to track the optional data-descriptor variant.
 *
 * @param bytes Raw response bytes from `getCheckRunLogs`.
 * @param maxBytes Optional upper bound on the *total decompressed*
 *   bytes the helper materializes. When set, decompression stops as
 *   soon as the running total reaches this value; entries past the
 *   budget are skipped and a `[…truncated;…]` marker is appended so
 *   downstream readers can tell the excerpt was capped during
 *   extraction (separately from the {@link trimLogToBudget} cap that
 *   may further trim the rendered prompt). If omitted or non-finite,
 *   no extraction-time cap is applied.
 * @returns Concatenated UTF-8 text of every text-like ZIP entry, or the
 *   raw byte decode if the input is not a ZIP.
 */
export async function decodeCheckRunLogs(
  bytes: Uint8Array,
  maxBytes?: number,
): Promise<string> {
  if (!looksLikeZip(bytes)) {
    return decodeUtf8Lossy(bytes);
  }
  try {
    const { entries, truncated } = await extractZipEntriesAsText(bytes, maxBytes);
    if (entries.length === 0) {
      return decodeUtf8Lossy(bytes);
    }
    const joined = entries
      .map(({ name, text }) => `--- ${name} ---\n${text}`)
      .join("\n");
    if (truncated) {
      return `${joined}\n[…truncated; extraction stopped at ${maxBytes} bytes…]`;
    }
    return joined;
  } catch {
    // Best-effort fallback: surface whatever the raw bytes decode to.
    // The trim-to-budget step below will still cap the output, so a
    // garbage payload cannot blow the prompt budget.
    return decodeUtf8Lossy(bytes);
  }
}

/**
 * Parse a ZIP container's central directory and return every entry as
 * UTF-8 text.
 *
 * Supports the two compression methods GitHub uses: method 0 (`STORED`)
 * copies the local-header payload verbatim; method 8 (`DEFLATE`) feeds
 * the payload through `DecompressionStream("deflate-raw")`. Other
 * methods raise — `decodeCheckRunLogs` catches the throw and falls back
 * to a raw-byte decode.
 *
 * Reads the End-of-Central-Directory (EOCD) record from the tail to
 * locate the central directory, then walks each Central-Directory File
 * Header (CDFH) to find local-header offsets. Reading from the central
 * directory (rather than streaming local headers) sidesteps the
 * optional data-descriptor variant some encoders emit after the
 * compressed stream.
 *
 * Memory safety: when `maxBytes` is provided, the per-entry
 * decompression keeps at most `remainingBudget` bytes resident via a
 * tail-retaining ring buffer (see {@link TailRetainer}); subsequent
 * entries are skipped once the running total fills the cap. The
 * resident footprint is capped at roughly `maxBytes` rather than the
 * full uncompressed archive size — important because GitHub log
 * archives can unpack to tens of MB.
 *
 * Tail vs. prefix (round-4 fix): the retained slice is the **tail** of
 * each entry, not its prefix. Round-3 truncated to the head, which was
 * memory-safe but content-wrong: CI failures live at the end of the
 * log and the downstream {@link trimLogToBudget} step is also
 * tail-oriented, so a head-truncated entry would never have surfaced
 * the failing assertion lines.
 *
 * @param bytes Raw ZIP archive.
 * @param maxBytes Optional upper bound on the total decompressed bytes
 *   to materialize across all entries. When provided, decompression
 *   stops short and the returned tuple's `truncated` flag is set.
 * @returns A discriminated record with the list of `{ name, text }`
 *   entries in central-directory order and a `truncated` flag set when
 *   extraction stopped early due to `maxBytes`. Empty entries (size 0)
 *   are kept so the caller can still see the file name in the merged
 *   output.
 * @throws If the EOCD cannot be located, the central directory walk
 *   runs past `bytes`, or an entry uses an unsupported compression
 *   method.
 */
async function extractZipEntriesAsText(
  bytes: Uint8Array,
  maxBytes?: number,
): Promise<{
  readonly entries: readonly { readonly name: string; readonly text: string }[];
  readonly truncated: boolean;
}> {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const eocdOffset = findEocdOffset(view);
  if (eocdOffset === -1) {
    throw new Error("ZIP: end-of-central-directory record not found");
  }
  const totalEntries = view.getUint16(eocdOffset + 10, true);
  const cdSize = view.getUint32(eocdOffset + 12, true);
  const cdOffset = view.getUint32(eocdOffset + 16, true);
  if (cdOffset + cdSize > bytes.byteLength) {
    throw new Error("ZIP: central directory exceeds archive bounds");
  }
  const decoder = new TextDecoder("utf-8", { fatal: false });
  const entries: { name: string; text: string }[] = [];
  // Effective budget: a finite positive number means we cap; anything
  // else (undefined, NaN, Infinity, <= 0) disables the cap.
  const hasBudget = typeof maxBytes === "number" && Number.isFinite(maxBytes) && maxBytes > 0;
  let remainingBudget = hasBudget && maxBytes !== undefined ? Math.floor(maxBytes) : Infinity;
  let truncated = false;
  let cursor = cdOffset;
  for (let i = 0; i < totalEntries; i += 1) {
    if (cursor + 46 > bytes.byteLength) {
      throw new Error("ZIP: central-directory entry header truncated");
    }
    if (view.getUint32(cursor, true) !== CDFH_SIGNATURE) {
      throw new Error("ZIP: bad central-directory file-header signature");
    }
    const compressionMethod = view.getUint16(cursor + 10, true);
    const compressedSize = view.getUint32(cursor + 20, true);
    const nameLength = view.getUint16(cursor + 28, true);
    const extraLength = view.getUint16(cursor + 30, true);
    const commentLength = view.getUint16(cursor + 32, true);
    const localHeaderOffset = view.getUint32(cursor + 42, true);
    const nameBytes = bytes.subarray(cursor + 46, cursor + 46 + nameLength);
    const name = decoder.decode(nameBytes);
    cursor += 46 + nameLength + extraLength + commentLength;

    if (hasBudget && remainingBudget <= 0) {
      // Budget exhausted; skip every remaining entry rather than
      // decompressing data we'd immediately discard.
      truncated = true;
      continue;
    }

    // Walk the local file header to find the actual data offset (the
    // local header has its own variable-length name + extra fields).
    if (localHeaderOffset + 30 > bytes.byteLength) {
      throw new Error(`ZIP: local header for "${name}" truncated`);
    }
    const localNameLength = view.getUint16(localHeaderOffset + 26, true);
    const localExtraLength = view.getUint16(localHeaderOffset + 28, true);
    const dataStart = localHeaderOffset + 30 + localNameLength + localExtraLength;
    const dataEnd = dataStart + compressedSize;
    if (dataEnd > bytes.byteLength) {
      throw new Error(`ZIP: data for "${name}" exceeds archive bounds`);
    }
    if (name.endsWith("/") && compressedSize === 0) {
      // Directory entry — skip.
      continue;
    }
    const compressed = bytes.subarray(dataStart, dataEnd);
    let raw: Uint8Array;
    if (compressionMethod === 0) {
      // STORED entries are already raw. When the entry overflows the
      // remaining budget we slice the **tail** (not the head), because
      // CI failures live at the end of the log and the downstream
      // `trimLogToBudget` step is also tail-oriented (round-4 fix —
      // round-3 mistakenly kept the prefix here).
      if (hasBudget && compressed.byteLength > remainingBudget) {
        raw = compressed.subarray(compressed.byteLength - remainingBudget);
        truncated = true;
      } else {
        raw = compressed;
      }
    } else if (compressionMethod === 8) {
      // DEFLATE entries stream chunk-by-chunk through a tail-retaining
      // circular buffer: the entire compressed stream is drained (so
      // the kept bytes are the **last** `remainingBudget`, not the
      // first), but only `remainingBudget` bytes are ever resident at
      // peak. This costs the CPU of full inflation but keeps memory
      // bounded — and crucially keeps the part of the log operators
      // actually need (round-4 fix; round-3 stopped the stream early
      // and captured the prefix instead of the tail).
      const inflated = await inflateRawBounded(
        compressed,
        hasBudget ? remainingBudget : Infinity,
      );
      raw = inflated.bytes;
      if (inflated.truncated) {
        truncated = true;
      }
    } else {
      throw new Error(
        `ZIP: unsupported compression method ${compressionMethod} for "${name}"`,
      );
    }
    entries.push({ name, text: decoder.decode(raw) });
    if (hasBudget) {
      remainingBudget -= raw.byteLength;
    }
  }
  return { entries, truncated };
}

/**
 * Locate the End-of-Central-Directory record by scanning backwards from
 * the tail. The record is variable-length (a trailing comment up to
 * 64 KiB), so the spec mandates a reverse scan for its 4-byte signature.
 */
function findEocdOffset(view: DataView): number {
  const end = view.byteLength;
  const minStart = Math.max(0, end - EOCD_FIXED_SIZE - EOCD_MAX_COMMENT);
  for (let offset = end - EOCD_FIXED_SIZE; offset >= minStart; offset -= 1) {
    if (view.getUint32(offset, true) === EOCD_SIGNATURE) {
      return offset;
    }
  }
  return -1;
}

/**
 * Fixed-capacity ring buffer that retains the **last** `capacity` bytes
 * pushed into it. Used by {@link inflateRawBounded} to drain the entire
 * DEFLATE stream while keeping only the tail in memory.
 *
 * The buffer is allocated once at `capacity` bytes and never grown.
 * Writes that overflow wrap to the start, overwriting older content.
 * `toUint8Array()` materializes the in-order tail by stitching the two
 * halves of the wrap. Inflater chunks are pushed verbatim, so a single
 * push can be much larger than the capacity — in that case only the
 * trailing `capacity` bytes of the chunk survive.
 *
 * Memory is exactly `capacity` (plus the per-chunk transient that the
 * `DecompressionStream` allocates and we copy out of), independent of
 * the original entry's uncompressed size.
 */
class TailRetainer {
  private readonly buffer: Uint8Array;
  private readonly capacity: number;
  private writePos = 0;
  private filled = 0;
  // Total bytes ever pushed across the lifetime of the retainer. Used
  // by the regression test to assert that the inflater really did see
  // the full uncompressed stream (i.e. memory stayed bounded but the
  // pipeline still consumed everything, which is the whole point of
  // the round-4 fix).
  private _totalSeen = 0;

  constructor(capacity: number) {
    if (!Number.isFinite(capacity) || capacity <= 0) {
      throw new Error(`TailRetainer: capacity must be positive (got ${capacity})`);
    }
    this.capacity = Math.floor(capacity);
    this.buffer = new Uint8Array(this.capacity);
  }

  push(chunk: Uint8Array): void {
    this._totalSeen += chunk.byteLength;
    if (chunk.byteLength === 0) return;
    // If the chunk is bigger than the whole ring, the only bytes that
    // can survive are its trailing `capacity` — the rest will be
    // immediately overwritten. Skip the wasted copies.
    const effectiveStart = chunk.byteLength > this.capacity ? chunk.byteLength - this.capacity : 0;
    const effective = effectiveStart === 0 ? chunk : chunk.subarray(effectiveStart);
    const len = effective.byteLength;
    // Chunk fits without wrapping past the end of the buffer.
    if (this.writePos + len <= this.capacity) {
      this.buffer.set(effective, this.writePos);
      this.writePos = (this.writePos + len) % this.capacity;
    } else {
      // Split write across the wrap.
      const firstPart = this.capacity - this.writePos;
      this.buffer.set(effective.subarray(0, firstPart), this.writePos);
      this.buffer.set(effective.subarray(firstPart), 0);
      this.writePos = len - firstPart;
    }
    this.filled = Math.min(this.capacity, this.filled + len);
  }

  toUint8Array(): Uint8Array {
    if (this.filled < this.capacity) {
      // Buffer never wrapped; the live region is `[0, filled)`.
      return this.buffer.slice(0, this.filled);
    }
    // Buffer wrapped at least once; the oldest byte is at `writePos`.
    const out = new Uint8Array(this.capacity);
    const tailLen = this.capacity - this.writePos;
    out.set(this.buffer.subarray(this.writePos), 0);
    out.set(this.buffer.subarray(0, this.writePos), tailLen);
    return out;
  }

  /** Total bytes ever pushed (regression-test instrumentation). */
  get totalSeen(): number {
    return this._totalSeen;
  }
}

/**
 * Decompress a raw DEFLATE stream (no zlib header) and return at most
 * the **trailing** `maxBytes` of inflated output.
 *
 * Round-3 stopped reading the stream once the budget was reached, which
 * bounded memory but kept the **prefix** of the entry — exactly the
 * wrong half for CI logs, where the failure typically lives at the
 * tail. Round-4 keeps the same memory bound (`maxBytes` resident at
 * peak via {@link TailRetainer}) but drains the entire stream so the
 * retained slice is the last `maxBytes` of the inflated output.
 *
 * Trade-off: we pay the CPU cost of inflating bytes we'll discard, but
 * the alternative (random-access inflate from the tail) requires either
 * a second pass through the stream or a non-streaming zlib library —
 * both rejected against ADR-004's no-new-deps constraint and the
 * fact that GitHub log archives are still bounded at low tens of MB.
 *
 * For the unbounded case (`maxBytes === Infinity`), behavior matches
 * round-2: every chunk is collected into a flat `Uint8Array` and
 * returned. This preserves the existing test contract for callers that
 * omit `maxBytes`.
 *
 * @param compressed Raw DEFLATE bytes (no zlib/gzip header).
 * @param maxBytes Upper bound on the **resident** inflated size — and,
 *   when finite, the size of the returned trailing slice. `Infinity`
 *   disables the cap and returns the full inflated output.
 * @returns Inflated bytes — the full output when unbounded, or the
 *   trailing `maxBytes` when bounded — and a `truncated` flag set when
 *   the original stream produced more output than fit in the budget.
 */
async function inflateRawBounded(
  compressed: Uint8Array,
  maxBytes: number,
): Promise<{ readonly bytes: Uint8Array; readonly truncated: boolean }> {
  // `Blob` types accept `BlobPart`, which (under TS lib.dom 5.5+) requires
  // an `ArrayBuffer` rather than the broader `ArrayBufferLike` a generic
  // `Uint8Array` exposes. Materialize a fresh `ArrayBuffer`-backed view
  // so the Blob constructor sees a concrete buffer type.
  const owned = new Uint8Array(compressed.byteLength);
  owned.set(compressed);
  const stream = new Blob([owned])
    .stream()
    .pipeThrough(new DecompressionStream("deflate-raw"));
  const reader = stream.getReader();
  // Bounded path: tail-retaining ring buffer. Unbounded path: legacy
  // chunk list (used for the no-cap test case and any future caller
  // that wants the whole entry — we still drain the full stream there).
  const bounded = Number.isFinite(maxBytes) && maxBytes > 0;
  const retainer = bounded ? new TailRetainer(maxBytes) : null;
  const chunks: Uint8Array[] = [];
  let totalSeen = 0;
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      if (value === undefined) continue;
      totalSeen += value.byteLength;
      if (retainer !== null) {
        retainer.push(value);
      } else {
        chunks.push(value);
      }
    }
  } finally {
    // Best-effort: the reader releases its lock when garbage-collected,
    // but an explicit release keeps Deno's resource table tidy for
    // back-to-back tests.
    try {
      reader.releaseLock();
    } catch {
      // ignore
    }
  }
  if (retainer !== null) {
    return {
      bytes: retainer.toUint8Array(),
      truncated: totalSeen > maxBytes,
    };
  }
  // Unbounded fast path: stitch chunks into a flat array.
  const out = new Uint8Array(totalSeen);
  let written = 0;
  for (const chunk of chunks) {
    out.set(chunk, written);
    written += chunk.byteLength;
  }
  return { bytes: out, truncated: false };
}

/**
 * Trim `text` to fit within `budgetBytes` UTF-8 bytes, preferring a
 * line boundary so the truncation is readable.
 *
 * Strategy:
 *
 *  1. If the encoded length already fits within `budgetBytes`, return
 *     `text` unchanged.
 *  2. Otherwise, walk the suffix backwards in line-sized chunks until
 *     the remaining tail fits. The CI phase wants the *trailing* part
 *     of the log (the lines around the failed assertion) so we keep the
 *     **end** of the input.
 *  3. If a single trailing line exceeds `budgetBytes` (e.g. a
 *     megabyte-long stack-trace line), fall back to a hard byte slice
 *     against the encoded form and a UTF-8 decode of the tail.
 *  4. Prepend a marker line so downstream readers (the agent prompt,
 *     log viewers) can tell at a glance that the excerpt is truncated.
 *
 * @param text Source text. Long inputs are sliced from the **end**.
 * @param budgetBytes Maximum UTF-8 byte budget (must be a positive
 *   finite integer; non-positive values are clamped to zero, in which
 *   case the marker line alone is returned).
 * @returns Trimmed text with a leading marker.
 *
 * @example
 * ```ts
 * const trimmed = trimLogToBudget(rawLog, 100 * 1024);
 * // "[…truncated; showing trailing 102400 bytes…]\n…"
 * ```
 */
export function trimLogToBudget(text: string, budgetBytes: number): string {
  const safeBudget = Number.isFinite(budgetBytes) && budgetBytes > 0 ? Math.floor(budgetBytes) : 0;
  const encoder = new TextEncoder();
  const encoded = encoder.encode(text);
  if (encoded.byteLength <= safeBudget) {
    return text;
  }
  const decoder = new TextDecoder("utf-8", { fatal: false });
  // Try to keep whole trailing lines first — split on `\n`, walk
  // backwards, and concatenate until we run out of budget.
  const lines = text.split("\n");
  let remaining = safeBudget;
  const tail: string[] = [];
  for (let i = lines.length - 1; i >= 0; i -= 1) {
    const line = lines[i] ?? "";
    // The terminator costs one byte (UTF-8 `\n`) for every line we
    // re-join, except for the last one which we already account for in
    // the marker prepending below. Compute the byte cost of this line
    // in isolation; `encoder.encode(line)` has no trailing terminator.
    const lineBytes = encoder.encode(line).byteLength + (tail.length === 0 ? 0 : 1);
    if (lineBytes > remaining) {
      break;
    }
    remaining -= lineBytes;
    tail.unshift(line);
  }
  const marker = `[…truncated; showing trailing ${safeBudget} bytes…]`;
  if (tail.length > 0) {
    return `${marker}\n${tail.join("\n")}`;
  }
  // Pathological case: the trailing line alone exceeds the budget. Hard
  // byte slice against the UTF-8 encoded form and decode the tail.
  if (safeBudget === 0) {
    return marker;
  }
  const sliceStart = Math.max(0, encoded.byteLength - safeBudget);
  const tailBytes = encoded.subarray(sliceStart);
  return `${marker}\n${decoder.decode(tailBytes)}`;
}

/**
 * Build the agent prompt for a stabilize-CI fix iteration.
 *
 * The prompt opens with the issue context, lists every failing check
 * with its name and url, and embeds each job's trimmed log excerpt
 * inside a fenced code block. The shape is asserted by the unit tests
 * so any drift is caught before the model sees it.
 *
 * Exported so tests can compare against the literal prompt.
 *
 * @param args Prompt inputs.
 * @returns The prompt text.
 *
 * @example
 * ```ts
 * const prompt = buildCiAgentPrompt({
 *   issueNumber: makeIssueNumber(42),
 *   headSha: "deadbeefcafe",
 *   failingChecks: [{ id: 1, name: "build", status: "completed", conclusion: "failure", htmlUrl: "https://…" }],
 *   logsByJob: new Map([[1, "expected 200, got 500"]]),
 * });
 * ```
 */
export function buildCiAgentPrompt(args: {
  readonly issueNumber: IssueNumber;
  readonly headSha: string;
  readonly failingChecks: readonly CheckRunSummary[];
  readonly logsByJob: ReadonlyMap<number, string>;
}): string {
  const header = [
    `# Stabilize CI fix for issue #${args.issueNumber}`,
    "",
    `CI failed against commit \`${args.headSha}\` on this PR. Read the failing-job`,
    "logs below, identify the root cause, fix it in the worktree, and commit",
    "the fix. The supervisor will push and re-run CI on the new commit.",
    "",
    `## Failing checks (${args.failingChecks.length})`,
    "",
  ];
  const summary = args.failingChecks.map((check) => {
    const conclusion = check.conclusion ?? "unknown";
    return `- **${check.name}** (id=${check.id}, conclusion=\`${conclusion}\`) — ${check.htmlUrl}`;
  });
  const sections: string[] = [];
  for (const check of args.failingChecks) {
    const log = args.logsByJob.get(check.id) ?? "[no log captured]";
    sections.push("");
    sections.push(`### Logs — ${check.name} (id=${check.id})`);
    sections.push("");
    sections.push("```");
    sections.push(log);
    sections.push("```");
  }
  return [...header, ...summary, ...sections].join("\n");
}
