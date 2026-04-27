/**
 * Integration test for `src/daemon/agent-runner.ts`. Exercises the
 * production path (real SDK, real `claude` CLI, a temp git worktree)
 * end-to-end so we have one test that *actually* spawns the agent.
 *
 * **Gating.** This test is intentionally expensive: it spawns the
 * `claude` CLI subprocess, talks to the Anthropic API, and depends on
 * network reachability. It runs only when `AGENT_RUNNER_REAL_TEST=1` is
 * set in the environment; the daemon CI workflow leaves it off by
 * default. When the gate is off the test is `ignore:`-skipped at the
 * outer `Deno.test` level — no `t.step` runs and no console note is
 * printed. The console note (see below) is only emitted when the gate
 * is on but `claude` cannot be located on PATH.
 *
 * **Skip when claude is missing.** Even with the gate enabled, the test
 * skips with a clear note when `which claude` returns nothing — that
 * means the host doesn't have Claude Code installed and there is
 * nothing to integrate against. The skip is logged so a misconfigured
 * CI host produces an obvious signal.
 *
 * **Worktree boundary.** A `Deno.makeTempDir`-backed git repo is
 * initialised in the test body (no shared state with other tests) and
 * removed on settlement. The agent is asked a tiny prompt with a small
 * `maxTurns`-equivalent budget so the run terminates quickly.
 */

import { assert, assertExists } from "@std/assert";

import { createAgentRunner } from "../../src/daemon/agent-runner.ts";
import { createEventBus } from "../../src/daemon/event-bus.ts";
import { makeIssueNumber, makeTaskId, type TaskEvent } from "../../src/types.ts";

const REAL_TEST_ENV_VAR = "AGENT_RUNNER_REAL_TEST";
const REAL_TEST_ENABLED_VALUE = "1";

/**
 * Resolve `claude` via `which`. Returns `undefined` when the binary is
 * not installed so the test can skip with a clear note.
 */
async function claudePathOrUndefined(): Promise<string | undefined> {
  try {
    const command = new Deno.Command("which", {
      args: ["claude"],
      stdout: "piped",
      stderr: "piped",
    });
    const result = await command.output();
    if (!result.success) return undefined;
    const path = new TextDecoder().decode(result.stdout).split("\n")[0]?.trim() ?? "";
    return path.length === 0 ? undefined : path;
  } catch {
    return undefined;
  }
}

/** Initialise a tiny git worktree under `dir` so the agent has somewhere to operate. */
async function initWorktree(dir: string): Promise<void> {
  const init = await new Deno.Command("git", {
    args: ["init", "--quiet", dir],
    stdout: "null",
    stderr: "piped",
  }).output();
  assert(init.success, new TextDecoder().decode(init.stderr));
  await Deno.writeTextFile(`${dir}/README.md`, "# fixture\n");
}

Deno.test({
  name: "agent_runner_real: end-to-end run against the real SDK",
  // Resource-leak detection in Deno's test runner trips on the SDK's
  // long-lived subprocess; the SDK manages it itself, but the harness
  // would flag the open pipe as a sanitizer error.
  sanitizeResources: false,
  sanitizeOps: false,
  ignore: Deno.env.get(REAL_TEST_ENV_VAR) !== REAL_TEST_ENABLED_VALUE,
  async fn(t) {
    await t.step({
      name: "skip note when claude is not on PATH",
      ignore: false,
      async fn() {
        const claudePath = await claudePathOrUndefined();
        if (claudePath === undefined) {
          // The harness ignores `t.step` failure-or-skip semantics, so we
          // explicitly bail with a console note rather than failing.
          // eslint-disable-next-line no-console -- informational skip note
          console.warn(
            `agent_runner_real: \`claude\` is not on PATH; skipping. ` +
              `Install Claude Code to exercise this path.`,
          );
          return;
        }

        const tempDir = await Deno.makeTempDir({ prefix: "makina-agent-runner-it-" });
        try {
          await initWorktree(tempDir);

          const bus = createEventBus();
          const taskId = makeTaskId("task_real_integration");
          const issueNumber = makeIssueNumber(1);
          const events: TaskEvent[] = [];
          const sub = bus.subscribe(taskId, (event) => events.push(event));
          const runner = createAgentRunner({
            eventBus: bus,
            pathToClaudeCodeExecutable: claudePath,
          });

          try {
            // A short, deterministic prompt so the agent finishes quickly.
            // The model id is hard-coded to Sonnet 4.6 because the test
            // environment is expected to have it available.
            const result = await runner.runAndCollect({
              taskId,
              issueNumber,
              worktreePath: tempDir,
              prompt: "Reply with the single word ACK and nothing else.",
              model: "claude-sonnet-4-6",
            });
            assertExists(result);
            // We do not assert message contents — the model may add
            // formatting — but at least one message must have arrived
            // and the stream must publish to the bus.
            assert(result.messageCount > 0);
            assert(events.length > 0);
            assert(events.every((event) => event.taskId === taskId));
          } finally {
            sub.unsubscribe();
          }
        } finally {
          await Deno.remove(tempDir, { recursive: true });
        }
      },
    });
  },
});
