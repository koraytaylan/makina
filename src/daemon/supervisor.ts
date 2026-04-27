/**
 * daemon/supervisor.ts — per-task state machine that walks an issue
 * end-to-end through the lifecycle defined in `docs/lifecycle.md`.
 *
 * Wave 3 (#12) ships the skeleton: the supervisor drives a task through
 * `INIT → CLONING_WORKTREE → DRAFTING → COMMITTING → PUSHING → PR_OPEN
 *  → STABILIZING → READY_TO_MERGE → MERGED | NEEDS_HUMAN | FAILED`.
 * The three `STABILIZING` sub-phases (`REBASE`, `CI`, `CONVERSATIONS`)
 * are stubbed: each one publishes its `state-changed` event with the
 * matching {@link StabilizePhase} on the bus so observers see the
 * expected timeline, and the loop exits to `READY_TO_MERGE`
 * immediately. Wave 4 (#15+) replaces those stubs with the real
 * stabilize logic.
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
   * If `true`, the supervisor leaves the per-task worktree on disk
   * after `MERGED`; if `false`, it tears the worktree down via
   * {@link WorktreeManagerImpl.removeWorktree} as the final step of
   * the merge pipeline. Mirrors `lifecycle.preserveWorktreeOnMerge`
   * from `config.json`. Defaults to `false` so a fresh `makina setup`
   * leaves disk usage bounded; operators who want to keep the
   * worktree for follow-up flip the bit in their config.
   */
  readonly preserveWorktreeOnMerge?: boolean;
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

  /**
   * Force the merge of a task currently parked at `READY_TO_MERGE`.
   *
   * Wired to the `/merge <task-id>` slash command for tasks configured
   * with `mergeMode === "manual"` (the only mode that parks rather
   * than auto-merging once stabilize settles). Re-enters the FSM at
   * `READY_TO_MERGE` and runs the same merge → cleanup pipeline as the
   * auto-merge path: GitHub `mergePullRequest` is invoked with the
   * task's recorded mode (overridden internally to `squash`/`rebase`
   * — the supervisor refuses to call the API with `manual`, since
   * GitHub has no equivalent strategy), the worktree is cleaned up
   * (or preserved per `preserveWorktreeOnMerge`), and the task lands
   * in `MERGED`. Failures classify the same way as the auto-merge
   * path (see {@link MergeError.category}).
   *
   * @param taskId Identifier of the task to merge.
   * @param overrideMode Optional override of the merge strategy for
   *   this single call. Defaults to `"squash"` when the task itself
   *   carries `manual` (the API needs a concrete strategy); ignored
   *   otherwise.
   * @returns The persisted task record after the merge attempt
   *   (`MERGED`, `NEEDS_HUMAN`, or `FAILED`).
   * @throws SupervisorError when the task does not exist, or its
   *   current state is not `READY_TO_MERGE`. The thrown error carries
   *   a `kind` discriminator so the daemon's command dispatcher can
   *   reply with a precise `ack { ok: false, error }`.
   */
  mergeReadyTask(
    taskId: TaskId,
    overrideMode?: MergeMode,
  ): Promise<Task>;
}

/**
 * Categories of caller-visible failures the supervisor surfaces
 * synchronously through {@link SupervisorError}. The daemon's command
 * dispatcher reads the `kind` to map an exception onto a precise
 * `ack { ok: false, error }`:
 *
 * - `duplicate-start` — `start()` was called for a `(repo, issue)`
 *   pair already in flight.
 * - `unknown-task` — `mergeReadyTask()` was passed a task id the
 *   supervisor does not own.
 * - `not-ready-to-merge` — `mergeReadyTask()` was called for a task
 *   not currently in `READY_TO_MERGE`.
 * - `persistence` — the initial `start()` save failed (no record on
 *   disk; in-memory entry rolled back so callers can retry).
 */
export type SupervisorErrorKind =
  | "duplicate-start"
  | "unknown-task"
  | "not-ready-to-merge"
  | "persistence";

/**
 * Domain error class thrown by the supervisor for caller-visible
 * failures (double-start, unknown task, brand violation). FSM-internal
 * failures (worktree creation, PR open, merge) are *not* thrown — they
 * land the task in `FAILED` (or `NEEDS_HUMAN` for non-mergeable PRs)
 * and the caller observes the terminal record's `terminalReason`.
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
 * @throws by {@link TaskSupervisorImpl.start} and
 *   {@link TaskSupervisorImpl.mergeReadyTask} when the FSM rejects the
 *   call before any state transition is persisted.
 */
