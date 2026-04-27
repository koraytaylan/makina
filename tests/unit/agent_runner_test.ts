/**
 * Unit tests for `src/daemon/agent-runner.ts`. Covers:
 *
 *  - Iterable shape: the runner streams `AgentRunnerMessage` projections
 *    and exhausts cleanly.
 *  - Event-bus integration: each yielded message is also published as an
 *    `agent-message` tagged with the run's `taskId`.
 *  - Session resumption: `sessionId` from the SDK is carried out via
 *    `runAndCollect`, and a caller-supplied `sessionId` is forwarded to
 *    the SDK as `options.resume`.
 *  - System-prompt addendum: a snapshot of the addendum guards against
 *    silent drift; the issue number flows through verbatim.
 *  - SDK options: `cwd`, `executable: "deno"`, `permissionMode:
 *    "acceptEdits"`, `model`, and `pathToClaudeCodeExecutable` are
 *    forwarded as expected.
 *  - Executable discovery: a custom `resolveExecutable` resolves; a
 *    `claude` not on PATH yields an `AgentRunnerError` with a remediation
 *    hint.
 *  - Error paths: invalid args, SDK failures, and a publish-side bus
 *    throw are wrapped/handled per the contract.
 *  - `MockAgentRunner` shape compatibility — using the helper as the
 *    supervisor would.
 */

import {
  assert,
  assertEquals,
  assertExists,
  assertInstanceOf,
  assertRejects,
  assertStringIncludes,
} from "@std/assert";

import {
  AgentRunnerError,
  type AgentRunnerImpl,
  type AgentRunnerOptions,
  buildSystemPromptAddendum,
  createAgentRunner,
  type ExecutableResolver,
  type SdkMessageProjection,
  type SdkQueryFunction,
  type SdkQueryOptions,
} from "../../src/daemon/agent-runner.ts";
import { createEventBus } from "../../src/daemon/event-bus.ts";
import { AGENT_MESSAGE_TEXT_TRUNCATION_CODE_UNITS } from "../../src/constants.ts";
import {
  type AgentRunnerMessage,
  type EventBus,
  makeIssueNumber,
  makeTaskId,
  type TaskEvent,
  type TaskId,
} from "../../src/types.ts";
import { MockAgentRunner } from "../helpers/mock_agent_runner.ts";

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

interface QueryCapture {
  readonly prompt: string;
  readonly options: SdkQueryOptions;
}

interface FakeQueryConfig {
  readonly messages: ReadonlyArray<SdkMessageProjection>;
  readonly throwAt?: number;
  readonly throwError?: Error;
}

interface FakeQuery {
  readonly fn: SdkQueryFunction;
  readonly captures: QueryCapture[];
}

/**
 * Build a structural stub of the SDK's `query` function. Captures every
 * call in `captures` and yields the scripted messages. If `throwAt` is
 * defined, the iterable rejects after yielding `throwAt` messages; the
 * thrown error defaults to `Error("scripted SDK failure")`.
 */
function fakeQuery(config: FakeQueryConfig): FakeQuery {
  const captures: QueryCapture[] = [];
  const fn: SdkQueryFunction = (params) => {
    captures.push({ prompt: params.prompt, options: params.options });
    return iterate(config);
  };
  return { fn, captures };
}

async function* iterate(config: FakeQueryConfig): AsyncIterable<SdkMessageProjection> {
  let i = 0;
  for (const m of config.messages) {
    if (config.throwAt !== undefined && i === config.throwAt) {
      throw config.throwError ?? new Error("scripted SDK failure");
    }
    // `await` makes the iteration genuinely async so a consumer cannot
    // accidentally rely on synchronous yields.
    await Promise.resolve();
    yield m;
    i += 1;
  }
  if (config.throwAt !== undefined && i === config.throwAt) {
    throw config.throwError ?? new Error("scripted SDK failure");
  }
}

/** Recording logger that captures every `warn` call. */
function recordingLogger(): { warn: (message: string) => void; messages: string[] } {
  const messages: string[] = [];
  return {
    messages,
    warn(message: string): void {
      messages.push(message);
    },
  };
}

/**
 * Drain `iterable` and return the materialized list. Keeps test bodies
 * compact and lets us assert on the exact projection.
 */
async function drain<T>(iterable: AsyncIterable<T>): Promise<T[]> {
  const out: T[] = [];
  for await (const item of iterable) {
    out.push(item);
  }
  return out;
}

