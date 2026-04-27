/**
 * Integration tests for `src/daemon/supervisor.ts`.
 *
 * The supervisor is the per-task FSM that walks an issue end-to-end
 * through the lifecycle. This file drives the happy-path scenario the
 * #12 brief calls out:
 *
 *  1. The supervisor mints a {@link TaskId}, walks `(repo, issue)`
 *     from `INIT` → … → `MERGED`.
 *  2. Every transition appears as a `state-changed` {@link TaskEvent}
 *     on the event bus, in order.
 *  3. Persistence replay reconstructs the final task identically to
 *     the in-memory snapshot returned by `start()`.
 *  4. The Copilot reviewer is requested immediately after the PR is
 *     opened — the integration assertion compares the exact call ordering
 *     against the {@link InMemoryGitHubClient}'s recorded log.
 *
 * The fixture wires the real {@link WorktreeManagerImpl},
 * {@link Persistence}, and {@link EventBus} from W2 against a
 * `Deno.makeTempDir`-backed workspace. The GitHub side is the
 * {@link InMemoryGitHubClient} double; the agent side is the
 * {@link MockAgentRunner} double. No global mutation is performed
 * (Lesson #3 — the test injects collaborators and never overrides
 * `Deno.open`, `Deno.env`, or `fetch`).
 */

import { assert, assertEquals, assertRejects } from "@std/assert";
import { join } from "@std/path";

import {
  branchNameFor,
  COPILOT_REVIEWER_LOGIN,
  createTaskSupervisor,
  DEFAULT_BASE_BRANCH,
  formatRebaseNeedsHumanReason,
  prBodyFor,
  prTitleFor,
  type SupervisorClock,
  SupervisorError,
  type SupervisorRandomSource,
} from "../../src/daemon/supervisor.ts";
import type { GitInvocationResult, StabilizeGitInvoker } from "../../src/daemon/stabilize.ts";
import { createEventBus } from "../../src/daemon/event-bus.ts";
import { createPersistence } from "../../src/daemon/persistence.ts";
import { createWorktreeManager } from "../../src/daemon/worktree-manager.ts";
import { MAX_TASK_ITERATIONS } from "../../src/constants.ts";
import {
  type IssueNumber,
  makeIssueNumber,
  makeRepoFullName,
  type RepoFullName,
  type TaskEvent,
  type TaskState,
} from "../../src/types.ts";
import { InMemoryGitHubClient } from "../helpers/in_memory_github_client.ts";
import { MockAgentRunner } from "../helpers/mock_agent_runner.ts";

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

interface SupervisorTestRig {
  readonly workspace: string;
  readonly statePath: string;
  readonly source: { dir: string; url: string };
  readonly githubClient: InMemoryGitHubClient;
  readonly agentRunner: MockAgentRunner;
  readonly events: TaskEvent[];
  readonly repo: RepoFullName;
  readonly issueNumber: IssueNumber;
  readonly clock: DeterministicClock;
  cleanup(): Promise<void>;
}

/**
 * Deterministic clock used by every test. Returns a known initial
 * timestamp and advances by one second on each `nowIso()` call so two
 * transitions in the same FSM walk produce strictly increasing
 * timestamps the test can compare verbatim.
 */
class DeterministicClock implements SupervisorClock {
  private milliseconds = Date.UTC(2026, 3, 26, 12, 0, 0);

  /** Return the next ISO timestamp. */
  nowIso(): string {
    const iso = new Date(this.milliseconds).toISOString();
    this.milliseconds += 1_000;
    return iso;
  }
}

/**
 * Deterministic random source for the supervisor's task-id mint. Tests
 * that read persisted records by deep equality cannot tolerate a real
 * `crypto.getRandomValues` here; the source returns the same byte
 * sequence on every call.
 */
class FixedRandomSource implements SupervisorRandomSource {
  fillRandomBytes(bytes: Uint8Array): void {
    for (let i = 0; i < bytes.length; i += 1) {
      // Arbitrary but stable bytes. The supervisor encodes them to hex
      // and slices to TASK_ID_RANDOM_SUFFIX_LENGTH_CHARACTERS, so any
      // non-trivial byte pattern works.
      bytes[i] = (i * 17) & 0xff;
    }
  }
}

