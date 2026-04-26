/**
 * tests/helpers/mock_agent_runner.ts — scripted-stream {@link AgentRunner}
 * double for supervisor and stabilize-loop tests.
 *
 * Wave 3's real `src/daemon/agent-runner.ts` wraps `query()` from
 * `npm:@anthropic-ai/claude-agent-sdk`, which streams `SDKMessage`s. We
 * deliberately do **not** import the SDK from this helper:
 *
 *  1. It is a heavyweight optional dependency (subprocess management,
 *     MCP, hooks). Pulling it into the test graph slows every test run
 *     and blocks Deno's `npm:` resolution path on hosts without the
 *     supporting native bits.
 *  2. The supervisor only consumes the projection in
 *     {@link AgentRunnerMessage} (`role`, `text`). Anything more is
 *     SDK-specific surface that does not belong in the contract.
 *
 * The mock therefore yields {@link AgentRunnerMessage}s directly. Wave 3's
 * runner adapts the SDK's `SDKMessage` into the same projection. This is
 * the only intentional deviation from the issue brief; it is documented
 * here and in `src/types.ts`.
 */

import type { AgentRunner, AgentRunnerMessage, TaskId } from "../../src/types.ts";

/**
 * One scripted run. Each invocation of {@link MockAgentRunner.runAgent}
 * consumes one entry; the iterable yields the messages in `messages` and
 * then completes.
 */
export interface ScriptedAgentRun {
  /** Messages to yield from the run, in order. */
  readonly messages: readonly AgentRunnerMessage[];
  /**
   * If present, the iterable rejects with this error after yielding
   * `messages`. Lets tests cover error paths.
   */
  readonly error?: Error;
}

/** Recorded invocation, useful for assertions. */
export interface RecordedAgentRunInvocation {
  readonly taskId: TaskId;
  readonly worktreePath: string;
  readonly prompt: string;
  readonly model: string;
  readonly sessionId?: string;
}

/**
 * In-memory {@link AgentRunner}. Each call to `runAgent` consumes one
 * scripted run; when the queue is empty further calls throw so the test
 * fails loudly.
 *
 * @example
 * ```ts
 * const runner = new MockAgentRunner();
 * runner.queueRun({
 *   messages: [
 *     { role: "assistant", text: "Reading repo..." },
 *     { role: "tool-use", text: "git diff" },
 *     { role: "assistant", text: "Done." },
 *   ],
 * });
 *
 * const messages: AgentRunnerMessage[] = [];
 * for await (const message of runner.runAgent({
 *   taskId, worktreePath, prompt, model,
 * })) {
 *   messages.push(message);
 * }
 * assertEquals(messages.length, 3);
 * ```
 */
export class MockAgentRunner implements AgentRunner {
  private readonly scriptedRuns: ScriptedAgentRun[] = [];
  private readonly invocations: RecordedAgentRunInvocation[] = [];

  /**
   * Queue the next scripted run.
   *
   * @param run Run definition.
   */
  queueRun(run: ScriptedAgentRun): void {
    this.scriptedRuns.push(run);
  }

  /**
   * Read every recorded invocation.
   *
   * @returns A defensive copy of the call log, in order.
   */
  recordedInvocations(): readonly RecordedAgentRunInvocation[] {
    return [...this.invocations];
  }

  /**
   * Implementation of {@link AgentRunner.runAgent}.
   *
   * @param args Run arguments, mirrored straight into the recorded
   *   invocation log.
   * @returns Async iterable of scripted messages.
   * @throws Error when no scripted run is queued.
   */
  runAgent(args: {
    readonly taskId: TaskId;
    readonly worktreePath: string;
    readonly prompt: string;
    readonly sessionId?: string;
    readonly model: string;
  }): AsyncIterable<AgentRunnerMessage> {
    const recorded: RecordedAgentRunInvocation = args.sessionId === undefined
      ? {
        taskId: args.taskId,
        worktreePath: args.worktreePath,
        prompt: args.prompt,
        model: args.model,
      }
      : {
        taskId: args.taskId,
        worktreePath: args.worktreePath,
        prompt: args.prompt,
        model: args.model,
        sessionId: args.sessionId,
      };
    this.invocations.push(recorded);
    const run = this.scriptedRuns.shift();
    if (run === undefined) {
      throw new Error("MockAgentRunner.runAgent called with no scripted run queued");
    }
    return iterateScriptedRun(run);
  }
}

async function* iterateScriptedRun(
  run: ScriptedAgentRun,
): AsyncIterable<AgentRunnerMessage> {
  for (const message of run.messages) {
    // `await` makes this truly async so consumers cannot rely on
    // synchronous yields by accident.
    await Promise.resolve();
    yield message;
  }
  if (run.error !== undefined) {
    throw run.error;
  }
}