/** Subscribe to `bus` for `target` and return the recorded events. */
function recordBus(bus: EventBus, target: TaskId | "*"): {
  readonly events: TaskEvent[];
  readonly stop: () => void;
} {
  const events: TaskEvent[] = [];
  const sub = bus.subscribe(target, (event) => {
    events.push(event);
  });
  return {
    events,
    stop: () => sub.unsubscribe(),
  };
}

/**
 * Wait one macrotask so the event-bus pump has a chance to deliver any
 * still-queued events to its subscribers.
 */
function flush(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

/** Build a minimal `assistant` SDK message carrying a single text block. */
function assistantMessage(text: string, sessionId = "session-stub"): SdkMessageProjection {
  return {
    type: "assistant",
    session_id: sessionId,
    message: {
      content: [{ type: "text", text }],
    },
  };
}

/** Build a minimal `result` SDK message. */
function resultMessage(result: string, sessionId = "session-stub"): SdkMessageProjection {
  return {
    type: "result",
    session_id: sessionId,
    result,
  };
}

/**
 * Boilerplate for the runner: returns a runner wired to a fresh event
 * bus, the bus's recording subscription, and the captured queries.
 */
function buildRunner(
  config: FakeQueryConfig & {
    readonly resolveExecutable?: ExecutableResolver;
    readonly pathOverride?: string;
    readonly nowIso?: () => string;
  },
): {
  readonly runner: AgentRunnerImpl;
  readonly bus: EventBus;
  readonly captures: QueryCapture[];
  readonly events: TaskEvent[];
  readonly stop: () => void;
} {
  const bus = createEventBus({ logger: recordingLogger() });
  const recording = recordBus(bus, "*");
  const fake = fakeQuery(config);
  // `exactOptionalPropertyTypes: true` forbids passing `undefined` for
  // an optional field, so we build the runner options conditionally.
  const opts: AgentRunnerOptions = {
    eventBus: bus,
    query: fake.fn,
    nowIso: config.nowIso ?? (() => "2026-04-26T00:00:00.000Z"),
    ...(config.resolveExecutable === undefined
      ? {}
      : { resolveExecutable: config.resolveExecutable }),
    ...(config.pathOverride === undefined
      ? {}
      : { pathToClaudeCodeExecutable: config.pathOverride }),
  };
  const runner = createAgentRunner(opts);
  return {
    runner,
    bus,
    captures: fake.captures,
    events: recording.events,
    stop: recording.stop,
  };
}

const FIXTURE_TASK_ID = makeTaskId("task_unit_agent_runner");
const FIXTURE_ISSUE = makeIssueNumber(42);
const FIXTURE_WORKTREE = "/var/lib/makina/worktrees/owner__name/issue-42";
const FIXTURE_PROMPT = "Implement the requested feature.";
const FIXTURE_MODEL = "claude-sonnet-4-6";
const FIXTURE_CLAUDE = "/usr/local/bin/claude";

// ---------------------------------------------------------------------------
// Iterable + event-bus integration
// ---------------------------------------------------------------------------

Deno.test("runAgent yields projected messages and exhausts", async () => {
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [
      assistantMessage("Reading the repository..."),
      assistantMessage("Done."),
      resultMessage("Done."),
    ],
  });
  try {
    const messages = await drain(
      runner.runAgent({
        taskId: FIXTURE_TASK_ID,
        issueNumber: FIXTURE_ISSUE,
        worktreePath: FIXTURE_WORKTREE,
        prompt: FIXTURE_PROMPT,
        model: FIXTURE_MODEL,
      }),
    );
    assertEquals(messages.length, 3);
    assertEquals(messages[0]?.role, "assistant");
    assertEquals(messages[0]?.text, "Reading the repository...");
    assertEquals(messages[2]?.role, "system"); // `result` falls into `system`
    assertEquals(messages[2]?.text, "Done.");
  } finally {
    stop();
  }
});

Deno.test("runAgent publishes each message as an event-bus event tagged with taskId", async () => {
  const { runner, events, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [
      assistantMessage("Hello"),
      assistantMessage("World"),
    ],
  });
  try {
    await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    await flush();
    assertEquals(events.length, 2);
    for (const event of events) {
      assertEquals(event.taskId, FIXTURE_TASK_ID);
      assertEquals(event.kind, "agent-message");
      assertEquals(event.atIso, "2026-04-26T00:00:00.000Z");
    }
    assert(events[0]?.kind === "agent-message");
    assertEquals(events[0]?.data.text, "Hello");
    assert(events[1]?.kind === "agent-message");
    assertEquals(events[1]?.data.text, "World");
  } finally {
    stop();
  }
});

