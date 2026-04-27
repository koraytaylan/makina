/**
 * Unit tests for the stabilize loop's CI sub-phase
 * (`src/daemon/supervisor.ts` — issue #16).
 *
 * The CI phase is wired through {@link createTaskSupervisor}; these tests
 * drive the supervisor end-to-end against the {@link InMemoryGitHubClient}
 * double and the {@link MockAgentRunner} double, exercising scripted
 * timelines:
 *
 *  - **Green on first poll** — `getCombinedStatus` returns `success`
 *    immediately; the CI phase completes without dispatching the agent
 *    and the task continues to {@link "READY_TO_MERGE"} → {@link "MERGED"}.
 *  - **Red then fixed** — `failure` first, agent fix runs, second poll
 *    returns `success` on the new head SHA; the task reaches
 *    {@link "MERGED"} after exactly one extra agent iteration.
 *  - **Perpetually red** — every poll returns `failure`; the supervisor
 *    burns the {@link MAX_TASK_ITERATIONS} budget and lands the task in
 *    {@link "NEEDS_HUMAN"} with the iteration-cap reason.
 *  - **Log-budget trimming** — {@link trimLogToBudget} preserves whole
 *    trailing lines when possible and falls back to a hard byte slice
 *    when a single line exceeds the budget.
 *  - **Prompt shape** — {@link buildCiAgentPrompt} renders the issue
 *    header, failing-check list, and per-job log fences in the order
 *    the agent expects.
 *
 * The poller is replaced with a synchronous double that calls the
 * fetcher inline so each test runs as fast as the microtask queue can
 * drain — no real polling cadence, no synthetic clock plumbing.
 */

import { assert, assertEquals, assertStringIncludes } from "@std/assert";

import {
  branchNameFor,
  buildCiAgentPrompt,
  createTaskSupervisor,
  DEFAULT_BASE_BRANCH,
  type SupervisorClock,
  type SupervisorRandomSource,
  trimLogToBudget,
} from "../../src/daemon/supervisor.ts";
import { createEventBus } from "../../src/daemon/event-bus.ts";
import {
  type IssueNumber,
  makeIssueNumber,
  makeRepoFullName,
  type Persistence,
  type Task,
  type TaskEvent,
  type TaskId,
} from "../../src/types.ts";
import type { CheckRunSummary, StabilizeGitHubClient } from "../../src/github/client.ts";
import type { PollerImpl } from "../../src/daemon/poller.ts";
import { InMemoryGitHubClient } from "../helpers/in_memory_github_client.ts";
import { MockAgentRunner } from "../helpers/mock_agent_runner.ts";
import { STABILIZE_CI_LOG_BUDGET_BYTES } from "../../src/constants.ts";

// ---------------------------------------------------------------------------
// Test rig
// ---------------------------------------------------------------------------

class FixedClock implements SupervisorClock {
  private milliseconds = Date.UTC(2026, 3, 26, 12, 0, 0);
  nowIso(): string {
    const iso = new Date(this.milliseconds).toISOString();
    this.milliseconds += 1_000;
    return iso;
  }
}

class FixedRandomSource implements SupervisorRandomSource {
  fillRandomBytes(bytes: Uint8Array): void {
    for (let i = 0; i < bytes.length; i += 1) {
      bytes[i] = (i * 13) & 0xff;
    }
  }
}

/**
 * In-memory persistence double that records every save into a list so
 * the test can assert the on-disk timeline. No global mutation; every
 * test instantiates its own.
 */
class InMemoryPersistence implements Persistence {
  readonly saves: Task[] = [];
  loadAll(): Promise<readonly Task[]> {
    // The CI tests never round-trip through `loadAll`; the supervisor
    // hydrates from `start()` exclusively.
    return Promise.resolve([]);
  }
  save(task: Task): Promise<void> {
    this.saves.push(structuredClone(task));
    return Promise.resolve();
  }
  remove(_taskId: TaskId): Promise<void> {
    return Promise.resolve();
  }
}