/** Run `git` against `cwd`; throw with stderr on failure. */
async function git(args: readonly string[], cwd: string): Promise<void> {
  const command = new Deno.Command("git", {
    args: [...args],
    cwd,
    stdout: "piped",
    stderr: "piped",
    env: {
      GIT_AUTHOR_NAME: "makina-test",
      GIT_AUTHOR_EMAIL: "makina-test@example.com",
      GIT_COMMITTER_NAME: "makina-test",
      GIT_COMMITTER_EMAIL: "makina-test@example.com",
    },
  });
  const result = await command.output();
  if (!result.success) {
    const stderr = new TextDecoder().decode(result.stderr);
    throw new Error(`git ${args.join(" ")} failed (exit ${result.code}): ${stderr}`);
  }
}

/**
 * Build a self-contained source repo with two commits on `main`, then
 * return a `file://` URL the worktree manager can clone from.
 */
async function makeSourceRepo(): Promise<{ dir: string; url: string }> {
  const dir = await Deno.makeTempDir({ prefix: "makina-supervisor-src-" });
  await git(["init", "--quiet", "--initial-branch=main", dir], dir);
  await Deno.writeTextFile(join(dir, "README.md"), "# fixture\n");
  await git(["add", "."], dir);
  await git(["commit", "--quiet", "-m", "feat: initial commit"], dir);
  await Deno.writeTextFile(join(dir, "VERSION"), "0.0.1\n");
  await git(["add", "."], dir);
  await git(["commit", "--quiet", "-m", "chore: bump"], dir);
  return { dir, url: `file://${dir}` };
}

async function makeRig(): Promise<SupervisorTestRig> {
  const workspace = await Deno.makeTempDir({ prefix: "makina-supervisor-ws-" });
  const source = await makeSourceRepo();
  const statePath = join(workspace, "state.json");
  return {
    workspace,
    statePath,
    source,
    githubClient: new InMemoryGitHubClient(),
    agentRunner: new MockAgentRunner(),
    events: [],
    repo: makeRepoFullName("koraytaylan/makina"),
    issueNumber: makeIssueNumber(1),
    clock: new DeterministicClock(),
    async cleanup() {
      await Deno.remove(workspace, { recursive: true });
      await Deno.remove(source.dir, { recursive: true });
    },
  };
}

/**
 * Pre-script the GitHub client for a happy-path run: `getIssue` returns
 * a fixture issue, `createPullRequest` returns PR #42 with head ref
 * matching the supervisor's branch naming, `requestReviewers` and
 * `mergePullRequest` succeed.
 */
function scriptHappyPath(rig: SupervisorTestRig): void {
  rig.githubClient.queueGetIssue({
    kind: "value",
    value: {
      number: rig.issueNumber,
      title: "Add a hello-world endpoint",
      body: "We need a /hello endpoint that returns 200 OK.",
      state: "open",
    },
  });
  rig.githubClient.queueCreatePullRequest({
    kind: "value",
    value: {
      number: makeIssueNumber(42),
      headSha: "deadbeefcafe",
      headRef: branchNameFor(rig.issueNumber),
      baseRef: DEFAULT_BASE_BRANCH,
      state: "open",
    },
  });
  rig.githubClient.queueRequestReviewers({ kind: "value", value: undefined });
  rig.githubClient.queueMergePullRequest({ kind: "value", value: undefined });
}

/**
 * Pre-script the agent runner with a single drafting iteration. The
 * messages are plain so the test can assert them as `agent-message`
 * events on the bus.
 */
function scriptDraftingRun(rig: SupervisorTestRig): void {
  rig.agentRunner.queueRun({
    messages: [
      { role: "assistant", text: "Reading the issue..." },
      { role: "tool-use", text: "edit src/server.ts" },
      { role: "assistant", text: "Done." },
    ],
  });
}

function recordEvents(
  rig: SupervisorTestRig,
  bus: ReturnType<typeof createEventBus>,
): { unsubscribe(): void } {
  return bus.subscribe("*", (event) => {
    rig.events.push(event);
  });
}