Deno.test("runAgent emits only to subscribers of the matching taskId", async () => {
  const otherTaskId = makeTaskId("task_other");
  const { runner, bus, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [assistantMessage("only me")],
  });
  const matching: TaskEvent[] = [];
  const nonMatching: TaskEvent[] = [];
  const subA = bus.subscribe(FIXTURE_TASK_ID, (e) => matching.push(e));
  const subB = bus.subscribe(otherTaskId, (e) => nonMatching.push(e));
  try {
    await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    await flush();
    assertEquals(matching.length, 1);
    assertEquals(nonMatching.length, 0);
  } finally {
    subA.unsubscribe();
    subB.unsubscribe();
    stop();
  }
});

// ---------------------------------------------------------------------------
// Session resumption
// ---------------------------------------------------------------------------

Deno.test("runAndCollect carries the SDK session id out", async () => {
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [
      assistantMessage("Step one", "session-from-sdk"),
      assistantMessage("Step two", "session-from-sdk"),
    ],
  });
  try {
    const result = await runner.runAndCollect({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    });
    assertEquals(result.sessionId, "session-from-sdk");
    assertEquals(result.messageCount, 2);
  } finally {
    stop();
  }
});

Deno.test("runAndCollect omits sessionId when the SDK never provides one", async () => {
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [{ type: "system", subtype: "init" } as SdkMessageProjection],
  });
  try {
    const result = await runner.runAndCollect({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    });
    assertEquals(result.sessionId, undefined);
    assertEquals(result.messageCount, 1);
  } finally {
    stop();
  }
});

Deno.test("runAgent forwards a caller-supplied sessionId to the SDK as resume", async () => {
  const { runner, captures, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [assistantMessage("ok", "session-roundtrip")],
  });
  try {
    await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
      sessionId: "session-roundtrip",
    }));
    assertEquals(captures.length, 1);
    assertEquals(captures[0]?.options.resume, "session-roundtrip");
  } finally {
    stop();
  }
});

Deno.test("runAgent omits resume when no sessionId was passed", async () => {
  const { runner, captures, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [assistantMessage("ok")],
  });
  try {
    await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    assertEquals(captures.length, 1);
    const captured = captures[0];
    assertExists(captured);
    assertEquals(Object.prototype.hasOwnProperty.call(captured.options, "resume"), false);
  } finally {
    stop();
  }
});

// ---------------------------------------------------------------------------
// System-prompt addendum
// ---------------------------------------------------------------------------

Deno.test("buildSystemPromptAddendum embeds the issue number verbatim (snapshot)", () => {
  const text = buildSystemPromptAddendum(makeIssueNumber(99));
  // Snapshot the exact wording so silent drift is caught in PR review.
  // Re-baseline only when intentionally changing the agent's commit
  // protocol; bump the assertion together with the change.
  const expected = [
    "When you author commits in this worktree, every commit message MUST follow",
    "the Conventional Commits format with the issue scope:",
    "",
    "    <type>(#99): <subject>",
    "",
    "where `<type>` is one of: feat, fix, docs, refactor, perf, test, build, ci,",
    "chore, revert. The subject is imperative, lower-case, no trailing period,",
    "and 72 characters or fewer. Body and footer are optional but follow the",
    "same convention. Examples:",
    "",
    "    feat(#99): add task switcher overlay",
    "    fix(#99): handle SIGPIPE on TUI disconnect",
    "",
    "Do not deviate from this format; the repository's commit-msg hook rejects",
    "non-conforming subjects.",
  ].join("\n");
  assertEquals(text, expected);
});

