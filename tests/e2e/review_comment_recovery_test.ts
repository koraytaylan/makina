/**
 * tests/e2e/review_comment_recovery_test.ts — Wave 5 verification
 * scenario 3.
 *
 * **Scenario.** A review comment is left manually (or by a teammate)
 * on the PR mid-flight. The supervisor's `STABILIZING` loop's
 * `CONVERSATIONS` phase observes the new comment, dispatches the agent
 * with the comment context, pushes the fix, resolves the thread via
 * GraphQL, re-requests Copilot review, and eventually settles +
 * merges.
 *
 * The test asserts:
 *   1. The supervisor enters `STABILIZING` with a `CONVERSATIONS`
 *      sub-phase at least once.
 *   2. The task lands in `MERGED` (the supervisor closed the loop).
 *
 * **Important operator setup.** This scenario presumes a manual or
 * scripted reviewer leaves a comment on the PR after the initial push
 * but before settling. Two plausible setups:
 *
 *   - **Manual.** Run the test from a paired environment where a
 *     human reviewer is watching the sandbox PR and types a comment
 *     when the PR opens.
 *   - **Scripted.** Use a side-channel script (not in this repo) to
 *     poll the GitHub API for the new PR and post a deterministic
 *     review comment as soon as it appears.
 *
 * Either way, this test only drives the daemon and observes events;
 * the comment-injection step is out of scope.
 *
 * **Inputs.** Sandbox repo + GitHub App credentials via the
 * `MAKINA_E2E_*` env family. The scenario issue is named via
 * `MAKINA_E2E_REVIEW_COMMENT_ISSUE`.
 *
 * @module
 */

import { assert, assertEquals } from "@std/assert";

import { type AckPayload } from "@makina/core";
import { makeIssueNumber } from "@makina/core";
import { type Harness, registerE2eTest, type ResolvedE2eEnv } from "./_e2e_harness.ts";

registerE2eTest(
  "e2e: review-comment recovery — comment → agent fix → thread resolved → merged",
  "MAKINA_E2E_REVIEW_COMMENT_ISSUE",
  async (env: ResolvedE2eEnv, harness: Harness): Promise<void> => {
    const issueRaw = Deno.env.get("MAKINA_E2E_REVIEW_COMMENT_ISSUE");
    assert(
      issueRaw !== undefined,
      "review-comment issue env was checked by the harness",
    );
    const issueNumber = makeIssueNumber(Number(issueRaw));

    const issueAck = await harness.send({
      id: "rc-issue-1",
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

    // The conversations phase only fires after the comment lands; we
    // observe its first entry as evidence that the supervisor saw the
    // comment and dispatched the agent.
    let observedConversationsPhase = false;
    let observedTaskId: string | undefined;
    const terminal = await harness.waitForEvent((event) => {
      if (event.kind !== "state-changed") return false;
      if (event.data.stabilizePhase === "CONVERSATIONS") {
        observedConversationsPhase = true;
      }
      const { toState } = event.data;
      if (toState === "MERGED" || toState === "NEEDS_HUMAN" || toState === "FAILED") {
        observedTaskId = event.taskId;
        return true;
      }
      return false;
    }, env.timeoutMilliseconds);

    assert(
      observedConversationsPhase,
      "expected supervisor to enter the CONVERSATIONS sub-phase at least once " +
        "(was a review comment posted on the PR while the agent was running?)",
    );
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

    if (observedTaskId !== undefined) {
      const statusReply = await harness.send({
        id: "rc-status-1",
        type: "command",
        payload: { name: "status", args: [] },
      });
      const statusAck = statusReply.payload as AckPayload;
      assertEquals(statusAck.ok, true);
      const data = statusAck.data as
        | { tasks?: Array<{ id: string; state?: string }> }
        | undefined;
      const merged = data?.tasks?.find((task) => task.id === observedTaskId);
      assert(merged !== undefined, "merged task missing from /status");
      assertEquals(merged.state, "MERGED");
    }
  },
);
