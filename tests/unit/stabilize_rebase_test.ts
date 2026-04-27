/**
 * Unit tests for `src/daemon/stabilize.ts` — scripted-timeline coverage
 * of the rebase sub-phase against an in-memory git invoker, an
 * in-memory conflict-file reader, and the {@link MockAgentRunner}.
 *
 * The tests never spawn `git`, never read the real filesystem (other
 * than `Deno.makeTempDir` for the worktree shell), and never override
 * any process global (no `mock.install` on `Deno.Command`,
 * `Deno.readTextFile`, etc.). Per Lesson #3 from the Wave 3 brief,
 * collaborators are injected through {@link runRebasePhase}'s options
 * bag.
 *
 * Coverage map:
 *
 *  - Clean rebase (`git fetch ok`, `git rebase ok`) resolves with
 *    `kind: "clean"` after zero agent invocations.
 *  - Conflict-then-resolve in one iteration: the rebase first fails
 *    with conflicts, the agent runs, `git add -A` succeeds, the
 *    `git rebase --continue` exits 0, and the phase resolves with
 *    `kind: "clean", iterations: 1`.
 *  - Iteration-budget exhaustion: every `git rebase --continue` keeps
 *    failing with conflicting files; the phase tries `git rebase
 *    --abort` and resolves with `kind: "needs-human"` carrying the
 *    final conflicting file list.
 *  - Fetch failure surfaces as {@link StabilizeRebaseError} with
 *    `operation: "fetch"`.
 *  - Validation rejects empty/relative paths and bad
 *    `maxIterations` values.
 *  - Conflict-marker reader failure is logged but does not abort the
 *    phase; the prompt embeds the read failure inline.
 *  - The conflict prompt embeds the issue number, base branch, file
 *    list, and conflict-marker preview.
 *  - The agent runner sees the supplied session id and returns a new
 *    one (when it carries `sessionId`).
 *  - Default git invoker and conflict-file reader compile and exist
 *    as real exports (smoke-checked via name-binding existence
 *    rather than spawning real `git`).
 */

import {
  assert,
  assertEquals,
  assertInstanceOf,
  assertRejects,
  assertStringIncludes,
} from "@std/assert";

import { STABILIZE_REBASE_CONFLICT_PROMPT_HEAD } from "../../src/constants.ts";
import {
  defaultConflictFileReader,
  defaultGitInvoker,
  type GitInvocationResult,
  runRebasePhase,
  type StabilizeGitInvoker,
  type StabilizeLogger,
  StabilizeRebaseError,
  type StabilizeRebaseOptions,
} from "../../src/daemon/stabilize.ts";
import {
  type AgentRunnerMessage,
  type IssueNumber,
  makeIssueNumber,
  makeTaskId,
  type TaskId,
} from "../../src/types.ts";
import { MockAgentRunner } from "../helpers/mock_agent_runner.ts";

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/** Recording logger that captures every call. */
function recordingLogger(): StabilizeLogger & {
  readonly infos: string[];
  readonly warns: string[];
} {
  const infos: string[] = [];
  const warns: string[] = [];
  return {
    infos,
    warns,
    info(message: string): void {
      infos.push(message);
    },
    warn(message: string): void {
      warns.push(message);
    },
  };
}

/**
 * One scripted reply for a particular argv pattern.
 *
 * Each entry consumes the next matching invocation of the predicate.
 * The predicate is `(args) => boolean`; the test supplies one for each
 * git command it expects so the helper fails loudly on unscripted
 * calls.
 */
interface GitScriptedStep {
  readonly match: (args: readonly string[]) => boolean;
  readonly result: GitInvocationResult;
}

interface RecordedGitCall {
  readonly args: readonly string[];
  readonly cwd: string;
}

/**
 * Build a scripted git invoker. Each step matches one invocation; the
 * helper throws if the order does not match the script (so a missing
 * call surfaces as a test failure rather than silent fallback).
 */