function stateTransitions(events: readonly TaskEvent[]): readonly {
  fromState: TaskState;
  toState: TaskState;
}[] {
  const out: { fromState: TaskState; toState: TaskState }[] = [];
  for (const event of events) {
    if (event.kind !== "state-changed") continue;
    out.push({ fromState: event.data.fromState, toState: event.data.toState });
  }
  return out;
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

Deno.test(
  "supervisor walks INIT through MERGED on the happy path and persists every transition",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig);
      scriptDraftingRun(rig);

      const bus = createEventBus();
      const subscription = recordEvents(rig, bus);
      const persistence = createPersistence({ path: rig.statePath });
      const worktreeManager = createWorktreeManager({ workspace: rig.workspace });

      const supervisor = createTaskSupervisor({
        githubClient: rig.githubClient,
        worktreeManager,
        persistence,
        eventBus: bus,
        agentRunner: rig.agentRunner,
        cloneUrlFor: () => rig.source.url,
        clock: rig.clock,
        randomSource: new FixedRandomSource(),
      });

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
      });

      // Wait for asynchronous event-bus deliveries to settle. The bus
      // pumps via the microtask queue and the test subscriber runs
      // synchronously, so a single round-trip yields enough.
      await new Promise<void>((resolve) => setTimeout(resolve, 0));

      // Final state is MERGED.
      assertEquals(finalTask.state, "MERGED");
      assertEquals(finalTask.terminalReason, "merged");
      assertEquals(finalTask.prNumber, makeIssueNumber(42));
      assertEquals(finalTask.branchName, branchNameFor(rig.issueNumber));
      assert(finalTask.worktreePath !== undefined);

      // Persistence replay reconstructs the task identically.
      const replay = await persistence.loadAll();
      assertEquals(replay.length, 1);
      assertEquals(replay[0], finalTask);

      // listTasks returns the same record we returned from start().
      const fromRegistry = supervisor.listTasks();
      assertEquals(fromRegistry, [finalTask]);
      assertEquals(supervisor.getTask(finalTask.id), finalTask);

      // The full transition timeline must appear on the bus, in order.
      const transitions = stateTransitions(rig.events);
      assertEquals(transitions, [
        // start() emits a self-edge once the INIT record is persisted.
        { fromState: "INIT", toState: "INIT" },
        { fromState: "INIT", toState: "CLONING_WORKTREE" },
        { fromState: "CLONING_WORKTREE", toState: "DRAFTING" },
        { fromState: "DRAFTING", toState: "COMMITTING" },
        { fromState: "COMMITTING", toState: "PUSHING" },
        // PUSHING → PR_OPEN: the supervisor opens the PR as the
        // observable side effect of the push completing.
        { fromState: "PUSHING", toState: "PR_OPEN" },
        // PR open → STABILIZING after the Copilot reviewer is
        // requested.
        { fromState: "PR_OPEN", toState: "STABILIZING" },
        // Three stub stabilize sub-phases (REBASE → CI → CONVERSATIONS).
        { fromState: "STABILIZING", toState: "STABILIZING" },
        { fromState: "STABILIZING", toState: "STABILIZING" },
        { fromState: "STABILIZING", toState: "STABILIZING" },
        { fromState: "STABILIZING", toState: "READY_TO_MERGE" },
        { fromState: "READY_TO_MERGE", toState: "MERGED" },
      ]);

      // Stabilize sub-phase labels appear on the bus in order. The
      // first PR_OPEN→STABILIZING transition records `REBASE`, then
      // three stub-driven self-transitions cycle through
      // `REBASE → CI → CONVERSATIONS`.
      const stabilizePhases: string[] = [];
      for (const event of rig.events) {
        if (event.kind !== "state-changed") continue;
        if (event.data.toState !== "STABILIZING") continue;
        if (event.data.stabilizePhase !== undefined) {
          stabilizePhases.push(event.data.stabilizePhase);
        }
      }
      assertEquals(stabilizePhases, ["REBASE", "REBASE", "CI", "CONVERSATIONS"]);

      // Copilot is requested as reviewer immediately after PR open.
      const callLog = rig.githubClient.recordedCalls();
      const createPrIndex = callLog.findIndex((call) => call.method === "createPullRequest");
      const requestReviewersIndex = callLog.findIndex((call) => call.method === "requestReviewers");
      assert(createPrIndex >= 0, "createPullRequest must be called");
      assert(requestReviewersIndex >= 0, "requestReviewers must be called");
      assertEquals(requestReviewersIndex, createPrIndex + 1);
      const reviewersCall = callLog[requestReviewersIndex];
      if (reviewersCall === undefined || reviewersCall.method !== "requestReviewers") {
        throw new Error("expected requestReviewers entry");
      }
      assertEquals(reviewersCall.reviewers, [COPILOT_REVIEWER_LOGIN]);
      assertEquals(reviewersCall.pullRequestNumber, makeIssueNumber(42));

      // The PR was opened with the supervisor's expected metadata.
      const createCall = callLog[createPrIndex];
      if (createCall === undefined || createCall.method !== "createPullRequest") {
        throw new Error("expected createPullRequest entry");
      }
      assertEquals(createCall.args, {
        headRef: branchNameFor(rig.issueNumber),
        baseRef: DEFAULT_BASE_BRANCH,
        title: prTitleFor(rig.issueNumber),
        body: prBodyFor(rig.issueNumber),
      });

      // Drafting messages reach the bus as `agent-message` events in
      // their original order.
      const agentEvents = rig.events.filter((event) => event.kind === "agent-message");
      assertEquals(agentEvents.length, 3);
      assertEquals(
        agentEvents.map((event) => (event.kind === "agent-message" ? event.data.text : "")),
        ["Reading the issue...", "edit src/server.ts", "Done."],
      );

      // The mock agent runner saw exactly one invocation with the
      // supervisor's branded task id.
      const invocations = rig.agentRunner.recordedInvocations();
      assertEquals(invocations.length, 1);
      assertEquals(invocations[0]?.taskId, finalTask.id);
      assertEquals(invocations[0]?.worktreePath, finalTask.worktreePath);

      // The worktree manager actually created a directory.
      const stat = await Deno.stat(finalTask.worktreePath as string);
      assert(stat.isDirectory);

      subscription.unsubscribe();
    } finally {
      await rig.cleanup();
    }
  },
);

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

