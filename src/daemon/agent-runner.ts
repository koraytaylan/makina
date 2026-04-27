/**
 * daemon/agent-runner.ts — wraps `query()` from
 * `npm:@anthropic-ai/claude-agent-sdk` and surfaces a deterministic,
 * supervisor-friendly streaming interface.
 *
 * The supervisor (Wave 3) drives every agent iteration through this module:
 *
 *  1. Mint or recover the task's `sessionId` (see `Task` in `src/types.ts`).
 *  2. Call {@link AgentRunnerImpl.runAgent} with the per-task
 *     `worktreePath` pinned as `cwd`.
 *  3. Iterate the returned async iterable. Each yielded
 *     {@link AgentRunnerMessage} is also published onto the injected
 *     {@link EventBus} as a `task-event` of kind `agent-message`,
 *     **tagged with the same {@link TaskId}** so TUI subscribers can
 *     correlate.
 *  4. Persist {@link AgentRunResult.sessionId} after the iterable settles
 *     and pass it back on subsequent calls so model context carries
 *     forward across the CI / review / rebase loop.
 *
 * **Auto-discovery.** The Claude Code CLI binary is resolved once per
 * factory via `which claude` (the path can be overridden through
 * {@link AgentRunnerOptions.pathToClaudeCodeExecutable}). Discovery is
 * lazy: the first `runAgent` call awaits the lookup, subsequent calls
 * reuse the memoized path. If `claude` is not on PATH and no override was
 * passed, the call rejects with an {@link AgentRunnerError} carrying a
 * remediation hint.
 *
 * **System-prompt addendum.** The runner appends a short paragraph to the
 * SDK preset's system prompt (via the `append` field of `systemPrompt`)
 * that instructs the agent to author commits in the
 * `<type>(#<issueNumber>): <subject>` Conventional Commits format that
 * Wave 0's {@link https://github.com/koraytaylan/makina/blob/develop/docs/adrs/009-conventional-commits-and-git-cliff.md ADR-009}
 * standardises across the codebase. The supervisor passes the issue
 * number; the runner formats the addendum.
 *
 * **Session resumption.** When `sessionId` is provided to `runAgent`, the
 * runner forwards it as `Options.resume` so the SDK reattaches to the
 * persisted JSONL session. The first message that arrives carries the
 * SDK-assigned `session_id`, which the runner returns to the caller via
 * {@link AgentRunResult.sessionId}.
 *
 * **Test seam.** The factory accepts an optional `query` injection so unit
 * tests drive the runner against a scripted async iterable instead of
 * spawning the real CLI subprocess. The integration test in
 * `tests/integration/agent_runner_real_test.ts` exercises the production
 * path end-to-end and is gated behind `AGENT_RUNNER_REAL_TEST=1`.
 *
 * @module
 */

import { getLogger } from "@std/log";
import { isAbsolute } from "@std/path";

import { AGENT_MESSAGE_TEXT_TRUNCATION_CODE_UNITS } from "../constants.ts";
import type {
  AgentRunner,
  AgentRunnerMessage,
  EventBus,
  IssueNumber,
  TaskEvent,
  TaskId,
} from "../types.ts";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/**
 * Narrow projection of `SDKMessage` from `@anthropic-ai/claude-agent-sdk`.
 *
 * The SDK's `SDKMessage` is a wide discriminated union (28+ variants:
 * `assistant`, `user`, `result`, `system/init`, `tool_progress`, …). The
 * runner only consumes the few fields below; defining the structural type
 * here means tests can drive the runner with a scripted iterable without
 * importing the SDK from the test graph (the SDK ships a multi-megabyte
 * native binary per platform).
 *
 * The `type` discriminant matches the SDK's wire-level field; the runner
 * preserves it verbatim so the TUI can dispatch on it. Any field the
 * runner does not use is permitted via the index signature so a richer
 * SDK message still satisfies the type at the call site.
 */