/**
 * Synchronous poller double: invokes the fetcher in a loop, forwarding
 * every result through `onResult` and every rejection through
 * `onError` (when present), with no inter-tick spacing. The CI loop
 * settles on the first non-pending tick, so this is a faithful
 * stand-in for the real poller's "cadence" while keeping tests fast.
 */
function syncPoller(): PollerImpl {
  return {
    poll<TResult>(args: {
      readonly taskId: TaskId;
      readonly intervalMilliseconds: number;
      readonly fetcher: () => Promise<TResult>;
      readonly onResult: (result: TResult) => void;
      readonly onError?: (error: unknown) => void;
      readonly signal?: AbortSignal;
    }): { cancel(): void } {
      let cancelled = false;
      (async () => {
        while (!cancelled) {
          try {
            const result = await args.fetcher();
            if (cancelled) return;
            args.onResult(result);
          } catch (error) {
            if (cancelled) return;
            args.onError?.(error);
            // No backoff; the test's scripted timeline either cancels
            // here or feeds a deterministic next reply.
          }
        }
      })();
      return {
        cancel(): void {
          cancelled = true;
        },
      };
    },
  };
}

interface CiTestRig {
  readonly githubClient: InMemoryGitHubClient;
  readonly agentRunner: MockAgentRunner;
  readonly persistence: InMemoryPersistence;
  readonly events: TaskEvent[];
  readonly bus: ReturnType<typeof createEventBus>;
  readonly issueNumber: IssueNumber;
  readonly headShas: string[];
}

function makeRig(): CiTestRig {
  return {
    githubClient: new InMemoryGitHubClient(),
    agentRunner: new MockAgentRunner(),
    persistence: new InMemoryPersistence(),
    events: [],
    bus: createEventBus(),
    issueNumber: makeIssueNumber(42),
    headShas: ["sha-initial"],
  };
}

interface FakeWorktreeManager {
  ensureBareClone(repo: unknown, url: string): Promise<string>;
  createWorktreeForIssue(repo: unknown, issueNumber: IssueNumber): Promise<string>;
  registerTaskId(taskId: TaskId, path: string): void;
  removeWorktree(taskId: TaskId): Promise<void>;
}

/**
 * Worktree manager double — the CI phase only reads
 * `task.worktreePath`, so the double returns a constant path, never
 * touches the filesystem, and survives every test without any
 * cleanup. The supervisor calls `registerTaskId` and `ensureBareClone`
 * on the way through; both are no-ops here.
 */
function fakeWorktreeManager(): FakeWorktreeManager {
  return {
    ensureBareClone(): Promise<string> {
      return Promise.resolve("/tmp/fake-bare");
    },
    createWorktreeForIssue(): Promise<string> {
      return Promise.resolve("/tmp/fake-worktree");
    },
    registerTaskId(): void {},
    removeWorktree(): Promise<void> {
      return Promise.resolve();
    },
  };
}

function recordEvents(rig: CiTestRig): { unsubscribe(): void } {
  return rig.bus.subscribe("*", (event) => {
    rig.events.push(event);
  });
}

function scriptHappyPathPrefix(rig: CiTestRig): void {
  // Walk the FSM through INIT → CLONING_WORKTREE → DRAFTING → COMMITTING
  // → PUSHING → PR_OPEN → STABILIZING. The CI phase begins after.
  rig.githubClient.queueGetIssue({
    kind: "value",
    value: {
      number: rig.issueNumber,
      title: "Stabilize-CI fixture",
      body: "Body.",
      state: "open",
    },
  });
  rig.githubClient.queueCreatePullRequest({
    kind: "value",
    value: {
      number: makeIssueNumber(7),
      headSha: rig.headShas[0] ?? "sha-initial",
      headRef: branchNameFor(rig.issueNumber),
      baseRef: DEFAULT_BASE_BRANCH,
      state: "open",
    },
  });
  rig.githubClient.queueRequestReviewers({ kind: "value", value: undefined });
  rig.agentRunner.queueRun({
    messages: [{ role: "assistant", text: "drafted" }],
  });
}