Deno.test("buildSystemPromptAddendum drops the issue scope when no issueNumber is given", () => {
  // The W1 `AgentRunner` contract does not include `issueNumber`; a
  // consumer typed as `AgentRunner` legally omits it. The runner
  // degrades the format to plain Conventional Commits (no scope) so we
  // never produce a `#undefined` literal in the prompt.
  const text = buildSystemPromptAddendum();
  const expected = [
    "When you author commits in this worktree, every commit message MUST follow",
    "the Conventional Commits format:",
    "",
    "    <type>: <subject>",
    "",
    "where `<type>` is one of: feat, fix, docs, refactor, perf, test, build, ci,",
    "chore, revert. The subject is imperative, lower-case, no trailing period,",
    "and 72 characters or fewer. Body and footer are optional but follow the",
    "same convention. Examples:",
    "",
    "    feat: add task switcher overlay",
    "    fix: handle SIGPIPE on TUI disconnect",
    "",
    "Do not deviate from this format; the repository's commit-msg hook rejects",
    "non-conforming subjects.",
  ].join("\n");
  assertEquals(text, expected);
});

Deno.test("runAgent appends the addendum to the SDK's preset system prompt", async () => {
  const { runner, captures, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [assistantMessage("ok")],
  });
  try {
    await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    const prompt = captures[0]?.options.systemPrompt;
    assertExists(prompt);
    assertEquals(prompt.type, "preset");
    assertEquals(prompt.preset, "claude_code");
    assertStringIncludes(prompt.append, "(#42)");
    assertStringIncludes(prompt.append, "Conventional Commits");
  } finally {
    stop();
  }
});

// ---------------------------------------------------------------------------
// SDK options forwarded
// ---------------------------------------------------------------------------

Deno.test("runAgent forwards cwd / executable / permissionMode / model / claude path", async () => {
  const { runner, captures, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [assistantMessage("ok")],
  });
  try {
    await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    const captured = captures[0];
    assertExists(captured);
    assertEquals(captured.prompt, FIXTURE_PROMPT);
    assertEquals(captured.options.cwd, FIXTURE_WORKTREE);
    assertEquals(captured.options.executable, "deno");
    assertEquals(captured.options.permissionMode, "acceptEdits");
    assertEquals(captured.options.model, FIXTURE_MODEL);
    assertEquals(captured.options.pathToClaudeCodeExecutable, FIXTURE_CLAUDE);
  } finally {
    stop();
  }
});

// ---------------------------------------------------------------------------
// Executable discovery
// ---------------------------------------------------------------------------

Deno.test("runAgent uses an injected resolveExecutable when no override is set", async () => {
  const resolutions: string[] = [];
  const resolveExecutable: ExecutableResolver = (name) => {
    resolutions.push(name);
    return Promise.resolve("/opt/homebrew/bin/claude");
  };
  const { runner, captures, stop } = buildRunner({
    resolveExecutable,
    messages: [assistantMessage("ok")],
  });
  try {
    await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    assertEquals(resolutions, ["claude"]);
    assertEquals(captures[0]?.options.pathToClaudeCodeExecutable, "/opt/homebrew/bin/claude");
  } finally {
    stop();
  }
});

Deno.test("resolveExecutable is memoized after the first successful call", async () => {
  let count = 0;
  const resolveExecutable: ExecutableResolver = () => {
    count += 1;
    return Promise.resolve("/usr/local/bin/claude");
  };
  const { runner, stop } = buildRunner({
    resolveExecutable,
    messages: [assistantMessage("ok")],
  });
  try {
    for (let i = 0; i < 3; i += 1) {
      await drain(runner.runAgent({
        taskId: FIXTURE_TASK_ID,
        issueNumber: FIXTURE_ISSUE,
        worktreePath: FIXTURE_WORKTREE,
        prompt: FIXTURE_PROMPT,
        model: FIXTURE_MODEL,
      }));
    }
    assertEquals(count, 1);
  } finally {
    stop();
  }
});

Deno.test("runAgent rejects with AgentRunnerError when claude is not on PATH", async () => {
  const resolveExecutable: ExecutableResolver = () =>
    Promise.reject(new Error("which claude failed (exit 1)"));
  const { runner, stop } = buildRunner({
    resolveExecutable,
    messages: [],
  });
  try {
    const error = await assertRejects(
      () =>
        drain(runner.runAgent({
          taskId: FIXTURE_TASK_ID,
          issueNumber: FIXTURE_ISSUE,
          worktreePath: FIXTURE_WORKTREE,
          prompt: FIXTURE_PROMPT,
          model: FIXTURE_MODEL,
        })),
      AgentRunnerError,
    );
    assertEquals(error.operation, "resolveExecutable");
    assertEquals(error.field, "pathToClaudeCodeExecutable");
    assertStringIncludes(error.message, "Could not locate the `claude` CLI");
    assertExists(error.cause);
  } finally {
    stop();
  }
});

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

