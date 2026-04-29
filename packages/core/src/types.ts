/**
 * types.ts — foundational typed contracts for makina.
 *
 * Everything Wave 2+ implements **against** lives here: the task-state
 * machine, the events the supervisor publishes, branded primitive ids that
 * cannot be cross-assigned, and the thin interface stubs every consumer
 * imports (`GitHubClient`, `WorktreeManager`, etc.). The interface bodies
 * are deliberately conservative — Wave 1 freezes the surface that lets the
 * eight Wave 2 branches build in parallel; consumer waves narrow the types
 * further when they need to.
 *
 * **Branded ids.** Plain `string` and `number` are too loose: the daemon
 * passes around `TaskId`, `RepoFullName`, `IssueNumber`, and
 * `InstallationId` constantly, and a `TaskId` accidentally given where a
 * `RepoFullName` is expected is a hard bug to spot. Each id is a structural
 * brand: `string & { readonly __brand: "TaskId" }`. The constructors below
 * (`makeTaskId`, `makeRepoFullName`, …) validate the value, return the
 * branded type, and are the *only* way to mint one — the brand cannot be
 * forged from a plain literal because the brand symbol is private to this
 * module.
 *
 * @module
 */

import { MIN_GITHUB_INSTALLATION_ID, MIN_GITHUB_ISSUE_NUMBER } from "./constants.ts";

// ---------------------------------------------------------------------------
// Branded primitive ids
// ---------------------------------------------------------------------------

/**
 * Phantom symbol used to brand {@link TaskId}. The symbol is exported so
 * the documentation tool can resolve every reference, but its declared
 * (`declare const`) status means there is no runtime value: the brand can
 * only ever be produced by the `make*` constructors below.
 */
export declare const TASK_ID_BRAND: unique symbol;
/** Phantom symbol used to brand {@link RepoFullName}. See {@link TASK_ID_BRAND}. */
export declare const REPO_FULL_NAME_BRAND: unique symbol;
/** Phantom symbol used to brand {@link IssueNumber}. See {@link TASK_ID_BRAND}. */
export declare const ISSUE_NUMBER_BRAND: unique symbol;
/** Phantom symbol used to brand {@link InstallationId}. See {@link TASK_ID_BRAND}. */
export declare const INSTALLATION_ID_BRAND: unique symbol;

/**
 * Opaque identifier for a single task tracked by the supervisor.
 *
 * Constructed by `makeTaskId`. Cross-assignment to other branded id types
 * is a compile error.
 *
 * @example
 * ```ts
 * const id: TaskId = makeTaskId("task_2026-04-26T12-00-00_abc123");
 * ```
 */
export type TaskId = string & { readonly [TASK_ID_BRAND]: void };

/**
 * Owner-and-name pair, e.g. `"koraytaylan/makina"`.
 *
 * Constructed by `makeRepoFullName`, which validates the
 * `<owner>/<name>` shape.
 */
export type RepoFullName = string & { readonly [REPO_FULL_NAME_BRAND]: void };

/**
 * GitHub issue number within a single repository.
 *
 * Constructed by `makeIssueNumber`, which rejects non-positive integers.
 */
export type IssueNumber = number & { readonly [ISSUE_NUMBER_BRAND]: void };

/**
 * GitHub App installation id.
 *
 * Constructed by `makeInstallationId`, which rejects non-positive integers.
 */
export type InstallationId =
  & number
  & { readonly [INSTALLATION_ID_BRAND]: void };

/**
 * Construct a {@link TaskId} from a string.
 *
 * Trims surrounding whitespace and rejects empty strings. Wave 2's
 * supervisor mints task ids of the form `task_<isoDate>_<rand6>`; this
 * constructor only enforces non-empty so test harnesses can mint shorter
 * ids without coupling to that scheme.
 *
 * @param raw The candidate identifier.
 * @returns The branded {@link TaskId}.
 * @throws RangeError if `raw` is empty after trimming.
 *
 * @example
 * ```ts
 * const id = makeTaskId("task_2026-04-26T12-00-00_abc123");
 * ```
 */
export function makeTaskId(raw: string): TaskId {
  const trimmed = raw.trim();
  if (trimmed.length === 0) {
    throw new RangeError("TaskId cannot be empty");
  }
  return trimmed as TaskId;
}

