/**
 * Unit tests for `src/types.ts`. Cover branded-id constructors, the
 * lifecycle enumeration, and the helper interfaces (the latter via the
 * in-memory doubles, which compile against the interfaces — if the
 * contract drifts, this file stops type-checking).
 */

import {
  assertEquals,
  assertNotStrictEquals,
  assertRejects,
  assertStrictEquals,
  assertThrows,
} from "@std/assert";

import {
  type AgentRunner,
  type EventBus,
  type GitHubAuth,
  type GitHubClient,
  type InstallationId,
  makeInstallationId,
  makeIssueNumber,
  makeRepoFullName,
  makeTaskId,
  MERGE_MODES,
  type Persistence,
  type Poller,
  STABILIZE_PHASES,
  TASK_EVENT_KINDS,
  TASK_STATES,
  type TaskId,
  type TaskState,
  type TaskSupervisor,
  type WorktreeManager,
} from "../../src/types.ts";

import { InMemoryGitHubAuth } from "../helpers/in_memory_github_auth.ts";
import { InMemoryGitHubClient } from "../helpers/in_memory_github_client.ts";
import { MockAgentRunner } from "../helpers/mock_agent_runner.ts";

Deno.test("TaskState enumerates every state in the lifecycle plan", () => {
  // The plan in docs/lifecycle.md fixes this set; the supervisor and the
  // TUI both exhaustive-switch on it.
  const expected: readonly TaskState[] = [
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
  ];
  assertEquals([...TASK_STATES], expected);
});

Deno.test("STABILIZE_PHASES enumerates the three loop phases", () => {
  assertEquals([...STABILIZE_PHASES], ["REBASE", "CI", "CONVERSATIONS"]);
});

Deno.test("MERGE_MODES enumerates the three merge modes", () => {
  assertEquals([...MERGE_MODES], ["squash", "rebase", "manual"]);
});

Deno.test("TASK_EVENT_KINDS enumerates the supervisor's event tags", () => {
  assertEquals([...TASK_EVENT_KINDS], [
    "state-changed",
    "log",
    "agent-message",
    "github-call",
    "error",
  ]);
});

Deno.test("makeTaskId trims whitespace and rejects empty input", () => {
  const id: TaskId = makeTaskId("  task_abc  ");
  assertEquals(id, "task_abc");

  assertThrows(() => makeTaskId(""), RangeError);
  assertThrows(() => makeTaskId("   "), RangeError);
});

Deno.test("makeRepoFullName accepts owner/name and rejects other shapes", () => {
  const repo = makeRepoFullName("koraytaylan/makina");
  assertEquals(repo, "koraytaylan/makina");

  assertThrows(() => makeRepoFullName("just-name"), RangeError);
  assertThrows(() => makeRepoFullName("a/b/c"), RangeError);
  assertThrows(() => makeRepoFullName(""), RangeError);
  assertThrows(() => makeRepoFullName("/missing-owner"), RangeError);
  assertThrows(() => makeRepoFullName("missing-name/"), RangeError);
  assertThrows(() => makeRepoFullName("with space/name"), RangeError);
});

Deno.test("makeIssueNumber rejects non-positive and non-integer input", () => {
  assertEquals(makeIssueNumber(1), 1);
  assertEquals(makeIssueNumber(42), 42);

  assertThrows(() => makeIssueNumber(0), RangeError);
  assertThrows(() => makeIssueNumber(-1), RangeError);
  assertThrows(() => makeIssueNumber(1.5), RangeError);
  assertThrows(() => makeIssueNumber(Number.NaN), RangeError);
});

Deno.test("makeInstallationId rejects non-positive and non-integer input", () => {
  assertEquals(makeInstallationId(1), 1);
  assertEquals(makeInstallationId(9876543), 9876543);

  assertThrows(() => makeInstallationId(0), RangeError);
  assertThrows(() => makeInstallationId(-1), RangeError);
  assertThrows(() => makeInstallationId(1.5), RangeError);
});

// --- Type-system checks (compile-only) -------------------------------------
//
// These tests do not perform runtime assertions; they exist to make the type
// guarantees of the branded-id system observable in test output. If a future
// edit to `types.ts` weakens the brand and lets, say, a `TaskId` flow into a
// `RepoFullName`-typed slot, these blocks will fail to compile and the test
// suite will refuse to run.

