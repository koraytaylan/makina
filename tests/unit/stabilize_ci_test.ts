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
import { PollerError, type PollerImpl } from "../../src/daemon/poller.ts";
import { decodeCheckRunLogs } from "../../src/daemon/supervisor.ts";
import { STABILIZE_CI_MAX_CONSECUTIVE_FETCHER_ERRORS } from "../../src/constants.ts";
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

// ---------------------------------------------------------------------------
// Round-2 follow-ups (Copilot)
// ---------------------------------------------------------------------------

Deno.test("ci-precondition message lists every required StabilizeGitHubClient method", async () => {
  const rig = makeRig();
  scriptHappyPathPrefix(rig);
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
  const finalTask = await supervisor.start({
    repo: makeRepoFullName("o/r"),
    issueNumber: rig.issueNumber,
  });
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  const reason = finalTask.terminalReason ?? "";
  // Round-2: the message must enumerate the full StabilizeGitHubClient
  // surface (four methods), matching what `asStabilizeClient` actually
  // feature-detects. Round-1 listed only two methods, which was
  // misleading when a partial double tripped the precondition.
  for (const method of ["listCheckRuns", "getCheckRunLogs", "listReviews", "listReviewComments"]) {
    assertStringIncludes(reason, method);
  }
});

Deno.test("decodeCheckRunLogs extracts text entries from a real ZIP container", async () => {
  // Minimal ZIP with a single STORED entry — STORED has no compression
  // and lets the test exercise the central-directory parser without
  // depending on a deflate fixture.
  const zip = buildStoredZip([{ name: "0_step.txt", text: "hello world\n" }]);
  const decoded = await decodeCheckRunLogs(zip);
  assertStringIncludes(decoded, "--- 0_step.txt ---");
  assertStringIncludes(decoded, "hello world");
});

Deno.test("decodeCheckRunLogs extracts a DEFLATE-compressed entry", async () => {
  // Round-trip a known string through the platform's CompressionStream
  // so the test does not hard-code deflate bytes.
  const text = "deflate-compressed log line\n";
  const compressed = await deflateRawBytes(new TextEncoder().encode(text));
  const zip = buildDeflateZip("step.txt", compressed, text.length);
  const decoded = await decodeCheckRunLogs(zip);
  assertStringIncludes(decoded, "--- step.txt ---");
  assertStringIncludes(decoded, "deflate-compressed log line");
});

Deno.test("decodeCheckRunLogs falls back to raw decode for non-ZIP bytes", async () => {
  // The in-memory test double queues pre-extracted text bytes; this
  // path must still pass through unchanged so existing tests do not
  // need a ZIP fixture.
  const decoded = await decodeCheckRunLogs(new TextEncoder().encode("plain text\n"));
  assertEquals(decoded, "plain text\n");
});

Deno.test("decodeCheckRunLogs falls back to raw decode for malformed ZIP bytes", async () => {
  // PK magic but truncated central directory — should not throw; the
  // helper degrades gracefully so the agent prompt still has *some*
  // context.
  const truncated = new Uint8Array([0x50, 0x4b, 0x03, 0x04, 0x00, 0x00, 0x00, 0x00]);
  const decoded = await decodeCheckRunLogs(truncated);
  // Raw UTF-8 decode of the byte garbage; just assert the call returns
  // a string rather than throwing.
  assertEquals(typeof decoded, "string");
});

Deno.test("stabilize CI ignores rate-limited PollerErrors when accumulating consecutive errors", async () => {
  const rig = makeRig();
  scriptHappyPathPrefix(rig);
  // Fire one more rate-limited rejection than the consecutive-error
  // threshold; if the supervisor counted them, the task would land in
  // FAILED. With the round-2 fix it must keep going and consume the
  // green tick that follows.
  const rateLimitTickets = STABILIZE_CI_MAX_CONSECUTIVE_FETCHER_ERRORS + 2;
  for (let i = 0; i < rateLimitTickets; i += 1) {
    rig.githubClient.queueGetCombinedStatus({
      kind: "error",
      error: new PollerError("rate-limited", `tick ${i}`, { retryAfterMs: 1 }),
    });
  }
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
  // The supervisor reached MERGED — proving the rate-limited stream did
  // not trip the fatal threshold.
  assertEquals(finalTask.state, "MERGED");
});