/**
 * Construct a {@link RepoFullName} from `<owner>/<name>` text.
 *
 * Owner and name must each be non-empty and free of whitespace and slashes
 * (one slash separator only). The check matches the looseness of GitHub's
 * URL parser; stricter validation happens at the GitHub API boundary.
 *
 * @param raw The candidate `<owner>/<name>` string.
 * @returns The branded {@link RepoFullName}.
 * @throws RangeError if the shape is wrong.
 *
 * @example
 * ```ts
 * const repo = makeRepoFullName("koraytaylan/makina");
 * ```
 */
export function makeRepoFullName(raw: string): RepoFullName {
  const trimmed = raw.trim();
  const slashCount = trimmed.split("/").length - 1;
  if (slashCount !== 1) {
    throw new RangeError(
      `RepoFullName must be "<owner>/<name>"; got ${JSON.stringify(raw)}`,
    );
  }
  const [owner, name] = trimmed.split("/");
  if (owner === undefined || owner.length === 0) {
    throw new RangeError(
      `RepoFullName owner segment is empty: ${JSON.stringify(raw)}`,
    );
  }
  if (name === undefined || name.length === 0) {
    throw new RangeError(
      `RepoFullName name segment is empty: ${JSON.stringify(raw)}`,
    );
  }
  if (/\s/.test(trimmed)) {
    throw new RangeError(
      `RepoFullName cannot contain whitespace: ${JSON.stringify(raw)}`,
    );
  }
  return trimmed as RepoFullName;
}

/**
 * Construct an {@link IssueNumber}.
 *
 * @param raw The candidate issue number.
 * @returns The branded {@link IssueNumber}.
 * @throws RangeError if `raw` is not a positive integer.
 *
 * @example
 * ```ts
 * const issue = makeIssueNumber(42);
 * ```
 */
export function makeIssueNumber(raw: number): IssueNumber {
  if (!Number.isInteger(raw) || raw < MIN_GITHUB_ISSUE_NUMBER) {
    throw new RangeError(
      `IssueNumber must be a positive integer; got ${raw}`,
    );
  }
  return raw as IssueNumber;
}

/**
 * Construct an {@link InstallationId}.
 *
 * @param raw The candidate installation id.
 * @returns The branded {@link InstallationId}.
 * @throws RangeError if `raw` is not a positive integer.
 *
 * @example
 * ```ts
 * const installation = makeInstallationId(9876543);
 * ```
 */
export function makeInstallationId(raw: number): InstallationId {
  if (!Number.isInteger(raw) || raw < MIN_GITHUB_INSTALLATION_ID) {
    throw new RangeError(
      `InstallationId must be a positive integer; got ${raw}`,
    );
  }
  return raw as InstallationId;
}

// ---------------------------------------------------------------------------
// Task state machine
// ---------------------------------------------------------------------------

/**
 * Every state the per-issue task supervisor walks through.
 *
 * Mirrors the lifecycle diagram in `docs/lifecycle.md`. Wave 3 implements
 * the supervisor; this enum is the contract every event consumer (TUI,
 * persistence, integration tests) reads from.
 *
 * Terminal states: {@link "MERGED"}, {@link "NEEDS_HUMAN"},
 * {@link "FAILED"}.
 */
export const TASK_STATES = [
  "INIT",
  "CLONING_WORKTREE",
  "DRAFTING",
  "COMMITTING",
  "PUSHING",
  "PR_OPEN",
  "STABILIZING",
  "READY_TO_MERGE",
  "MERGED",
  "NEEDS_HUMAN",
  "FAILED",
] as const;

/**
 * Discriminant union of valid {@link Task.state} values.
 *
 * @example
 * ```ts
 * function isTerminal(state: TaskState): boolean {
 *   return state === "MERGED" || state === "NEEDS_HUMAN" || state === "FAILED";
 * }
 * ```
 */
export type TaskState = typeof TASK_STATES[number];

/**
 * Substates of {@link "STABILIZING"}; the stabilize loop's three phases.
 *
 * Wave 4 implements each phase; the contract lives here so events
 * generated by intermediate waves carry consistent labels.
 */
