/**
 * tests/e2e/happy_path_test.ts — Wave 5 verification scenario 1.
 *
 * **Scenario.** A small, well-scoped issue is picked up via `/issue`,
 * the agent implements, commits, pushes, opens a PR, requests Copilot
 * review, watches CI to green, observes no review comments, settles,
 * and squash-merges. The supervisor lands the task in `MERGED` without
 * any operator intervention.
 *
 * **Inputs.** Sandbox repo + GitHub App credentials via the
 * `MAKINA_E2E_*` env family. The specific issue is named via
 * `MAKINA_E2E_HAPPY_ISSUE`. See `_e2e_harness.ts` for the full
 * environment contract.
 *
 * **Skip behaviour.** Without the gate (`MAKINA_E2E=1`) or any required
 * variable, the test prints a one-line skip note and resolves
 * successfully. `deno task test` therefore stays fast and offline-safe;
 * `deno task ci` continues to run on every push without any sandbox
 * dependency.
 *
 * @module
 */

import { assert, assertEquals } from "@std/assert";

import { type AckPayload } from "@makina/core";
import { makeIssueNumber } from "@makina/core";
import { type Harness, registerE2eTest, type ResolvedE2eEnv } from "./_e2e_harness.ts";

registerE2eTest(
  "e2e: happy path — agent ships, CI green, Copilot clean, squash-merged",
  "MAKINA_E2E_HAPPY_ISSUE",
  async (env: ResolvedE2eEnv, harness: Harness): Promise<void> => {
    const issueRaw = Deno.env.get("MAKINA_E2E_HAPPY_ISSUE");
    assert(issueRaw !== undefined, "happy issue env was checked by the harness");
    const issueNumber = makeIssueNumber(Number(issueRaw));

    // Drive the supervisor.
    const issueAck = await harness.send({
      id: "happy-issue-1",
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

    // Wait for the supervisor to land the task in MERGED. Any other
    // terminal state (`NEEDS_HUMAN`, `FAILED`) is a scenario failure
    // — surface it as the test rejection.
    let observedTaskId: string | undefined;
    const terminal = await harness.waitForEvent((event) => {
      if (event.kind !== "state-changed") return false;
      const { toState } = event.data;
      if (toState === "MERGED" || toState === "NEEDS_HUMAN" || toState === "FAILED") {
        observedTaskId = event.taskId;
        return true;
      }
      return false;
    }, env.timeoutMilliseconds);
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

    // Sanity-check `/status` reflects the merged task.
    if (observedTaskId !== undefined) {
      const statusReply = await harness.send({
        id: "happy-status-1",
        type: "command",
        payload: { name: "status", args: [] },
      });
      const statusAck = statusReply.payload as AckPayload;
      assertEquals(statusAck.ok, true, "status command should succeed");
      const data = statusAck.data as { tasks?: Array<{ id: string; state: string }> } | undefined;
      const merged = data?.tasks?.find((task) => task.id === observedTaskId);
      assert(
        merged !== undefined,
        `task ${observedTaskId} missing from /status output`,
      );
      assertEquals(merged.state, "MERGED");
    }
  },
);