export interface SdkMessageProjection {
  /**
   * SDK-level discriminant. The values used by the runner today are
   * `"assistant"`, `"user"`, `"result"`, `"system"`,
   * `"tool_progress"`, and `"tool_use_summary"`; any other string is
   * tolerated and surfaces as a `system` agent-message kind.
   */
  readonly type: string;
  /** SDK-assigned session id; populated on every message. */
  readonly session_id?: string;
  /**
   * The model's response payload for `type === "assistant"` messages
   * **and** the tool-output payload for `type === "user"` messages.
   * Mirrors the SDK's `BetaMessage` / `MessageParam` shape; only
   * `content` is consumed.
   */
  readonly message?: {
    readonly content?: ReadonlyArray<SdkContentBlockProjection>;
  };
  /**
   * The SDK's final result payload for `type === "result"` messages.
   * The runner surfaces this as the rendered text when the SDK signals
   * the run has settled.
   */
  readonly result?: string;
  /** Free-form fields that may exist on richer SDK message variants. */
  readonly [key: string]: unknown;
}

/**
 * One content block inside an assistant message. Only `text` and
 * `tool_use` blocks contribute to the rendered text projection; everything
 * else is ignored.
 */
export interface SdkContentBlockProjection {
  /** Block discriminant: `"text"`, `"tool_use"`, etc. */
  readonly type: string;
  /** Rendered text for `text` blocks. */
  readonly text?: string;
  /** Tool name for `tool_use` blocks. */
  readonly name?: string;
  /** Tool input for `tool_use` blocks. */
  readonly input?: unknown;
}

/**
 * Subset of the SDK's `Options` surface the runner forwards to `query()`.
 *
 * The runner only sets the fields it needs; everything else is left at the
 * SDK default. The shape is intentionally structural so a stub `query`
 * implementation in tests can match it without importing the SDK.
 */
export interface SdkQueryOptions {
  /** Working directory for the agent. */
  readonly cwd: string;
  /** JavaScript runtime executing Claude Code. */
  readonly executable: "deno" | "node" | "bun";
  /** Permission mode applied to tool calls. */
  readonly permissionMode: "default" | "acceptEdits" | "bypassPermissions" | "plan" | "dontAsk";
  /** Anthropic model id, e.g. `"claude-sonnet-4-6"`. */
  readonly model: string;
  /** Resolved path to the `claude` CLI binary. */
  readonly pathToClaudeCodeExecutable: string;
  /** SDK session id to resume from, when present. */
  readonly resume?: string;
  /** System-prompt configuration; the runner appends the commit-format addendum. */
  readonly systemPrompt: {
    readonly type: "preset";
    readonly preset: "claude_code";
    readonly append: string;
  };
}

/**
 * Structural projection of the SDK's `query()` function. Tests pass a stub
 * matching this shape; production code passes the SDK's real `query`.
 *
 * The return value is iterated as an `AsyncIterable<SdkMessageProjection>`.
 * The SDK's `Query` extends `AsyncGenerator<SDKMessage, void>`, which
 * trivially satisfies this signature.
 */
export type SdkQueryFunction = (params: {
  readonly prompt: string;
  readonly options: SdkQueryOptions;
}) => AsyncIterable<SdkMessageProjection>;

/**
 * Construction options for {@link createAgentRunner}.
 */
export interface AgentRunnerOptions {
  /** Event bus to publish `agent-message` events onto. */
  readonly eventBus: EventBus;
  /**
   * Override the path to the `claude` CLI binary. When omitted, the
   * runner discovers it via `which claude` on the first call and memoizes
   * the result.
   */
  readonly pathToClaudeCodeExecutable?: string;
  /**
   * Inject the SDK's `query` function. Production code leaves this
   * undefined and the factory imports the real `query` from
   * `@anthropic-ai/claude-agent-sdk` the first time a run starts.
   *
   * Tests pass a scripted stub so no real subprocess is spawned.
   */
  readonly query?: SdkQueryFunction;
  /**
   * Inject the `which`-style executable resolver. The default uses
   * `Deno.Command("which", [name])`; tests pass a stub that returns a
   * synthetic path (or rejects with `Deno.errors.NotFound` to drive the
   * "claude not on PATH" branch). Always preferred over global mutation
   * (lessons learned #3).
   */
  readonly resolveExecutable?: ExecutableResolver;
  /**
   * Logger used for non-fatal warnings (e.g. an event-bus publish that
   * threw because a subscriber misbehaved). Defaults to `getLogger()`
   * from `@std/log`, adapted to the {@link AgentRunnerLogger} surface.
   */
  readonly logger?: AgentRunnerLogger;
  /**
   * Wall-clock source used to stamp event-bus publishes. Defaults to
   * `() => new Date().toISOString()`. Tests inject a deterministic clock
   * to keep snapshots stable.
   */
  readonly nowIso?: () => string;
}