Deno.test("runAgent rejects when worktreePath is empty", async () => {
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [],
  });
  try {
    const error = await assertRejects(
      () =>
        drain(runner.runAgent({
          taskId: FIXTURE_TASK_ID,
          issueNumber: FIXTURE_ISSUE,
          worktreePath: "",
          prompt: FIXTURE_PROMPT,
          model: FIXTURE_MODEL,
        })),
      AgentRunnerError,
    );
    assertEquals(error.operation, "validateArgs");
    assertEquals(error.field, "worktreePath");
  } finally {
    stop();
  }
});

Deno.test("runAgent rejects when worktreePath is not absolute", async () => {
  // The error message claims an absolute path is required; the
  // validator must enforce that, not just non-empty.
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [],
  });
  try {
    const error = await assertRejects(
      () =>
        drain(runner.runAgent({
          taskId: FIXTURE_TASK_ID,
          issueNumber: FIXTURE_ISSUE,
          worktreePath: "relative/path",
          prompt: FIXTURE_PROMPT,
          model: FIXTURE_MODEL,
        })),
      AgentRunnerError,
    );
    assertEquals(error.operation, "validateArgs");
    assertEquals(error.field, "worktreePath");
    assertStringIncludes(error.message, "absolute");
  } finally {
    stop();
  }
});

Deno.test("runAgent rejects when prompt is empty", async () => {
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [],
  });
  try {
    const error = await assertRejects(
      () =>
        drain(runner.runAgent({
          taskId: FIXTURE_TASK_ID,
          issueNumber: FIXTURE_ISSUE,
          worktreePath: FIXTURE_WORKTREE,
          prompt: "",
          model: FIXTURE_MODEL,
        })),
      AgentRunnerError,
    );
    assertEquals(error.operation, "validateArgs");
    assertEquals(error.field, "prompt");
  } finally {
    stop();
  }
});

Deno.test("runAgent rejects when model is empty", async () => {
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [],
  });
  try {
    const error = await assertRejects(
      () =>
        drain(runner.runAgent({
          taskId: FIXTURE_TASK_ID,
          issueNumber: FIXTURE_ISSUE,
          worktreePath: FIXTURE_WORKTREE,
          prompt: FIXTURE_PROMPT,
          model: "",
        })),
      AgentRunnerError,
    );
    assertEquals(error.operation, "validateArgs");
    assertEquals(error.field, "model");
  } finally {
    stop();
  }
});

Deno.test("runAgent wraps SDK rejections as AgentRunnerError carrying the cause", async () => {
  const cause = new Error("network unreachable");
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [assistantMessage("first")],
    throwAt: 1,
    throwError: cause,
  });
  try {
    const error = await assertRejects(
      () =>
        drain(runner.runAgent({
          taskId: FIXTURE_TASK_ID,
          issueNumber: FIXTURE_ISSUE,
          worktreePath: FIXTURE_WORKTREE,
          prompt: FIXTURE_PROMPT,
          model: FIXTURE_MODEL,
        })),
      AgentRunnerError,
    );
    assertEquals(error.operation, "sdkQuery");
    assertEquals(error.cause, cause);
    assertStringIncludes(error.message, "network unreachable");
  } finally {
    stop();
  }
});

Deno.test("runAgent wraps synchronous queryFn throws as AgentRunnerError", async () => {
  // The SDK's `query` (or a misbehaving test stub) may throw before
  // returning the iterable. The runner must wrap that synchronous throw
  // the same way it wraps async iteration failures so the supervisor
  // sees a single error type.
  const cause = new Error("synchronous SDK boom");
  const throwingQuery: SdkQueryFunction = () => {
    throw cause;
  };
  const bus = createEventBus({ logger: recordingLogger() });
  const runner = createAgentRunner({
    eventBus: bus,
    query: throwingQuery,
    pathToClaudeCodeExecutable: FIXTURE_CLAUDE,
    nowIso: () => "2026-04-26T00:00:00.000Z",
  });
  const error = await assertRejects(
    () =>
      drain(runner.runAgent({
        taskId: FIXTURE_TASK_ID,
        issueNumber: FIXTURE_ISSUE,
        worktreePath: FIXTURE_WORKTREE,
        prompt: FIXTURE_PROMPT,
        model: FIXTURE_MODEL,
      })),
    AgentRunnerError,
  );
  assertEquals(error.operation, "sdkQuery");
  assertEquals(error.cause, cause);
  assertStringIncludes(error.message, "synchronous SDK boom");
});