Deno.test("stabilize CI surfaces a synchronous poller throw as a fatal outcome", async () => {
  const rig = makeRig();
  scriptHappyPathPrefix(rig);
  // Replace the poller with one whose `poll()` throws synchronously
  // (the same shape `poller.poll` itself uses for non-finite intervals
  // per `src/daemon/poller.ts:411-415`). Without the round-2 try/catch
  // this would bubble out of `runStabilizeCi` and skip the FAILED
  // transition; the fix routes the throw through the discriminated
  // outcome path.
  const throwingPoller: PollerImpl = {
    poll(): { cancel(): void } {
      throw new RangeError("synthetic non-finite interval");
    },
  };
  const supervisor = createTaskSupervisor({
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
    poller: throwingPoller,
    pollIntervalMilliseconds: 1,
  });
  const finalTask = await supervisor.start({
    repo: makeRepoFullName("o/r"),
    issueNumber: rig.issueNumber,
  });
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  assertEquals(finalTask.state, "FAILED");
  assert(finalTask.terminalReason?.startsWith("ci-poll:"));
});

// ---------------------------------------------------------------------------
// Round-3 follow-ups (Copilot)
// ---------------------------------------------------------------------------

Deno.test("decodeCheckRunLogs caps DEFLATE extraction at maxBytes (no full decompression)", async () => {
  // Build a single DEFLATE entry whose uncompressed payload is much
  // larger than the budget. Round-2 would have inflated the entire
  // entry first and then trimmed; round-3 must stop the stream early
  // so peak memory stays bounded by `maxBytes`.
  const huge = "X".repeat(64 * 1024); // 64 KiB uncompressed
  const compressed = await deflateRawBytes(new TextEncoder().encode(huge));
  const zip = buildDeflateZip("step.txt", compressed, huge.length);
  const cap = 1024;
  const decoded = await decodeCheckRunLogs(zip, cap);
  // The header banner alone is small; the payload after it must be
  // bounded by the cap. Allow generous headroom for the entry banner
  // and the truncation marker tail.
  const decodedBytes = new TextEncoder().encode(decoded).byteLength;
  assert(
    decodedBytes <= cap + 256,
    `decoded ${decodedBytes} bytes, expected <= ${cap + 256}`,
  );
  // The truncation marker is appended by `decodeCheckRunLogs` when the
  // extractor stops short — operators reading the agent prompt should
  // see *something* indicating the cap fired.
  assertStringIncludes(decoded, "[…truncated; extraction stopped at");
});

Deno.test("decodeCheckRunLogs without maxBytes still drains the entire DEFLATE entry", async () => {
  // Backwards-compatibility: omitting the budget keeps the legacy
  // behavior so existing tests (and the in-memory test double) do not
  // need to thread a cap through.
  const text = "first line\nsecond line\nthird line\n";
  const compressed = await deflateRawBytes(new TextEncoder().encode(text));
  const zip = buildDeflateZip("step.txt", compressed, text.length);
  const decoded = await decodeCheckRunLogs(zip);
  assertStringIncludes(decoded, "first line");
  assertStringIncludes(decoded, "second line");
  assertStringIncludes(decoded, "third line");
  // No truncation marker on the unbounded path.
  assert(!decoded.includes("extraction stopped"));
});

Deno.test("decodeCheckRunLogs caps STORED extraction at maxBytes and skips later entries", async () => {
  // Two STORED entries; the first alone exceeds the budget, so the
  // second entry must be skipped entirely (not just trimmed).
  const big = "A".repeat(8192);
  const small = "tail entry\n";
  const zip = buildStoredZip([
    { name: "first.txt", text: big },
    { name: "second.txt", text: small },
  ]);
  const cap = 1024;
  const decoded = await decodeCheckRunLogs(zip, cap);
  // First entry is sliced to the budget — the banner is present but
  // the second entry's banner must be absent (it was skipped).
  assertStringIncludes(decoded, "--- first.txt ---");
  assert(
    !decoded.includes("--- second.txt ---"),
    "second entry must be skipped once the budget is exhausted",
  );
  assertStringIncludes(decoded, "[…truncated; extraction stopped at");
});

