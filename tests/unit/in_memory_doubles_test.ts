/**
 * Unit tests for the in-memory doubles in `tests/helpers/`. The doubles
 * are test infrastructure, but they are also a contract — Wave 2
 * consumers test against them. If a double drifts (queue semantics
 * change, recorded-call ordering becomes nondeterministic, etc.) every
 * downstream wave breaks. Pinning behavior here keeps that surface
 * stable.
 */

import { assertEquals, assertNotEquals, assertRejects, assertThrows } from "@std/assert";

import { type EventPayload, parseEnvelope } from "../../src/ipc/protocol.ts";
import { type AckPayload, type PongPayload } from "../../src/ipc/protocol.ts";
import {
  makeInstallationId,
  makeIssueNumber,
  makeRepoFullName,
  makeTaskId,
} from "../../src/types.ts";
import { INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS } from "../../src/constants.ts";

import { InMemoryGitHubAuth } from "../helpers/in_memory_github_auth.ts";
import { InMemoryGitHubClient } from "../helpers/in_memory_github_client.ts";
import { InMemoryDaemonClient } from "../helpers/in_memory_daemon_client.ts";
import { MockAgentRunner } from "../helpers/mock_agent_runner.ts";

const ONE_HOUR_MILLISECONDS = 60 * 60 * 1_000;

// ---------------------------------------------------------------------------
// InMemoryGitHubAuth
// ---------------------------------------------------------------------------

Deno.test("InMemoryGitHubAuth: caches a token across consecutive calls", async () => {
  const auth = new InMemoryGitHubAuth();
  const installation = makeInstallationId(7);
  const first = await auth.getInstallationToken(installation);
  const second = await auth.getInstallationToken(installation);
  assertEquals(first, second);
  assertEquals(auth.recordedRequests().length, 2);
});

Deno.test("InMemoryGitHubAuth: refreshes a token after the TTL elapses", async () => {
  const auth = new InMemoryGitHubAuth();
  const installation = makeInstallationId(7);
  const first = await auth.getInstallationToken(installation);
  // Advance past the TTL minus the refresh lead — the cache treats the
  // token as expired and mints a new one.
  auth.advanceClockMilliseconds(
    ONE_HOUR_MILLISECONDS - INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS,
  );
  const second = await auth.getInstallationToken(installation);
  assertNotEquals(first, second);
});

Deno.test("InMemoryGitHubAuth: distinct installations get distinct tokens", async () => {
  const auth = new InMemoryGitHubAuth();
  const a = await auth.getInstallationToken(makeInstallationId(1));
  const b = await auth.getInstallationToken(makeInstallationId(2));
  assertNotEquals(a, b);
});

Deno.test("InMemoryGitHubAuth: advanceClockMilliseconds rejects negative input", () => {
  const auth = new InMemoryGitHubAuth(0);
  assertThrows(() => auth.advanceClockMilliseconds(-1), RangeError);
});

Deno.test("InMemoryGitHubAuth: nowMilliseconds tracks the virtual clock", () => {
  const auth = new InMemoryGitHubAuth(1_000);
  assertEquals(auth.nowMilliseconds(), 1_000);
  auth.advanceClockMilliseconds(500);
  assertEquals(auth.nowMilliseconds(), 1_500);
});

Deno.test("InMemoryGitHubAuth: resetCache forces a fresh mint", async () => {
  const auth = new InMemoryGitHubAuth();
  const installation = makeInstallationId(11);
  const first = await auth.getInstallationToken(installation);
  auth.resetCache();
  const second = await auth.getInstallationToken(installation);
  assertNotEquals(first, second);
});

// ---------------------------------------------------------------------------
// InMemoryGitHubClient
// ---------------------------------------------------------------------------

Deno.test("InMemoryGitHubClient: serves replies in queued order", async () => {
  const client = new InMemoryGitHubClient();
  const repo = makeRepoFullName("a/b");
  client.queueGetCombinedStatus({
    kind: "value",
    value: { state: "pending", sha: "abc" },
  });
  client.queueGetCombinedStatus({
    kind: "value",
    value: { state: "success", sha: "abc" },
  });
  const first = await client.getCombinedStatus(repo, "abc");
  const second = await client.getCombinedStatus(repo, "abc");
  assertEquals(first.state, "pending");
  assertEquals(second.state, "success");
});