Deno.test(
  "runAgent logs and continues when the bus publish call throws synchronously",
  async () => {
    const logger = recordingLogger();
    const throwingBus: EventBus = {
      publish() {
        throw new Error("bus publish exploded");
      },
      subscribe() {
        return { unsubscribe() {} };
      },
    };
    const fake = fakeQuery({
      messages: [assistantMessage("Hello"), assistantMessage("World")],
    });
    const runner = createAgentRunner({
      eventBus: throwingBus,
      query: fake.fn,
      pathToClaudeCodeExecutable: FIXTURE_CLAUDE,
      logger,
      nowIso: () => "2026-04-26T00:00:00.000Z",
    });
    const messages = await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    assertEquals(messages.length, 2);
    // Two messages, two warnings, one per failed publish.
    assertEquals(logger.messages.length, 2);
    const firstWarning = logger.messages[0];
    assertExists(firstWarning);
    assertStringIncludes(firstWarning, "event-bus publish threw");
  },
);

Deno.test("createAgentRunner rejects a missing eventBus at construction", () => {
  let caught: unknown;
  try {
    // Intentionally bypass the type to exercise the runtime guard. The
    // runtime check is the contract-side defense for callers that ignore
    // the compiler error.
    createAgentRunner({ eventBus: undefined as unknown as EventBus });
  } catch (error) {
    caught = error;
  }
  assertInstanceOf(caught, RangeError);
  assertStringIncludes((caught as RangeError).message, "eventBus is required");
});

// ---------------------------------------------------------------------------
// Message rendering
// ---------------------------------------------------------------------------

Deno.test("assistant messages with tool_use blocks render a `[tool ...]` line", async () => {
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [
      {
        type: "assistant",
        session_id: "session-stub",
        message: {
          content: [
            { type: "text", text: "Reading the file." },
            {
              type: "tool_use",
              name: "Read",
              input: { path: "/etc/hosts" },
            },
          ],
        },
      },
    ],
  });
  try {
    const messages = await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    assertEquals(messages.length, 1);
    const first = messages[0];
    assertExists(first);
    assertStringIncludes(first.text, "Reading the file.");
    assertStringIncludes(first.text, "[tool Read]");
    assertStringIncludes(first.text, "/etc/hosts");
  } finally {
    stop();
  }
});

Deno.test("messages of unknown SDK type render as `[<type>]`", async () => {
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [{ type: "tool_progress", session_id: "session-stub" }],
  });
  try {
    const messages = await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    assertEquals(messages[0]?.text, "[tool_progress]");
    assertEquals(messages[0]?.role, "system");
  } finally {
    stop();
  }
});

Deno.test("user messages with text content render the concatenated text", async () => {
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [
      {
        type: "user",
        session_id: "session-stub",
        message: {
          content: [
            { type: "text", text: "Tool result A" },
            { type: "text", text: "Tool result B" },
          ],
        },
      },
    ],
  });
  try {
    const messages = await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    assertEquals(messages[0]?.role, "user");
    assertEquals(messages[0]?.text, "Tool result A\nTool result B");
  } finally {
    stop();
  }
});

Deno.test("tool_use blocks render gracefully when input has unrepresentable values", async () => {
  // BigInt is not JSON-serialisable and should not abort the agent loop.
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [
      {
        type: "assistant",
        session_id: "session-stub",
        message: {
          content: [
            {
              type: "tool_use",
              name: "Bash",
              input: { count: 10n },
            },
          ],
        },
      },
    ],
  });
  try {
    const messages = await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    const first = messages[0];
    assertExists(first);
    assertStringIncludes(first.text, "[tool Bash]");
  } finally {
    stop();
  }
});

Deno.test("tool_use blocks fall back to String() when JSON.stringify returns undefined", async () => {
  // `JSON.stringify(<function>)` and `JSON.stringify(<symbol>)` return
  // `undefined`. Without an explicit guard, string interpolation
  // converts that to the literal `"undefined"`. The renderer must fall
  // back to `String(input)` so the line carries something more useful
  // (the function/symbol's debug representation) than `undefined`.
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [
      {
        type: "assistant",
        session_id: "session-stub",
        message: {
          content: [
            {
              type: "tool_use",
              name: "Weird",
              input: function namedFn() {/* nothing */},
            },
          ],
        },
      },
    ],
  });
  try {
    const messages = await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    const first = messages[0];
    assertExists(first);
    assertStringIncludes(first.text, "[tool Weird]");
    // The fallback path is `String(input)`, which for a function yields
    // its source. We just assert the literal `undefined` does not leak.
    assertEquals(first.text.includes("undefined"), false);
  } finally {
    stop();
  }
});

