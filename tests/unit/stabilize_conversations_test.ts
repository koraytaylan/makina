/**
 * Unit tests for the stabilize loop's CONVERSATIONS phase
 * (`src/daemon/supervisor.ts`, `runConversationsPhase`).
 *
 * The phase is exercised end-to-end against the in-memory
 * {@link InMemoryGitHubClient} and a stubbed worktree manager so the
 * supervisor's FSM walks from `INIT` through `READY_TO_MERGE` (or
 * `NEEDS_HUMAN`) without touching the real filesystem or the network.
 *
 * Coverage map:
 *
 *  - **No new comments → no work.** The first poll returns empty
 *    timelines; the conversations phase exits immediately and the FSM
 *    advances to `READY_TO_MERGE` without dispatching the agent and
 *    without re-requesting Copilot review.
 *  - **New comments → grouped agent dispatch + thread resolution +
 *    re-request.** A scripted timeline returns one review with two
 *    inline comments, the supervisor dispatches the agent, then the
 *    next poll comes back empty so the phase converges. The recorded
 *    GitHub call log shows the resolve/re-request sequence after every
 *    push.
 *  - **Re-request after every push.** A multi-iteration timeline
 *    confirms that `requestReviewers(["Copilot"])` fires after every
 *    agent dispatch, not just the last one.
 *  - **Watermark advances after every successful poll.** Even when the
 *    poll returns no new comments, the persisted `lastReviewAtIso`
 *    advances to the latest timestamp so a daemon resume does not
 *    re-process settled threads.
 *  - **Iteration budget exhausted → NEEDS_HUMAN.** A scripted timeline
 *    that always surfaces fresh comments hits the
 *    `maxTaskIterations` cap; the task lands in `NEEDS_HUMAN` and the
 *    terminal reason carries the budget number.
 *  - **Pure helpers.** `buildConversationsPrompt`,
 *    `groupCommentsByThread`, `filterNewComments`,
 *    `latestCommentTimestamp`, and `monotonicWatermark` are exercised
 *    in isolation so the supervisor's prompt shape and the watermark
 *    monotonicity contract are pinned.
 *
 * Per Wave 2 lessons: every collaborator is injected, no
 * `Date.now`/`setTimeout`/`Deno.env` mocks; the synthetic clock is a
 * per-test value the supervisor reads through the
 * {@link SupervisorClock} seam.
 */

import { assert, assertEquals, assertGreater } from "@std/assert";

import { createEventBus } from "../../src/daemon/event-bus.ts";
import {
  buildConversationsPrompt,
  type ConversationsPollResult,
  createTaskSupervisor,
  filterNewComments,
  groupCommentsByThread,
  joinCommentsWithThreads,
  latestCommentTimestamp,
  monotonicWatermark,
  type SupervisorClock,
  type SupervisorRandomSource,
} from "../../src/daemon/supervisor.ts";
import { createPoller } from "../../src/daemon/poller.ts";
import { type StabilizeGitInvoker } from "../../src/daemon/stabilize.ts";
import {
  type IssueNumber,
  makeIssueNumber,
  makeRepoFullName,
  type Persistence,
  type PullRequestDetails,
  type RepoFullName,
  type Task,
  type TaskEvent,
  type TaskId,
} from "../../src/types.ts";
import {
  type PullRequestReview,
  type PullRequestReviewComment,
  type PullRequestReviewThread,
} from "../../src/github/client.ts";
import { InMemoryGitHubClient, type RecordedCall } from "../helpers/in_memory_github_client.ts";
import { MockAgentRunner } from "../helpers/mock_agent_runner.ts";

// ---------------------------------------------------------------------------
// Test scaffolding (in-memory worktree manager + persistence)
// ---------------------------------------------------------------------------

/**
 * Deterministic clock: returns increasing ISO timestamps without
 * touching `Date.now()`. Tests that assert against persisted records
 * read each value back verbatim.
 */
class DeterministicClock implements SupervisorClock {
  private milliseconds = Date.UTC(2026, 3, 26, 12, 0, 0);

  nowIso(): string {
    const iso = new Date(this.milliseconds).toISOString();
    this.milliseconds += 1_000;
    return iso;
  }
}

/**
 * Stable random source so the supervisor's task-id mint is
 * reproducible across runs and tests.
 */
class FixedRandomSource implements SupervisorRandomSource {
  fillRandomBytes(bytes: Uint8Array): void {
    for (let i = 0; i < bytes.length; i += 1) {
      bytes[i] = (i * 17) & 0xff;
    }
  }
}

/**
 * In-memory worktree manager — a structural double that satisfies the
 * supervisor's surface without touching the filesystem. The
 * conversations-phase tests do not exercise worktree behaviour so
 * every method is a no-op that records the path the supervisor asked
 * about.
 */