function makeSupervisor(rig: CiTestRig, opts: {
  readonly maxIterations?: number;
} = {}) {
  return createTaskSupervisor({
    githubClient: rig.githubClient,
    worktreeManager:
      // deno-lint-ignore no-explicit-any
      fakeWorktreeManager() as any,
    persistence: rig.persistence,
    eventBus: rig.bus,
    agentRunner: rig.agentRunner,
    cloneUrlFor: () => "file:///dev/null",
    clock: new FixedClock(),
    randomSource: new FixedRandomSource(),
    poller: syncPoller(),
    pollIntervalMilliseconds: 1,
    ...(opts.maxIterations !== undefined ? { maxIterations: opts.maxIterations } : {}),
    resolveHeadSha: (task: Task) => {
      // The drafting phase bumps `iterationCount` to 1 before the CI
      // phase enters; from there each CI agent iteration bumps it
      // again. Map `iterationCount - 1` onto the test's scripted SHA
      // list so:
      //   iterationCount === 1 → sha-initial (CI entry, before any fix)
      //   iterationCount === 2 → sha-after-fix (after one agent fix)
      //   ...
      const index = Math.max(0, task.iterationCount - 1);
      const sha = rig.headShas[Math.min(rig.headShas.length - 1, index)];
      return Promise.resolve(sha ?? rig.headShas[rig.headShas.length - 1] ?? "sha-fallback");
    },
  });
}

// ---------------------------------------------------------------------------
// CI phase scenarios
// ---------------------------------------------------------------------------

Deno.test("stabilize CI green-on-first-poll completes the phase without dispatching the agent", async () => {
  const rig = makeRig();
  scriptHappyPathPrefix(rig);
  // First combined-status poll is success — no `listCheckRuns` needed.
  rig.githubClient.queueGetCombinedStatus({
    kind: "value",
    value: { state: "success", sha: rig.headShas[0] ?? "sha-initial" },
  });
  rig.githubClient.queueMergePullRequest({ kind: "value", value: undefined });

  const subscription = recordEvents(rig);
  const supervisor = makeSupervisor(rig);
  const finalTask = await supervisor.start({
    repo: makeRepoFullName("o/r"),
    issueNumber: rig.issueNumber,
  });
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  subscription.unsubscribe();

  assertEquals(finalTask.state, "MERGED");
  assertEquals(finalTask.terminalReason, "merged");
  // CI green means: exactly one combined-status call, zero `listCheckRuns`,
  // zero `getCheckRunLogs`, zero agent CI iterations beyond the drafting one.
  const calls = rig.githubClient.recordedCalls();
  const ciCombinedCalls = calls.filter((c) => c.method === "getCombinedStatus");
  const checkRunsCalls = calls.filter((c) => c.method === "listCheckRuns");
  const logsCalls = calls.filter((c) => c.method === "getCheckRunLogs");
  assertEquals(ciCombinedCalls.length, 1);
  assertEquals(checkRunsCalls.length, 0);
  assertEquals(logsCalls.length, 0);
  // Drafting consumed exactly one agent invocation; the CI phase added
  // none on the green path.
  assertEquals(rig.agentRunner.recordedInvocations().length, 1);
});