Deno.test("InMemoryGitHubClient: throws when the queue is empty", () => {
  const client = new InMemoryGitHubClient();
  assertThrows(() => {
    void client.getIssue(makeRepoFullName("a/b"), makeIssueNumber(1));
  });
});

Deno.test("InMemoryGitHubClient: forwards scripted errors as rejected promises", async () => {
  const client = new InMemoryGitHubClient();
  client.queueRequestReviewers({ kind: "error", error: new Error("418") });
  await assertRejects(
    () =>
      client.requestReviewers(
        makeRepoFullName("a/b"),
        makeIssueNumber(1),
        ["Copilot"],
      ),
    Error,
    "418",
  );
});

Deno.test("InMemoryGitHubClient: records every method invocation", async () => {
  const client = new InMemoryGitHubClient();
  const repo = makeRepoFullName("a/b");
  client.queueGetIssue({
    kind: "value",
    value: {
      number: makeIssueNumber(42),
      title: "t",
      body: "b",
      state: "open",
    },
  });
  client.queueCreatePullRequest({
    kind: "value",
    value: {
      number: makeIssueNumber(7),
      headSha: "h",
      headRef: "feat",
      baseRef: "main",
      state: "open",
    },
  });
  client.queueRequestReviewers({ kind: "value", value: undefined });
  client.queueMergePullRequest({ kind: "value", value: undefined });

  await client.getIssue(repo, makeIssueNumber(42));
  await client.createPullRequest(repo, {
    headRef: "feat",
    baseRef: "main",
    title: "t",
    body: "b",
  });
  await client.requestReviewers(repo, makeIssueNumber(7), ["Copilot"]);
  await client.mergePullRequest(repo, makeIssueNumber(7), "squash");

  const calls = client.recordedCalls();
  assertEquals(calls.length, 4);
  assertEquals(calls[0]?.method, "getIssue");
  assertEquals(calls[1]?.method, "createPullRequest");
  assertEquals(calls[2]?.method, "requestReviewers");
  assertEquals(calls[3]?.method, "mergePullRequest");
});

Deno.test("InMemoryGitHubClient: records the four stabilize-phase methods", async () => {
  const client = new InMemoryGitHubClient();
  const repo = makeRepoFullName("a/b");
  client.queueGetCombinedStatus({
    kind: "value",
    value: { state: "success", sha: "abc" },
  });
  client.queueListCheckRuns({
    kind: "value",
    value: [
      {
        id: 1,
        name: "build",
        status: "completed",
        conclusion: "success",
        htmlUrl: "https://github.com/check/1",
      },
    ],
  });
  client.queueGetCheckRunLogs({
    kind: "value",
    value: new Uint8Array([1, 2, 3]),
  });
  client.queueListReviews({
    kind: "value",
    value: [
      {
        id: 10,
        user: "Copilot",
        state: "COMMENTED",
        body: "looks ok",
        submittedAtIso: "2026-04-26T12:00:00Z",
      },
    ],
  });
  client.queueListReviewComments({
    kind: "value",
    value: [
      {
        id: 100,
        pullRequestReviewId: 10,
        user: "Copilot",
        body: "Consider renaming.",
        path: "src/x.ts",
        line: 12,
        inReplyToId: null,
        createdAtIso: "2026-04-26T12:00:00Z",
      },
    ],
  });

  const status = await client.getCombinedStatus(repo, "abc");
  const runs = await client.listCheckRuns(repo, "abc");
  const logs = await client.getCheckRunLogs(repo, 1);
  const reviews = await client.listReviews(repo, makeIssueNumber(7));
  const comments = await client.listReviewComments(repo, makeIssueNumber(7));

  assertEquals(status.state, "success");
  assertEquals(runs.length, 1);
  assertEquals(runs[0]?.name, "build");
  assertEquals(Array.from(logs), [1, 2, 3]);
  assertEquals(reviews.length, 1);
  assertEquals(reviews[0]?.state, "COMMENTED");
  assertEquals(comments.length, 1);
  assertEquals(comments[0]?.line, 12);

  const calls = client.recordedCalls();
  assertEquals(calls.length, 5);
  assertEquals(calls[0]?.method, "getCombinedStatus");
  assertEquals(calls[1]?.method, "listCheckRuns");
  assertEquals(calls[2]?.method, "getCheckRunLogs");
  assertEquals(calls[3]?.method, "listReviews");
  assertEquals(calls[4]?.method, "listReviewComments");
});