export class SupervisorError extends Error {
  /** Discriminator visible in stack traces and `error.name === ...` checks. */
  override readonly name = "SupervisorError";
  /** Category tag — the daemon's `/merge` dispatcher reads this. */
  readonly kind: SupervisorErrorKind;

  /**
   * Build a supervisor error.
   *
   * @param kind Failure category (see {@link SupervisorErrorKind}).
   * @param message Human-readable description.
   * @param options Optional standard `cause` carrying the underlying
   *   exception.
   */
  constructor(
    kind: SupervisorErrorKind,
    message: string,
    options?: { readonly cause?: unknown },
  ) {
    super(message, options);
    this.kind = kind;
  }
}

/**
 * Categories of {@link MergeError} the supervisor produces during
 * `READY_TO_MERGE → MERGED`. Mapped onto a destination FSM state by
 * {@link mergeErrorTerminalState}:
 *
 * - `not-mergeable` — GitHub refused the merge because the PR was
 *   not in a mergeable state (HTTP `405`, conflicts, base protection,
 *   stale head SHA). The supervisor escalates to `NEEDS_HUMAN` so an
 *   operator can investigate without losing the PR.
 * - `transient` — a 5xx, network glitch, or other category-unaware
 *   failure. The supervisor lands the task in `FAILED` so a follow-up
 *   `start()` (or, for manual mode, a follow-up `/merge`) can retry.
 */
export type MergeErrorCategory = "not-mergeable" | "transient";

/**
 * Domain error wrapping a `mergePullRequest` failure with a
 * caller-visible `category` so the supervisor can decide whether to
 * escalate the task to `NEEDS_HUMAN` (the PR is genuinely
 * non-mergeable: conflicts, base-branch protection, stale head) or
 * `FAILED` (the API call faulted for a transient reason).
 *
 * The class never escapes the supervisor — it is constructed inside
 * the merge step and consumed by the FSM transition. Tests inspect the
 * resulting `terminalReason` rather than this class directly.
 *
 * @example
 * ```ts
 * try {
 *   await client.mergePullRequest(repo, prNumber, "squash");
 * } catch (error) {
 *   throw classifyMergeError(error);
 * }
 * ```
 */
export class MergeError extends Error {
  /** Discriminator visible in stack traces and `error.name === ...` checks. */
  override readonly name = "MergeError";
  /** Category tag — drives the FSM destination state. */
  readonly category: MergeErrorCategory;

  /**
   * Build a merge error.
   *
   * @param category Whether the failure is the PR being non-mergeable
   *   (operator action required) or a transient API fault.
   * @param message Human-readable description.
   * @param options Optional standard `cause` carrying the underlying
   *   GitHub-client exception.
   */
  constructor(
    category: MergeErrorCategory,
    message: string,
    options?: { readonly cause?: unknown },
  ) {
    super(message, options);
    this.category = category;
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
  const preserveWorktreeOnMerge: boolean = opts.preserveWorktreeOnMerge ?? false;

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
        "persistence",
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
    // Stub bodies for the three stabilize sub-phases. Each one walks
    // through `REBASE → CI → CONVERSATIONS` so observers see the
    // expected timeline; Wave 4 fills in the per-phase logic.
    let current = task;
    for (const phase of ["REBASE", "CI", "CONVERSATIONS"] as const) {
      current = await transition(
        current,
        { state: "STABILIZING", stabilizePhase: phase },
        `stabilize phase ${phase} (stub)`,
      );
    }
    return await transition(
      current,
      { state: "READY_TO_MERGE" },
      "stabilize loop settled",
    );
  }

  /**
   * Side effect for `READY_TO_MERGE`. Three branches:
   *
   *  - **`manual`** parks the FSM at `READY_TO_MERGE` without calling
   *    `mergePullRequest`. The auto-driver loop in {@link drive}
   *    returns the task; the operator unblocks it later through
   *    {@link mergeReadyTask} (wired to the `/merge <task-id>`
   *    slash command).
   *  - **`squash` / `rebase`** call the GitHub API straight through
   *    {@link runMergeStep}.
   *
   * Any failure path inside `runMergeStep` is absorbed into a
   * persisted `MERGED | NEEDS_HUMAN | FAILED` transition; this helper
   * never throws.
   */
  async function runReadyToMerge(task: Task): Promise<Task> {
    if (task.mergeMode === "manual") {
      // Park the task; an operator takes over by issuing
      // `/merge <task-id>`. The driver loop in `drive()` returns this
      // task as-is, leaving the in-memory + persistence record at
      // `READY_TO_MERGE`. We do **not** publish a "parked" event —
      // the `READY_TO_MERGE` state-changed event already published by
      // the stabilize loop's exit covers the observer's needs.
      return task;
    }
    return await runMergeStep(task, task.mergeMode);
  }