Deno.test("stabilize CI publishes a `github-call` event with `error` when getCombinedStatus rejects", async () => {
  // Round-3: a rejection in the fetcher must still surface a
  // `github-call` event (with `error` set) so the TUI sees *which*
  // request failed. Pre-fix, the event was only published on the
  // success path, leaving operators blind to outages and 404s.
  const rig = makeRig();
  scriptHappyPathPrefix(rig);
  // First reply: a rejection (the `kind: "error"` shape on the
  // in-memory client makes `getCombinedStatus` reject with the
  // supplied error).
  rig.githubClient.queueGetCombinedStatus({
    kind: "error",
    error: new Error("simulated 503 from GitHub"),
  });
  // Second reply: a normal green so the supervisor exits cleanly
  // through MERGED rather than burning the consecutive-error budget.
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

  // Find every `github-call` event for the combined-status endpoint
  // and assert the rejection produced one with `error` set.
  const githubEvents = rig.events.filter((e) => e.kind === "github-call");
  const statusEvents = githubEvents.filter((e) =>
    e.kind === "github-call" && e.data.endpoint.includes("/status")
  );
  assert(statusEvents.length >= 2, "expected one event per status call (reject + green)");
  const erroredEvents = statusEvents.filter((e) =>
    e.kind === "github-call" && e.data.error !== undefined
  );
  assertEquals(erroredEvents.length, 1);
  const firstErrored = erroredEvents[0];
  if (firstErrored?.kind !== "github-call") throw new Error("unreachable");
  assertStringIncludes(firstErrored.data.error ?? "", "simulated 503");
});

Deno.test("stabilize CI publishes a `github-call` event with `error` when listCheckRuns rejects", async () => {
  // The fetcher only reaches `listCheckRuns` on a non-success status.
  // Queue a failure status that lands at the second call site, then
  // reject `listCheckRuns` to confirm the second emit path also
  // surfaces `error` on the bus.
  const rig = makeRig();
  scriptHappyPathPrefix(rig);
  rig.githubClient.queueGetCombinedStatus({
    kind: "value",
    value: { state: "failure", sha: rig.headShas[0] ?? "sha-initial" },
  });
  rig.githubClient.queueListCheckRuns({
    kind: "error",
    error: new Error("simulated 404 from GitHub"),
  });
  // The poller's onError will retry — feed a green tick on the next
  // pass so the supervisor exits via MERGED rather than the
  // consecutive-error fatal path.
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
  const checkRunEvents = githubEvents.filter((e) =>
    e.kind === "github-call" && e.data.endpoint.includes("/check-runs")
  );
  const erroredEvents = checkRunEvents.filter((e) =>
    e.kind === "github-call" && e.data.error !== undefined
  );
  assertEquals(erroredEvents.length, 1);
  const firstErrored = erroredEvents[0];
  if (firstErrored?.kind !== "github-call") throw new Error("unreachable");
  assertStringIncludes(firstErrored.data.error ?? "", "simulated 404");
});