// ---------------------------------------------------------------------------
// InMemoryDaemonClient
// ---------------------------------------------------------------------------

Deno.test("InMemoryDaemonClient: ping returns pong with daemonVersion", async () => {
  const client = new InMemoryDaemonClient();
  const reply = await client.send({ id: "1", type: "ping", payload: {} });
  assertEquals(reply.type, "pong");
  assertEquals((reply.payload as PongPayload).daemonVersion.length > 0, true);
});

Deno.test("InMemoryDaemonClient: non-ping requests get an ack", async () => {
  const client = new InMemoryDaemonClient();
  const reply = await client.send({
    id: "2",
    type: "command",
    payload: { name: "status", args: [] },
  });
  assertEquals(reply.type, "ack");
  assertEquals((reply.payload as AckPayload).ok, true);
});

Deno.test("InMemoryDaemonClient: setRequestHandler customizes ack payloads", async () => {
  const client = new InMemoryDaemonClient();
  client.setRequestHandler((envelope) => {
    if (envelope.type === "ping") {
      return Promise.resolve({ daemonVersion: "test" });
    }
    return Promise.resolve({ ok: false, error: "denied" });
  });
  const ack = await client.send({
    id: "3",
    type: "command",
    payload: { name: "boom", args: [] },
  });
  assertEquals(ack.type, "ack");
  assertEquals((ack.payload as AckPayload).ok, false);
  assertEquals((ack.payload as AckPayload).error, "denied");
});

Deno.test("InMemoryDaemonClient: simulateEvent fans out to subscribers", () => {
  const client = new InMemoryDaemonClient();
  const seen: EventPayload[] = [];
  const subscription = client.subscribeEvents((event) => {
    seen.push(event);
  });
  assertEquals(client.activeSubscriptionCount(), 1);
  const parsed = parseEnvelope({
    id: "e",
    type: "event",
    payload: {
      taskId: "t",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "log",
      data: { level: "info", message: "hello" },
    },
  });
  if (!parsed.success || parsed.data.type !== "event") {
    throw new Error("test fixture should produce an event envelope");
  }
  client.simulateEvent(parsed.data.payload);
  subscription.unsubscribe();
  client.simulateEvent(parsed.data.payload);
  assertEquals(seen.length, 1);
  assertEquals(client.activeSubscriptionCount(), 0);
});

Deno.test("InMemoryDaemonClient: send rejects malformed envelopes", async () => {
  const client = new InMemoryDaemonClient();
  await assertRejects(() =>
    client.send(
      // Deliberately wrong type discriminant.
      { id: "x", type: "nope" } as unknown as Parameters<typeof client.send>[0],
    )
  );
});

Deno.test("InMemoryDaemonClient: recordedSends returns sends in order", async () => {
  const client = new InMemoryDaemonClient();
  await client.send({ id: "1", type: "ping", payload: {} });
  await client.send({
    id: "2",
    type: "subscribe",
    payload: { target: "*" },
  });
  const sends = client.recordedSends();
  assertEquals(sends.length, 2);
  assertEquals(sends[0]?.id, "1");
  assertEquals(sends[1]?.id, "2");
});

// ---------------------------------------------------------------------------
// MockAgentRunner
// ---------------------------------------------------------------------------

Deno.test("MockAgentRunner: throws when no scripted run is queued", () => {
  const runner = new MockAgentRunner();
  assertThrows(() => {
    runner.runAgent({
      taskId: makeTaskId("t"),
      worktreePath: "/x",
      prompt: "p",
      model: "m",
    });
  });
});

Deno.test("MockAgentRunner: records sessionId only when present", () => {
  const runner = new MockAgentRunner();
  runner.queueRun({ messages: [] });
  runner.runAgent({
    taskId: makeTaskId("t1"),
    worktreePath: "/x",
    prompt: "p",
    model: "m",
  });
  runner.queueRun({ messages: [] });
  runner.runAgent({
    taskId: makeTaskId("t2"),
    worktreePath: "/x",
    prompt: "p",
    model: "m",
    sessionId: "session-1",
  });
  const calls = runner.recordedInvocations();
  assertEquals(calls.length, 2);
  assertEquals(calls[0]?.sessionId, undefined);
  assertEquals(calls[1]?.sessionId, "session-1");
});