function scriptedGitInvoker(steps: readonly GitScriptedStep[]): {
  readonly invoker: StabilizeGitInvoker;
  readonly calls: readonly RecordedGitCall[];
} {
  const remaining: GitScriptedStep[] = [...steps];
  const calls: RecordedGitCall[] = [];
  return {
    calls,
    invoker(args, options) {
      calls.push({ args, cwd: options.cwd });
      const next = remaining.shift();
      if (next === undefined) {
        return Promise.reject(
          new Error(
            `unexpected git invocation: git ${args.join(" ")} (no more scripted steps)`,
          ),
        );
      }
      if (!next.match(args)) {
        return Promise.reject(
          new Error(
            `unexpected git invocation order: got "git ${args.join(" ")}", ` +
              "did not match next scripted step",
          ),
        );
      }
      return Promise.resolve(next.result);
    },
  };
}

const SUCCESS: GitInvocationResult = { exitCode: 0, stdout: "", stderr: "" };

/**
 * Mint a `task_*` brand for the test. The real supervisor formats it
 * with an ISO timestamp; tests use a static suffix for readability.
 */
function fakeTaskId(suffix: string): TaskId {
  return makeTaskId(`task_test_${suffix}`);
}

const REPO_ISSUE: IssueNumber = makeIssueNumber(42);

const BASE_OPTS = (overrides: Partial<StabilizeRebaseOptions>): StabilizeRebaseOptions => ({
  taskId: fakeTaskId("rebase"),
  issueNumber: REPO_ISSUE,
  worktreePath: "/tmp/fake-worktree",
  baseBranch: "main",
  model: "claude-sonnet-4-6",
  agentRunner: new MockAgentRunner(),
  ...overrides,
});

// ---------------------------------------------------------------------------
// Clean-rebase happy path
// ---------------------------------------------------------------------------

Deno.test(
  "runRebasePhase resolves with kind clean when fetch and rebase both succeed",
  async () => {
    const { invoker, calls } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args[1] === "refs/remotes/origin/main",
        result: SUCCESS,
      },
    ]);
    const runner = new MockAgentRunner();
    const result = await runRebasePhase(
      BASE_OPTS({ gitInvoker: invoker, agentRunner: runner }),
    );
    assertEquals(result, { kind: "clean", iterations: 0 });
    assertEquals(runner.recordedInvocations().length, 0);
    assertEquals(calls.length, 2);
    assertEquals(calls[0]?.args, [
      "fetch",
      "origin",
      "main:refs/remotes/origin/main",
    ]);
    assertEquals(calls[1]?.args, ["rebase", "refs/remotes/origin/main"]);
    // Both invocations target the supplied worktree path.
    assertEquals(calls[0]?.cwd, "/tmp/fake-worktree");
    assertEquals(calls[1]?.cwd, "/tmp/fake-worktree");
  },
);

// ---------------------------------------------------------------------------
// Conflict-then-resolve in one iteration
// ---------------------------------------------------------------------------

Deno.test(
  "runRebasePhase resolves a conflict in one agent iteration",
  async () => {
    const { invoker, calls } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      // First rebase fails with conflicts.
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: {
          exitCode: 1,
          stdout: "CONFLICT (content): Merge conflict in src/a.ts",
          stderr: "",
        },
      },
      // diff lists the conflicting files.
      {
        match: (args) => args[0] === "diff",
        result: { exitCode: 0, stdout: "src/a.ts\n", stderr: "" },
      },
      // git add -A succeeds.
      {
        match: (args) => args[0] === "add",
        result: SUCCESS,
      },
      // git rebase --continue succeeds.
      {
        match: (args) => args[0] === "rebase" && args[1] === "--continue",
        result: SUCCESS,
      },
    ]);
    const runner = new MockAgentRunner();
    runner.queueRun({
      messages: [
        { role: "assistant", text: "I'll resolve the conflict in src/a.ts." },
        { role: "tool-use", text: "edit src/a.ts" },
        { role: "assistant", text: "Done." },
      ],
    });
    const reader = (path: string): Promise<string> => {
      if (path.endsWith("a.ts")) {
        return Promise.resolve(
          "<<<<<<< HEAD\nfoo\n=======\nbar\n>>>>>>> origin/main\n",
        );
      }
      return Promise.reject(new Error(`unexpected read: ${path}`));
    };
    const result = await runRebasePhase(
      BASE_OPTS({
        gitInvoker: invoker,
        agentRunner: runner,
        conflictFileReader: reader,
      }),
    );
    assertEquals(result, { kind: "clean", iterations: 1 });
    const invocations = runner.recordedInvocations();
    assertEquals(invocations.length, 1);
    // The dispatched prompt embeds the conflict context.
    const prompt = invocations[0]?.prompt ?? "";
    assertStringIncludes(prompt, STABILIZE_REBASE_CONFLICT_PROMPT_HEAD);
    assertStringIncludes(prompt, "Issue: #42");
    assertStringIncludes(prompt, "Base branch: main");
    assertStringIncludes(prompt, "src/a.ts");
    assertStringIncludes(prompt, "<<<<<<<");
    assertStringIncludes(prompt, "=======");
    assertStringIncludes(prompt, ">>>>>>>");
    // The git command sequence covers the full conflict-resolve cycle.
    assertEquals(calls.map((call) => call.args[0]), [
      "fetch",
      "rebase",
      "diff",
      "add",
      "rebase",
    ]);
  },
);