/**
 * Resolves an executable name to an absolute path, mirroring the success
 * subset of POSIX `which(1)`.
 */
export type ExecutableResolver = (name: string) => Promise<string>;

/**
 * Narrow logger surface used by the agent runner. The `@std/log` `Logger`
 * class satisfies this shape (its `warn` accepts a string), but the
 * narrower interface lets tests inject a recording double without
 * matching the SDK's full overload signature.
 */
export interface AgentRunnerLogger {
  /** Emit a warning. */
  warn(message: string): void;
}

/**
 * Arguments accepted by {@link AgentRunnerImpl.runAgent}.
 *
 * Mirrors the {@link AgentRunner.runAgent} contract from `src/types.ts`
 * and adds an optional `issueNumber`, which the runner uses to format the
 * conventional-commits system-prompt addendum. `issueNumber` is optional
 * so the runner remains assignable to the W1 `AgentRunner` interface
 * (which does not include it). When omitted, the addendum is rendered
 * with the issue scope dropped — the format is `<type>: <subject>`
 * instead of `<type>(#<n>): <subject>` — and the model is told to author
 * commits in plain Conventional Commits without an issue scope. The
 * supervisor (the only production caller) always passes `issueNumber`;
 * the optional shape exists so consumers typed as `AgentRunner` cannot
 * trip a `#undefined` rendering.
 */
export interface RunAgentArgs {
  /** Task this run belongs to; events are published tagged with this id. */
  readonly taskId: TaskId;
  /**
   * GitHub issue number; embedded into the system-prompt addendum. When
   * omitted (e.g. a generic `AgentRunner` consumer that has not adopted
   * the W3 extension) the runner builds an addendum without the
   * `(#<n>)` issue scope; the commit format degrades to plain
   * Conventional Commits.
   */
  readonly issueNumber?: IssueNumber;
  /** Absolute path of the per-task git worktree; pinned as `cwd`. */
  readonly worktreePath: string;
  /** Prompt to send to the model. */
  readonly prompt: string;
  /**
   * SDK session id to resume; omit on the first iteration of a task. The
   * runner returns the SDK-assigned session id via the resolved
   * {@link AgentRunResult}.
   */
  readonly sessionId?: string;
  /** Anthropic model id, e.g. `"claude-sonnet-4-6"`. */
  readonly model: string;
}

/**
 * Settled result of a `runAgent` call.
 *
 * Returned by {@link AgentRunnerImpl.runAgent} once the async iterable is
 * exhausted. The supervisor stores `sessionId` on the task record so the
 * next iteration can resume.
 */
export interface AgentRunResult {
  /**
   * SDK-assigned session id observed during the run. Present when at
   * least one SDK message carrying a `session_id` was observed; absent
   * when the SDK rejected before producing any message (e.g. the `claude`
   * binary is missing — the SDK emits a single `result/error` first; we
   * still surface its session id when present).
   */
  readonly sessionId?: string;
  /** Number of SDK messages observed across the run. */
  readonly messageCount: number;
}

/**
 * Concrete return type of {@link createAgentRunner}.
 *
 * Widens the W1 {@link AgentRunner} contract with a `runAndCollect`
 * helper that exhausts the iterable and returns the
 * {@link AgentRunResult} so the supervisor never has to manage the
 * `sessionId` plumbing twice.
 */
export interface AgentRunnerImpl extends AgentRunner {
  /**
   * Run the agent and return the settled {@link AgentRunResult}.
   *
   * Equivalent to iterating {@link AgentRunner.runAgent} to exhaustion and
   * tracking the last observed SDK session id; the helper exists so the
   * supervisor's hot path is one call.
   *
   * @param args Run arguments. See {@link RunAgentArgs}.
   * @returns Resolves with the SDK session id (when observed) and the
   *   total message count.
   * @throws {AgentRunnerError} when the SDK rejects, the executable cannot
   *   be discovered, or the runner is asked to run with empty arguments.
   *
   * @example
   * ```ts
   * const runner = createAgentRunner({ eventBus });
   * const result = await runner.runAndCollect({
   *   taskId,
   *   issueNumber: makeIssueNumber(42),
   *   worktreePath: "/var/lib/makina/worktrees/owner__name/issue-42",
   *   prompt: "Implement the feature.",
   *   model: "claude-sonnet-4-6",
   * });
   * task.sessionId = result.sessionId;
   * ```
   */
  runAndCollect(args: RunAgentArgs): Promise<AgentRunResult>;