export const STABILIZE_PHASES = ["REBASE", "CI", "CONVERSATIONS"] as const;

/**
 * Discriminant union of valid {@link Task.stabilizePhase} values.
 */
export type StabilizePhase = typeof STABILIZE_PHASES[number];

/**
 * The merge mode applied once a task reaches {@link "READY_TO_MERGE"}.
 *
 * - `squash`: the supervisor performs the merge via the GitHub API.
 * - `rebase`: the supervisor performs the merge via the GitHub API using
 *   the rebase strategy.
 * - `manual`: the supervisor stops at `READY_TO_MERGE` and waits for
 *   `/merge`.
 */
export const MERGE_MODES = ["squash", "rebase", "manual"] as const;

/** Discriminant union of valid merge modes. */
export type MergeMode = typeof MERGE_MODES[number];

/**
 * Persisted record of an in-flight (or recently-finished) task.
 *
 * The supervisor mutates this through narrow transition helpers; the
 * Persistence module writes it atomically to JSON. `prNumber`,
 * `branchName`, `worktreePath`, `sessionId`, `lastReviewAtIso`, and
 * `stabilizePhase` are absent until their respective stages produce them.
 *
 * @example
 * ```ts
 * const task: Task = {
 *   id: makeTaskId("task_2026-04-26T12-00-00_abc123"),
 *   repo: makeRepoFullName("koraytaylan/makina"),
 *   issueNumber: makeIssueNumber(42),
 *   state: "INIT",
 *   mergeMode: "squash",
 *   model: "claude-sonnet-4-6",
 *   iterationCount: 0,
 *   createdAtIso: new Date().toISOString(),
 *   updatedAtIso: new Date().toISOString(),
 * };
 * ```
 */
export interface Task {
  /** Unique opaque task identifier. */
  readonly id: TaskId;
  /** Repository the task targets. */
  readonly repo: RepoFullName;
  /** Issue number within {@link Task.repo}. */
  readonly issueNumber: IssueNumber;
  /** Current state in the lifecycle graph. */
  readonly state: TaskState;
  /**
   * Active stabilize phase when {@link Task.state} === `"STABILIZING"`,
   * otherwise omitted.
   */
  readonly stabilizePhase?: StabilizePhase;
  /** Per-task merge mode (defaults to `lifecycle.mergeMode`). */
  readonly mergeMode: MergeMode;
  /** Anthropic model id used for this task's agent runs. */
  readonly model: string;
  /**
   * Number of agent iterations spent so far on this task. Bounded by
   * `agent.maxIterationsPerTask`.
   */
  readonly iterationCount: number;
  /**
   * Pull-request number once {@link Task.state} reaches `"PR_OPEN"` or
   * later.
   */
  readonly prNumber?: IssueNumber;
  /** Per-task feature branch name, e.g. `makina/issue-42`. */
  readonly branchName?: string;
  /** Absolute filesystem path of the per-task git worktree. */
  readonly worktreePath?: string;
  /**
   * Claude Agent SDK session id, reused across iterations so model context
   * carries forward.
   */
  readonly sessionId?: string;
  /**
   * ISO-8601 timestamp of the last review activity the supervisor has
   * processed; the conversations phase uses it as a high-water mark.
   */
  readonly lastReviewAtIso?: string;
  /** ISO-8601 timestamp of task creation. */
  readonly createdAtIso: string;
  /** ISO-8601 timestamp of the most recent state change. */
  readonly updatedAtIso: string;
  /**
   * Optional human-readable explanation of how the task reached a terminal
   * state (`MERGED`, `NEEDS_HUMAN`, `FAILED`).
   */
  readonly terminalReason?: string;
}

// ---------------------------------------------------------------------------
// Task events
// ---------------------------------------------------------------------------

/**
 * The kinds of event the supervisor publishes on the event bus.
 *
 * Wave 2 modules (`event-bus`, `daemon-server`) round-trip these events;
 * the TUI renders them in the scrollback. The narrow tag set lets the
 * renderer dispatch on `kind` without parsing payload text.
 */
export const TASK_EVENT_KINDS = [
  "state-changed",
  "log",
  "agent-message",
  "github-call",
  "error",
] as const;