// ---------------------------------------------------------------------------
// Iteration budget exhausts → NEEDS_HUMAN
// ---------------------------------------------------------------------------

Deno.test(
  "runRebasePhase surfaces needs-human when the iteration budget is exhausted",
  async () => {
    // Budget 2: each iteration runs `diff`, `add`, `rebase --continue`,
    // then a follow-up `diff` to confirm the conflict set is non-empty.
    // After two failing iterations the phase issues a final `diff` and
    // a `rebase --abort` before resolving needs-human.
    const conflictDiff: GitInvocationResult = {
      exitCode: 0,
      stdout: "src/a.ts\nsrc/b.ts\n",
      stderr: "",
    };
    const continueFailure: GitInvocationResult = {
      exitCode: 1,
      stdout: "still conflicting",
      stderr: "",
    };
    const { invoker, calls } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: {
          exitCode: 1,
          stdout: "",
          stderr: "CONFLICT in two files",
        },
      },
      // Iteration 1
      { match: (args) => args[0] === "diff", result: conflictDiff },
      { match: (args) => args[0] === "add", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args[1] === "--continue",
        result: continueFailure,
      },
      { match: (args) => args[0] === "diff", result: conflictDiff },
      // Iteration 2
      { match: (args) => args[0] === "diff", result: conflictDiff },
      { match: (args) => args[0] === "add", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args[1] === "--continue",
        result: continueFailure,
      },
      { match: (args) => args[0] === "diff", result: conflictDiff },
      // Final diff after exhaustion + abort.
      { match: (args) => args[0] === "diff", result: conflictDiff },
      {
        match: (args) => args[0] === "rebase" && args[1] === "--abort",
        result: SUCCESS,
      },
    ]);
    const runner = new MockAgentRunner();
    // Two scripted runs (one per iteration).
    runner.queueRun({
      messages: [{ role: "assistant", text: "trying iteration 1" }],
    });
    runner.queueRun({
      messages: [{ role: "assistant", text: "trying iteration 2" }],
    });
    const reader = (_path: string): Promise<string> => {
      return Promise.resolve(
        "<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> origin/main\n",
      );
    };
    const result = await runRebasePhase(
      BASE_OPTS({
        gitInvoker: invoker,
        agentRunner: runner,
        conflictFileReader: reader,
        maxIterations: 2,
      }),
    );
    assertEquals(result.kind, "needs-human");
    if (result.kind === "needs-human") {
      assertEquals(result.iterations, 2);
      assertEquals(result.conflictingFiles, ["src/a.ts", "src/b.ts"]);
    }
    assertEquals(runner.recordedInvocations().length, 2);
    // The final two invocations are the closing diff and the abort.
    assertEquals(calls.at(-2)?.args, ["diff", "--name-only", "--diff-filter=U"]);
    assertEquals(calls.at(-1)?.args, ["rebase", "--abort"]);
  },
);

// ---------------------------------------------------------------------------
// Fetch failure surfaces as a fatal StabilizeRebaseError
// ---------------------------------------------------------------------------