  /**
   * Perform the merge-then-cleanup pipeline for a task at
   * `READY_TO_MERGE`. The flow is:
   *
   *   1. Validate the precondition (`prNumber !== undefined`).
   *   2. Call `mergePullRequest(repo, prNumber, mode)`.
   *   3. Classify any failure via {@link classifyMergeError} and
   *      escalate to `NEEDS_HUMAN` (non-mergeable PR) or `FAILED`
   *      (transient API fault).
   *   4. On success, optionally tear down the worktree per
   *      `preserveWorktreeOnMerge` and persist `MERGED`.
   *
   * Used both from {@link runReadyToMerge} (auto-merge) and from
   * {@link mergeReadyTask} (the `/merge` slash command) so the two
   * paths share identical error handling and cleanup behaviour.
   *
   * @param task The READY_TO_MERGE task record.
   * @param mode The merge strategy to use. Callers pass the task's
   *   own `mergeMode` for auto-merge; the manual-merge entry point
   *   substitutes a concrete strategy when the task itself was
   *   configured with `manual`.
   * @returns The persisted post-merge record (`MERGED`,
   *   `NEEDS_HUMAN`, or `FAILED`).
   */
  async function runMergeStep(task: Task, mode: MergeMode): Promise<Task> {
    if (task.prNumber === undefined) {
      return await fail(
        task,
        new Error("READY_TO_MERGE entered without a PR number"),
        "merge-precondition",
      );
    }
    if (mode === "manual") {
      // Defensive: `manual` is not a GitHub merge_method; the caller
      // (mergeReadyTask) already substitutes a concrete mode before
      // reaching here. Treat any leak as a programming error rather
      // than a silent escalation.
      return await fail(
        task,
        new Error('runMergeStep called with mode="manual"; expected squash or rebase'),
        "merge-precondition",
      );
    }
    try {
      await opts.githubClient.mergePullRequest(task.repo, task.prNumber, mode);
    } catch (error) {
      const merge = classifyMergeError(error);
      logger.warn(
        `supervisor: mergePullRequest failed for task ${task.id} (${merge.category}): ${merge.message}`,
      );
      if (merge.category === "not-mergeable") {
        return await escalateToHuman(task, merge);
      }
      return await fail(task, merge, "merge");
    }
    const merged = await transition(
      task,
      { state: "MERGED", terminalReason: "merged" },
      `PR merged (${mode})`,
    );
    await maybeCleanupWorktree(merged);
    return merged;
  }

  /**
   * Tear down the per-task worktree after a successful merge unless
   * `preserveWorktreeOnMerge` is set. Cleanup failures are logged but
   * never re-thrown — the merge is the load-bearing transition; a
   * stuck worktree is recoverable manually and the FSM has already
   * persisted `MERGED`.
   *
   * Logs a single info line either way so an operator scanning logs
   * sees what happened to the worktree without grepping multiple
   * sources.
   */
  async function maybeCleanupWorktree(task: Task): Promise<void> {
    const worktreePath = task.worktreePath;
    if (worktreePath === undefined) {
      return;
    }
    if (preserveWorktreeOnMerge) {
      logger.info(
        `supervisor: preserving worktree for task ${task.id} at ${worktreePath}`,
      );
      return;
    }
    try {
      await opts.worktreeManager.removeWorktree(task.id);
      logger.info(
        `supervisor: removed worktree for task ${task.id} at ${worktreePath}`,
      );
    } catch (error) {
      logger.warn(
        `supervisor: removeWorktree failed for task ${task.id} at ${worktreePath}: ${
          errorMessage(error)
        }`,
      );
    }
  }