Deno.test("stabilize CI red-then-fixed dispatches the agent once and re-polls the new head SHA", async () => {
  const rig = makeRig();
  rig.headShas.push("sha-after-fix");
  scriptHappyPathPrefix(rig);
  // First poll: failure with one failing job.
  rig.githubClient.queueGetCombinedStatus({
    kind: "value",
    value: { state: "failure", sha: rig.headShas[0] ?? "sha-initial" },
  });
  const failingCheck: CheckRunSummary = {
    id: 100,
    name: "build",
    status: "completed",
    conclusion: "failure",
    htmlUrl: "https://github.com/o/r/runs/100",
  };
  rig.githubClient.queueListCheckRuns({
    kind: "value",
    value: [failingCheck],
  });
  rig.githubClient.queueGetCheckRunLogs({
    kind: "value",
    value: new TextEncoder().encode("expected 200, got 500\nstack frame 1\nstack frame 2\n"),
  });
  // Agent run for the fix.
  rig.agentRunner.queueRun({
    messages: [
      { role: "assistant", text: "investigating CI failure" },
      { role: "tool-use", text: "edit src/server.ts" },
      { role: "assistant", text: "committed fix" },
    ],
  });
  // Second poll: success on the new SHA.
  rig.githubClient.queueGetCombinedStatus({
    kind: "value",
    value: { state: "success", sha: "sha-after-fix" },
  });
  rig.githubClient.queueMergePullRequest({ kind: "value", value: undefined });

  const subscription = recordEvents(rig);
  const supervisor = makeSupervisor(rig);
  const finalTask = await supervisor.start({
    repo: makeRepoFullName("o/r"),
    issueNumber: rig.issueNumber,
  });
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  subscription.unsubscribe();

  assertEquals(finalTask.state, "MERGED");
  // Iteration counter advanced once for the drafting run plus once for
  // the CI fix.
  assertEquals(finalTask.iterationCount, 2);
  const calls = rig.githubClient.recordedCalls();
  const combinedCalls = calls.filter((c) => c.method === "getCombinedStatus");
  const checkRunsCalls = calls.filter((c) => c.method === "listCheckRuns");
  const logCalls = calls.filter((c) => c.method === "getCheckRunLogs");
  assertEquals(combinedCalls.length, 2);
  assertEquals(checkRunsCalls.length, 1);
  assertEquals(logCalls.length, 1);
  // Two combined-status calls — first against the initial SHA, second
  // against the post-fix SHA.
  assertEquals(
    combinedCalls.map((c) => (c.method === "getCombinedStatus" ? c.sha : "")),
    ["sha-initial", "sha-after-fix"],
  );
  // Agent runs: one drafting + one CI fix.
  const invocations = rig.agentRunner.recordedInvocations();
  assertEquals(invocations.length, 2);
  // The CI fix prompt embeds the failing-job summary.
  assertStringIncludes(invocations[1]?.prompt ?? "", "Stabilize CI fix");
  assertStringIncludes(invocations[1]?.prompt ?? "", "build");
  assertStringIncludes(invocations[1]?.prompt ?? "", "expected 200, got 500");
});

Deno.test("stabilize CI perpetually red exhausts the iteration budget and lands NEEDS_HUMAN", async () => {
  const maxIterations = 2;
  const rig = makeRig();
  // Each iteration produces a fresh head SHA so the supervisor polls
  // the "new" commit. Drafting consumes iteration 1 of the
  // shared budget; the CI loop has 1 iteration left before exhaustion.
  for (let i = 1; i <= maxIterations; i += 1) {
    rig.headShas.push(`sha-${i}`);
  }
  scriptHappyPathPrefix(rig);
  // Every CI poll returns failure with the same failing job. The
  // budget runs out after `maxIterations` total agent iterations
  // (drafting included).
  const failingCheck: CheckRunSummary = {
    id: 1,
    name: "lint",
    status: "completed",
    conclusion: "failure",
    htmlUrl: "https://github.com/o/r/runs/1",
  };
  for (let i = 0; i < maxIterations; i += 1) {
    rig.githubClient.queueGetCombinedStatus({
      kind: "value",
      value: { state: "failure", sha: `sha-${i}` },
    });
    rig.githubClient.queueListCheckRuns({
      kind: "value",
      value: [failingCheck],
    });
    rig.githubClient.queueGetCheckRunLogs({
      kind: "value",
      value: new TextEncoder().encode("error line\n"),
    });
    rig.agentRunner.queueRun({
      messages: [{ role: "assistant", text: `attempt ${i + 1}` }],
    });
  }
  // One final combined-status that the budget check rejects before
  // calling — the supervisor must not consume this reply.
  rig.githubClient.queueGetCombinedStatus({
    kind: "value",
    value: { state: "failure", sha: `sha-${maxIterations}` },
  });
  rig.githubClient.queueListCheckRuns({
    kind: "value",
    value: [failingCheck],
  });

  const subscription = recordEvents(rig);
  const supervisor = makeSupervisor(rig, { maxIterations });
  const finalTask = await supervisor.start({
    repo: makeRepoFullName("o/r"),
    issueNumber: rig.issueNumber,
  });
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  subscription.unsubscribe();

  assertEquals(finalTask.state, "NEEDS_HUMAN");
  assert(finalTask.terminalReason?.startsWith("ci-perpetually-red"));
  // Iteration counter saturates at the cap.
  assertEquals(finalTask.iterationCount, maxIterations);
  // The supervisor never reaches READY_TO_MERGE, so `mergePullRequest`
  // is not called. The mock would throw if called without a queued reply
  // — the test asserts the call is absent.
  const calls = rig.githubClient.recordedCalls();
  assertEquals(calls.filter((c) => c.method === "mergePullRequest").length, 0);
});