/** Discriminant union of valid {@link TaskEvent.kind} values. */
export type TaskEventKind = typeof TASK_EVENT_KINDS[number];

/** A state transition crossed by the supervisor. */
export interface StateChangedPayload {
  /** State before the transition. */
  readonly fromState: TaskState;
  /** State after the transition. */
  readonly toState: TaskState;
  /**
   * Active stabilize phase after the transition, when relevant.
   */
  readonly stabilizePhase?: StabilizePhase;
  /** Optional one-line rationale for the transition (free text). */
  readonly reason?: string;
}

/**
 * Severity classes for `log` events.
 */
export const LOG_LEVELS = ["debug", "info", "warn", "error"] as const;

/** Discriminant union of valid log levels. */
export type LogLevel = typeof LOG_LEVELS[number];

/** A free-form log line emitted while a task is running. */
export interface LogPayload {
  /** Severity of the log line. */
  readonly level: LogLevel;
  /** Human-readable text. */
  readonly message: string;
}

/**
 * A streamed message from the agent runner. The shape mirrors the subset
 * of `SDKMessage` the supervisor cares about; the agent runner adapts the
 * full SDK type into this projection so test doubles avoid pulling in the
 * SDK.
 */
export interface AgentMessagePayload {
  /**
   * Direction the message flows. `assistant` and `tool-use` originate from
   * the model; `user` and `tool-result` originate from the harness.
   */
  readonly role: "user" | "assistant" | "system" | "tool-use" | "tool-result";
  /** Best-effort plain-text rendering of the message. */
  readonly text: string;
}

/** A single GitHub HTTP call (or GraphQL query) executed by the daemon. */
export interface GitHubCallPayload {
  /** HTTP method, e.g. `"GET"`. */
  readonly method: string;
  /**
   * Path or operation name, e.g. `"GET /repos/{owner}/{repo}/issues/{n}"` or
   * `"graphql:resolveReviewThread"`.
   */
  readonly endpoint: string;
  /** HTTP status code returned by GitHub, when known. */
  readonly statusCode?: number;
  /** Wall-clock duration of the request in milliseconds. */
  readonly durationMilliseconds?: number;
  /**
   * Single-line error message when the call rejected (network error,
   * 4xx/5xx that the client surfaces, JSON parse failure, etc.).
   * Absent on successful calls.
   *
   * Operators (and the TUI) read this to see *which* request failed
   * and why; without it, a sustained outage is invisible because the
   * supervisor only emits `github-call` after the request resolves.
   */
  readonly error?: string;
}

/** An error the supervisor surfaces to the TUI. */
export interface ErrorPayload {
  /** Single-sentence description suitable for a status bar. */
  readonly message: string;
  /** Optional stack trace or extended diagnostic. */
  readonly detail?: string;
}

/**
 * Discriminated union of the per-kind payload shapes.
 */
export type TaskEventPayload =
  | { readonly kind: "state-changed"; readonly data: StateChangedPayload }
  | { readonly kind: "log"; readonly data: LogPayload }
  | { readonly kind: "agent-message"; readonly data: AgentMessagePayload }
  | { readonly kind: "github-call"; readonly data: GitHubCallPayload }
  | { readonly kind: "error"; readonly data: ErrorPayload };

/**
 * A single event published to the bus for a given task.
 *
 * Wave 2's `event-bus` exposes `publish(taskId, event)` and a wildcard
 * subscription that fans out across `task:<id>` topics; the TUI treats
 * `at` as monotonic per task.
 */
export type TaskEvent =
  & {
    /** Task this event belongs to. */
    readonly taskId: TaskId;
    /** Wall-clock ISO timestamp at publish time. */
    readonly atIso: string;
  }
  & TaskEventPayload;

// ---------------------------------------------------------------------------
// Consumer-facing interfaces (thin contracts; bodies arrive in Wave 2+)
// ---------------------------------------------------------------------------

/**
 * Mints GitHub App installation tokens with expiry-aware caching.
 *
 * Wave 2's `src/github/app-auth.ts` implements this against
 * `@octokit/auth-app`. The in-memory double in
 * `tests/helpers/in_memory_github_auth.ts` returns deterministic fake
 * tokens and exposes a fake clock.
 */