  /**
   * Run the agent. Yields the SDK messages as they arrive, also
   * publishing each one onto the {@link EventBus} as an `agent-message`
   * tagged with `args.taskId`.
   *
   * The yielded messages are the runner's structural projection of the
   * SDK type — the supervisor and the TUI never see the raw SDK shape.
   *
   * @param args Run arguments. See {@link RunAgentArgs}.
   * @returns Async iterable of {@link AgentRunnerMessage}s.
   * @throws {AgentRunnerError} when the SDK rejects, the executable cannot
   *   be discovered, or the runner is asked to run with empty arguments.
   */
  runAgent(args: RunAgentArgs): AsyncIterable<AgentRunnerMessage>;
}

/**
 * Domain error raised when the runner cannot complete a `runAgent` call.
 *
 * Wraps the underlying cause (executable lookup failure, SDK exception)
 * and carries the operation name plus the offending field so the
 * supervisor can render a precise event without unpacking the cause
 * chain.
 */
export class AgentRunnerError extends Error {
  /**
   * Build an agent-runner error.
   *
   * @param message Human-readable description.
   * @param operation Short name of the operation that failed
   *   (e.g. `"resolveExecutable"`, `"sdkQuery"`, `"validateArgs"`).
   * @param field Optional argument field that failed validation
   *   (e.g. `"worktreePath"`, `"prompt"`, `"model"`).
   * @param cause Underlying error chained for diagnostics.
   */
  constructor(
    message: string,
    public readonly operation: string,
    public readonly field?: string,
    public override readonly cause?: unknown,
  ) {
    super(message);
    this.name = "AgentRunnerError";
  }
}

// ---------------------------------------------------------------------------
// Public factory
// ---------------------------------------------------------------------------

/**
 * Construct an {@link AgentRunnerImpl}.
 *
 * The factory does not perform any IO; it returns an object whose methods
 * lazily resolve the `claude` binary and the SDK `query` function the
 * first time `runAgent` is called. This keeps construction cheap and
 * makes the runner trivial to instantiate inside tests.
 *
 * @param opts Configuration. See {@link AgentRunnerOptions}.
 * @returns A fresh {@link AgentRunnerImpl}.
 * @throws {RangeError} when `opts.eventBus` is missing.
 *
 * @example
 * ```ts
 * import { createAgentRunner } from "./agent-runner.ts";
 * import { createEventBus } from "./event-bus.ts";
 *
 * const eventBus = createEventBus();
 * const runner = createAgentRunner({ eventBus });
 * for await (const msg of runner.runAgent({
 *   taskId, issueNumber, worktreePath, prompt, model,
 * })) {
 *   console.log(msg.role, msg.text);
 * }
 * ```
 */