Deno.test(
  "supervisor lands the task in FAILED when createPullRequest throws",
  async () => {
    const rig = await makeRig();
    try {
      rig.githubClient.queueGetIssue({
        kind: "value",
        value: {
          number: rig.issueNumber,
          title: "PR-open failure case",
          body: "GitHub returns 500 on createPullRequest.",
          state: "open",
        },
      });
      rig.githubClient.queueCreatePullRequest({
        kind: "error",
        error: new Error("HTTP 500: createPullRequest failed"),
      });
      scriptDraftingRun(rig);

      const bus = createEventBus();
      const subscription = recordEvents(rig, bus);
      const persistence = createPersistence({ path: rig.statePath });
      const worktreeManager = createWorktreeManager({ workspace: rig.workspace });

      const supervisor = createTaskSupervisor({
        githubClient: rig.githubClient,
        worktreeManager,
        persistence,
        eventBus: bus,
        agentRunner: rig.agentRunner,
        cloneUrlFor: () => rig.source.url,
        clock: rig.clock,
        randomSource: new FixedRandomSource(),
      });

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
      });

      assertEquals(finalTask.state, "FAILED");
      assert(
        finalTask.terminalReason !== undefined &&
          finalTask.terminalReason.startsWith("create-pull-request:"),
      );

      // Persistence carries the FAILED record.
      const replay = await persistence.loadAll();
      assertEquals(replay.length, 1);
      assertEquals(replay[0]?.state, "FAILED");

      // An `error` event was published before the FAILED transition.
      await new Promise<void>((resolve) => setTimeout(resolve, 0));
      const kinds = rig.events.map((event) => event.kind);
      const errorIndex = kinds.indexOf("error");
      assert(errorIndex >= 0, "error event must be published");

      subscription.unsubscribe();
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "supervisor lands the task in FAILED when requestReviewers throws after PR open",
  async () => {
    const rig = await makeRig();
    try {
      rig.githubClient.queueGetIssue({
        kind: "value",
        value: {
          number: rig.issueNumber,
          title: "Reviewer failure case",
          body: "requestReviewers fails after PR open.",
          state: "open",
        },
      });
      rig.githubClient.queueCreatePullRequest({
        kind: "value",
        value: {
          number: makeIssueNumber(99),
          headSha: "abc",
          headRef: branchNameFor(rig.issueNumber),
          baseRef: DEFAULT_BASE_BRANCH,
          state: "open",
        },
      });
      rig.githubClient.queueRequestReviewers({
        kind: "error",
        error: new Error("422 Unprocessable Entity"),
      });
      scriptDraftingRun(rig);

      const bus = createEventBus();
      recordEvents(rig, bus);
      const persistence = createPersistence({ path: rig.statePath });
      const worktreeManager = createWorktreeManager({ workspace: rig.workspace });

      const supervisor = createTaskSupervisor({
        githubClient: rig.githubClient,
        worktreeManager,
        persistence,
        eventBus: bus,
        agentRunner: rig.agentRunner,
        cloneUrlFor: () => rig.source.url,
        clock: rig.clock,
        randomSource: new FixedRandomSource(),
      });

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
      });

      assertEquals(finalTask.state, "FAILED");
      assertEquals(finalTask.prNumber, makeIssueNumber(99));
      assert(
        finalTask.terminalReason !== undefined &&
          finalTask.terminalReason.startsWith("request-reviewers:"),
      );
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "supervisor stops at READY_TO_MERGE when mergeMode is manual",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig);
      // The merge call should *not* fire on manual mode; clear the
      // queued mergePullRequest so any accidental call surfaces as
      // "no scripted reply queued".
      // (scriptHappyPath queues one; we replace it with nothing by
      // shifting its entry off the timeline via a fresh client below.)
      // Easier: drop the merge entry by re-scripting from a clean slate.
      const fresh = new InMemoryGitHubClient();
      fresh.queueGetIssue({
        kind: "value",
        value: {
          number: rig.issueNumber,
          title: "Manual merge",
          body: "Operator merges manually.",
          state: "open",
        },
      });
      fresh.queueCreatePullRequest({
        kind: "value",
        value: {
          number: makeIssueNumber(7),
          headSha: "abcdef",
          headRef: branchNameFor(rig.issueNumber),
          baseRef: DEFAULT_BASE_BRANCH,
          state: "open",
        },
      });
      fresh.queueRequestReviewers({ kind: "value", value: undefined });
      scriptDraftingRun(rig);

      const bus = createEventBus();
      const persistence = createPersistence({ path: rig.statePath });
      const worktreeManager = createWorktreeManager({ workspace: rig.workspace });

      const supervisor = createTaskSupervisor({
        githubClient: fresh,
        worktreeManager,
        persistence,
        eventBus: bus,
        agentRunner: rig.agentRunner,
        cloneUrlFor: () => rig.source.url,
        clock: rig.clock,
        randomSource: new FixedRandomSource(),
      });

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "manual",
      });

      assertEquals(finalTask.state, "READY_TO_MERGE");
      const calls = fresh.recordedCalls();
      assert(
        !calls.some((call) => call.method === "mergePullRequest"),
        "mergePullRequest must not be invoked in manual mode",
      );
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "supervisor lands the task in FAILED when ensureBareClone throws",
  async () => {
    const rig = await makeRig();
    try {
      // Pointing the cloneUrl at a nonexistent local file makes git
      // clone fail with a real subprocess error — exercising the
      // production worktree-manager error path without mocking it.
      const bus = createEventBus();
      const subscription = recordEvents(rig, bus);
      const persistence = createPersistence({ path: rig.statePath });
      const worktreeManager = createWorktreeManager({ workspace: rig.workspace });

      const supervisor = createTaskSupervisor({
        githubClient: rig.githubClient,
        worktreeManager,
        persistence,
        eventBus: bus,
        agentRunner: rig.agentRunner,
        cloneUrlFor: () => "file:///definitely-does-not-exist-makina",
        clock: rig.clock,
        randomSource: new FixedRandomSource(),
      });

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
      });

      assertEquals(finalTask.state, "FAILED");
      assert(
        finalTask.terminalReason !== undefined &&
          finalTask.terminalReason.startsWith("worktree-clone:"),
      );

      await new Promise<void>((resolve) => setTimeout(resolve, 0));
      const errorEvent = rig.events.find((event) => event.kind === "error");
      assert(errorEvent !== undefined);

      subscription.unsubscribe();
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "supervisor lands the task in FAILED when getIssue throws during DRAFTING",
  async () => {
    const rig = await makeRig();
    try {
      rig.githubClient.queueGetIssue({
        kind: "error",
        error: new Error("404 Not Found"),
      });

      const bus = createEventBus();
      const persistence = createPersistence({ path: rig.statePath });
      const worktreeManager = createWorktreeManager({ workspace: rig.workspace });

      const supervisor = createTaskSupervisor({
        githubClient: rig.githubClient,
        worktreeManager,
        persistence,
        eventBus: bus,
        agentRunner: rig.agentRunner,
        cloneUrlFor: () => rig.source.url,
        clock: rig.clock,
        randomSource: new FixedRandomSource(),
      });

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
      });

      assertEquals(finalTask.state, "FAILED");
      assert(
        finalTask.terminalReason !== undefined &&
          finalTask.terminalReason.startsWith("drafting:"),
      );
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "supervisor lands the task in FAILED when mergePullRequest throws",
  async () => {
    const rig = await makeRig();
    try {
      rig.githubClient.queueGetIssue({
        kind: "value",
        value: {
          number: rig.issueNumber,
          title: "Merge failure",
          body: "Body.",
          state: "open",
        },
      });
      rig.githubClient.queueCreatePullRequest({
        kind: "value",
        value: {
          number: makeIssueNumber(13),
          headSha: "13",
          headRef: branchNameFor(rig.issueNumber),
          baseRef: DEFAULT_BASE_BRANCH,
          state: "open",
        },
      });
      rig.githubClient.queueRequestReviewers({ kind: "value", value: undefined });
      rig.githubClient.queueMergePullRequest({
        kind: "error",
        error: new Error("405 Method Not Allowed: not mergeable"),
      });
      scriptDraftingRun(rig);

      const bus = createEventBus();
      const persistence = createPersistence({ path: rig.statePath });
      const worktreeManager = createWorktreeManager({ workspace: rig.workspace });

      const supervisor = createTaskSupervisor({
        githubClient: rig.githubClient,
        worktreeManager,
        persistence,
        eventBus: bus,
        agentRunner: rig.agentRunner,
        cloneUrlFor: () => rig.source.url,
        clock: rig.clock,
        randomSource: new FixedRandomSource(),
      });

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
      });

      assertEquals(finalTask.state, "FAILED");
      assert(
        finalTask.terminalReason !== undefined &&
          finalTask.terminalReason.startsWith("merge:"),
      );
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "supervisor lands the task in NEEDS_HUMAN when the rebase phase exhausts its budget",
  async () => {
    const rig = await makeRig();
    try {
      // Use a fresh GitHub client (not `rig.githubClient` via
      // `scriptHappyPath`) because NEEDS_HUMAN means we never reach
      // merge, so we deliberately do NOT queue a `mergePullRequest`
      // reply: an unexpected merge attempt would surface as
      // "no scripted reply queued" and fail the test loudly.
      const fresh = new InMemoryGitHubClient();
      fresh.queueGetIssue({
        kind: "value",
        value: {
          number: rig.issueNumber,
          title: "Conflict scenario",
          body: "Body.",
          state: "open",
        },
      });
      fresh.queueCreatePullRequest({
        kind: "value",
        value: {
          number: makeIssueNumber(11),
          headSha: "deadbeef",
          headRef: branchNameFor(rig.issueNumber),
          baseRef: DEFAULT_BASE_BRANCH,
          state: "open",
        },
      });
      fresh.queueRequestReviewers({ kind: "value", value: undefined });
      scriptDraftingRun(rig);

      // Pattern-matching git invoker that always reports conflicts so
      // the rebase phase exhausts its iteration budget. Each call
      // returns the right reply for the argv shape; the rebase phase
      // loops MAX_TASK_ITERATIONS times before surrendering.
      const SUCCESS: GitInvocationResult = { exitCode: 0, stdout: "", stderr: "" };
      const conflictingDiff: GitInvocationResult = {
        exitCode: 0,
        stdout: "src/conflict.ts\n",
        stderr: "",
      };
      const invoker: StabilizeGitInvoker = (args, _options) => {
        if (args[0] === "fetch") return Promise.resolve(SUCCESS);
        if (args[0] === "rev-parse") {
          // The rebase phase captures the base-branch SHA right after
          // fetch so the conflict prompt can embed it. Stub a stable
          // value so the loop keeps iterating into the conflict path.
          return Promise.resolve({
            exitCode: 0,
            stdout: "feedfacefeedfacefeedfacefeedfacefeedface\n",
            stderr: "",
          });
        }
        if (args[0] === "rebase") {
          if (args[1] === "--continue") {
            return Promise.resolve({
              exitCode: 1,
              stdout: "",
              stderr: "still conflicting",
            });
          }
          if (args[1] === "--abort") return Promise.resolve(SUCCESS);
          // Initial `git rebase <ref>` — emit a conflict so the loop
          // enters the agent-resolve path on the first pass.
          return Promise.resolve({
            exitCode: 1,
            stdout: "",
            stderr: "CONFLICT",
          });
        }
        if (args[0] === "diff") return Promise.resolve(conflictingDiff);
        if (args[0] === "add") return Promise.resolve(SUCCESS);
        return Promise.reject(
          new Error(`unexpected git invocation: git ${args.join(" ")}`),
        );
      };

      // Queue an agent run for every iteration the budget allows. We
      // queue MAX_TASK_ITERATIONS runs (the default budget) so each
      // iteration's MockAgentRunner.runAgent finds a scripted reply.
      for (let i = 0; i < MAX_TASK_ITERATIONS; i += 1) {
        rig.agentRunner.queueRun({
          messages: [{ role: "assistant", text: `iteration ${i + 1}` }],
        });
      }

      const bus = createEventBus();
      const subscription = recordEvents(rig, bus);
      const persistence = createPersistence({ path: rig.statePath });
      const worktreeManager = createWorktreeManager({ workspace: rig.workspace });

      const supervisor = createTaskSupervisor({
        githubClient: fresh,
        worktreeManager,
        persistence,
        eventBus: bus,
        agentRunner: rig.agentRunner,
        cloneUrlFor: () => rig.source.url,
        clock: rig.clock,
        randomSource: new FixedRandomSource(),
        gitInvoker: invoker,
        conflictFileReader: () =>
          Promise.resolve(
            "<<<<<<< HEAD\nour\n=======\ntheir\n>>>>>>> origin/main\n",
          ),
      });

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
      });

      assertEquals(finalTask.state, "NEEDS_HUMAN");
      assert(
        finalTask.terminalReason !== undefined &&
          finalTask.terminalReason.includes("stabilize-rebase") &&
          finalTask.terminalReason.includes("src/conflict.ts"),
        `expected rebase NEEDS_HUMAN reason; got ${finalTask.terminalReason}`,
      );
      assertEquals(
        finalTask.terminalReason,
        formatRebaseNeedsHumanReason(["src/conflict.ts"]),
      );

      // The worktree directory is preserved (Lesson #15: the rebase
      // phase aborts the rebase but never tears the worktree down).
      const stat = await Deno.stat(finalTask.worktreePath as string);
      assert(stat.isDirectory);

      // Persistence carries the NEEDS_HUMAN record.
      const replay = await persistence.loadAll();
      assertEquals(replay.length, 1);
      assertEquals(replay[0]?.state, "NEEDS_HUMAN");

      // No merge attempt was made.
      const calls = fresh.recordedCalls();
      assert(
        !calls.some((call) => call.method === "mergePullRequest"),
        "mergePullRequest must not be invoked when the rebase phase escalates",
      );

      subscription.unsubscribe();
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "supervisor lands the task in FAILED when git fetch errors during the rebase phase",
  async () => {
    const rig = await makeRig();
    try {
      const fresh = new InMemoryGitHubClient();
      fresh.queueGetIssue({
        kind: "value",
        value: {
          number: rig.issueNumber,
          title: "Fetch failure",
          body: "Body.",
          state: "open",
        },
      });
      fresh.queueCreatePullRequest({
        kind: "value",
        value: {
          number: makeIssueNumber(13),
          headSha: "abc",
          headRef: branchNameFor(rig.issueNumber),
          baseRef: DEFAULT_BASE_BRANCH,
          state: "open",
        },
      });
      fresh.queueRequestReviewers({ kind: "value", value: undefined });
      scriptDraftingRun(rig);

      const invoker: StabilizeGitInvoker = (_args, _options) =>
        Promise.resolve({
          exitCode: 128,
          stdout: "",
          stderr: "fatal: could not read from remote",
        });

      const bus = createEventBus();
      const persistence = createPersistence({ path: rig.statePath });
      const worktreeManager = createWorktreeManager({ workspace: rig.workspace });

      const supervisor = createTaskSupervisor({
        githubClient: fresh,
        worktreeManager,
        persistence,
        eventBus: bus,
        agentRunner: rig.agentRunner,
        cloneUrlFor: () => rig.source.url,
        clock: rig.clock,
        randomSource: new FixedRandomSource(),
        gitInvoker: invoker,
      });

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
      });

      assertEquals(finalTask.state, "FAILED");
      assert(
        finalTask.terminalReason !== undefined &&
          finalTask.terminalReason.startsWith("stabilize-rebase-fetch:"),
        `expected stabilize-rebase-fetch terminalReason; got ${finalTask.terminalReason}`,
      );
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "supervisor allows a fresh start once the prior task reaches a terminal state",
  async () => {
    const rig = await makeRig();
    try {
      // First task: drafting fails so it lands in FAILED quickly.
      rig.githubClient.queueGetIssue({
        kind: "error",
        error: new Error("404 Not Found"),
      });
      // Second task: full happy path with fresh queues.
      rig.githubClient.queueGetIssue({
        kind: "value",
        value: {
          number: rig.issueNumber,
          title: "Round 2",
          body: "Body.",
          state: "open",
        },
      });
      rig.githubClient.queueCreatePullRequest({
        kind: "value",
        value: {
          number: makeIssueNumber(2),
          headSha: "abc",
          headRef: branchNameFor(rig.issueNumber),
          baseRef: DEFAULT_BASE_BRANCH,
          state: "open",
        },
      });
      rig.githubClient.queueRequestReviewers({ kind: "value", value: undefined });
      rig.githubClient.queueMergePullRequest({ kind: "value", value: undefined });
      rig.agentRunner.queueRun({ messages: [{ role: "assistant", text: "ok" }] });

      const bus = createEventBus();
      const persistence = createPersistence({ path: rig.statePath });
      const worktreeManager = createWorktreeManager({ workspace: rig.workspace });

      const supervisor = createTaskSupervisor({
        githubClient: rig.githubClient,
        worktreeManager,
        persistence,
        eventBus: bus,
        agentRunner: rig.agentRunner,
        cloneUrlFor: () => rig.source.url,
        clock: rig.clock,
        randomSource: new FixedRandomSource(),
      });

      const failedTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
      });
      assertEquals(failedTask.state, "FAILED");

      const mergedTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
      });
      assertEquals(mergedTask.state, "MERGED");
      assert(failedTask.id !== mergedTask.id);

      // Both records survive in persistence: the FAILED entry is not
      // overwritten when the next start mints a new task id.
      const replay = await persistence.loadAll();
      assertEquals(replay.length, 2);
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "supervisor rejects a duplicate start for the same (repo, issue)",
  async () => {
    const rig = await makeRig();
    try {
      // Script a happy path so the first start runs to completion, then
      // assert the SECOND start (while still terminal) is allowed but
      // an in-flight start would be rejected. We exercise the rejection
      // by starting a second time *before* the first finishes — the
      // FSM walks synchronously enough for this to be hard, so we
      // instead drive the path manually: queue only the worktree-clone
      // dependency, leave PR creation un-scripted so the FSM stalls.
      rig.githubClient.queueGetIssue({
        kind: "value",
        value: {
          number: rig.issueNumber,
          title: "Duplicate start",
          body: "Body.",
          state: "open",
        },
      });
      rig.githubClient.queueCreatePullRequest({
        kind: "value",
        value: {
          number: makeIssueNumber(1),
          headSha: "x",
          headRef: branchNameFor(rig.issueNumber),
          baseRef: DEFAULT_BASE_BRANCH,
          state: "open",
        },
      });
      rig.githubClient.queueRequestReviewers({ kind: "value", value: undefined });
      rig.githubClient.queueMergePullRequest({ kind: "value", value: undefined });
      scriptDraftingRun(rig);

      const bus = createEventBus();
      const persistence = createPersistence({ path: rig.statePath });
      const worktreeManager = createWorktreeManager({ workspace: rig.workspace });

      const supervisor = createTaskSupervisor({
        githubClient: rig.githubClient,
        worktreeManager,
        persistence,
        eventBus: bus,
        agentRunner: rig.agentRunner,
        cloneUrlFor: () => rig.source.url,
        clock: rig.clock,
        randomSource: new FixedRandomSource(),
      });

      // Kick off the first start without awaiting; immediately attempt
      // a second one for the same (repo, issue). The duplicate detector
      // runs synchronously inside `start()` before any await so the
      // duplicate throws even though the first task hasn't reached a
      // terminal state yet.
      const firstStart = supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
      });

      await assertRejects(
        () => supervisor.start({ repo: rig.repo, issueNumber: rig.issueNumber }),
        SupervisorError,
        "task already in flight",
      );

      const finalTask = await firstStart;
      assertEquals(finalTask.state, "MERGED");

      // After the first task is terminal, a fresh start IS allowed.
      // We re-script the github client because the queues are drained.
      rig.githubClient.queueGetIssue({
        kind: "value",
        value: {
          number: rig.issueNumber,
          title: "Round 2",
          body: "Body.",
          state: "open",
        },
      });
      rig.githubClient.queueCreatePullRequest({
        kind: "value",
        value: {
          number: makeIssueNumber(2),
          headSha: "y",
          headRef: branchNameFor(rig.issueNumber),
          baseRef: DEFAULT_BASE_BRANCH,
          state: "open",
        },
      });
      rig.githubClient.queueRequestReviewers({ kind: "value", value: undefined });
      rig.githubClient.queueMergePullRequest({ kind: "value", value: undefined });
      rig.agentRunner.queueRun({
        messages: [{ role: "assistant", text: "round 2" }],
      });
      const round2 = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
      });
      assertEquals(round2.state, "MERGED");
      assertEquals(round2.id !== finalTask.id, true);
    } finally {
      await rig.cleanup();
    }
  },
);