  /**
   * Drive `task` to `NEEDS_HUMAN`, recording the merge error on
   * `terminalReason` and the bus's `error` event. Reserved for
   * non-mergeable PRs — i.e. cases an operator must look at (the
   * code is not at fault but the PR cannot be merged automatically:
   * conflicts, base-branch protection, stale head SHA).
   */
  async function escalateToHuman(task: Task, error: MergeError): Promise<Task> {
    publish(opts.eventBus, {
      taskId: task.id,
      atIso: clock.nowIso(),
      kind: "error",
      data: { message: `merge: ${error.message}` },
    });
    return await transition(
      task,
      {
        state: "NEEDS_HUMAN",
        terminalReason: `merge (not-mergeable): ${error.message}`,
      },
      "PR not mergeable; escalating to operator",
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

  /**
   * Implementation of {@link TaskSupervisorImpl.mergeReadyTask}.
   *
   * Validates the task exists and is at `READY_TO_MERGE` synchronously
   * (so a `/merge` for a `DRAFTING` or `MERGED` task fails fast with
   * a precise error), then re-uses {@link runMergeStep} to share the
   * GitHub call, classification, and cleanup behaviour with the
   * auto-merge path.
   */
  async function mergeReadyTask(
    taskId: TaskId,
    overrideMode?: MergeMode,
  ): Promise<Task> {
    const task = tasks.get(taskId);
    if (task === undefined) {
      throw new SupervisorError(
        "unknown-task",
        `no task found for id ${taskId}`,
      );
    }
    if (task.state !== "READY_TO_MERGE") {
      throw new SupervisorError(
        "not-ready-to-merge",
        `task ${taskId} is not at READY_TO_MERGE (current state: ${task.state})`,
      );
    }
    // The task's recorded `mergeMode` may be `manual` (the typical
    // case for `/merge`) or one of the auto modes (rare: an operator
    // can issue `/merge` to force-finish a parked auto-merge task
    // that was paused mid-flight by daemon restart). Either way, the
    // GitHub API needs a concrete strategy: `manual` falls back to
    // `squash` unless the caller overrode it.
    const mode: MergeMode = overrideMode ??
      (task.mergeMode === "manual" ? "squash" : task.mergeMode);
    return await runMergeStep(task, mode);
  }

  return {
    start,
    listTasks(): readonly Task[] {
      return [...tasks.values()];
    },
    getTask(taskId: TaskId): Task | undefined {
      return tasks.get(taskId);
    },
    mergeReadyTask,
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
 * HTTP status codes the supervisor treats as "PR is genuinely not
 * mergeable" — i.e. an operator must look at the PR before merging it.
 *
 * Per GitHub's [merge a PR](https://docs.github.com/en/rest/pulls/pulls#merge-a-pull-request)
 * semantics:
 *
 * - `405 Method Not Allowed` — the response GitHub sends for "Pull
 *   Request is not mergeable" (conflicts, missing reviews on a
 *   protected branch, etc.).
 * - `409 Conflict` — head SHA mismatch when the caller passed
 *   `sha`; treated the same way because the underlying state requires
 *   a fresh PR view to resolve.
 *
 * Centralised here (rather than inlined into a string match) so the
 * unit tests assert against the same constant the production path
 * keys off.
 */
export const MERGE_NOT_MERGEABLE_HTTP_STATUSES: readonly number[] = [405, 409];

/**
 * Classify a thrown error from `mergePullRequest` into a
 * {@link MergeError} carrying a category that drives the FSM
 * destination state. The classifier inspects:
 *
 *  1. Pre-classified `MergeError` instances pass through untouched
 *     (an already-classified caller wins).
 *  2. Errors carrying an HTTP `status` (Octokit's `RequestError`
 *     and similar shapes) get matched against
 *     {@link MERGE_NOT_MERGEABLE_HTTP_STATUSES}.
 *  3. Anything else falls back to `transient`.
 *
 * Exported alongside the supervisor so unit tests can assert the
 * mapping directly.
 *
 * @param error The raw error thrown by the GitHub client.
 * @returns A {@link MergeError} ready for FSM consumption.
 *
 * @example
 * ```ts
 * try {
 *   await client.mergePullRequest(repo, prNumber, "squash");
 * } catch (error) {
 *   const merge = classifyMergeError(error);
 *   if (merge.category === "not-mergeable") escalateToHuman(task, merge);
 * }
 * ```
 */
export function classifyMergeError(error: unknown): MergeError {
  if (error instanceof MergeError) {
    return error;
  }
  const status = readHttpStatus(error);
  const message = errorMessage(error);
  if (status !== undefined && MERGE_NOT_MERGEABLE_HTTP_STATUSES.includes(status)) {
    return new MergeError("not-mergeable", message, { cause: error });
  }
  return new MergeError("transient", message, { cause: error });
}

/**
 * Best-effort extraction of an HTTP status from an error. Mirrors
 * `src/github/client.ts`'s reader without importing it (keeps the
 * supervisor decoupled from the concrete client class).
 */
function readHttpStatus(error: unknown): number | undefined {
  if (typeof error !== "object" || error === null) {
    return undefined;
  }
  const candidate = error as { status?: unknown };
  if (typeof candidate.status === "number") {
    return candidate.status;
  }
  return undefined;
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
        "duplicate-start",
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