Deno.test(
  "runRebasePhase throws StabilizeRebaseError when git fetch fails",
  async () => {
    const { invoker } = scriptedGitInvoker([
      {
        match: (args) => args[0] === "fetch",
        result: {
          exitCode: 128,
          stdout: "",
          stderr: "fatal: could not read from remote",
        },
      },
    ]);
    await assertRejects(
      async () => {
        await runRebasePhase(BASE_OPTS({ gitInvoker: invoker }));
      },
      StabilizeRebaseError,
      "git fetch origin main:refs/remotes/origin/main failed",
    );
  },
);

// ---------------------------------------------------------------------------
// Non-conflict rebase failure surfaces as fatal
// ---------------------------------------------------------------------------

Deno.test(
  "runRebasePhase throws when initial rebase fails without conflicting files",
  async () => {
    const { invoker } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: {
          exitCode: 128,
          stdout: "",
          stderr: "fatal: refspec missing",
        },
      },
      {
        match: (args) => args[0] === "diff",
        result: { exitCode: 0, stdout: "", stderr: "" },
      },
    ]);
    const error = await assertRejects(
      async () => {
        await runRebasePhase(BASE_OPTS({ gitInvoker: invoker }));
      },
      StabilizeRebaseError,
    );
    assertEquals(error.operation, "rebase-start");
  },
);

Deno.test(
  "runRebasePhase throws when rebase --continue fails without conflicting files",
  async () => {
    const conflictDiff: GitInvocationResult = {
      exitCode: 0,
      stdout: "src/a.ts\n",
      stderr: "",
    };
    const cleanDiff: GitInvocationResult = {
      exitCode: 0,
      stdout: "",
      stderr: "",
    };
    const { invoker } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: { exitCode: 1, stdout: "", stderr: "CONFLICT" },
      },
      { match: (args) => args[0] === "diff", result: conflictDiff },
      { match: (args) => args[0] === "add", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args[1] === "--continue",
        result: { exitCode: 128, stdout: "", stderr: "fatal: random failure" },
      },
      { match: (args) => args[0] === "diff", result: cleanDiff },
    ]);
    const runner = new MockAgentRunner();
    runner.queueRun({ messages: [{ role: "assistant", text: "ok" }] });
    const reader = (_path: string): Promise<string> => Promise.resolve("x");
    const error = await assertRejects(
      async () => {
        await runRebasePhase(
          BASE_OPTS({
            gitInvoker: invoker,
            agentRunner: runner,
            conflictFileReader: reader,
          }),
        );
      },
      StabilizeRebaseError,
    );
    assertEquals(error.operation, "rebase-continue");
  },
);

Deno.test(
  "runRebasePhase throws when git add fails between iterations",
  async () => {
    const conflictDiff: GitInvocationResult = {
      exitCode: 0,
      stdout: "src/a.ts\n",
      stderr: "",
    };
    const { invoker } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: { exitCode: 1, stdout: "", stderr: "CONFLICT" },
      },
      { match: (args) => args[0] === "diff", result: conflictDiff },
      {
        match: (args) => args[0] === "add",
        result: {
          exitCode: 1,
          stdout: "",
          stderr: "fatal: could not stage",
        },
      },
    ]);
    const runner = new MockAgentRunner();
    runner.queueRun({ messages: [{ role: "assistant", text: "ok" }] });
    const reader = (_path: string): Promise<string> => Promise.resolve("x");
    const error = await assertRejects(
      async () => {
        await runRebasePhase(
          BASE_OPTS({
            gitInvoker: invoker,
            agentRunner: runner,
            conflictFileReader: reader,
          }),
        );
      },
      StabilizeRebaseError,
    );
    assertEquals(error.operation, "add");
  },
);

Deno.test(
  "runRebasePhase throws when the diff invocation itself fails",
  async () => {
    const { invoker } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: { exitCode: 1, stdout: "", stderr: "CONFLICT" },
      },
      {
        match: (args) => args[0] === "diff",
        result: {
          exitCode: 128,
          stdout: "",
          stderr: "fatal: bad object",
        },
      },
    ]);
    const error = await assertRejects(
      async () => {
        await runRebasePhase(BASE_OPTS({ gitInvoker: invoker }));
      },
      StabilizeRebaseError,
    );
    assertEquals(error.operation, "diff-conflicts");
  },
);

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