Deno.test("stabilize CI publishes a `github-call` event with `error` when getCheckRunLogs rejects", async () => {
  // The third GitHub call from the CI loop is the per-job log fetch
  // inside `dispatchCiAgent`. A rejection there must also surface a
  // `github-call` event with `error` set (in addition to the existing
  // human-readable warn-level `log` event).
  const rig = makeRig();
  rig.headShas.push("sha-after-fix");
  scriptHappyPathPrefix(rig);
  rig.githubClient.queueGetCombinedStatus({
    kind: "value",
    value: { state: "failure", sha: rig.headShas[0] ?? "sha-initial" },
  });
  rig.githubClient.queueListCheckRuns({
    kind: "value",
    value: [{
      id: 99,
      name: "build",
      status: "completed",
      conclusion: "failure",
      htmlUrl: "https://github.com/o/r/runs/99",
    }],
  });
  rig.githubClient.queueGetCheckRunLogs({
    kind: "error",
    error: new Error("simulated logs 502"),
  });
  // Agent runs once; the next poll is green on the post-fix SHA.
  rig.agentRunner.queueRun({
    messages: [{ role: "assistant", text: "fix attempt" }],
  });
  rig.githubClient.queueGetCombinedStatus({
    kind: "value",
    value: { state: "success", sha: "sha-after-fix" },
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
  const logEvents = githubEvents.filter((e) =>
    e.kind === "github-call" && e.data.endpoint.includes("/logs")
  );
  const erroredEvents = logEvents.filter((e) =>
    e.kind === "github-call" && e.data.error !== undefined
  );
  assertEquals(erroredEvents.length, 1);
  const firstErrored = erroredEvents[0];
  if (firstErrored?.kind !== "github-call") throw new Error("unreachable");
  assertStringIncludes(firstErrored.data.error ?? "", "simulated logs 502");
});

// ---------------------------------------------------------------------------
// ZIP fixture helpers (round-2)
// ---------------------------------------------------------------------------

interface ZipEntry {
  readonly name: string;
  readonly text: string;
}

/**
 * Build a minimal ZIP archive whose every entry uses compression
 * method 0 (`STORED`). Enough of the spec to round-trip through the
 * supervisor's central-directory reader; not a general-purpose ZIP
 * encoder.
 */
function buildStoredZip(entries: readonly ZipEntry[]): Uint8Array {
  const encoder = new TextEncoder();
  const localHeaders: Uint8Array[] = [];
  const cdHeaders: Uint8Array[] = [];
  let offset = 0;
  for (const entry of entries) {
    const data = encoder.encode(entry.text);
    const nameBytes = encoder.encode(entry.name);
    const localHeader = buildLocalHeader({
      compressionMethod: 0,
      compressedSize: data.byteLength,
      uncompressedSize: data.byteLength,
      crc32: crc32(data),
      nameBytes,
    });
    const localPayload = concat([localHeader, data]);
    localHeaders.push(localPayload);
    cdHeaders.push(buildCdfh({
      compressionMethod: 0,
      compressedSize: data.byteLength,
      uncompressedSize: data.byteLength,
      crc32: crc32(data),
      nameBytes,
      localHeaderOffset: offset,
    }));
    offset += localPayload.byteLength;
  }
  const cd = concat(cdHeaders);
  const eocd = buildEocd({
    totalEntries: entries.length,
    cdSize: cd.byteLength,
    cdOffset: offset,
  });
  return concat([...localHeaders, cd, eocd]);
}

/**
 * Build a ZIP with a single DEFLATE-compressed entry. The compressed
 * payload comes from {@link deflateRawBytes}.
 */
function buildDeflateZip(
  name: string,
  compressed: Uint8Array,
  uncompressedSize: number,
): Uint8Array {
  const encoder = new TextEncoder();
  const nameBytes = encoder.encode(name);
  // CRC32 is computed over the *uncompressed* data — deflate fixtures
  // don't actually verify the CRC, but the central directory still
  // expects a value, so use 0.
  const crc = 0;
  const localHeader = buildLocalHeader({
    compressionMethod: 8,
    compressedSize: compressed.byteLength,
    uncompressedSize,
    crc32: crc,
    nameBytes,
  });
  const localPayload = concat([localHeader, compressed]);
  const cdHeader = buildCdfh({
    compressionMethod: 8,
    compressedSize: compressed.byteLength,
    uncompressedSize,
    crc32: crc,
    nameBytes,
    localHeaderOffset: 0,
  });
  const eocd = buildEocd({
    totalEntries: 1,
    cdSize: cdHeader.byteLength,
    cdOffset: localPayload.byteLength,
  });
  return concat([localPayload, cdHeader, eocd]);
}

function buildLocalHeader(args: {
  readonly compressionMethod: number;
  readonly compressedSize: number;
  readonly uncompressedSize: number;
  readonly crc32: number;
  readonly nameBytes: Uint8Array;
}): Uint8Array {
  const buffer = new Uint8Array(30 + args.nameBytes.byteLength);
  const view = new DataView(buffer.buffer);
  view.setUint32(0, 0x04034b50, true); // local file header signature
  view.setUint16(4, 20, true); // version needed
  view.setUint16(6, 0, true); // gp flags
  view.setUint16(8, args.compressionMethod, true);
  view.setUint16(10, 0, true); // mod time
  view.setUint16(12, 0, true); // mod date
  view.setUint32(14, args.crc32, true);
  view.setUint32(18, args.compressedSize, true);
  view.setUint32(22, args.uncompressedSize, true);
  view.setUint16(26, args.nameBytes.byteLength, true);
  view.setUint16(28, 0, true); // extra length
  buffer.set(args.nameBytes, 30);
  return buffer;
}

function buildCdfh(args: {
  readonly compressionMethod: number;
  readonly compressedSize: number;
  readonly uncompressedSize: number;
  readonly crc32: number;
  readonly nameBytes: Uint8Array;
  readonly localHeaderOffset: number;
}): Uint8Array {
  const buffer = new Uint8Array(46 + args.nameBytes.byteLength);
  const view = new DataView(buffer.buffer);
  view.setUint32(0, 0x02014b50, true); // central-directory file header
  view.setUint16(4, 20, true); // version made by
  view.setUint16(6, 20, true); // version needed
  view.setUint16(8, 0, true); // gp flags
  view.setUint16(10, args.compressionMethod, true);
  view.setUint16(12, 0, true);
  view.setUint16(14, 0, true);
  view.setUint32(16, args.crc32, true);
  view.setUint32(20, args.compressedSize, true);
  view.setUint32(24, args.uncompressedSize, true);
  view.setUint16(28, args.nameBytes.byteLength, true);
  view.setUint16(30, 0, true);
  view.setUint16(32, 0, true);
  view.setUint16(34, 0, true); // disk
  view.setUint16(36, 0, true); // internal attrs
  view.setUint32(38, 0, true); // external attrs
  view.setUint32(42, args.localHeaderOffset, true);
  buffer.set(args.nameBytes, 46);
  return buffer;
}

function buildEocd(args: {
  readonly totalEntries: number;
  readonly cdSize: number;
  readonly cdOffset: number;
}): Uint8Array {
  const buffer = new Uint8Array(22);
  const view = new DataView(buffer.buffer);
  view.setUint32(0, 0x06054b50, true);
  view.setUint16(4, 0, true); // disk
  view.setUint16(6, 0, true); // disk with CD start
  view.setUint16(8, args.totalEntries, true);
  view.setUint16(10, args.totalEntries, true);
  view.setUint32(12, args.cdSize, true);
  view.setUint32(16, args.cdOffset, true);
  view.setUint16(20, 0, true); // comment length
  return buffer;
}

function concat(parts: readonly Uint8Array[]): Uint8Array {
  let total = 0;
  for (const part of parts) total += part.byteLength;
  const out = new Uint8Array(total);
  let written = 0;
  for (const part of parts) {
    out.set(part, written);
    written += part.byteLength;
  }
  return out;
}

/** CRC32 (IEEE polynomial) used by ZIP central-directory entries. */
function crc32(bytes: Uint8Array): number {
  let crc = 0xffffffff;
  for (let i = 0; i < bytes.length; i += 1) {
    crc ^= bytes[i] ?? 0;
    for (let j = 0; j < 8; j += 1) {
      crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

/**
 * Compress `bytes` through the platform's `CompressionStream("deflate-raw")`
 * so the test fixture exercises the same primitive the supervisor's
 * `inflateRaw` reads.
 */
async function deflateRawBytes(bytes: Uint8Array): Promise<Uint8Array> {
  const owned = new Uint8Array(bytes.byteLength);
  owned.set(bytes);
  const stream = new Blob([owned])
    .stream()
    .pipeThrough(new CompressionStream("deflate-raw"));
  const reader = stream.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    if (value !== undefined) {
      chunks.push(value);
      total += value.byteLength;
    }
  }
  return concat(chunks);
}