export interface GitHubAuth {
  /**
   * Return a usable installation token for the given installation id.
   *
   * Tokens are cached and refreshed `INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS`
   * before their `expires_at`.
   *
   * @param installationId Target installation.
   * @returns A short-lived installation access token.
   */
  getInstallationToken(installationId: InstallationId): Promise<string>;
}

/**
 * Issue payload subset the supervisor needs.
 *
 * Wave 2's GitHub client returns this shape from `getIssue`; later waves
 * extend the interface in a backwards-compatible way (additive fields
 * only).
 */
export interface IssueDetails {
  /** Issue number within the repository. */
  readonly number: IssueNumber;
  /** Issue title. */
  readonly title: string;
  /** Issue body (markdown). */
  readonly body: string;
  /** Open/closed status. */
  readonly state: "open" | "closed";
}

/**
 * Pull-request payload subset the supervisor needs.
 */
export interface PullRequestDetails {
  /** Pull-request number within the repository. */
  readonly number: IssueNumber;
  /** Commit SHA of the head ref. */
  readonly headSha: string;
  /** Name of the head branch (the PR's source). */
  readonly headRef: string;
  /** Name of the base branch (the PR's target). */
  readonly baseRef: string;
  /** Open/closed/merged status. */
  readonly state: "open" | "closed" | "merged";
}

/**
 * Combined-status payload subset the supervisor reads during the CI phase.
 */
export interface CombinedStatus {
  /** Aggregate status across every contributing check. */
  readonly state: "pending" | "success" | "failure" | "error";
  /** Commit SHA the status applies to. */
  readonly sha: string;
}

/**
 * High-level GitHub client used by the supervisor.
 *
 * Wave 2's `src/github/client.ts` implements this against `@octokit/core`
 * with `@octokit/auth-app`. The in-memory double
 * `tests/helpers/in_memory_github_client.ts` records every call and lets
 * tests script per-method timelines.
 */
export interface GitHubClient {
  /** Fetch issue details. */
  getIssue(repo: RepoFullName, issueNumber: IssueNumber): Promise<IssueDetails>;
  /** Open a pull request. */
  createPullRequest(
    repo: RepoFullName,
    args: { headRef: string; baseRef: string; title: string; body: string },
  ): Promise<PullRequestDetails>;
  /** Request the listed reviewers (e.g. `"Copilot"`). */
  requestReviewers(
    repo: RepoFullName,
    pullRequestNumber: IssueNumber,
    reviewers: readonly string[],
  ): Promise<void>;
  /** Read the combined commit status for `sha`. */
  getCombinedStatus(repo: RepoFullName, sha: string): Promise<CombinedStatus>;
  /** Merge the pull request per `mode`. */
  mergePullRequest(
    repo: RepoFullName,
    pullRequestNumber: IssueNumber,
    mode: MergeMode,
  ): Promise<void>;
}

/**
 * Per-repo bare clone and per-task worktree management.
 *
 * Wave 2's `src/daemon/worktree-manager.ts` implements this over real
 * `git` invocations against `Deno.makeTempDir`-backed repos.
 */
export interface WorktreeManager {
  /** Ensure a bare clone of `repo` exists; clone if missing. */
  ensureBareClone(repo: RepoFullName): Promise<string>;
  /** Create a worktree for the given issue and return its absolute path. */
  createWorktreeForIssue(
    repo: RepoFullName,
    issueNumber: IssueNumber,
  ): Promise<string>;
  /** Tear down the worktree associated with `taskId`. */
  removeWorktree(taskId: TaskId): Promise<void>;
}

/**
 * Atomic JSON-backed task store.
 *
 * Wave 2's `src/daemon/persistence.ts` implements this with
 * write-tmp-then-rename semantics so a crash mid-write cannot corrupt the
 * canonical file.
 */
export interface Persistence {
  /** Load every persisted task on daemon start. */
  loadAll(): Promise<readonly Task[]>;
  /** Save (overwrite) a single task. */
  save(task: Task): Promise<void>;
  /** Remove the persisted record for a task. */
  remove(taskId: TaskId): Promise<void>;
}

/**
 * Subscription handle returned by {@link EventBus.subscribe}.
 *
 * Calling `unsubscribe` is idempotent.
 */