Deno.test("stabilize CI surfaces a `github-call` event for every combined-status poll", async () => {
  const rig = makeRig();
  rig.headShas.push("sha-after-fix");
  scriptHappyPathPrefix(rig);
  rig.githubClient.queueGetCombinedStatus({
    kind: "value",
    value: { state: "success", sha: rig.headShas[0] ?? "sha-initial" },
  });
  rig.githubClient.queueMergePullRequest({ kind: "value", value: undefined });

  const subscription = recordEvents(rig);
  const supervisor = makeSupervisor(rig);
  await supervisor.start({
    repo: makeRepoFullName("o/r"),
    issueNumber: rig.issueNumber,
  });
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  subscription.unsubscribe();

  const githubEvents = rig.events.filter((e) => e.kind === "github-call");
  // At least the combined-status read fired — the green path skips the
  // listCheckRuns and getCheckRunLogs reads.
  assert(githubEvents.length >= 1);
  const endpoints = githubEvents.map((e) => e.kind === "github-call" ? e.data.endpoint : "");
  assert(endpoints.some((e) => e.includes("/status")));
});

// ---------------------------------------------------------------------------
// Log-budget trimming
// ---------------------------------------------------------------------------

Deno.test("trimLogToBudget returns the input unchanged when it already fits", () => {
  const input = "first line\nsecond line\nthird line\n";
  const trimmed = trimLogToBudget(input, 1024);
  assertEquals(trimmed, input);
});

Deno.test("trimLogToBudget keeps whole trailing lines when the input exceeds the budget", () => {
  const lines = [];
  for (let i = 0; i < 100; i += 1) {
    lines.push(`line ${i.toString().padStart(3, "0")} payload`);
  }
  const input = lines.join("\n");
  const trimmed = trimLogToBudget(input, 200);
  assertStringIncludes(trimmed, "[…truncated;");
  assertStringIncludes(trimmed, "showing trailing 200 bytes");
  // The very last line must still be present.
  assertStringIncludes(trimmed, "line 099 payload");
  // The first line must have been dropped.
  assert(!trimmed.includes("line 000 payload"));
});

Deno.test("trimLogToBudget falls back to a hard byte slice when a single line exceeds the budget", () => {
  const giantLine = "x".repeat(10_000);
  const trimmed = trimLogToBudget(giantLine, 100);
  assertStringIncludes(trimmed, "[…truncated;");
  // The entire excerpt is bounded near the budget. Allow some headroom
  // for the marker line (which the marker prepends but is not subject
  // to the budget itself).
  assert(new TextEncoder().encode(trimmed).byteLength <= 200);
  // The kept tail consists entirely of `x`s — the slice came from the
  // end of the input.
  const tail = trimmed.split("\n").slice(1).join("\n");
  assert(/^x+$/.test(tail));
});

Deno.test("trimLogToBudget treats non-positive budgets as zero and returns only the marker", () => {
  const trimmed = trimLogToBudget("anything", 0);
  assertStringIncludes(trimmed, "[…truncated;");
  // No content lines past the marker.
  assertEquals(trimmed.split("\n").length, 1);
});

