/**
 * daemon/supervisor.ts — per-task state machine that walks an issue
 * end-to-end through the lifecycle defined in `docs/lifecycle.md`.
 *
 * Wave 3 (#12) ships the skeleton: the supervisor drives a task through
 * `INIT → CLONING_WORKTREE → DRAFTING → COMMITTING → PUSHING → PR_OPEN
 *  → STABILIZING → READY_TO_MERGE → MERGED | NEEDS_HUMAN | FAILED`.
 *
 * Wave 4 fills in the three `STABILIZING` sub-phases — `REBASE` (#15),
 * `CI` (#16), and `CONVERSATIONS` (#17). This file ships the
 * conversations phase as the first non-stub Wave 4 implementation:
 * `runConversationsPhase` polls `listReviews` + `listReviewComments`
 * via the injected {@link PollerImpl}, groups new comments by review
 * thread, dispatches the agent runner, then resolves each thread via
 * GraphQL `resolveReviewThread` and re-requests Copilot review on
 * every push (per ADR-019). REBASE and CI remain stubs that emit a
 * single `state-changed` event each so the observable timeline stays
 * `REBASE → CI → CONVERSATIONS`.
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
import {
  type PullRequestReview,
  type PullRequestReviewComment,
  type StabilizeGitHubClient,
} from "../github/client.ts";
import { createPoller, type PollerImpl } from "./poller.ts";
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
   * Optional poller override used by the stabilize loop's CI and
   * conversations phases. Defaults to {@link createPoller} with library
   * defaults; tests inject one with a synthetic clock so cadence
   * assertions are deterministic.
   */
  readonly poller?: PollerImpl;
  /**
   * Per-task GitHub poll cadence, in milliseconds, forwarded to the
   * {@link PollerImpl.poll} call inside the conversations phase. Tests
   * pass `0` to drive ticks back-to-back; production omits this and
   * picks up {@link POLL_INTERVAL_MILLISECONDS}.
   */
  readonly pollIntervalMilliseconds?: number;
  /**
   * Maximum number of agent iterations the conversations phase will
   * dispatch on a single task before transitioning the task to
   * `NEEDS_HUMAN`. Defaults to {@link MAX_TASK_ITERATIONS}; tests pass
   * a tighter bound to exercise the exhaustion path without scripting
   * dozens of replies.
   */
  readonly maxTaskIterations?: number;
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
  const maxTaskIterations = opts.maxTaskIterations ?? MAX_TASK_ITERATIONS;

  /**
   * In-memory task table. Wave 3's daemon hydrates this on boot via
   * `persistence.loadAll()` (Wave 4 wires the loader); for the
   * supervisor skeleton the table is populated by `start()` and read
   * back by `listTasks()` and `getTask()` so test assertions can
   * inspect mid-flight transitions.
   */
  const tasks = new Map<TaskId, Task>();

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
    lastReviewAtIso?: string,
  ): Promise<Task> {
    const updated = applyTransition(prev, next, clock.nowIso(), lastReviewAtIso);
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
    // The three stabilize sub-phases run in sequence. Wave 4 ships
    // CONVERSATIONS in this PR (#17); REBASE and CI are still stubs and
    // emit a single `state-changed` event so the observable timeline
    // continues to read `REBASE → CI → CONVERSATIONS`. The companion
    // Wave 4 PRs (#15, #16) replace those stubs with their real bodies.
    let current = task;
    for (const phase of ["REBASE", "CI"] as const) {
      current = await transition(
        current,
        { state: "STABILIZING", stabilizePhase: phase },
        `stabilize phase ${phase} (stub)`,
      );
    }
    current = await transition(
      current,
      { state: "STABILIZING", stabilizePhase: "CONVERSATIONS" },
      "stabilize phase CONVERSATIONS",
    );
    current = await runConversationsPhase(current);
    if (current.state !== "STABILIZING") {
      // The conversations phase escalated to NEEDS_HUMAN (or FAILED via
      // `fail`) and persisted that transition itself; the parent driver
      // returns the new state directly without forcing READY_TO_MERGE.
      return current;
    }
    return await transition(
      current,
      { state: "READY_TO_MERGE" },
      "stabilize loop settled",
    );
  }

  /**
   * Conversations-phase loop.
   *
   * One iteration:
   *
   *  1. Poll {@link StabilizeGitHubClient.listReviews} +
   *     {@link StabilizeGitHubClient.listReviewComments} for the task's
   *     PR, via the injected {@link PollerImpl}.
   *  2. Filter to comments whose `createdAtIso` is strictly newer than
   *     the task's `lastReviewAtIso` watermark.
   *  3. If new comments are present: group them by review thread,
   *     dispatch the agent runner with the grouped context as the
   *     prompt, then resolve every grouped thread via
   *     {@link StabilizeGitHubClient.resolveReviewThread} and
   *     re-request Copilot review.
   *  4. Update the watermark to the latest seen `createdAtIso` so the
   *     next poll only sees fresh comments.
   *  5. If iteration count exceeds {@link MAX_TASK_ITERATIONS}, escalate
   *     to `NEEDS_HUMAN`. Otherwise loop back to (1) until a poll
   *     returns no new comments.
   *
   * The Poller's tick is consumed via a `Promise.withResolvers()` so a
   * single polling tick yields a single decision; the supervisor cancels
   * the per-tick handle as soon as the tick settles. Failures inside the
   * fetcher take the Poller's exponential-backoff path; fatal failures
   * (auth revocation) propagate via the `onError` callback and we tear
   * down the loop into `FAILED`.
   */
  async function runConversationsPhase(task: Task): Promise<Task> {
    if (task.prNumber === undefined) {
      return await fail(
        task,
        new Error("CONVERSATIONS entered without a PR number"),
        "conversations-precondition",
      );
    }
    if (!isStabilizeClient(opts.githubClient)) {
      return await fail(
        task,
        new Error(
          "CONVERSATIONS requires a StabilizeGitHubClient (listReviews/listReviewComments)",
        ),
        "conversations-precondition",
      );
    }
    const stabilizeClient = opts.githubClient;
    let current = task;
    while (true) {
      let pollResult: ConversationsPollResult;
      try {
        pollResult = await runConversationsPoll(
          stabilizeClient,
          current.id,
          current.repo,
          current.prNumber as IssueNumber,
        );
      } catch (error) {
        logger.warn(
          `supervisor: conversations poll failed for task ${current.id}: ${errorMessage(error)}`,
        );
        return await fail(current, error, "conversations-poll");
      }
      // Persist the watermark on every successful poll, even when the
      // poll yielded no work — the brief calls for an updated
      // `lastReviewAt` after every successful tick so a subsequent
      // resume of the task does not re-process settled threads.
      const nextWatermark = pollResult.latestCreatedAtIso ?? current.lastReviewAtIso;
      const newComments = filterNewComments(
        pollResult.comments,
        current.lastReviewAtIso,
      );
      if (newComments.length === 0) {
        // No new conversations to address — phase converges. Persist
        // the (possibly-advanced) watermark and return the in-memory
        // record unchanged otherwise.
        if (
          nextWatermark !== undefined &&
          nextWatermark !== current.lastReviewAtIso
        ) {
          current = await transition(
            current,
            { state: "STABILIZING", stabilizePhase: "CONVERSATIONS" },
            "conversations watermark advanced",
            nextWatermark,
          );
        }
        return current;
      }
      // Bound the loop. We compare *after* a non-empty batch because the
      // brief calls for the exhaustion path to fire when work *exists*
      // but the budget is spent.
      if (current.iterationCount >= maxTaskIterations) {
        publish(opts.eventBus, {
          taskId: current.id,
          atIso: clock.nowIso(),
          kind: "log",
          data: {
            level: "warn",
            message:
              `conversations: iteration budget (${maxTaskIterations}) exhausted with ${newComments.length} unresolved comment(s)`,
          },
        });
        return await transition(
          current,
          {
            state: "NEEDS_HUMAN",
            terminalReason: `conversations: iteration budget (${maxTaskIterations}) exhausted`,
          },
          "iteration budget exhausted",
        );
      }
      // Dispatch the agent run.
      try {
        current = await runConversationsAgentDispatch(
          current,
          pollResult.reviews,
          newComments,
        );
      } catch (error) {
        logger.warn(
          `supervisor: conversations agent run failed for task ${current.id}: ${
            errorMessage(error)
          }`,
        );
        return await fail(current, error, "conversations-agent");
      }
      // Resolve threads + re-request Copilot review on every push.
      try {
        await resolveAddressedThreads(stabilizeClient, newComments);
        await stabilizeClient.requestReviewers(
          current.repo,
          current.prNumber as IssueNumber,
          [COPILOT_REVIEWER_LOGIN],
        );
      } catch (error) {
        logger.warn(
          `supervisor: conversations resolve/re-request failed for task ${current.id}: ${
            errorMessage(error)
          }`,
        );
        return await fail(current, error, "conversations-resolve");
      }
      // Persist the watermark advance so the next poll's filter is
      // tight; the iteration counter was already incremented inside
      // `runConversationsAgentDispatch`.
      if (
        nextWatermark !== undefined &&
        nextWatermark !== current.lastReviewAtIso
      ) {
        current = await transition(
          current,
          { state: "STABILIZING", stabilizePhase: "CONVERSATIONS" },
          "conversations watermark advanced",
          nextWatermark,
        );
      }
    }
  }

  /**
   * Run a single poll of the conversations endpoints via the injected
   * {@link PollerImpl}. Resolves with the latest reviews and comments
   * the moment one tick settles, then cancels the poll handle.
   *
   * The poller drives the tick on the synthetic interval the supervisor
   * was constructed with (`pollIntervalMilliseconds`); tests pass `0`
   * to fire back-to-back. A `PollerError(kind: "fatal")` propagates as
   * a regular rejection — the loop's caller absorbs it into a `FAILED`
   * transition.
   */
  function runConversationsPoll(
    stabilizeClient: StabilizeGitHubClient,
    taskId: TaskId,
    repo: RepoFullName,
    pullRequestNumber: IssueNumber,
  ): Promise<ConversationsPollResult> {
    return new Promise<ConversationsPollResult>((resolve, reject) => {
      let settled = false;
      // The handle reference is captured *after* `poll(...)` returns,
      // but the success/error callbacks may fire synchronously inside
      // `poll(...)` if the fetcher resolves before the next microtask.
      // We therefore wire the cancel call through a deferred reference
      // so the handlers can cancel the prior tick once they have decided.
      let handle: { cancel(): void } | undefined;
      const cancel = (): void => {
        if (handle !== undefined) {
          handle.cancel();
          handle = undefined;
        }
      };
      handle = poller.poll<ConversationsPollResult>({
        taskId,
        intervalMilliseconds: pollIntervalMilliseconds,
        fetcher: async () => {
          const [reviews, comments] = await Promise.all([
            stabilizeClient.listReviews(repo, pullRequestNumber),
            stabilizeClient.listReviewComments(repo, pullRequestNumber),
          ]);
          const latestCreatedAtIso = latestCommentTimestamp(comments);
          // `exactOptionalPropertyTypes` forbids assigning a literal
          // `undefined` to an optional field; project the result with
          // an explicit shape that omits the key when there is no
          // latest timestamp to share.
          const projected: ConversationsPollResult = latestCreatedAtIso === undefined
            ? { reviews, comments }
            : { reviews, comments, latestCreatedAtIso };
          return projected;
        },
        onResult: (result) => {
          if (settled) return;
          settled = true;
          cancel();
          resolve(result);
        },
        onError: (error) => {
          if (settled) return;
          settled = true;
          cancel();
          reject(error);
        },
      });
    });
  }

  /**
   * Dispatch the agent runner with the grouped review comments as
   * prompt context, stream every message onto the event bus, and
   * transition the task with `iterationCount + 1` and (optionally) the
   * SDK's session id.
   */
  async function runConversationsAgentDispatch(
    task: Task,
    reviews: readonly PullRequestReview[],
    comments: readonly PullRequestReviewComment[],
  ): Promise<Task> {
    if (task.worktreePath === undefined) {
      throw new Error("CONVERSATIONS dispatch entered without a worktree path");
    }
    const prompt = buildConversationsPrompt(reviews, comments);
    let lastSessionId: string | undefined = task.sessionId;
    for await (
      const message of opts.agentRunner.runAgent(
        task.sessionId === undefined
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
          },
      )
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
    return await transition(
      task,
      lastSessionId === undefined
        ? {
          state: "STABILIZING",
          stabilizePhase: "CONVERSATIONS",
          iterationCount: task.iterationCount + 1,
        }
        : {
          state: "STABILIZING",
          stabilizePhase: "CONVERSATIONS",
          iterationCount: task.iterationCount + 1,
          sessionId: lastSessionId,
        },
      "conversations: agent draft complete",
    );
  }

  /**
   * Resolve every distinct review thread referenced by `comments` via
   * the GraphQL `resolveReviewThread` mutation. Comments whose
   * `threadNodeId` is `undefined` (e.g. surfaced by a REST listing that
   * cannot map back to GraphQL) are skipped — the brief calls out
   * "resolve each thread", not "resolve every comment", and a missing
   * id is the documented signal that a thread is not GraphQL-addressable
   * yet.
   */
  async function resolveAddressedThreads(
    stabilizeClient: StabilizeGitHubClient,
    comments: readonly PullRequestReviewComment[],
  ): Promise<void> {
    const seen = new Set<string>();
    for (const comment of comments) {
      const threadId = comment.threadNodeId;
      if (threadId === undefined) continue;
      if (seen.has(threadId)) continue;
      seen.add(threadId);
      await stabilizeClient.resolveReviewThread(threadId);
    }
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
  lastReviewAtIso?: string,
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
  // `lastReviewAtIso` is only set by the conversations phase, but any
  // transition can carry it forward (the FSM never erases the
  // watermark — once advanced, it stays advanced through to terminal).
  const resolvedReviewIso = lastReviewAtIso ?? prev.lastReviewAtIso;
  const withReviewIso: Task = resolvedReviewIso !== undefined
    ? { ...withSession, lastReviewAtIso: resolvedReviewIso }
    : withSession;
  const withTerminal: Task = next.terminalReason !== undefined
    ? { ...withReviewIso, terminalReason: next.terminalReason }
    : (prev.terminalReason !== undefined && isTerminal(next.state)
      ? { ...withReviewIso, terminalReason: prev.terminalReason }
      : withReviewIso);
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
 * Result the conversations-phase poller surfaces to the caller after a
 * single tick.
 *
 * Exported so unit tests can inspect the same shape the supervisor
 * consumes; production wiring keeps the type local to the supervisor.
 */
export interface ConversationsPollResult {
  /** Reviews submitted on the PR, in submission order. */
  readonly reviews: readonly PullRequestReview[];
  /** Inline review comments on the PR, in creation order. */
  readonly comments: readonly PullRequestReviewComment[];
  /**
   * ISO-8601 timestamp of the most recently created comment, or
   * `undefined` when the PR has none. The supervisor uses it as the
   * next watermark when the poll yields.
   */
  readonly latestCreatedAtIso?: string;
}

/**
 * Build the conversations-phase agent prompt by grouping the new
 * comments by review thread and emitting a markdown block per thread.
 *
 * The grouping key is `threadNodeId` when present (so the agent sees
 * the same boundaries the supervisor uses to call
 * {@link StabilizeGitHubClient.resolveReviewThread}), falling back to
 * the comment's own id when the GraphQL id has not been resolved.
 * Comments inside a group are listed in their original order.
 *
 * Reviews supply preamble headers; per-review-id review bodies appear
 * as bulleted notes above the threads they cover. Comments without a
 * matching review (orphans) get a generic "Inline comment" header so
 * the agent can still address them.
 *
 * @param reviews Reviews collected from `listReviews`.
 * @param comments Inline comments newer than the watermark.
 * @returns The prompt body the agent runner receives.
 */
export function buildConversationsPrompt(
  reviews: readonly PullRequestReview[],
  comments: readonly PullRequestReviewComment[],
): string {
  const grouped = groupCommentsByThread(comments);
  const reviewById = new Map<number, PullRequestReview>();
  for (const review of reviews) {
    reviewById.set(review.id, review);
  }
  const lines: string[] = [
    "# Address pull-request review feedback",
    "",
    "The following review threads need to be addressed. Make the requested changes,",
    "commit, and push. Each thread starts with the review body (when present) and",
    "lists every inline comment in order.",
    "",
  ];
  for (const group of grouped) {
    const headerComment = group.comments[0];
    if (headerComment === undefined) continue;
    const reviewId = headerComment.pullRequestReviewId;
    const review = reviewId !== null ? reviewById.get(reviewId) : undefined;
    lines.push(
      `## Thread ${group.key} (${headerComment.path}${
        headerComment.line !== null ? `:${headerComment.line}` : ""
      })`,
    );
    if (review !== undefined && review.body.length > 0) {
      lines.push(`> Review by @${review.user} (${review.state}): ${review.body}`);
      lines.push("");
    }
    for (const comment of group.comments) {
      lines.push(`- @${comment.user}: ${comment.body}`);
    }
    lines.push("");
  }
  return lines.join("\n");
}

/**
 * Group `comments` by review thread so the conversations phase can
 * dispatch one prompt per thread *and* call
 * {@link StabilizeGitHubClient.resolveReviewThread} once per thread
 * after the agent push.
 *
 * Grouping key is `threadNodeId` when present, otherwise the comment's
 * own id when it is the root, otherwise `inReplyToId` (a reply joins
 * its parent's group). Output groups follow first-appearance order so
 * test assertions over the prompt and the resolve calls are stable.
 *
 * @param comments Comments to group.
 * @returns One group per thread, with the comments inside listed in
 *   their original order.
 */
export function groupCommentsByThread(
  comments: readonly PullRequestReviewComment[],
): readonly { readonly key: string; readonly comments: readonly PullRequestReviewComment[] }[] {
  const groupsByKey = new Map<string, PullRequestReviewComment[]>();
  const orderedKeys: string[] = [];
  for (const comment of comments) {
    const key = comment.threadNodeId ?? `comment-${comment.inReplyToId ?? comment.id}`;
    let bucket = groupsByKey.get(key);
    if (bucket === undefined) {
      bucket = [];
      groupsByKey.set(key, bucket);
      orderedKeys.push(key);
    }
    bucket.push(comment);
  }
  return orderedKeys.map((key) => ({
    key,
    comments: groupsByKey.get(key) as PullRequestReviewComment[],
  }));
}

/**
 * Filter `comments` to those strictly newer than `watermark`.
 *
 * Comparison uses ISO-8601 lexicographic ordering — same-millisecond
 * comments tie and are kept (the comparison is `>` not `>=`). When the
 * watermark is `undefined`, every comment is returned.
 *
 * @param comments Candidate comments.
 * @param watermark High-water mark.
 * @returns The subset of `comments` strictly newer than `watermark`.
 */
export function filterNewComments(
  comments: readonly PullRequestReviewComment[],
  watermark: string | undefined,
): readonly PullRequestReviewComment[] {
  if (watermark === undefined) return [...comments];
  return comments.filter((comment) => comment.createdAtIso > watermark);
}

/**
 * Return the latest `createdAtIso` across `comments`, or `undefined`
 * when the array is empty.
 *
 * The conversations phase uses this as the next watermark to persist
 * after a successful poll, so a subsequent resume of the task does not
 * re-process comments it already saw.
 *
 * @param comments Comments collected from `listReviewComments`.
 * @returns The latest ISO timestamp present, or `undefined`.
 */
export function latestCommentTimestamp(
  comments: readonly PullRequestReviewComment[],
): string | undefined {
  let latest: string | undefined;
  for (const comment of comments) {
    if (latest === undefined || comment.createdAtIso > latest) {
      latest = comment.createdAtIso;
    }
  }
  return latest;
}

/**
 * Narrow a {@link GitHubClient} to a {@link StabilizeGitHubClient}.
 *
 * The supervisor accepts the W1 `GitHubClient` contract on
 * construction (so consumers that never enter the stabilize loop can
 * pass the narrower interface), but the conversations phase needs the
 * additive review-listing and thread-resolution methods. This guard
 * checks for them at runtime so a misconfigured supervisor surfaces a
 * loud `FAILED` instead of a `TypeError` deep inside the loop.
 */
function isStabilizeClient(client: GitHubClient): client is StabilizeGitHubClient {
  const candidate = client as Partial<StabilizeGitHubClient>;
  return typeof candidate.listReviews === "function" &&
    typeof candidate.listReviewComments === "function" &&
    typeof candidate.resolveReviewThread === "function";
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