export interface EventSubscription {
  /** Stop receiving events on this subscription. */
  unsubscribe(): void;
}

/**
 * In-process pub/sub for {@link TaskEvent}s.
 *
 * Wave 2's `src/daemon/event-bus.ts` implements this with bounded
 * per-subscriber buffers; slow consumers drop events and a warning is
 * logged.
 */
export interface EventBus {
  /** Publish an event to subscribers of `event.taskId` and `*`. */
  publish(event: TaskEvent): void;
  /**
   * Subscribe to events for a specific task or every task (`*`).
   *
   * @param target Either a {@link TaskId} or the wildcard `"*"`.
   * @param handler Callback invoked synchronously per event; throwing
   *   inside `handler` does not propagate to the publisher.
   * @returns A handle whose `unsubscribe()` stops further deliveries.
   */
  subscribe(
    target: TaskId | "*",
    handler: (event: TaskEvent) => void,
  ): EventSubscription;
}

/**
 * Minimal projection of a single message streamed from the Claude Agent
 * SDK's `query()` call. The full SDK type lives behind
 * `npm:@anthropic-ai/claude-agent-sdk`; consumer waves project the SDK
 * type into this surface so the supervisor and the TUI never depend on
 * the SDK directly.
 *
 * Wave 1's mock runner ships its own structurally-identical type; Wave 3's
 * real runner adapts the SDK's `SDKMessage` into this shape.
 */
export interface AgentRunnerMessage {
  /** Direction the message flows. */
  readonly role: "user" | "assistant" | "system" | "tool-use" | "tool-result";
  /** Best-effort plain-text rendering of the message. */
  readonly text: string;
}

/**
 * Streams agent runs against a per-task worktree.
 *
 * Wave 3's `src/daemon/agent-runner.ts` wraps `query()` from
 * `@anthropic-ai/claude-agent-sdk`. Tests use the mock runner in
 * `tests/helpers/mock_agent_runner.ts`.
 */
export interface AgentRunner {
  /**
   * Run the agent against a task. The async iterable yields messages as
   * the model produces them. The runner persists `sessionId` after the
   * first message so subsequent calls resume.
   */
  runAgent(args: {
    readonly taskId: TaskId;
    readonly worktreePath: string;
    readonly prompt: string;
    readonly sessionId?: string;
    readonly model: string;
  }): AsyncIterable<AgentRunnerMessage>;
}

/**
 * Polling abstraction with backoff and rate-limit honoring.
 *
 * Wave 3's `src/daemon/poller.ts` implements one timer per task; the
 * abstraction is the contract the stabilize loop's CI and conversations
 * phases drive.
 */
export interface Poller {
  /**
   * Begin polling. The supervisor invokes `fetcher` every
   * `intervalMilliseconds` and forwards the result to `onResult` until
   * `cancel()` is called on the returned handle.
   */
  poll<TResult>(args: {
    readonly taskId: TaskId;
    readonly intervalMilliseconds: number;
    readonly fetcher: () => Promise<TResult>;
    readonly onResult: (result: TResult) => void;
  }): { cancel(): void };
}

/**
 * Owns the per-issue state machine.
 *
 * Wave 3's `src/daemon/supervisor.ts` implements this; consumer waves
 * (TUI, daemon server) interact with the supervisor only through this
 * interface.
 */
export interface TaskSupervisor {
  /**
   * Start a new task for `(repo, issueNumber)` if one is not already
   * in-flight. Returns the task id.
   */
  startTask(args: {
    readonly repo: RepoFullName;
    readonly issueNumber: IssueNumber;
    readonly mergeMode: MergeMode;
    readonly model: string;
  }): Promise<TaskId>;
  /** Return the in-memory snapshot of every task the supervisor knows. */
  listTasks(): readonly Task[];
  /** Cancel a non-terminal task; preserves the worktree. */
  cancelTask(taskId: TaskId): Promise<void>;
  /** Re-enter a `NEEDS_HUMAN` task after a manual fix. */
  retryTask(taskId: TaskId): Promise<void>;
  /** Force the merge of a `READY_TO_MERGE` task (used in `manual` mode). */
  mergeTask(taskId: TaskId): Promise<void>;
}