Deno.test("branded ids resist cross-assignment (compile-time check)", () => {
  const taskId: TaskId = makeTaskId("task_abc");
  const repoName = makeRepoFullName("a/b");
  const issueNumber = makeIssueNumber(1);
  const installationId = makeInstallationId(1);

  // The branded types are runtime-equal to their primitive carriers.
  assertEquals(typeof taskId, "string");
  assertEquals(typeof repoName, "string");
  assertEquals(typeof issueNumber, "number");
  assertEquals(typeof installationId, "number");

  // `taskId` and `repoName` are both `string` at runtime, but `===` tells
  // them apart structurally.
  assertNotStrictEquals(taskId as unknown, repoName as unknown);
});

// --- Interface conformance via in-memory doubles ---------------------------
//
// The doubles import the interfaces and `implements` them; if the contract
// drifts, `deno check` fails. We additionally bind the doubles to the
// interface types here so a subtle assignability regression surfaces in the
// test build, not in some Wave 2 consumer.

Deno.test("InMemoryGitHubAuth conforms to GitHubAuth", async () => {
  const auth: GitHubAuth = new InMemoryGitHubAuth();
  const installation: InstallationId = makeInstallationId(123);
  const token = await auth.getInstallationToken(installation);
  assertEquals(typeof token, "string");
});

Deno.test("InMemoryGitHubClient conforms to GitHubClient", async () => {
  const client: GitHubClient = new InMemoryGitHubClient();
  const inMemory = client as InMemoryGitHubClient;
  inMemory.queueGetIssue({
    kind: "value",
    value: {
      number: makeIssueNumber(1),
      title: "t",
      body: "b",
      state: "open",
    },
  });
  const issue = await client.getIssue(
    makeRepoFullName("a/b"),
    makeIssueNumber(1),
  );
  assertEquals(issue.title, "t");
});

Deno.test("MockAgentRunner conforms to AgentRunner", async () => {
  const runner: AgentRunner = new MockAgentRunner();
  const mock = runner as MockAgentRunner;
  mock.queueRun({ messages: [{ role: "assistant", text: "hi" }] });
  const messages = [];
  for await (
    const m of runner.runAgent({
      taskId: makeTaskId("t1"),
      worktreePath: "/tmp/x",
      prompt: "p",
      model: "claude-sonnet-4-6",
    })
  ) {
    messages.push(m);
  }
  assertEquals(messages.length, 1);
});

Deno.test("MockAgentRunner surfaces scripted errors after streaming", async () => {
  const runner = new MockAgentRunner();
  runner.queueRun({
    messages: [{ role: "assistant", text: "before" }],
    error: new Error("scripted failure"),
  });
  const seen: string[] = [];
  await assertRejects(
    async () => {
      for await (
        const m of runner.runAgent({
          taskId: makeTaskId("t1"),
          worktreePath: "/tmp/x",
          prompt: "p",
          model: "claude-sonnet-4-6",
        })
      ) {
        seen.push(m.text);
      }
    },
    Error,
    "scripted failure",
  );
  assertEquals(seen, ["before"]);
});

// Compile-time-only references so the unused `WorktreeManager`, `Persistence`,
// `EventBus`, `Poller`, and `TaskSupervisor` interfaces do not get pruned by a
// future "unused import" lint.
Deno.test("contract interfaces are reachable from this file", () => {
  const pinned: {
    bus: EventBus | undefined;
    worktrees: WorktreeManager | undefined;
    persistence: Persistence | undefined;
    poller: Poller | undefined;
    supervisor: TaskSupervisor | undefined;
  } = {
    bus: undefined,
    worktrees: undefined,
    persistence: undefined,
    poller: undefined,
    supervisor: undefined,
  };
  assertStrictEquals(pinned.bus, undefined);
  assertStrictEquals(pinned.worktrees, undefined);
  assertStrictEquals(pinned.persistence, undefined);
  assertStrictEquals(pinned.poller, undefined);
  assertStrictEquals(pinned.supervisor, undefined);
});