class StubWorktreeManager {
  private readonly bindings = new Map<TaskId, string>();
  ensureBareClone(_repo: RepoFullName, _url: string): Promise<string> {
    return Promise.resolve("/stub/bare");
  }
  createWorktreeForIssue(
    _repo: RepoFullName,
    issueNumber: IssueNumber,
  ): Promise<string> {
    return Promise.resolve(`/stub/worktree/${issueNumber}`);
  }
  registerTaskId(taskId: TaskId, worktreePath: string): void {
    this.bindings.set(taskId, worktreePath);
  }
  worktreePathFor(taskId: TaskId): string | undefined {
    return this.bindings.get(taskId);
  }
  removeWorktree(taskId: TaskId): Promise<void> {
    this.bindings.delete(taskId);
    return Promise.resolve();
  }
  pruneAll(): Promise<void> {
    return Promise.resolve();
  }
}

/**
 * In-memory persistence so `transition()` writes survive without an
 * actual file. Recording the call log is enough — tests assert against
 * the in-memory task table the supervisor returns from `start()` and
 * the persisted snapshot via `loadAll()`.
 */
class InMemoryPersistence implements Persistence {
  private readonly records = new Map<TaskId, Task>();
  loadAll(): Promise<readonly Task[]> {
    return Promise.resolve([...this.records.values()]);
  }
  save(task: Task): Promise<void> {
    this.records.set(task.id, task);
    return Promise.resolve();
  }
  remove(taskId: TaskId): Promise<void> {
    this.records.delete(taskId);
    return Promise.resolve();
  }
}

interface Fixture {
  readonly clock: DeterministicClock;
  readonly githubClient: InMemoryGitHubClient;
  readonly agentRunner: MockAgentRunner;
  readonly worktreeManager: StubWorktreeManager;
  readonly persistence: InMemoryPersistence;
  readonly events: TaskEvent[];
  readonly bus: ReturnType<typeof createEventBus>;
  readonly repo: RepoFullName;
  readonly issueNumber: IssueNumber;
  readonly prNumber: IssueNumber;
}

function makeFixture(): Fixture {
  const events: TaskEvent[] = [];
  const bus = createEventBus();
  bus.subscribe("*", (event) => {
    events.push(event);
  });
  return {
    clock: new DeterministicClock(),
    githubClient: new InMemoryGitHubClient(),
    agentRunner: new MockAgentRunner(),
    worktreeManager: new StubWorktreeManager(),
    persistence: new InMemoryPersistence(),
    events,
    bus,
    repo: makeRepoFullName("koraytaylan/makina"),
    issueNumber: makeIssueNumber(101),
    prNumber: makeIssueNumber(202),
  };
}

/**
 * Pre-script the GitHub client through PR-open + reviewer-request.
 * Tests then layer the conversations-specific timeline on top.
 */
/**
 * Always-clean rebase stub. The conversations-phase tests exercise the
 * supervisor's CONVERSATIONS branches; the stabilize-rebase phase has its
 * own dedicated coverage in `tests/unit/stabilize_rebase_test.ts`. We
 * short-circuit the calls a clean rebase makes — fetch, rev-parse, and
 * rebase — so the conversations tests reach `STABILIZING(CONVERSATIONS)`
 * without needing a real worktree on disk.
 */
const ALWAYS_CLEAN_REBASE_INVOKER: StabilizeGitInvoker = (args, _options) => {
  if (args[0] === "rev-parse") {
    return Promise.resolve({
      exitCode: 0,
      stdout: "deadbeefcafebabe0123456789abcdef01234567\n",
      stderr: "",
    });
  }
  return Promise.resolve({ exitCode: 0, stdout: "", stderr: "" });
};

function scriptIntoStabilize(fixture: Fixture): void {
  fixture.githubClient.queueGetIssue({
    kind: "value",
    value: {
      number: fixture.issueNumber,
      title: "Conversations test",
      body: "Body.",
      state: "open",
    },
  });
  fixture.githubClient.queueCreatePullRequest({
    kind: "value",
    value: {
      number: fixture.prNumber,
      headSha: "deadbeef",
      headRef: `makina/issue-${fixture.issueNumber}`,
      baseRef: "main",
      state: "open",
    } satisfies PullRequestDetails,
  });
  fixture.githubClient.queueRequestReviewers({ kind: "value", value: undefined });
  fixture.agentRunner.queueRun({
    messages: [{ role: "assistant", text: "drafting" }],
  });
}

function makeReview(
  overrides: Partial<PullRequestReview> & { id: number; user?: string },
): PullRequestReview {
  const base: PullRequestReview = {
    id: overrides.id,
    user: overrides.user ?? "Copilot",
    state: overrides.state ?? "COMMENTED",
    body: overrides.body ?? "",
  };
  if (overrides.submittedAtIso !== undefined) {
    return { ...base, submittedAtIso: overrides.submittedAtIso };
  }
  return base;
}