Deno.test(
  "runRebasePhase rejects an empty worktreePath",
  async () => {
    await assertRejects(
      async () => {
        await runRebasePhase(BASE_OPTS({ worktreePath: "" }));
      },
      StabilizeRebaseError,
      "worktreePath",
    );
  },
);

Deno.test(
  "runRebasePhase rejects a non-absolute worktreePath",
  async () => {
    await assertRejects(
      async () => {
        await runRebasePhase(BASE_OPTS({ worktreePath: "relative/path" }));
      },
      StabilizeRebaseError,
      "absolute",
    );
  },
);

Deno.test(
  "runRebasePhase rejects an empty baseBranch",
  async () => {
    await assertRejects(
      async () => {
        await runRebasePhase(BASE_OPTS({ baseBranch: "" }));
      },
      StabilizeRebaseError,
      "baseBranch",
    );
  },
);

Deno.test(
  "runRebasePhase rejects an empty model",
  async () => {
    await assertRejects(
      async () => {
        await runRebasePhase(BASE_OPTS({ model: "" }));
      },
      StabilizeRebaseError,
      "model",
    );
  },
);

Deno.test(
  "runRebasePhase rejects a zero/negative maxIterations",
  async () => {
    await assertRejects(
      async () => {
        await runRebasePhase(BASE_OPTS({ maxIterations: 0 }));
      },
      StabilizeRebaseError,
      "maxIterations",
    );
    await assertRejects(
      async () => {
        await runRebasePhase(BASE_OPTS({ maxIterations: -1 }));
      },
      StabilizeRebaseError,
      "maxIterations",
    );
  },
);

// ---------------------------------------------------------------------------
// Conflict-marker reader failure is logged but does not abort the phase.
// ---------------------------------------------------------------------------

Deno.test(
  "runRebasePhase logs and continues when the conflict-file reader rejects",
  async () => {
    const { invoker } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: { exitCode: 1, stdout: "", stderr: "CONFLICT" },
      },
      {
        match: (args) => args[0] === "diff",
        result: { exitCode: 0, stdout: "src/a.ts\n", stderr: "" },
      },
      { match: (args) => args[0] === "add", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args[1] === "--continue",
        result: SUCCESS,
      },
    ]);
    const runner = new MockAgentRunner();
    runner.queueRun({ messages: [{ role: "assistant", text: "ok" }] });
    const reader = (_path: string): Promise<string> => {
      return Promise.reject(new Deno.errors.NotFound("vanished"));
    };
    const logger = recordingLogger();
    const result = await runRebasePhase(
      BASE_OPTS({
        gitInvoker: invoker,
        agentRunner: runner,
        conflictFileReader: reader,
        logger,
      }),
    );
    assertEquals(result, { kind: "clean", iterations: 1 });
    // The reader failure surfaced as a warn line.
    assert(
      logger.warns.some((line) => line.includes("could not read conflicted file src/a.ts")),
      `expected warn about file read failure; got ${logger.warns.join("\n")}`,
    );
    // The agent prompt still ran, with the reader failure embedded inline.
    const invocations = runner.recordedInvocations();
    assertEquals(invocations.length, 1);
    const prompt = invocations[0]?.prompt ?? "";
    assertStringIncludes(prompt, "could not read file");
  },
);

// ---------------------------------------------------------------------------
// Conflict file content is truncated to the preview budget
// ---------------------------------------------------------------------------

Deno.test(
  "runRebasePhase truncates large conflict files in the agent prompt",
  async () => {
    const { invoker } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: { exitCode: 1, stdout: "", stderr: "CONFLICT" },
      },
      {
        match: (args) => args[0] === "diff",
        result: { exitCode: 0, stdout: "src/big.ts\n", stderr: "" },
      },
      { match: (args) => args[0] === "add", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args[1] === "--continue",
        result: SUCCESS,
      },
    ]);
    const runner = new MockAgentRunner();
    runner.queueRun({ messages: [{ role: "assistant", text: "ok" }] });
    // Build a payload comfortably larger than the truncation budget.
    const huge = "x".repeat(20_000);
    const reader = (_path: string): Promise<string> => Promise.resolve(huge);
    const result = await runRebasePhase(
      BASE_OPTS({
        gitInvoker: invoker,
        agentRunner: runner,
        conflictFileReader: reader,
      }),
    );
    assertEquals(result.kind, "clean");
    const prompt = runner.recordedInvocations()[0]?.prompt ?? "";
    assertStringIncludes(prompt, "...[truncated]");
    // The full payload must NOT be embedded.
    assert(!prompt.includes(huge));
  },
);