Deno.test("very long assistant text yields full length but truncates on the event bus", async () => {
  // The iterable carries the full rendered text so a future consumer
  // (e.g. a persistence sink) is not silently capped at the bus budget.
  // Only the bus-side payload is truncated to keep subscriber queues
  // bounded.
  const oversizedLength = AGENT_MESSAGE_TEXT_TRUNCATION_CODE_UNITS + 100;
  const oversized = "x".repeat(oversizedLength);
  const { runner, events, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [assistantMessage(oversized)],
  });
  try {
    const messages = await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    await flush();
    // Iterable: full length, no ellipsis.
    assertEquals(messages[0]?.text.length, oversizedLength);
    assertEquals(messages[0]?.text.endsWith("…"), false);
    // Bus: capped at the budget plus one ellipsis code unit.
    assertEquals(events.length, 1);
    assert(events[0]?.kind === "agent-message");
    assertEquals(
      events[0]?.data.text.length,
      AGENT_MESSAGE_TEXT_TRUNCATION_CODE_UNITS + 1,
    );
    assert(events[0]?.data.text.endsWith("…"));
  } finally {
    stop();
  }
});

Deno.test("string tool inputs pass through without JSON wrapping", async () => {
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [
      {
        type: "assistant",
        session_id: "session-stub",
        message: {
          content: [{ type: "tool_use", name: "Read", input: "raw-string" }],
        },
      },
    ],
  });
  try {
    const messages = await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    const first = messages[0];
    assertExists(first);
    // Bare string, not JSON-wrapped.
    assertStringIncludes(first.text, "[tool Read] raw-string");
  } finally {
    stop();
  }
});

// ---------------------------------------------------------------------------
// MockAgentRunner shape compatibility (lessons learned: the contract
// surface is the wire across waves; the mock is the supervisor's test
// substitute).
// ---------------------------------------------------------------------------

Deno.test("MockAgentRunner implements the supervisor's AgentRunner shape", async () => {
  const mock = new MockAgentRunner();
  mock.queueRun({
    messages: [
      { role: "assistant", text: "Hello" },
      { role: "tool-use", text: "git diff" },
    ],
  });
  const got: AgentRunnerMessage[] = [];
  for await (
    const msg of mock.runAgent({
      taskId: FIXTURE_TASK_ID,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    })
  ) {
    got.push(msg);
  }
  assertEquals(got.length, 2);
  assertEquals(mock.recordedInvocations().length, 1);
  assertEquals(mock.recordedInvocations()[0]?.taskId, FIXTURE_TASK_ID);
});

Deno.test("MockAgentRunner records a roundtripped sessionId", () => {
  const mock = new MockAgentRunner();
  mock.queueRun({ messages: [] });
  // `runAgent` records the invocation synchronously; the iterable does
  // not need to be drained for the assertion below.
  mock.runAgent({
    taskId: FIXTURE_TASK_ID,
    worktreePath: FIXTURE_WORKTREE,
    prompt: FIXTURE_PROMPT,
    model: FIXTURE_MODEL,
    sessionId: "previous-session",
  });
  assertEquals(mock.recordedInvocations()[0]?.sessionId, "previous-session");
});

// ---------------------------------------------------------------------------
// Standalone tool_use / tool_result message variants
// ---------------------------------------------------------------------------

Deno.test("standalone tool_use messages map to the tool-use role", async () => {
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [{ type: "tool_use", session_id: "s" }],
  });
  try {
    const messages = await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    assertEquals(messages[0]?.role, "tool-use");
  } finally {
    stop();
  }
});

Deno.test("standalone tool_use messages render their name and input", async () => {
  // Standalone `tool_use` messages carry `name`/`input` at the message
  // root (no embedded blocks). Render them the same way embedded
  // tool_use blocks render so the transcript is uniform regardless of
  // SDK variant.
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [
      {
        type: "tool_use",
        session_id: "s",
        name: "Bash",
        input: { command: "ls" },
      },
    ],
  });
  try {
    const messages = await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    const first = messages[0];
    assertExists(first);
    assertEquals(first.role, "tool-use");
    assertStringIncludes(first.text, "[tool Bash]");
    assertStringIncludes(first.text, "ls");
  } finally {
    stop();
  }
});