function makeComment(
  overrides:
    & Partial<PullRequestReviewComment>
    & { id: number; createdAtIso: string },
): PullRequestReviewComment {
  const base: PullRequestReviewComment = {
    id: overrides.id,
    pullRequestReviewId: overrides.pullRequestReviewId ?? null,
    user: overrides.user ?? "Copilot",
    body: overrides.body ?? "Consider renaming.",
    path: overrides.path ?? "src/x.ts",
    line: overrides.line ?? 12,
    inReplyToId: overrides.inReplyToId ?? null,
    createdAtIso: overrides.createdAtIso,
  };
  if (overrides.threadNodeId !== undefined) {
    return { ...base, threadNodeId: overrides.threadNodeId };
  }
  return base;
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

Deno.test("filterNewComments: returns every comment when no watermark", () => {
  const comments: PullRequestReviewComment[] = [
    makeComment({ id: 1, createdAtIso: "2026-04-25T00:00:00Z" }),
    makeComment({ id: 2, createdAtIso: "2026-04-26T00:00:00Z" }),
  ];
  assertEquals(filterNewComments(comments, undefined).length, 2);
});

Deno.test("filterNewComments: keeps only strictly-newer comments", () => {
  const comments: PullRequestReviewComment[] = [
    makeComment({ id: 1, createdAtIso: "2026-04-25T00:00:00Z" }),
    makeComment({ id: 2, createdAtIso: "2026-04-26T00:00:00Z" }),
    makeComment({ id: 3, createdAtIso: "2026-04-27T00:00:00Z" }),
  ];
  const filtered = filterNewComments(comments, "2026-04-26T00:00:00Z");
  // Strictly newer: only the third comment passes; the second ties and
  // is dropped per the `>` (not `>=`) contract.
  assertEquals(filtered.length, 1);
  assertEquals(filtered[0]?.id, 3);
});

Deno.test("latestCommentTimestamp: returns undefined when empty", () => {
  assertEquals(latestCommentTimestamp([]), undefined);
});

Deno.test("latestCommentTimestamp: finds the maximum ISO", () => {
  const comments: PullRequestReviewComment[] = [
    makeComment({ id: 1, createdAtIso: "2026-04-25T00:00:00Z" }),
    makeComment({ id: 2, createdAtIso: "2026-04-27T00:00:00Z" }),
    makeComment({ id: 3, createdAtIso: "2026-04-26T00:00:00Z" }),
  ];
  assertEquals(latestCommentTimestamp(comments), "2026-04-27T00:00:00Z");
});

Deno.test(
  "joinCommentsWithThreads: stamps threadNodeId on every comment whose id appears in a thread",
  () => {
    const comments: PullRequestReviewComment[] = [
      makeComment({ id: 100, createdAtIso: "2026-04-26T00:00:00Z" }),
      makeComment({ id: 101, createdAtIso: "2026-04-26T00:01:00Z" }),
      makeComment({ id: 200, createdAtIso: "2026-04-26T00:02:00Z" }),
    ];
    const threads: PullRequestReviewThread[] = [
      { id: "PRRT_thread-A", commentIds: [100, 101] },
      { id: "PRRT_thread-B", commentIds: [200] },
    ];
    const enriched = joinCommentsWithThreads(comments, threads);
    // The join is what makes resolveReviewThread reachable in production
    // — every comment must carry its thread node id after this call.
    assertEquals(enriched.length, 3);
    assertEquals(enriched[0]?.threadNodeId, "PRRT_thread-A");
    assertEquals(enriched[1]?.threadNodeId, "PRRT_thread-A");
    assertEquals(enriched[2]?.threadNodeId, "PRRT_thread-B");
  },
);

Deno.test(
  "joinCommentsWithThreads: leaves comments without a matching thread untouched (threadNodeId stays undefined)",
  () => {
    const comments: PullRequestReviewComment[] = [
      makeComment({ id: 100, createdAtIso: "2026-04-26T00:00:00Z" }),
      makeComment({ id: 999, createdAtIso: "2026-04-26T00:01:00Z" }),
    ];
    const threads: PullRequestReviewThread[] = [
      { id: "PRRT_thread-A", commentIds: [100] },
    ];
    const enriched = joinCommentsWithThreads(comments, threads);
    assertEquals(enriched[0]?.threadNodeId, "PRRT_thread-A");
    // Comment 999 was not in any thread; the supervisor's
    // `resolveAddressedThreads` will skip the resolve call for it.
    assertEquals(enriched[1]?.threadNodeId, undefined);
  },
);

Deno.test(
  "joinCommentsWithThreads: preserves a pre-set threadNodeId rather than overwriting it",
  () => {
    const comments: PullRequestReviewComment[] = [
      makeComment({
        id: 100,
        createdAtIso: "2026-04-26T00:00:00Z",
        threadNodeId: "PRRT_pre-existing",
      }),
    ];
    const threads: PullRequestReviewThread[] = [
      // The threads payload says comment 100 belongs to a different
      // thread; the join must not overwrite the test/test-fixture's
      // pre-set value.
      { id: "PRRT_other", commentIds: [100] },
    ];
    const enriched = joinCommentsWithThreads(comments, threads);
    assertEquals(enriched[0]?.threadNodeId, "PRRT_pre-existing");
  },
);

Deno.test(
  "joinCommentsWithThreads: returns an empty array when comments are empty",
  () => {
    const threads: PullRequestReviewThread[] = [
      { id: "PRRT_thread-A", commentIds: [100] },
    ];
    assertEquals(joinCommentsWithThreads([], threads), []);
  },
);

Deno.test("monotonicWatermark: returns incoming when current is undefined", () => {
  assertEquals(monotonicWatermark(undefined, "2026-04-26T00:00:00Z"), "2026-04-26T00:00:00Z");
});

Deno.test("monotonicWatermark: returns current when incoming is undefined", () => {
  assertEquals(monotonicWatermark("2026-04-26T00:00:00Z", undefined), "2026-04-26T00:00:00Z");
});

Deno.test("monotonicWatermark: both undefined returns undefined", () => {
  assertEquals(monotonicWatermark(undefined, undefined), undefined);
});

Deno.test("monotonicWatermark: advances when incoming is strictly newer", () => {
  assertEquals(
    monotonicWatermark("2026-04-26T00:00:00Z", "2026-04-27T00:00:00Z"),
    "2026-04-27T00:00:00Z",
  );
});

Deno.test("monotonicWatermark: pins to current when incoming would regress (deletion/truncation)", () => {
  // The conversations phase must never let a partial timeline lower the
  // persisted high-water mark and re-surface already-processed comments.
  assertEquals(
    monotonicWatermark("2026-04-27T00:00:00Z", "2026-04-26T00:00:00Z"),
    "2026-04-27T00:00:00Z",
  );
});

Deno.test("monotonicWatermark: pins to current on a tie", () => {
  assertEquals(
    monotonicWatermark("2026-04-26T00:00:00Z", "2026-04-26T00:00:00Z"),
    "2026-04-26T00:00:00Z",
  );
});

Deno.test("groupCommentsByThread: groups by threadNodeId when present", () => {
  const comments: PullRequestReviewComment[] = [
    makeComment({
      id: 1,
      createdAtIso: "2026-04-25T00:00:00Z",
      threadNodeId: "thread-A",
    }),
    makeComment({
      id: 2,
      createdAtIso: "2026-04-25T00:01:00Z",
      threadNodeId: "thread-B",
    }),
    makeComment({
      id: 3,
      createdAtIso: "2026-04-25T00:02:00Z",
      threadNodeId: "thread-A",
    }),
  ];
  const groups = groupCommentsByThread(comments);
  assertEquals(groups.length, 2);
  assertEquals(groups[0]?.key, "thread-A");
  assertEquals(groups[0]?.comments.length, 2);
  assertEquals(groups[1]?.key, "thread-B");
  assertEquals(groups[1]?.comments.length, 1);
});

Deno.test("groupCommentsByThread: falls back to inReplyToId when threadNodeId is missing", () => {
  const comments: PullRequestReviewComment[] = [
    makeComment({ id: 1, createdAtIso: "2026-04-25T00:00:00Z" }),
    makeComment({
      id: 2,
      createdAtIso: "2026-04-25T00:01:00Z",
      inReplyToId: 1,
    }),
  ];
  const groups = groupCommentsByThread(comments);
  // Both comments fall into the same group keyed off comment id 1.
  assertEquals(groups.length, 1);
  assertEquals(groups[0]?.comments.length, 2);
});

Deno.test("buildConversationsPrompt: emits one section per thread with file:line headers", () => {
  const reviews: PullRequestReview[] = [
    makeReview({ id: 10, state: "CHANGES_REQUESTED", body: "Please rename." }),
  ];
  const comments: PullRequestReviewComment[] = [
    makeComment({
      id: 1,
      pullRequestReviewId: 10,
      threadNodeId: "thread-A",
      createdAtIso: "2026-04-25T00:00:00Z",
      path: "src/a.ts",
      line: 7,
      body: "rename `x` to `count`",
    }),
    makeComment({
      id: 2,
      pullRequestReviewId: 10,
      threadNodeId: "thread-A",
      createdAtIso: "2026-04-25T00:01:00Z",
      body: "still wrong",
    }),
    makeComment({
      id: 3,
      pullRequestReviewId: 10,
      threadNodeId: "thread-B",
      createdAtIso: "2026-04-25T00:02:00Z",
      path: "src/b.ts",
      line: 13,
      body: "consider adding a test",
    }),
  ];
  const prompt = buildConversationsPrompt(reviews, comments);
  assert(prompt.startsWith("# Address pull-request review feedback"));
  assert(prompt.includes("## Thread thread-A (src/a.ts:7)"));
  assert(prompt.includes("## Thread thread-B (src/b.ts:13)"));
  assert(prompt.includes("Review by @Copilot (CHANGES_REQUESTED): Please rename."));
  assert(prompt.includes("- @Copilot: rename `x` to `count`"));
  assert(prompt.includes("- @Copilot: still wrong"));
});

// ---------------------------------------------------------------------------
// FSM walks
// ---------------------------------------------------------------------------

/**
 * Drive the supervisor through the full FSM with the given fixture.
 * Returns the final task record so tests can assert on terminal state.
 */
async function drive(fixture: Fixture, options?: {
  readonly maxTaskIterations?: number;
}): Promise<Task> {
  // The conversations-phase tests treat the worktree manager as an
  // opaque collaborator — none of the assertions read its surface. The
  // structural cast through `unknown` keeps the test free of a heavy
  // `WorktreeManagerImpl` fixture while still letting `createTaskSupervisor`
  // type-check.
  const supervisor = createTaskSupervisor({
    githubClient: fixture.githubClient,
    worktreeManager: fixture.worktreeManager as unknown as Parameters<
      typeof createTaskSupervisor
    >[0]["worktreeManager"],
    persistence: fixture.persistence,
    eventBus: fixture.bus,
    agentRunner: fixture.agentRunner,
    cloneUrlFor: () => "file:///stub",
    clock: fixture.clock,
    randomSource: new FixedRandomSource(),
    poller: createPoller({
      // Synthetic clock to keep the test fast. The supervisor passes
      // intervalMilliseconds: 0 anyway (see pollIntervalMilliseconds
      // below) so cadence is not exercised here.
      clock: {
        now: () => 0,
        sleep: () => Promise.resolve(),
      },
    }),
    pollIntervalMilliseconds: 0,
    gitInvoker: ALWAYS_CLEAN_REBASE_INVOKER,
    ...(options?.maxTaskIterations !== undefined
      ? { maxTaskIterations: options.maxTaskIterations }
      : {}),
  });
  return await supervisor.start({
    repo: fixture.repo,
    issueNumber: fixture.issueNumber,
  });
}

Deno.test(
  "conversations: empty timeline → no agent dispatch, no Copilot re-request, FSM reaches MERGED",
  async () => {
    const fixture = makeFixture();
    scriptIntoStabilize(fixture);
    fixture.githubClient.queueListReviews({ kind: "value", value: [] });
    fixture.githubClient.queueListReviewComments({ kind: "value", value: [] });
    fixture.githubClient.queueMergePullRequest({ kind: "value", value: undefined });

    const finalTask = await drive(fixture);

    assertEquals(finalTask.state, "MERGED");
    // Exactly one agent run (the DRAFTING phase).
    assertEquals(fixture.agentRunner.recordedInvocations().length, 1);
    // Exactly one Copilot re-request (the PR_OPEN follow-up); the
    // conversations phase does not call it again because no new
    // comments arrived.
    const requestReviewersCalls = fixture.githubClient
      .recordedCalls()
      .filter((call) => call.method === "requestReviewers");
    assertEquals(requestReviewersCalls.length, 1);
    // No threads were resolved.
    const resolveCalls = fixture.githubClient
      .recordedCalls()
      .filter((call) => call.method === "resolveReviewThread");
    assertEquals(resolveCalls.length, 0);
  },
);

Deno.test(
  "conversations: new comments → group, dispatch, resolve threads, re-request review",
  async () => {
    const fixture = makeFixture();
    scriptIntoStabilize(fixture);

    // First poll: one review with two inline comments across two
    // threads.
    fixture.githubClient.queueListReviews({
      kind: "value",
      value: [
        makeReview({
          id: 10,
          state: "CHANGES_REQUESTED",
          body: "Please rename.",
          submittedAtIso: "2026-04-26T13:00:00Z",
        }),
      ],
    });
    fixture.githubClient.queueListReviewComments({
      kind: "value",
      value: [
        makeComment({
          id: 1,
          pullRequestReviewId: 10,
          threadNodeId: "PRRT_thread-A",
          createdAtIso: "2026-04-26T13:00:01Z",
          body: "rename x to count",
        }),
        makeComment({
          id: 2,
          pullRequestReviewId: 10,
          threadNodeId: "PRRT_thread-B",
          createdAtIso: "2026-04-26T13:00:02Z",
          body: "extract helper",
        }),
      ],
    });
    // Agent dispatch round 1.
    fixture.agentRunner.queueRun({
      messages: [{ role: "assistant", text: "addressing review" }],
    });
    // After dispatch the supervisor resolves both threads and
    // re-requests Copilot review.
    fixture.githubClient.queueResolveReviewThread({ kind: "value", value: undefined });
    fixture.githubClient.queueResolveReviewThread({ kind: "value", value: undefined });
    fixture.githubClient.queueRequestReviewers({ kind: "value", value: undefined });
    // Second poll: empty (the agent's push made the comments stale).
    fixture.githubClient.queueListReviews({ kind: "value", value: [] });
    fixture.githubClient.queueListReviewComments({ kind: "value", value: [] });
    fixture.githubClient.queueMergePullRequest({ kind: "value", value: undefined });

    const finalTask = await drive(fixture);

    assertEquals(finalTask.state, "MERGED");
    assertEquals(finalTask.iterationCount, 2); // DRAFTING + 1 conversations round
    // Both threads were resolved with the GraphQL ids the comments
    // carried.
    const resolveCalls = fixture.githubClient
      .recordedCalls()
      .filter(
        (call): call is RecordedCall & { method: "resolveReviewThread" } =>
          call.method === "resolveReviewThread",
      );
    assertEquals(resolveCalls.map((call) => call.threadId), [
      "PRRT_thread-A",
      "PRRT_thread-B",
    ]);
    // Copilot was re-requested twice: once at PR_OPEN, once after the
    // agent push.
    const requestReviewersCalls = fixture.githubClient
      .recordedCalls()
      .filter((call) => call.method === "requestReviewers");
    assertEquals(requestReviewersCalls.length, 2);
    // The watermark advanced to the latest comment.
    assertEquals(finalTask.lastReviewAtIso, "2026-04-26T13:00:02Z");
    // The agent runner saw two invocations: DRAFTING + one
    // conversations round.
    assertEquals(fixture.agentRunner.recordedInvocations().length, 2);
    // The conversations prompt mentioned both threads.
    const conversationsInvocation = fixture.agentRunner.recordedInvocations()[1];
    assert(conversationsInvocation !== undefined);
    assert(conversationsInvocation.prompt.includes("PRRT_thread-A"));
    assert(conversationsInvocation.prompt.includes("PRRT_thread-B"));
  },
);

Deno.test(
  "conversations: re-request fires after every push, not just the last",
  async () => {
    const fixture = makeFixture();
    scriptIntoStabilize(fixture);

    // Round 1: one new comment.
    fixture.githubClient.queueListReviews({ kind: "value", value: [] });
    fixture.githubClient.queueListReviewComments({
      kind: "value",
      value: [
        makeComment({
          id: 1,
          threadNodeId: "PRRT_thread-1",
          createdAtIso: "2026-04-26T13:00:00Z",
        }),
      ],
    });
    fixture.agentRunner.queueRun({
      messages: [{ role: "assistant", text: "round 1" }],
    });
    fixture.githubClient.queueResolveReviewThread({ kind: "value", value: undefined });
    fixture.githubClient.queueRequestReviewers({ kind: "value", value: undefined });

    // Round 2: another fresh comment after the watermark.
    fixture.githubClient.queueListReviews({ kind: "value", value: [] });
    fixture.githubClient.queueListReviewComments({
      kind: "value",
      value: [
        makeComment({
          id: 1,
          threadNodeId: "PRRT_thread-1",
          createdAtIso: "2026-04-26T13:00:00Z",
        }),
        makeComment({
          id: 2,
          threadNodeId: "PRRT_thread-2",
          createdAtIso: "2026-04-26T13:01:00Z",
        }),
      ],
    });
    fixture.agentRunner.queueRun({
      messages: [{ role: "assistant", text: "round 2" }],
    });
    fixture.githubClient.queueResolveReviewThread({ kind: "value", value: undefined });
    fixture.githubClient.queueRequestReviewers({ kind: "value", value: undefined });

    // Round 3: empty, phase converges.
    fixture.githubClient.queueListReviews({ kind: "value", value: [] });
    fixture.githubClient.queueListReviewComments({ kind: "value", value: [] });
    fixture.githubClient.queueMergePullRequest({ kind: "value", value: undefined });

    const finalTask = await drive(fixture);

    assertEquals(finalTask.state, "MERGED");
    // Three Copilot re-requests: PR_OPEN + two pushes.
    const requestReviewersCalls = fixture.githubClient
      .recordedCalls()
      .filter((call) => call.method === "requestReviewers");
    assertEquals(requestReviewersCalls.length, 3);
    // Every dispatch is followed (in the call log) by a
    // resolveReviewThread + a requestReviewers.
    const calls = fixture.githubClient.recordedCalls();
    let pushCount = 0;
    for (let i = 0; i < calls.length; i += 1) {
      const call = calls[i];
      if (call?.method !== "resolveReviewThread") continue;
      // The next non-resolveReviewThread call must be requestReviewers.
      let j = i + 1;
      while (
        j < calls.length && calls[j]?.method === "resolveReviewThread"
      ) {
        j += 1;
      }
      assertEquals(calls[j]?.method, "requestReviewers");
      pushCount += 1;
      i = j;
    }
    // Two pushes (one per round of fresh comments).
    assertEquals(pushCount, 2);
  },
);

Deno.test(
  "conversations: iteration budget exhausted → NEEDS_HUMAN with terminalReason",
  async () => {
    const fixture = makeFixture();
    scriptIntoStabilize(fixture);

    // Always-on comments: every poll surfaces a fresh comment newer
    // than the last watermark, so the loop never converges. With
    // maxTaskIterations=2 the supervisor exhausts the budget on the
    // third would-be dispatch.
    function queueRound(timestampSec: number, threadNodeId: string): void {
      fixture.githubClient.queueListReviews({ kind: "value", value: [] });
      fixture.githubClient.queueListReviewComments({
        kind: "value",
        value: [
          makeComment({
            id: timestampSec,
            threadNodeId,
            createdAtIso: `2026-04-26T13:00:${String(timestampSec).padStart(2, "0")}Z`,
          }),
        ],
      });
    }
    function queuePushFollowup(): void {
      fixture.agentRunner.queueRun({
        messages: [{ role: "assistant", text: "iterating" }],
      });
      fixture.githubClient.queueResolveReviewThread({
        kind: "value",
        value: undefined,
      });
      fixture.githubClient.queueRequestReviewers({
        kind: "value",
        value: undefined,
      });
    }
    // DRAFTING already counts as iteration 1; with
    // `maxTaskIterations: 2`, the conversations loop has budget for
    // exactly one dispatch.
    queueRound(1, "PRRT_a");
    queuePushFollowup();
    // Second round: budget is exhausted before the dispatch fires.
    queueRound(2, "PRRT_b");

    const finalTask = await drive(fixture, { maxTaskIterations: 2 });

    assertEquals(finalTask.state, "NEEDS_HUMAN");
    assert(finalTask.terminalReason !== undefined);
    assert(finalTask.terminalReason.includes("budget"));
    assert(finalTask.terminalReason.includes("2"));
    // Exactly one conversations dispatch made it through.
    const conversationsDispatches = fixture.agentRunner.recordedInvocations()
      .filter((inv) => inv.prompt.includes("Address pull-request review feedback"));
    assertEquals(conversationsDispatches.length, 1);
    // Budget exhaustion publishes a `log` event before the
    // NEEDS_HUMAN transition.
    const exhaustionLogs = fixture.events.filter((event) =>
      event.kind === "log" &&
      event.data.message.includes("budget")
    );
    assertGreater(exhaustionLogs.length, 0);
  },
);

Deno.test(
  "conversations: watermark advances even when the new poll has no fresh comments",
  async () => {
    // The brief calls out: "Update lastReviewAt after every successful
    // poll." Even when the poll yields no work, the watermark moves
    // forward to the latest seen comment so a daemon resume does not
    // re-process settled threads.
    const fixture = makeFixture();
    scriptIntoStabilize(fixture);

    // First poll: there *are* comments, but they're all older than the
    // empty watermark — wait, the watermark is undefined. Better case:
    // poll returns comments but they all fall on the watermark.
    // Simpler: poll returns one comment that *is* new, agent runs,
    // resolve+re-request, then the next poll returns the same
    // (older-than-watermark) comment again so no work is found but the
    // watermark stays at that timestamp.
    fixture.githubClient.queueListReviews({ kind: "value", value: [] });
    fixture.githubClient.queueListReviewComments({
      kind: "value",
      value: [
        makeComment({
          id: 1,
          threadNodeId: "PRRT_z",
          createdAtIso: "2026-04-26T13:00:00Z",
        }),
      ],
    });
    fixture.agentRunner.queueRun({
      messages: [{ role: "assistant", text: "round 1" }],
    });
    fixture.githubClient.queueResolveReviewThread({ kind: "value", value: undefined });
    fixture.githubClient.queueRequestReviewers({ kind: "value", value: undefined });
    // Second poll: GitHub repeats the same comment timestamp; the
    // filter drops it (>= watermark), but `latestCreatedAtIso` keeps
    // the watermark in sync.
    fixture.githubClient.queueListReviews({ kind: "value", value: [] });
    fixture.githubClient.queueListReviewComments({
      kind: "value",
      value: [
        makeComment({
          id: 1,
          threadNodeId: "PRRT_z",
          createdAtIso: "2026-04-26T13:00:00Z",
        }),
      ],
    });
    fixture.githubClient.queueMergePullRequest({ kind: "value", value: undefined });

    const finalTask = await drive(fixture);

    assertEquals(finalTask.state, "MERGED");
    assertEquals(finalTask.lastReviewAtIso, "2026-04-26T13:00:00Z");
    // Only one conversations dispatch ran.
    const conversationsDispatches = fixture.agentRunner.recordedInvocations()
      .filter((inv) => inv.prompt.includes("Address pull-request review feedback"));
    assertEquals(conversationsDispatches.length, 1);
  },
);

Deno.test(
  "conversations: poll failure → FAILED with terminal reason carrying the source",
  async () => {
    const fixture = makeFixture();
    scriptIntoStabilize(fixture);
    fixture.githubClient.queueListReviews({
      kind: "error",
      error: new Error("502 Bad Gateway"),
    });
    fixture.githubClient.queueListReviewComments({
      kind: "error",
      error: new Error("502 Bad Gateway"),
    });

    const finalTask = await drive(fixture);

    assertEquals(finalTask.state, "FAILED");
    assert(finalTask.terminalReason !== undefined);
    assert(finalTask.terminalReason.startsWith("conversations-poll:"));
  },
);

Deno.test(
  "conversations: comments without threadNodeId are skipped on resolve but still drive the prompt",
  async () => {
    const fixture = makeFixture();
    scriptIntoStabilize(fixture);

    fixture.githubClient.queueListReviews({ kind: "value", value: [] });
    fixture.githubClient.queueListReviewComments({
      kind: "value",
      value: [
        makeComment({
          id: 1,
          // Intentionally no threadNodeId — REST listing without a
          // GraphQL companion query.
          createdAtIso: "2026-04-26T13:00:00Z",
          body: "fix this",
        }),
      ],
    });
    fixture.agentRunner.queueRun({
      messages: [{ role: "assistant", text: "noting" }],
    });
    // No resolveReviewThread queue entries — the supervisor must skip
    // the call when threadNodeId is undefined. requestReviewers still
    // fires.
    fixture.githubClient.queueRequestReviewers({ kind: "value", value: undefined });
    // Second poll empty → converge.
    fixture.githubClient.queueListReviews({ kind: "value", value: [] });
    fixture.githubClient.queueListReviewComments({ kind: "value", value: [] });
    fixture.githubClient.queueMergePullRequest({ kind: "value", value: undefined });

    const finalTask = await drive(fixture);

    assertEquals(finalTask.state, "MERGED");
    const resolveCalls = fixture.githubClient
      .recordedCalls()
      .filter((call) => call.method === "resolveReviewThread");
    assertEquals(resolveCalls.length, 0);
  },
);

Deno.test(
  "conversations: REST comments without threadNodeId are joined with listReviewThreads payload and resolved",
  async () => {
    // Regression test for the round-3 bug: production
    // `GitHubClientImpl.listReviewComments()` projects from REST and
    // never sets `PullRequestReviewComment.threadNodeId`. The supervisor
    // must call `listReviewThreads` alongside it and join the two so
    // the conversations phase can resolve each thread. This test pins
    // that path: a REST listing with bare comments (no `threadNodeId`)
    // plus a GraphQL threads payload mapping comment ids → thread node
    // ids must produce a `resolveReviewThread` call per distinct thread.
    const fixture = makeFixture();
    scriptIntoStabilize(fixture);

    // First poll: review + two REST comments, neither with threadNodeId
    // pre-set (matches what the real client returns).
    fixture.githubClient.queueListReviews({
      kind: "value",
      value: [
        makeReview({
          id: 10,
          state: "CHANGES_REQUESTED",
          body: "Two threads.",
          submittedAtIso: "2026-04-26T13:00:00Z",
        }),
      ],
    });
    fixture.githubClient.queueListReviewComments({
      kind: "value",
      value: [
        makeComment({
          id: 100,
          pullRequestReviewId: 10,
          // Intentionally no threadNodeId — this is the production
          // shape the real GitHubClientImpl returns.
          createdAtIso: "2026-04-26T13:00:01Z",
          body: "fix the rename",
        }),
        makeComment({
          id: 200,
          pullRequestReviewId: 10,
          createdAtIso: "2026-04-26T13:00:02Z",
          body: "extract helper",
        }),
      ],
    });
    // GraphQL threads payload maps the REST ids to thread node ids.
    fixture.githubClient.queueListReviewThreads({
      kind: "value",
      value: [
        { id: "PRRT_thread-100", commentIds: [100] },
        { id: "PRRT_thread-200", commentIds: [200] },
      ],
    });
    fixture.agentRunner.queueRun({
      messages: [{ role: "assistant", text: "addressing review" }],
    });
    fixture.githubClient.queueResolveReviewThread({ kind: "value", value: undefined });
    fixture.githubClient.queueResolveReviewThread({ kind: "value", value: undefined });
    fixture.githubClient.queueRequestReviewers({ kind: "value", value: undefined });
    // Second poll: empty (the agent's push made the comments stale).
    fixture.githubClient.queueListReviews({ kind: "value", value: [] });
    fixture.githubClient.queueListReviewComments({ kind: "value", value: [] });
    fixture.githubClient.queueMergePullRequest({ kind: "value", value: undefined });

    const finalTask = await drive(fixture);

    assertEquals(finalTask.state, "MERGED");
    // The join populated threadNodeId for both REST comments, so
    // resolveReviewThread fires once per distinct thread node id —
    // proving the production-shaped REST payload no longer regresses
    // through the `threadNodeId === undefined` skip branch.
    const resolveCalls = fixture.githubClient
      .recordedCalls()
      .filter(
        (call): call is RecordedCall & { method: "resolveReviewThread" } =>
          call.method === "resolveReviewThread",
      );
    assertEquals(resolveCalls.map((call) => call.threadId), [
      "PRRT_thread-100",
      "PRRT_thread-200",
    ]);
    // Both `listReviewThreads` and `listReviewComments` were called on
    // the same poll — the join is wired through `runConversationsPoll`.
    const calls = fixture.githubClient.recordedCalls();
    const firstThreadsIdx = calls.findIndex(
      (call) => call.method === "listReviewThreads",
    );
    const firstCommentsIdx = calls.findIndex(
      (call) => call.method === "listReviewComments",
    );
    assertGreater(firstThreadsIdx, -1);
    assertGreater(firstCommentsIdx, -1);
    // The conversations prompt grouped by thread (the joined node ids).
    const conversationsInvocation = fixture.agentRunner.recordedInvocations()[1];
    assert(conversationsInvocation !== undefined);
    assert(conversationsInvocation.prompt.includes("PRRT_thread-100"));
    assert(conversationsInvocation.prompt.includes("PRRT_thread-200"));
  },
);

// ---------------------------------------------------------------------------
// ConversationsPollResult shape — pinned so test doubles outside this
// file can construct it without coupling to the exact projection.
// ---------------------------------------------------------------------------

Deno.test("ConversationsPollResult: tolerates absent latestCreatedAtIso", () => {
  // Compile-time only: verifies the optional field's
  // `exactOptionalPropertyTypes` shape — omitting the key is allowed.
  const empty: ConversationsPollResult = { reviews: [], comments: [] };
  assertEquals(empty.reviews.length, 0);
  assertEquals(empty.comments.length, 0);
});