// ---------------------------------------------------------------------------
// Session id round trip
// ---------------------------------------------------------------------------

Deno.test(
  "runRebasePhase forwards the supplied sessionId to the agent runner",
  async () => {
    const { invoker } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: { exitCode: 1, stdout: "", stderr: "CONFLICT" },
      },
      {
        match: (args) => args[0] === "diff",
        result: { exitCode: 0, stdout: "src/a.ts\n", stderr: "" },
      },
      { match: (args) => args[0] === "add", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args[1] === "--continue",
        result: SUCCESS,
      },
    ]);
    const runner = new MockAgentRunner();
    runner.queueRun({ messages: [{ role: "assistant", text: "ok" }] });
    const reader = (_path: string): Promise<string> => Promise.resolve("x");
    const result = await runRebasePhase(
      BASE_OPTS({
        gitInvoker: invoker,
        agentRunner: runner,
        conflictFileReader: reader,
        sessionId: "session-abc",
      }),
    );
    assertEquals(result.kind, "clean");
    const invocations = runner.recordedInvocations();
    assertEquals(invocations[0]?.sessionId, "session-abc");
  },
);

// ---------------------------------------------------------------------------
// Agent dispatch failure
// ---------------------------------------------------------------------------

Deno.test(
  "runRebasePhase wraps an agent-dispatch failure into StabilizeRebaseError",
  async () => {
    const { invoker } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: { exitCode: 1, stdout: "", stderr: "CONFLICT" },
      },
      {
        match: (args) => args[0] === "diff",
        result: { exitCode: 0, stdout: "src/a.ts\n", stderr: "" },
      },
    ]);
    const runner = new MockAgentRunner();
    runner.queueRun({
      messages: [{ role: "assistant", text: "starting" }],
      error: new Error("agent crashed"),
    });
    const reader = (_path: string): Promise<string> => Promise.resolve("x");
    const error = await assertRejects(
      async () => {
        await runRebasePhase(
          BASE_OPTS({
            gitInvoker: invoker,
            agentRunner: runner,
            conflictFileReader: reader,
          }),
        );
      },
      StabilizeRebaseError,
    );
    assertEquals(error.operation, "agent-resolve");
    // Cause is preserved.
    assertInstanceOf(error.cause, Error);
  },
);

// ---------------------------------------------------------------------------
// Agent message with sessionId carries forward
// ---------------------------------------------------------------------------

Deno.test(
  "runRebasePhase tracks an SDK-assigned session id surfaced via message.sessionId",
  async () => {
    const conflictDiff: GitInvocationResult = {
      exitCode: 0,
      stdout: "src/a.ts\n",
      stderr: "",
    };
    const { invoker } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      // Initial rebase fails with a conflict.
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: { exitCode: 1, stdout: "", stderr: "CONFLICT" },
      },
      // Iteration 1: list conflicts, agent runs, stage, continue (still
      // conflicting), reconfirm conflict set is non-empty.
      { match: (args) => args[0] === "diff", result: conflictDiff },
      { match: (args) => args[0] === "add", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args[1] === "--continue",
        result: { exitCode: 1, stdout: "", stderr: "still conflicting" },
      },
      { match: (args) => args[0] === "diff", result: conflictDiff },
      // Iteration 2: list conflicts, agent, stage, continue (clean).
      { match: (args) => args[0] === "diff", result: conflictDiff },
      { match: (args) => args[0] === "add", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args[1] === "--continue",
        result: SUCCESS,
      },
    ]);
    const runner = new MockAgentRunner();
    // First run yields a message tagged with a session id.
    type SessionedMessage = AgentRunnerMessage & { readonly sessionId?: string };
    const taggedMessage: SessionedMessage = {
      role: "assistant",
      text: "session-bound",
      sessionId: "session-from-sdk",
    };
    runner.queueRun({ messages: [taggedMessage] });
    runner.queueRun({ messages: [{ role: "assistant", text: "iteration 2" }] });
    const reader = (_path: string): Promise<string> => Promise.resolve("x");
    const result = await runRebasePhase(
      BASE_OPTS({
        gitInvoker: invoker,
        agentRunner: runner,
        conflictFileReader: reader,
        maxIterations: 3,
      }),
    );
    assertEquals(result.kind, "clean");
    const invocations = runner.recordedInvocations();
    assertEquals(invocations.length, 2);
    // Second invocation carries the session id observed during the
    // first run.
    assertEquals(invocations[1]?.sessionId, "session-from-sdk");
  },
);