export function createAgentRunner(opts: AgentRunnerOptions): AgentRunnerImpl {
  if (opts.eventBus === undefined) {
    throw new RangeError("AgentRunnerOptions.eventBus is required");
  }
  const eventBus = opts.eventBus;
  const logger = opts.logger ?? defaultLogger();
  const nowIso = opts.nowIso ?? defaultNowIso;
  const resolveExecutable = opts.resolveExecutable ?? defaultResolveExecutable;
  const overrideExecutablePath = opts.pathToClaudeCodeExecutable;
  const overrideQuery = opts.query;

  // Memoized resolutions. Both are populated lazily on the first run; a
  // failure during the first run is *not* cached, so a transient `which`
  // failure does not poison subsequent calls.
  let memoizedExecutablePath: string | undefined = overrideExecutablePath;
  let memoizedQuery: SdkQueryFunction | undefined = overrideQuery;

  async function resolveClaudeExecutable(): Promise<string> {
    if (memoizedExecutablePath !== undefined) {
      return memoizedExecutablePath;
    }
    try {
      const path = await resolveExecutable("claude");
      memoizedExecutablePath = path;
      return path;
    } catch (caught) {
      throw new AgentRunnerError(
        "Could not locate the `claude` CLI on PATH. " +
          "Install Claude Code (https://docs.anthropic.com/en/docs/claude-code) " +
          "or pass `pathToClaudeCodeExecutable` to createAgentRunner().",
        "resolveExecutable",
        "pathToClaudeCodeExecutable",
        caught,
      );
    }
  }

  async function resolveSdkQuery(): Promise<SdkQueryFunction> {
    if (memoizedQuery !== undefined) {
      return memoizedQuery;
    }
    // The SDK is heavyweight; defer the import to first use so the
    // module load cost is paid only when the runner is actually invoked.
    // The cast is structural: the SDK's `query` returns a `Query` that
    // is `AsyncGenerator<SDKMessage, void>`, which matches our narrower
    // `AsyncIterable<SdkMessageProjection>` projection.
    let imported: { query: unknown };
    try {
      imported = await import("@anthropic-ai/claude-agent-sdk");
    } catch (caught) {
      throw new AgentRunnerError(
        "Failed to load `@anthropic-ai/claude-agent-sdk`. " +
          "Run `deno task ci` to ensure the npm dependency is cached.",
        "loadSdk",
        undefined,
        caught,
      );
    }
    if (typeof imported.query !== "function") {
      throw new AgentRunnerError(
        "`@anthropic-ai/claude-agent-sdk` did not export a `query` function. " +
          "The SDK's surface may have changed; pin a compatible version in deno.json.",
        "loadSdk",
      );
    }
    const queryFn = imported.query as SdkQueryFunction;
    memoizedQuery = queryFn;
    return queryFn;
  }

  function validateArgs(args: RunAgentArgs): void {
    if (args.worktreePath.length === 0) {
      throw new AgentRunnerError(
        "RunAgentArgs.worktreePath must be a non-empty absolute path",
        "validateArgs",
        "worktreePath",
      );
    }
    if (!isAbsolute(args.worktreePath)) {
      throw new AgentRunnerError(
        `RunAgentArgs.worktreePath must be an absolute path (got ${
          JSON.stringify(args.worktreePath)
        })`,
        "validateArgs",
        "worktreePath",
      );
    }
    if (args.prompt.length === 0) {
      throw new AgentRunnerError(
        "RunAgentArgs.prompt must be a non-empty string",
        "validateArgs",
        "prompt",
      );
    }
    if (args.model.length === 0) {
      throw new AgentRunnerError(
        "RunAgentArgs.model must be a non-empty string",
        "validateArgs",
        "model",
      );
    }
  }

  /**
   * Build the SDK options that bind the agent to the per-task worktree
   * and append the conventional-commits addendum.
   */
  function buildSdkOptions(args: RunAgentArgs, executablePath: string): SdkQueryOptions {
    const base = {
      cwd: args.worktreePath,
      executable: "deno" as const,
      permissionMode: "acceptEdits" as const,
      model: args.model,
      pathToClaudeCodeExecutable: executablePath,
      systemPrompt: {
        type: "preset" as const,
        preset: "claude_code" as const,
        append: buildSystemPromptAddendum(args.issueNumber),
      },
    };
    return args.sessionId === undefined ? base : { ...base, resume: args.sessionId };
  }

  /**
   * Adapt one SDK message into the runner's projection (yielded to the
   * iterable consumer at full length) plus the truncated payload for the
   * event bus, plus the session id (if observed).
   *
   * The iterable carries the full rendered text so a consumer that wants
   * the entire tool output (e.g. a future persistence sink) is not
   * silently capped at the bus budget. Only the bus-side payload is
   * truncated to the
   * {@link AGENT_MESSAGE_TEXT_TRUNCATION_CODE_UNITS} ceiling so subscriber
   * queues stay bounded.
   */
  function adaptMessage(
    sdkMessage: SdkMessageProjection,
  ): {
    readonly projection: AgentRunnerMessage;
    readonly busPayload: AgentRunnerMessage;
    readonly sessionId: string | undefined;
  } {
    const role = mapSdkTypeToRole(sdkMessage.type);
    const text = renderSdkMessageText(sdkMessage);
    const truncated = truncateForBus(text);
    const projection: AgentRunnerMessage = { role, text };
    const busPayload: AgentRunnerMessage = truncated === text
      ? projection
      : { role, text: truncated };
    return {
      projection,
      busPayload,
      sessionId: typeof sdkMessage.session_id === "string" ? sdkMessage.session_id : undefined,
    };
  }

  /**
   * Publish an `agent-message` event tagged with `taskId`. Caller is the
   * runner; subscribers see the bus-budget projection (truncated when the
   * rendered text exceeded {@link AGENT_MESSAGE_TEXT_TRUNCATION_CODE_UNITS}).
   * The yielded iterable carries the full text — see {@link adaptMessage}.
   */
  function publishMessage(taskId: TaskId, busPayload: AgentRunnerMessage): void {
    const event: TaskEvent = {
      taskId,
      atIso: nowIso(),
      kind: "agent-message",
      data: busPayload,
    };
    try {
      eventBus.publish(event);
    } catch (caught) {
      // The bus contract isolates handler failures inside the pump;
      // reaching this branch means a synchronous publisher-side throw
      // (e.g. an enqueue-side bug). We log and continue so a flaky bus
      // does not abort the agent loop.
      logger.warn(
        `agent-runner: event-bus publish threw, continuing. error=${stringifyError(caught)}`,
      );
    }
  }

  async function* runAgentInternal(
    args: RunAgentArgs,
    onResult: (result: AgentRunResult) => void,
  ): AsyncIterable<AgentRunnerMessage> {
    validateArgs(args);
    const [executablePath, queryFn] = await Promise.all([
      resolveClaudeExecutable(),
      resolveSdkQuery(),
    ]);
    const options = buildSdkOptions(args, executablePath);

    let messageCount = 0;
    let observedSessionId: string | undefined = args.sessionId;

    try {
      // `queryFn(...)` lives inside the try so a synchronous SDK throw
      // (e.g. test stub raising before yielding the iterable, or the
      // production SDK validating its arguments eagerly) is wrapped in
      // an `AgentRunnerError` consistent with the iteration-failure
      // path. Without this, a sync throw would bypass the wrapping and
      // surface a raw error type to the supervisor.
      const stream = queryFn({ prompt: args.prompt, options });
      for await (const sdkMessage of stream) {
        messageCount += 1;
        const adapted = adaptMessage(sdkMessage);
        if (adapted.sessionId !== undefined) {
          observedSessionId = adapted.sessionId;
        }
        publishMessage(args.taskId, adapted.busPayload);
        yield adapted.projection;
      }
    } catch (caught) {
      throw new AgentRunnerError(
        `Claude Agent SDK query failed: ${stringifyError(caught)}`,
        "sdkQuery",
        undefined,
        caught,
      );
    }
    onResult(
      observedSessionId === undefined ? { messageCount } : {
        sessionId: observedSessionId,
        messageCount,
      },
    );
  }

  function runAgent(args: RunAgentArgs): AsyncIterable<AgentRunnerMessage> {
    // The supervisor's hot path doesn't read the AgentRunResult; the
    // helper below does. Pass a no-op so the iterable contract is the
    // {@link AgentRunner.runAgent} signature.
    return runAgentInternal(args, () => {});
  }

  async function runAndCollect(args: RunAgentArgs): Promise<AgentRunResult> {
    let captured: AgentRunResult | undefined;
    const iterable = runAgentInternal(args, (result) => {
      captured = result;
    });
    for await (const _ of iterable) {
      // Drain the iterable to drive `onResult` to completion. The
      // projection is already published on the bus.
    }
    if (captured === undefined) {
      // Defensive: `runAgentInternal` always invokes the callback when the
      // generator settles normally. Reaching this branch means the
      // generator exited via an unhandled throw the for-await loop
      // somehow swallowed — which is impossible per the spec, but we
      // surface the impossibility as a domain error rather than `as`-ing.
      throw new AgentRunnerError(
        "Internal error: run completed without producing a result",
        "runAndCollect",
      );
    }
    return captured;
  }

  return { runAgent, runAndCollect };
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/**
 * Format the conventional-commits system-prompt addendum.
 *
 * The addendum is appended to the SDK's preset system prompt via the
 * `append` field. It is short, deterministic, and unambiguous so the
 * model can quote it back when it commits.
 *
 * When `issueNumber` is provided, the addendum embeds the
 * `<type>(#<issueNumber>): <subject>` Conventional Commits format with
 * the issue scope. When `issueNumber` is `undefined`, the format
 * degrades to plain `<type>: <subject>` so a consumer typed as the W1
 * `AgentRunner` (which does not pass `issueNumber`) never produces a
 * `#undefined` literal in the prompt.
 *
 * The exact wording is asserted by snapshot in
 * `tests/unit/agent_runner_test.ts` so any drift is caught before it
 * reaches a PR description.
 *
 * @param issueNumber The GitHub issue this run is operating on. Omit to
 *   render the addendum without an issue scope.
 * @returns The addendum text.
 *
 * @example
 * ```ts
 * const text = buildSystemPromptAddendum(makeIssueNumber(42));
 * // "When you author commits ..."
 * const generic = buildSystemPromptAddendum();
 * // Same body, scope-less examples.
 * ```
 */
export function buildSystemPromptAddendum(issueNumber?: IssueNumber): string {
  const scope = issueNumber === undefined ? "" : `(#${issueNumber})`;
  const formatHeader = issueNumber === undefined
    ? "the Conventional Commits format:"
    : "the Conventional Commits format with the issue scope:";
  return [
    "When you author commits in this worktree, every commit message MUST follow",
    formatHeader,
    "",
    `    <type>${scope}: <subject>`,
    "",
    "where `<type>` is one of: feat, fix, docs, refactor, perf, test, build, ci,",
    "chore, revert. The subject is imperative, lower-case, no trailing period,",
    "and 72 characters or fewer. Body and footer are optional but follow the",
    "same convention. Examples:",
    "",
    `    feat${scope}: add task switcher overlay`,
    `    fix${scope}: handle SIGPIPE on TUI disconnect`,
    "",
    "Do not deviate from this format; the repository's commit-msg hook rejects",
    "non-conforming subjects.",
  ].join("\n");
}

/** Map an SDK message `type` to the supervisor's narrow role union. */
function mapSdkTypeToRole(type: string): AgentRunnerMessage["role"] {
  switch (type) {
    case "assistant":
      return "assistant";
    case "user":
      return "user";
    case "tool_use":
      return "tool-use";
    case "tool_result":
      return "tool-result";
    default:
      // `system`, `result`, `tool_progress`, `task_started`, … all
      // funnel into the `system` bucket. The TUI renders them as a
      // muted status line; the supervisor does not branch on them.
      return "system";
  }
}

/**
 * Render the human-readable text for an SDK message.
 *
 * `assistant` messages carry a `BetaMessage` whose `content` is an array
 * of blocks; we concatenate the `text` of every text block and surface
 * `tool_use` blocks as a single `[tool <name>] <input>` line so the
 * TUI can show the tool call without parsing the SDK shape itself.
 *
 * `result` messages carry the final `result` string.
 *
 * Standalone `tool_use` / `tool_result` messages (which
 * {@link mapSdkTypeToRole} surfaces as the `tool-use` / `tool-result`
 * roles) render their `name`/`input` fields the same way embedded
 * `tool_use` blocks do, so the transcript stays informative whichever
 * shape the SDK chose.
 *
 * Other messages fall back to a bracketed type tag such as
 * `[tool_progress]` so the TUI has *something* to show without coupling
 * to every SDK variant.
 */
function renderSdkMessageText(message: SdkMessageProjection): string {
  if (message.type === "assistant" && message.message?.content !== undefined) {
    const parts: string[] = [];
    for (const block of message.message.content) {
      if (block.type === "text" && typeof block.text === "string") {
        parts.push(block.text);
      } else if (block.type === "tool_use" && typeof block.name === "string") {
        const inputText = renderToolInput(block.input);
        parts.push(`[tool ${block.name}] ${inputText}`);
      }
    }
    return parts.join("\n");
  }
  if (message.type === "user" && message.message?.content !== undefined) {
    const parts: string[] = [];
    for (const block of message.message.content) {
      if (block.type === "text" && typeof block.text === "string") {
        parts.push(block.text);
      }
    }
    return parts.join("\n");
  }
  if (message.type === "result" && typeof message.result === "string") {
    return message.result;
  }
  if (message.type === "tool_use") {
    // Standalone `tool_use` carries `name`/`input` at the message root
    // (no embedded content blocks). Render the same way the embedded
    // case does so the transcript line is uniform.
    const name = typeof message.name === "string" ? message.name : "";
    const inputText = renderToolInput(message.input);
    return name.length === 0 ? `[tool_use] ${inputText}`.trimEnd() : `[tool ${name}] ${inputText}`;
  }
  if (message.type === "tool_result") {
    // Standalone `tool_result` may carry the rendered output as
    // `content` (string) or a nested `result.content` payload depending
    // on SDK variant. Surface the string form when present; otherwise
    // fall back to the bracketed type so the discriminant remains
    // useful.
    if (typeof message.content === "string") {
      return `[tool_result] ${message.content}`;
    }
    if (typeof message.result === "string") {
      return `[tool_result] ${message.result}`;
    }
    return `[tool_result]`;
  }
  // Fallback for system/tool_progress/etc. — the type discriminant alone
  // is more useful than nothing, and any caller that wants the full
  // shape can hold the SDK message themselves (the runner exposes
  // `runAgent` as an iterable and only adapts the bus events).
  return `[${message.type}]`;
}

/**
 * Render the input of a `tool_use` block as a single line.
 *
 * Strings pass through verbatim, JSON-serializable values are
 * `JSON.stringify`'d, and unrepresentable values (cycles, BigInts,
 * functions, symbols) are stringified through `String()` so a malformed
 * tool call never aborts the agent loop. `JSON.stringify` returns
 * `undefined` for top-level functions and symbols; the explicit guard
 * below converts those to `String(input)` rather than the literal
 * `"undefined"` that string interpolation would otherwise produce.
 */
function renderToolInput(input: unknown): string {
  if (typeof input === "string") {
    return input;
  }
  if (input === undefined || input === null) {
    return "";
  }
  try {
    const stringified = JSON.stringify(input);
    return stringified === undefined ? String(input) : stringified;
  } catch {
    return String(input);
  }
}

/**
 * Slice `text` to the bus budget without exploding on a surrogate pair on
 * the boundary; appends an ellipsis token when truncation occurs so the
 * TUI can distinguish a hard cap from an empty model turn.
 */
function truncateForBus(text: string): string {
  if (text.length <= AGENT_MESSAGE_TEXT_TRUNCATION_CODE_UNITS) {
    return text;
  }
  return `${text.slice(0, AGENT_MESSAGE_TEXT_TRUNCATION_CODE_UNITS)}…`;
}

/**
 * Default `which`-style executable resolver.
 *
 * Spawns `which <name>` and returns the trimmed first line of stdout. A
 * non-zero exit (the canonical signal for "not found") is rethrown so
 * the caller's catch maps it onto an {@link AgentRunnerError}. We do not
 * branch on `Deno.build.os` here: `which` is on every supported macOS
 * and Linux host we ship to, and Windows support is deferred per ADR-008.
 */
async function defaultResolveExecutable(name: string): Promise<string> {
  const command = new Deno.Command("which", {
    args: [name],
    stdout: "piped",
    stderr: "piped",
  });
  const result = await command.output();
  if (!result.success) {
    const stderr = new TextDecoder().decode(result.stderr).trim();
    throw new Error(
      `which ${name} failed (exit ${result.code})${stderr.length === 0 ? "" : `: ${stderr}`}`,
    );
  }
  const stdout = new TextDecoder().decode(result.stdout);
  const trimmed = stdout.split("\n")[0]?.trim() ?? "";
  if (trimmed.length === 0) {
    throw new Error(`which ${name} returned an empty path`);
  }
  return trimmed;
}

/** Default ISO-timestamp source. */
function defaultNowIso(): string {
  return new Date().toISOString();
}

/**
 * Adapt the default-namespace `@std/log` logger to the narrow
 * {@link AgentRunnerLogger} surface. `getLogger()` returns a `Logger`
 * whose `warn` overload accepts a plain string, but TypeScript needs a
 * thin adapter to project the SDK's shape onto our interface.
 */
function defaultLogger(): AgentRunnerLogger {
  const inner = getLogger();
  return {
    warn(message: string): void {
      inner.warn(message);
    },
  };
}

/** Render an unknown error for inclusion in log lines and error messages. */
function stringifyError(caught: unknown): string {
  if (caught instanceof Error) {
    return `${caught.name}: ${caught.message}`;
  }
  return String(caught);
}