Deno.test("standalone tool_result messages map to the tool-result role", async () => {
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [{ type: "tool_result", session_id: "s" }],
  });
  try {
    const messages = await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    assertEquals(messages[0]?.role, "tool-result");
  } finally {
    stop();
  }
});

Deno.test("standalone tool_result messages render their content payload", async () => {
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [
      {
        type: "tool_result",
        session_id: "s",
        content: "command output line",
      },
    ],
  });
  try {
    const messages = await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    const first = messages[0];
    assertExists(first);
    assertEquals(first.role, "tool-result");
    assertStringIncludes(first.text, "[tool_result]");
    assertStringIncludes(first.text, "command output line");
  } finally {
    stop();
  }
});

Deno.test("tool_use blocks render an empty input string when input is null/undefined", async () => {
  const { runner, stop } = buildRunner({
    pathOverride: FIXTURE_CLAUDE,
    messages: [
      {
        type: "assistant",
        session_id: "s",
        message: {
          content: [
            { type: "tool_use", name: "Read", input: undefined },
            { type: "tool_use", name: "Glob", input: null },
          ],
        },
      },
    ],
  });
  try {
    const messages = await drain(runner.runAgent({
      taskId: FIXTURE_TASK_ID,
      issueNumber: FIXTURE_ISSUE,
      worktreePath: FIXTURE_WORKTREE,
      prompt: FIXTURE_PROMPT,
      model: FIXTURE_MODEL,
    }));
    const first = messages[0];
    assertExists(first);
    assertStringIncludes(first.text, "[tool Read]");
    assertStringIncludes(first.text, "[tool Glob]");
  } finally {
    stop();
  }
});

// ---------------------------------------------------------------------------
// Default executable resolver (real `which`)
// ---------------------------------------------------------------------------

Deno.test({
  name: "the default resolveExecutable invokes `which` against the real PATH",
  // The runner under test resolves `claude` via `which claude`, so this
  // test exercises both branches of `defaultResolveExecutable`:
  //  - success: `claude` is on PATH (developer has Claude Code installed),
  //    in which case the resolver returns a non-empty path and the stub
  //    query is invoked.
  //  - failure: `claude` is not on PATH (typical fresh CI), in which case
  //    the resolver throws an `AgentRunnerError` with
  //    `operation: "resolveExecutable"`.
  // Whichever branch the host happens to take, the assertions below cover
  // it — neither outcome should leave a fresh dev box red.
  async fn() {
    const fake = fakeQuery({ messages: [assistantMessage("ok")] });
    const bus = createEventBus({ logger: recordingLogger() });
    // Construct the runner with NO `resolveExecutable` override and NO
    // `pathToClaudeCodeExecutable`. This drives `defaultResolveExecutable`
    // — the real production path. We then expect `which claude` to fail
    // unless the developer has Claude Code installed; that's the
    // exception-throwing branch we want to cover. The test asserts an
    // `AgentRunnerError` with `operation: "resolveExecutable"` either
    // because `claude` is not on PATH (typical CI), or — if it *is*
    // installed — the resolver succeeds and the run reaches the
    // (test-only) stub query which proves the default path executed.
    const runner = createAgentRunner({
      eventBus: bus,
      query: fake.fn,
      nowIso: () => "2026-04-26T00:00:00.000Z",
    });
    try {
      await drain(runner.runAgent({
        taskId: FIXTURE_TASK_ID,
        issueNumber: FIXTURE_ISSUE,
        worktreePath: FIXTURE_WORKTREE,
        prompt: FIXTURE_PROMPT,
        model: FIXTURE_MODEL,
      }));
      // Reached here: `which claude` resolved. Coverage hit the success
      // branch and the runner forwarded a non-empty path.
      assertEquals(fake.captures.length, 1);
      const captured = fake.captures[0];
      assertExists(captured);
      assert(captured.options.pathToClaudeCodeExecutable.length > 0);
    } catch (caught) {
      // Reached here: `which claude` failed. Coverage hit the
      // failure branch.
      assertInstanceOf(caught, AgentRunnerError);
      assertEquals(caught.operation, "resolveExecutable");
    }
  },
});