Deno.test("STABILIZE_CI_LOG_BUDGET_BYTES default is 100 KB", () => {
  assertEquals(STABILIZE_CI_LOG_BUDGET_BYTES, 100 * 1024);
});

// ---------------------------------------------------------------------------
// Prompt shape
// ---------------------------------------------------------------------------

Deno.test("buildCiAgentPrompt renders the issue header, failing-check list, and per-job log fences", () => {
  const failingChecks: CheckRunSummary[] = [
    {
      id: 1,
      name: "build",
      status: "completed",
      conclusion: "failure",
      htmlUrl: "https://github.com/o/r/runs/1",
    },
    {
      id: 2,
      name: "lint",
      status: "completed",
      conclusion: "timed_out",
      htmlUrl: "https://github.com/o/r/runs/2",
    },
  ];
  const prompt = buildCiAgentPrompt({
    issueNumber: makeIssueNumber(42),
    headSha: "deadbeefcafe",
    failingChecks,
    logsByJob: new Map([
      [1, "build error: missing import"],
      [2, "lint warning: unused"],
    ]),
  });
  assertStringIncludes(prompt, "Stabilize CI fix for issue #42");
  assertStringIncludes(prompt, "deadbeefcafe");
  assertStringIncludes(prompt, "**build**");
  assertStringIncludes(prompt, "**lint**");
  assertStringIncludes(prompt, "build error: missing import");
  assertStringIncludes(prompt, "lint warning: unused");
  assertStringIncludes(prompt, "```");
});

Deno.test("buildCiAgentPrompt substitutes a placeholder when a job has no captured log", () => {
  const prompt = buildCiAgentPrompt({
    issueNumber: makeIssueNumber(1),
    headSha: "abcdef",
    failingChecks: [
      {
        id: 9,
        name: "test",
        status: "completed",
        conclusion: "failure",
        htmlUrl: "https://example.com",
      },
    ],
    logsByJob: new Map(),
  });
  assertStringIncludes(prompt, "[no log captured]");
});

// ---------------------------------------------------------------------------
// Pre-condition / wiring guards
// ---------------------------------------------------------------------------

Deno.test("stabilize CI requires a StabilizeGitHubClient and fails the task otherwise", async () => {
  const rig = makeRig();
  // Replace the github client surface with one that *only* implements
  // the W1 GitHubClient methods — `listCheckRuns` and friends are
  // missing, so `asStabilizeClient` should reject the wiring.
  scriptHappyPathPrefix(rig);
  // Strip the additive methods at runtime by wrapping in a proxy that
  // hides them. The W1 surface stays intact for `getIssue`,
  // `createPullRequest`, `requestReviewers`, `mergePullRequest`,
  // `getCombinedStatus`.
  const narrow = new Proxy(rig.githubClient, {
    get(target, prop, receiver) {
      if (
        prop === "listCheckRuns" || prop === "getCheckRunLogs" ||
        prop === "listReviews" || prop === "listReviewComments"
      ) {
        return undefined;
      }
      return Reflect.get(target, prop, receiver);
    },
  }) as unknown as StabilizeGitHubClient;
  // Patch the supervisor's githubClient with the narrowed surface.
  const supervisor = createTaskSupervisor({
    githubClient: narrow,
    worktreeManager:
      // deno-lint-ignore no-explicit-any
      fakeWorktreeManager() as any,
    persistence: rig.persistence,
    eventBus: rig.bus,
    agentRunner: rig.agentRunner,
    cloneUrlFor: () => "file:///dev/null",
    clock: new FixedClock(),
    randomSource: new FixedRandomSource(),
    poller: syncPoller(),
    pollIntervalMilliseconds: 1,
  });
  const subscription = recordEvents(rig);
  const finalTask = await supervisor.start({
    repo: makeRepoFullName("o/r"),
    issueNumber: rig.issueNumber,
  });
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  subscription.unsubscribe();

  assertEquals(finalTask.state, "FAILED");
  assert(finalTask.terminalReason?.startsWith("ci-precondition:"));
});
