/**
 * tests/e2e/ci_fail_recovery_test.ts — Wave 5 verification scenario 2.
 *
 * **Scenario.** An issue is intentionally constructed so the first
 * agent commit fails CI. The supervisor's `STABILIZING` loop detects
 * the red status, fetches the failing-job logs, dispatches the agent
 * with the failure context, observes the new green CI after the
 * follow-up push, settles, and merges.
 *
 * The test asserts:
 *   1. The supervisor enters `STABILIZING` with a `CI` sub-phase at
 *      least once.
 *   2. The supervisor produces at least one additional iteration after
 *      the initial commit (i.e. it actually re-ran the agent in
 *      response to red CI).
 *   3. The task lands in `MERGED`. (`NEEDS_HUMAN` from
 *      `MAX_TASK_ITERATIONS` exhaustion is a scenario failure unless
 *      the sandbox is misconfigured; the rejection prints the path.)
 *
 * **Inputs.** Same `MAKINA_E2E_*` env family as the happy path. The
 * scenario issue is `MAKINA_E2E_CI_FAIL_ISSUE`. The sandbox repo is
 * expected to have a CI workflow that fails on the first agent commit
 * but passes after the agent's fix; setting that up is the operator's
 * responsibility (documented in `docs/development.md`).
 *
 * @module
 */

import { assert, assertEquals } from "@std/assert";

import { type AckPayload } from "../../src/ipc/protocol.ts";
import { makeIssueNumber } from "../../src/types.ts";
import { type Harness, registerE2eTest, type ResolvedE2eEnv } from "./_e2e_harness.ts";

registerE2eTest(
  "e2e: CI-fail recovery — red CI → agent fix → green CI → merged",
  "MAKINA_E2E_CI_FAIL_ISSUE",
  async (env: ResolvedE2eEnv, harness: Harness): Promise<void> => {
    const issueRaw = Deno.env.get("MAKINA_E2E_CI_FAIL_ISSUE");
    assert(issueRaw !== undefined, "CI-fail issue env was checked by the harness");
    const issueNumber = makeIssueNumber(Number(issueRaw));

    const issueAck = await harness.send({
      id: "cifail-issue-1",
      type: "command",
      payload: {
        name: "issue",
        args: [String(issueNumber)],
        repo: env.repo,
        issueNumber,
      },
    });
    assertEquals(issueAck.type, "ack");
    const ack = issueAck.payload as AckPayload;
    assertEquals(
      ack.ok,
      true,
      `expected ack.ok=true; got error=${ack.error ?? "(none)"}`,
    );

    // Track CI-phase entries so we can assert the loop actually ran at
    // least once. The sandbox is expected to fail CI on the initial
    // commit; if the supervisor reaches MERGED without ever entering
    // the CI phase, the sandbox CI is mis-wired.
    let observedCiPhase = false;
    let observedTaskId: string | undefined;
    const terminal = await harness.waitForEvent((event) => {
      if (event.kind !== "state-changed") return false;
      if (event.data.stabilizePhase === "CI") {
        observedCiPhase = true;
      }
      const { toState } = event.data;
      if (toState === "MERGED" || toState === "NEEDS_HUMAN" || toState === "FAILED") {
        observedTaskId = event.taskId;
        return true;
      }
      return false;
    }, env.timeoutMilliseconds);

    assert(observedCiPhase, "expected supervisor to enter the CI sub-phase at least once");
    assert(
      terminal.kind === "state-changed",
      `expected state-changed event, got ${terminal.kind}`,
    );
    assertEquals(
      terminal.data.toState,
      "MERGED",
      `expected MERGED; supervisor reported ${terminal.data.toState}` +
        (terminal.data.reason !== undefined ? ` (reason: ${terminal.data.reason})` : ""),
    );

    // Verify iterationCount > 1, proving the agent ran at least twice
    // (initial draft + at least one CI-fix). The supervisor mints a
    // single `iterationCount` field per task; the projection on
    // `/status` exposes it.
    if (observedTaskId !== undefined) {
      const statusReply = await harness.send({
        id: "cifail-status-1",
        type: "command",
        payload: { name: "status", args: [] },
      });
      const statusAck = statusReply.payload as AckPayload;
      const data = statusAck.data as
        | { tasks?: Array<{ id: string; iterationCount?: number; state?: string }> }
        | undefined;
      const merged = data?.tasks?.find((task) => task.id === observedTaskId);
      assert(merged !== undefined, "merged task missing from /status");
      assert(
        (merged.iterationCount ?? 0) >= 2,
        `expected iterationCount >= 2 (initial draft + CI fix); ` +
          `got ${merged.iterationCount}`,
      );
    }
  },
);