// ---------------------------------------------------------------------------
// Default invoker / reader exports exist and have the right shape
// ---------------------------------------------------------------------------

Deno.test(
  "defaultGitInvoker and defaultConflictFileReader are exported as functions",
  () => {
    // Smoke check: the bindings exist and are callable. We do not run
    // them here — the integration test exercises the real `git` and
    // `Deno.readTextFile` paths.
    assert(typeof defaultGitInvoker === "function");
    assert(typeof defaultConflictFileReader === "function");
  },
);

// ---------------------------------------------------------------------------
// Abort-failure is logged but does not change the result
// ---------------------------------------------------------------------------

Deno.test(
  "runRebasePhase logs but does not throw when rebase --abort fails after exhaustion",
  async () => {
    const conflictDiff: GitInvocationResult = {
      exitCode: 0,
      stdout: "src/a.ts\n",
      stderr: "",
    };
    const continueFailure: GitInvocationResult = {
      exitCode: 1,
      stdout: "",
      stderr: "still conflicting",
    };
    const { invoker } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: { exitCode: 1, stdout: "", stderr: "CONFLICT" },
      },
      { match: (args) => args[0] === "diff", result: conflictDiff },
      { match: (args) => args[0] === "add", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args[1] === "--continue",
        result: continueFailure,
      },
      { match: (args) => args[0] === "diff", result: conflictDiff },
      { match: (args) => args[0] === "diff", result: conflictDiff },
      {
        match: (args) => args[0] === "rebase" && args[1] === "--abort",
        result: {
          exitCode: 128,
          stdout: "",
          stderr: "no rebase in progress",
        },
      },
    ]);
    const runner = new MockAgentRunner();
    runner.queueRun({ messages: [{ role: "assistant", text: "ok" }] });
    const reader = (_path: string): Promise<string> => Promise.resolve("x");
    const logger = recordingLogger();
    const result = await runRebasePhase(
      BASE_OPTS({
        gitInvoker: invoker,
        agentRunner: runner,
        conflictFileReader: reader,
        logger,
        maxIterations: 1,
      }),
    );
    assertEquals(result.kind, "needs-human");
    // The abort failure surfaced as a warn line.
    assert(
      logger.warns.some((line) => line.includes("git rebase --abort failed")),
      `expected warn about abort failure; got ${logger.warns.join("\n")}`,
    );
  },
);

// ---------------------------------------------------------------------------
// SDK session id surfaces on the result
// ---------------------------------------------------------------------------

Deno.test(
  "runRebasePhase surfaces a fresh SDK session id on a clean result",
  async () => {
    const conflictDiff: GitInvocationResult = {
      exitCode: 0,
      stdout: "src/a.ts\n",
      stderr: "",
    };
    const { invoker } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: { exitCode: 1, stdout: "", stderr: "CONFLICT" },
      },
      { match: (args) => args[0] === "diff", result: conflictDiff },
      { match: (args) => args[0] === "add", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args[1] === "--continue",
        result: SUCCESS,
      },
    ]);
    const runner = new MockAgentRunner();
    type SessionedMessage = AgentRunnerMessage & { readonly sessionId?: string };
    const tagged: SessionedMessage = {
      role: "assistant",
      text: "session-bound",
      sessionId: "session-from-sdk",
    };
    runner.queueRun({ messages: [tagged] });
    const reader = (_path: string): Promise<string> => Promise.resolve("x");
    const result = await runRebasePhase(
      BASE_OPTS({
        gitInvoker: invoker,
        agentRunner: runner,
        conflictFileReader: reader,
        maxIterations: 2,
      }),
    );
    assertEquals(result.kind, "clean");
    assertEquals(result.sessionId, "session-from-sdk");
  },
);

Deno.test(
  "runRebasePhase surfaces a fresh SDK session id on a needs-human result",
  async () => {
    const conflictDiff: GitInvocationResult = {
      exitCode: 0,
      stdout: "src/a.ts\n",
      stderr: "",
    };
    const continueFailure: GitInvocationResult = {
      exitCode: 1,
      stdout: "",
      stderr: "still conflicting",
    };
    const { invoker } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: { exitCode: 1, stdout: "", stderr: "CONFLICT" },
      },
      { match: (args) => args[0] === "diff", result: conflictDiff },
      { match: (args) => args[0] === "add", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args[1] === "--continue",
        result: continueFailure,
      },
      { match: (args) => args[0] === "diff", result: conflictDiff },
      { match: (args) => args[0] === "diff", result: conflictDiff },
      {
        match: (args) => args[0] === "rebase" && args[1] === "--abort",
        result: SUCCESS,
      },
    ]);
    const runner = new MockAgentRunner();
    type SessionedMessage = AgentRunnerMessage & { readonly sessionId?: string };
    const tagged: SessionedMessage = {
      role: "assistant",
      text: "session-bound",
      sessionId: "session-after-budget",
    };
    runner.queueRun({ messages: [tagged] });
    const reader = (_path: string): Promise<string> => Promise.resolve("x");
    const result = await runRebasePhase(
      BASE_OPTS({
        gitInvoker: invoker,
        agentRunner: runner,
        conflictFileReader: reader,
        maxIterations: 1,
      }),
    );
    assertEquals(result.kind, "needs-human");
    assertEquals(result.sessionId, "session-after-budget");
  },
);

Deno.test(
  "runRebasePhase omits sessionId when the runner emits no fresh id",
  async () => {
    // Clean rebase with no agent run — the field stays absent so the
    // supervisor doesn't emit a redundant transition.
    const { invoker } = scriptedGitInvoker([
      { match: (args) => args[0] === "fetch", result: SUCCESS },
      {
        match: (args) => args[0] === "rebase" && args.length === 2,
        result: SUCCESS,
      },
    ]);
    const result = await runRebasePhase(
      BASE_OPTS({ gitInvoker: invoker }),
    );
    assertEquals(result.kind, "clean");
    assert(
      !("sessionId" in result) || result.sessionId === undefined,
      "expected no sessionId when no agent ran",
    );
  },
);

// ---------------------------------------------------------------------------
// Invoker-throws path: gitInvoker rejection wraps into StabilizeRebaseError
// ---------------------------------------------------------------------------

Deno.test(
  "runRebasePhase wraps a thrown gitInvoker as StabilizeRebaseError tagged with the operation",
  async () => {
    const invoker: StabilizeGitInvoker = (args) => {
      if (args[0] === "fetch") {
        return Promise.reject(new Error("git binary not found on PATH"));
      }
      return Promise.resolve(SUCCESS);
    };
    const error = await assertRejects(
      () => runRebasePhase(BASE_OPTS({ gitInvoker: invoker })),
      StabilizeRebaseError,
    );
    assertEquals(error.operation, "fetch");
    assertStringIncludes(error.message, "git binary not found on PATH");
    assertInstanceOf(error.cause, Error);
  },
);

Deno.test(
  "runRebasePhase fetch error message embeds the explicit refspec",
  async () => {
    const invoker: StabilizeGitInvoker = (args) => {
      if (args[0] === "fetch") {
        return Promise.resolve({
          exitCode: 1,
          stdout: "",
          stderr: "fatal: refspec rejected",
        });
      }
      return Promise.resolve(SUCCESS);
    };
    const error = await assertRejects(
      () => runRebasePhase(BASE_OPTS({ gitInvoker: invoker, baseBranch: "develop" })),
      StabilizeRebaseError,
    );
    assertEquals(error.operation, "fetch");
    // The error message uses the full refspec, not just the base
    // branch name, so refspec/remote-tracking issues are debuggable.
    assertStringIncludes(error.message, "develop:refs/remotes/origin/develop");
  },
);
