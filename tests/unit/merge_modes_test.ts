/**
 * Unit tests for the supervisor's merge-modes handling under
 * `src/daemon/supervisor.ts`.
 *
 * Coverage targets called out in the W4-#18 brief:
 *
 *  - **Auto squash** — a `mergeMode: "squash"` task auto-merges via
 *    `GitHubClient.mergePullRequest` and lands in `MERGED`.
 *  - **Auto rebase** — same pipeline, rebase strategy.
 *  - **Manual** — the supervisor parks at `READY_TO_MERGE` and the
 *    GitHub merge endpoint is **not** invoked. A subsequent
 *    `mergeReadyTask()` call (the `/merge` slash command's plumbing)
 *    finishes the merge.
 *  - **Cleanup** — `preserveWorktreeOnMerge` flips the worktree
 *    teardown decision either way; both branches are verified against
 *    a recording {@link WorktreeManagerImpl} double *and* one
 *    integration check against a real {@link createWorktreeManager}
 *    over a `Deno.makeTempDir`-backed bare clone.
 *  - **Failure classification** — a `405` from `mergePullRequest`
 *    escalates the task to `NEEDS_HUMAN`; a transient (5xx-equivalent)
 *    failure lands the task in `FAILED`.
 *  - **`mergeReadyTask` guards** — calling it for an unknown task or
 *    a task not at `READY_TO_MERGE` rejects synchronously with a
 *    {@link SupervisorError} carrying the matching `kind`.
 *
 * Tests use the in-memory {@link InMemoryGitHubClient} for the
 * GitHub side and a recording worktree-manager double for the
 * worktree side; one test exercises the real worktree manager for the
 * "removeWorktree actually deletes the directory" check called out
 * in the brief.
 */

import { assert, assertEquals, assertRejects } from "@std/assert";
import { join } from "@std/path";

import {
  branchNameFor,
  classifyMergeError,
  createTaskSupervisor,
  DEFAULT_BASE_BRANCH,
  MERGE_NOT_MERGEABLE_HTTP_STATUSES,
  MergeError,
  type SupervisorClock,
  SupervisorError,
  type SupervisorRandomSource,
} from "../../src/daemon/supervisor.ts";
import { createEventBus } from "../../src/daemon/event-bus.ts";
import { createPersistence } from "../../src/daemon/persistence.ts";
import {
  createWorktreeManager,
  type WorktreeManagerImpl,
} from "../../src/daemon/worktree-manager.ts";
import {
  type IssueNumber,
  makeIssueNumber,
  makeRepoFullName,
  type RepoFullName,
  type Task,
  type TaskEvent,
  type TaskId,
} from "../../src/types.ts";
import { InMemoryGitHubClient } from "../helpers/in_memory_github_client.ts";
import { MockAgentRunner } from "../helpers/mock_agent_runner.ts";

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

class DeterministicClock implements SupervisorClock {
  private milliseconds = Date.UTC(2026, 3, 26, 12, 0, 0);

  /** Return the next ISO timestamp. */
  nowIso(): string {
    const iso = new Date(this.milliseconds).toISOString();
    this.milliseconds += 1_000;
    return iso;
  }
}

class FixedRandomSource implements SupervisorRandomSource {
  fillRandomBytes(bytes: Uint8Array): void {
    for (let i = 0; i < bytes.length; i += 1) {
      bytes[i] = (i * 17) & 0xff;
    }
  }
}

/**
 * Recording {@link WorktreeManagerImpl} double. Returns deterministic
 * paths so the supervisor's persisted records compare verbatim, and
 * captures every `removeWorktree` call so cleanup assertions don't
 * depend on the real filesystem.
 *
 * The supervisor's worktree side calls four methods —
 * `ensureBareClone`, `createWorktreeForIssue`, `registerTaskId`, and
 * `removeWorktree` — plus `worktreePathFor` and `pruneAll` which the
 * supervisor never invokes; the latter two are implemented as no-ops
 * for surface compatibility.
 */
class RecordingWorktreeManager implements WorktreeManagerImpl {
  readonly removed: TaskId[] = [];
  readonly removeErrors = new Map<TaskId, Error>();
  readonly registered = new Map<TaskId, string>();
  private readonly bareClone = "/tmp/makina-fake/bare.git";

  ensureBareClone(_repo: RepoFullName, _remoteUrl: string): Promise<string> {
    return Promise.resolve(this.bareClone);
  }

  createWorktreeForIssue(
    _repo: RepoFullName,
    issueNumber: IssueNumber,
  ): Promise<string> {
    return Promise.resolve(`/tmp/makina-fake/wt/issue-${issueNumber}`);
  }

  removeWorktree(taskId: TaskId): Promise<void> {
    this.removed.push(taskId);
    const error = this.removeErrors.get(taskId);
    if (error !== undefined) {
      return Promise.reject(error);
    }
    this.registered.delete(taskId);
    return Promise.resolve();
  }

  pruneAll(): Promise<void> {
    return Promise.resolve();
  }

  registerTaskId(taskId: TaskId, worktreePath: string): void {
    this.registered.set(taskId, worktreePath);
  }

  worktreePathFor(taskId: TaskId): string | undefined {
    return this.registered.get(taskId);
  }
}

interface MergeModesRig {
  readonly githubClient: InMemoryGitHubClient;
  readonly worktreeManager: RecordingWorktreeManager;
  readonly agentRunner: MockAgentRunner;
  readonly events: TaskEvent[];
  readonly statePath: string;
  readonly repo: RepoFullName;
  readonly issueNumber: IssueNumber;
  cleanup(): Promise<void>;
}

async function makeRig(): Promise<MergeModesRig> {
  const dir = await Deno.makeTempDir({ prefix: "makina-merge-modes-" });
  return {
    githubClient: new InMemoryGitHubClient(),
    worktreeManager: new RecordingWorktreeManager(),
    agentRunner: new MockAgentRunner(),
    events: [],
    statePath: join(dir, "state.json"),
    repo: makeRepoFullName("koraytaylan/makina"),
    issueNumber: makeIssueNumber(7),
    cleanup: () => Deno.remove(dir, { recursive: true }),
  };
}

/**
 * Pre-script the GitHub client for a happy-path run with a configurable
 * merge response. Includes the issue, PR, reviewer-request, and the
 * caller-supplied merge reply.
 */
function scriptHappyPath(
  rig: MergeModesRig,
  prNumber: IssueNumber,
  mergeReply:
    | { kind: "value"; value: undefined }
    | { kind: "error"; error: Error }
    | { kind: "skip" },
): void {
  rig.githubClient.queueGetIssue({
    kind: "value",
    value: {
      number: rig.issueNumber,
      title: "Add a /hello endpoint",
      body: "Body",
      state: "open",
    },
  });
  rig.githubClient.queueCreatePullRequest({
    kind: "value",
    value: {
      number: prNumber,
      headSha: "deadbeef",
      headRef: branchNameFor(rig.issueNumber),
      baseRef: DEFAULT_BASE_BRANCH,
      state: "open",
    },
  });
  rig.githubClient.queueRequestReviewers({ kind: "value", value: undefined });
  if (mergeReply.kind !== "skip") {
    rig.githubClient.queueMergePullRequest(mergeReply);
  }
  rig.agentRunner.queueRun({
    messages: [{ role: "assistant", text: "ok" }],
  });
}

function buildSupervisor(
  rig: MergeModesRig,
  preserveWorktreeOnMerge: boolean,
) {
  const bus = createEventBus();
  bus.subscribe("*", (event) => {
    rig.events.push(event);
  });
  const persistence = createPersistence({ path: rig.statePath });
  return createTaskSupervisor({
    githubClient: rig.githubClient,
    worktreeManager: rig.worktreeManager,
    persistence,
    eventBus: bus,
    agentRunner: rig.agentRunner,
    cloneUrlFor: () => "file:///does-not-matter",
    clock: new DeterministicClock(),
    randomSource: new FixedRandomSource(),
    preserveWorktreeOnMerge,
  });
}

// ---------------------------------------------------------------------------
// Auto-merge happy paths
// ---------------------------------------------------------------------------

Deno.test(
  "merge mode squash auto-merges via mergePullRequest and lands in MERGED",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig, makeIssueNumber(11), {
        kind: "value",
        value: undefined,
      });
      const supervisor = buildSupervisor(rig, false);

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "squash",
      });

      assertEquals(finalTask.state, "MERGED");
      assertEquals(finalTask.terminalReason, "merged");
      const mergeCall = rig.githubClient
        .recordedCalls()
        .find((call) => call.method === "mergePullRequest");
      assert(mergeCall !== undefined);
      if (mergeCall.method !== "mergePullRequest") throw new Error("unreachable");
      assertEquals(mergeCall.mode, "squash");
      assertEquals(mergeCall.pullRequestNumber, makeIssueNumber(11));
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "merge mode rebase forwards the rebase strategy to mergePullRequest",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig, makeIssueNumber(12), {
        kind: "value",
        value: undefined,
      });
      const supervisor = buildSupervisor(rig, false);

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "rebase",
      });

      assertEquals(finalTask.state, "MERGED");
      const mergeCall = rig.githubClient
        .recordedCalls()
        .find((call) => call.method === "mergePullRequest");
      assert(mergeCall !== undefined);
      if (mergeCall.method !== "mergePullRequest") throw new Error("unreachable");
      assertEquals(mergeCall.mode, "rebase");
    } finally {
      await rig.cleanup();
    }
  },
);

// ---------------------------------------------------------------------------
// Manual mode + /merge plumbing
// ---------------------------------------------------------------------------

Deno.test(
  "merge mode manual parks at READY_TO_MERGE and never calls mergePullRequest",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig, makeIssueNumber(21), { kind: "skip" });
      const supervisor = buildSupervisor(rig, false);

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "manual",
      });

      assertEquals(finalTask.state, "READY_TO_MERGE");
      assertEquals(finalTask.terminalReason, undefined);
      const calls = rig.githubClient.recordedCalls();
      assert(
        !calls.some((call) => call.method === "mergePullRequest"),
        "mergePullRequest must NOT be invoked in manual mode",
      );
      assertEquals(rig.worktreeManager.removed, []);
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "mergeReadyTask completes a manual task with squash by default",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig, makeIssueNumber(22), { kind: "skip" });
      // After /merge: a single mergePullRequest reply.
      rig.githubClient.queueMergePullRequest({
        kind: "value",
        value: undefined,
      });
      const supervisor = buildSupervisor(rig, false);

      const parked = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "manual",
      });
      assertEquals(parked.state, "READY_TO_MERGE");

      const merged = await supervisor.mergeReadyTask(parked.id);

      assertEquals(merged.state, "MERGED");
      assertEquals(merged.terminalReason, "merged");
      const mergeCall = rig.githubClient
        .recordedCalls()
        .find((call) => call.method === "mergePullRequest");
      assert(mergeCall !== undefined);
      if (mergeCall.method !== "mergePullRequest") throw new Error("unreachable");
      // Default substitution: manual → squash.
      assertEquals(mergeCall.mode, "squash");
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "mergeReadyTask honors an explicit override mode",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig, makeIssueNumber(23), { kind: "skip" });
      rig.githubClient.queueMergePullRequest({
        kind: "value",
        value: undefined,
      });
      const supervisor = buildSupervisor(rig, false);

      const parked = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "manual",
      });
      const merged = await supervisor.mergeReadyTask(parked.id, "rebase");

      assertEquals(merged.state, "MERGED");
      const mergeCall = rig.githubClient
        .recordedCalls()
        .find((call) => call.method === "mergePullRequest");
      assert(mergeCall !== undefined);
      if (mergeCall.method !== "mergePullRequest") throw new Error("unreachable");
      assertEquals(mergeCall.mode, "rebase");
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "mergeReadyTask rejects with kind 'unknown-task' for a missing id",
  async () => {
    const rig = await makeRig();
    try {
      const supervisor = buildSupervisor(rig, false);
      const error = await assertRejects(
        () => supervisor.mergeReadyTask("task_does_not_exist" as TaskId),
        SupervisorError,
      );
      assertEquals(error.kind, "unknown-task");
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "mergeReadyTask rejects with kind 'not-ready-to-merge' for a task in another state",
  async () => {
    const rig = await makeRig();
    try {
      // Drafting fails so the task lands in FAILED rather than
      // walking through to READY_TO_MERGE.
      rig.githubClient.queueGetIssue({
        kind: "error",
        error: new Error("404"),
      });
      const supervisor = buildSupervisor(rig, false);

      const failed = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "manual",
      });
      assertEquals(failed.state, "FAILED");

      const error = await assertRejects(
        () => supervisor.mergeReadyTask(failed.id),
        SupervisorError,
      );
      assertEquals(error.kind, "not-ready-to-merge");
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "mergeReadyTask rejects concurrent invocations with kind 'merge-in-flight'",
  async () => {
    const rig = await makeRig();
    try {
      // Park the task at READY_TO_MERGE via manual mode.
      scriptHappyPath(rig, makeIssueNumber(46), { kind: "skip" });
      const supervisor = buildSupervisor(rig, false);
      const parked = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "manual",
      });
      assertEquals(parked.state, "READY_TO_MERGE");

      // Stall the GitHub merge call so the first `mergeReadyTask`
      // promise is mid-flight when the second is issued. A deferred
      // promise resolved by the test gives us deterministic control
      // over the sequencing.
      let releaseMerge!: () => void;
      const mergeGate = new Promise<void>((resolve) => {
        releaseMerge = resolve;
      });
      const originalMerge = rig.githubClient.mergePullRequest.bind(
        rig.githubClient,
      );
      rig.githubClient.mergePullRequest = async (repo, pr, mode) => {
        await mergeGate;
        // Queue the response just-in-time so the original method has a
        // scripted reply to consume after the gate releases.
        rig.githubClient.queueMergePullRequest({
          kind: "value",
          value: undefined,
        });
        return await originalMerge(repo, pr, mode);
      };

      // Kick off the first /merge; it pauses inside `mergePullRequest`.
      const firstMerge = supervisor.mergeReadyTask(parked.id);
      // The second /merge for the same id must reject synchronously
      // with `merge-in-flight` while the first call is still pending.
      const error = await assertRejects(
        () => supervisor.mergeReadyTask(parked.id),
        SupervisorError,
      );
      assertEquals(error.kind, "merge-in-flight");

      // Release the gate so the first call can finish; the task lands
      // in MERGED and the in-flight marker is cleared in the `finally`.
      releaseMerge();
      const merged = await firstMerge;
      assertEquals(merged.state, "MERGED");
    } finally {
      await rig.cleanup();
    }
  },
);

// ---------------------------------------------------------------------------
// Cleanup behavior
// ---------------------------------------------------------------------------

Deno.test(
  "preserveWorktreeOnMerge=false invokes removeWorktree after a successful auto-merge",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig, makeIssueNumber(31), {
        kind: "value",
        value: undefined,
      });
      const supervisor = buildSupervisor(rig, false);

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "squash",
      });

      assertEquals(finalTask.state, "MERGED");
      assertEquals(rig.worktreeManager.removed, [finalTask.id]);
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "preserveWorktreeOnMerge=true skips removeWorktree after a successful auto-merge",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig, makeIssueNumber(32), {
        kind: "value",
        value: undefined,
      });
      const supervisor = buildSupervisor(rig, true);

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "squash",
      });

      assertEquals(finalTask.state, "MERGED");
      assertEquals(rig.worktreeManager.removed, []);
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "preserveWorktreeOnMerge applies to the manual-merge path too",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig, makeIssueNumber(33), { kind: "skip" });
      rig.githubClient.queueMergePullRequest({
        kind: "value",
        value: undefined,
      });
      const supervisor = buildSupervisor(rig, false);

      const parked = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "manual",
      });
      const merged = await supervisor.mergeReadyTask(parked.id);

      assertEquals(merged.state, "MERGED");
      assertEquals(rig.worktreeManager.removed, [merged.id]);
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "removeWorktree rejection keeps the task at MERGED",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig, makeIssueNumber(35), {
        kind: "value",
        value: undefined,
      });
      // Override the recording manager: reject the FIRST removeWorktree.
      const original = rig.worktreeManager.removeWorktree.bind(
        rig.worktreeManager,
      );
      let firstCall = true;
      rig.worktreeManager.removeWorktree = (taskId: TaskId): Promise<void> => {
        rig.worktreeManager.removed.push(taskId);
        if (firstCall) {
          firstCall = false;
          return Promise.reject(new Error("disk full"));
        }
        return original(taskId);
      };
      const supervisor = buildSupervisor(rig, false);

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "squash",
      });

      assertEquals(finalTask.state, "MERGED");
      assert(rig.worktreeManager.removed.length === 1);
    } finally {
      await rig.cleanup();
    }
  },
);

// ---------------------------------------------------------------------------
// Failure classification
// ---------------------------------------------------------------------------

Deno.test(
  "merge failure with HTTP 405 escalates the task to NEEDS_HUMAN",
  async () => {
    const rig = await makeRig();
    try {
      const notMergeable = new Error("Pull Request is not mergeable") as
        & Error
        & { status?: number };
      notMergeable.status = 405;
      scriptHappyPath(rig, makeIssueNumber(41), {
        kind: "error",
        error: notMergeable,
      });
      const supervisor = buildSupervisor(rig, false);

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "squash",
      });

      assertEquals(finalTask.state, "NEEDS_HUMAN");
      assert(
        finalTask.terminalReason !== undefined &&
          finalTask.terminalReason.startsWith("merge (not-mergeable)"),
      );
      // Worktree is preserved on NEEDS_HUMAN — the operator may want
      // to inspect or push fixes from the existing checkout.
      assertEquals(rig.worktreeManager.removed, []);
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "merge failure with HTTP 409 escalates the task to NEEDS_HUMAN",
  async () => {
    const rig = await makeRig();
    try {
      const conflict = new Error("Head branch was modified") as
        & Error
        & { status?: number };
      conflict.status = 409;
      scriptHappyPath(rig, makeIssueNumber(42), {
        kind: "error",
        error: conflict,
      });
      const supervisor = buildSupervisor(rig, false);

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "squash",
      });

      assertEquals(finalTask.state, "NEEDS_HUMAN");
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "merge failure with HTTP 500 lands the task in FAILED (transient)",
  async () => {
    const rig = await makeRig();
    try {
      const transient = new Error("Internal Server Error") as
        & Error
        & { status?: number };
      transient.status = 500;
      scriptHappyPath(rig, makeIssueNumber(43), {
        kind: "error",
        error: transient,
      });
      const supervisor = buildSupervisor(rig, false);

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "squash",
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
  "merge failure with no HTTP status falls back to transient (FAILED)",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig, makeIssueNumber(44), {
        kind: "error",
        error: new Error("network down"),
      });
      const supervisor = buildSupervisor(rig, false);

      const finalTask = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "squash",
      });

      assertEquals(finalTask.state, "FAILED");
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "manual /merge against a non-mergeable PR escalates to NEEDS_HUMAN",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig, makeIssueNumber(45), { kind: "skip" });
      const notMergeable = new Error("not mergeable") as
        & Error
        & { status?: number };
      notMergeable.status = 405;
      rig.githubClient.queueMergePullRequest({
        kind: "error",
        error: notMergeable,
      });
      const supervisor = buildSupervisor(rig, false);

      const parked = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "manual",
      });
      const result = await supervisor.mergeReadyTask(parked.id);

      assertEquals(result.state, "NEEDS_HUMAN");
    } finally {
      await rig.cleanup();
    }
  },
);

// ---------------------------------------------------------------------------
// classifyMergeError helper
// ---------------------------------------------------------------------------

Deno.test("classifyMergeError preserves an already-classified MergeError", () => {
  const original = new MergeError("not-mergeable", "stale head");
  const classified = classifyMergeError(original);
  assertEquals(classified, original);
});

Deno.test("classifyMergeError treats every status in MERGE_NOT_MERGEABLE_HTTP_STATUSES as not-mergeable", () => {
  for (const status of MERGE_NOT_MERGEABLE_HTTP_STATUSES) {
    const error = new Error(`HTTP ${status}`) as Error & { status?: number };
    error.status = status;
    const classified = classifyMergeError(error);
    assertEquals(classified.category, "not-mergeable");
  }
});

Deno.test("classifyMergeError treats other statuses as transient", () => {
  for (const status of [400, 401, 403, 404, 422, 500, 502, 503]) {
    const error = new Error(`HTTP ${status}`) as Error & { status?: number };
    error.status = status;
    const classified = classifyMergeError(error);
    assertEquals(classified.category, "transient");
  }
});

Deno.test("classifyMergeError treats non-Error throws as transient", () => {
  assertEquals(classifyMergeError("string oops").category, "transient");
  assertEquals(classifyMergeError(null).category, "transient");
  assertEquals(classifyMergeError(undefined).category, "transient");
  assertEquals(classifyMergeError(42).category, "transient");
});

// ---------------------------------------------------------------------------
// Cleanup against a real worktree manager
// ---------------------------------------------------------------------------

/** Run `git` against `cwd` for the integration-flavoured cleanup test. */
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
    throw new Error(`git ${args.join(" ")} failed: ${stderr}`);
  }
}

async function makeSourceRepo(): Promise<{ dir: string; url: string }> {
  const dir = await Deno.makeTempDir({ prefix: "makina-merge-modes-src-" });
  await git(["init", "--quiet", "--initial-branch=main", dir], dir);
  await Deno.writeTextFile(join(dir, "README.md"), "# fixture\n");
  await git(["add", "."], dir);
  await git(["commit", "--quiet", "-m", "feat: init"], dir);
  return { dir, url: `file://${dir}` };
}

Deno.test(
  "auto-merge with preserveWorktreeOnMerge=false removes the directory on disk",
  async () => {
    const workspace = await Deno.makeTempDir({ prefix: "makina-mm-ws-" });
    const source = await makeSourceRepo();
    try {
      const githubClient = new InMemoryGitHubClient();
      const agentRunner = new MockAgentRunner();
      const repo = makeRepoFullName("koraytaylan/makina");
      const issueNumber = makeIssueNumber(99);
      const prNumber = makeIssueNumber(99);
      githubClient.queueGetIssue({
        kind: "value",
        value: {
          number: issueNumber,
          title: "Cleanup integration",
          body: "Body",
          state: "open",
        },
      });
      githubClient.queueCreatePullRequest({
        kind: "value",
        value: {
          number: prNumber,
          headSha: "abc",
          headRef: branchNameFor(issueNumber),
          baseRef: DEFAULT_BASE_BRANCH,
          state: "open",
        },
      });
      githubClient.queueRequestReviewers({ kind: "value", value: undefined });
      githubClient.queueMergePullRequest({ kind: "value", value: undefined });
      agentRunner.queueRun({
        messages: [{ role: "assistant", text: "ok" }],
      });

      const bus = createEventBus();
      const persistence = createPersistence({
        path: join(workspace, "state.json"),
      });
      const worktreeManager = createWorktreeManager({ workspace });
      const supervisor = createTaskSupervisor({
        githubClient,
        worktreeManager,
        persistence,
        eventBus: bus,
        agentRunner,
        cloneUrlFor: () => source.url,
        clock: new DeterministicClock(),
        randomSource: new FixedRandomSource(),
        preserveWorktreeOnMerge: false,
      });

      const finalTask = await supervisor.start({
        repo,
        issueNumber,
        mergeMode: "squash",
      });

      assertEquals(finalTask.state, "MERGED");
      const path = finalTask.worktreePath;
      assert(path !== undefined);
      // The worktree directory must be gone after cleanup.
      await assertRejects(
        () => Deno.stat(path),
        Deno.errors.NotFound,
      );
    } finally {
      await Deno.remove(workspace, { recursive: true });
      await Deno.remove(source.dir, { recursive: true });
    }
  },
);

Deno.test(
  "auto-merge with preserveWorktreeOnMerge=true keeps the directory on disk",
  async () => {
    const workspace = await Deno.makeTempDir({ prefix: "makina-mm-ws-" });
    const source = await makeSourceRepo();
    try {
      const githubClient = new InMemoryGitHubClient();
      const agentRunner = new MockAgentRunner();
      const repo = makeRepoFullName("koraytaylan/makina");
      const issueNumber = makeIssueNumber(100);
      const prNumber = makeIssueNumber(100);
      githubClient.queueGetIssue({
        kind: "value",
        value: {
          number: issueNumber,
          title: "Preserve integration",
          body: "Body",
          state: "open",
        },
      });
      githubClient.queueCreatePullRequest({
        kind: "value",
        value: {
          number: prNumber,
          headSha: "abc",
          headRef: branchNameFor(issueNumber),
          baseRef: DEFAULT_BASE_BRANCH,
          state: "open",
        },
      });
      githubClient.queueRequestReviewers({ kind: "value", value: undefined });
      githubClient.queueMergePullRequest({ kind: "value", value: undefined });
      agentRunner.queueRun({
        messages: [{ role: "assistant", text: "ok" }],
      });

      const bus = createEventBus();
      const persistence = createPersistence({
        path: join(workspace, "state.json"),
      });
      const worktreeManager = createWorktreeManager({ workspace });
      const supervisor = createTaskSupervisor({
        githubClient,
        worktreeManager,
        persistence,
        eventBus: bus,
        agentRunner,
        cloneUrlFor: () => source.url,
        clock: new DeterministicClock(),
        randomSource: new FixedRandomSource(),
        preserveWorktreeOnMerge: true,
      });

      const finalTask = await supervisor.start({
        repo,
        issueNumber,
        mergeMode: "squash",
      });

      assertEquals(finalTask.state, "MERGED");
      const path = finalTask.worktreePath;
      assert(path !== undefined);
      const stat = await Deno.stat(path);
      assert(stat.isDirectory);
    } finally {
      await Deno.remove(workspace, { recursive: true });
      await Deno.remove(source.dir, { recursive: true });
    }
  },
);

// ---------------------------------------------------------------------------
// Persistence + event-bus contracts
// ---------------------------------------------------------------------------

Deno.test(
  "manual mode persists READY_TO_MERGE; mergeReadyTask persists MERGED",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig, makeIssueNumber(50), { kind: "skip" });
      rig.githubClient.queueMergePullRequest({
        kind: "value",
        value: undefined,
      });
      const supervisor = buildSupervisor(rig, false);
      const persistence = createPersistence({ path: rig.statePath });

      const parked = await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "manual",
      });
      const replay1 = await persistence.loadAll();
      assertEquals(replay1.length, 1);
      const parked1 = replay1[0] as Task;
      assertEquals(parked1.state, "READY_TO_MERGE");

      await supervisor.mergeReadyTask(parked.id);
      const replay2 = await persistence.loadAll();
      assertEquals(replay2.length, 1);
      const merged2 = replay2[0] as Task;
      assertEquals(merged2.state, "MERGED");
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "auto-merge publishes a READY_TO_MERGE → MERGED state-changed event",
  async () => {
    const rig = await makeRig();
    try {
      scriptHappyPath(rig, makeIssueNumber(60), {
        kind: "value",
        value: undefined,
      });
      const supervisor = buildSupervisor(rig, false);

      await supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "squash",
      });
      // Allow the bus's microtask queue to flush.
      await new Promise<void>((resolve) => setTimeout(resolve, 0));

      const transitions = rig.events
        .filter((event) => event.kind === "state-changed")
        .map((event) =>
          event.kind === "state-changed"
            ? { from: event.data.fromState, to: event.data.toState }
            : { from: "?", to: "?" }
        );
      const mergeTransition = transitions.find(
        (entry) => entry.from === "READY_TO_MERGE" && entry.to === "MERGED",
      );
      assert(mergeTransition !== undefined);
    } finally {
      await rig.cleanup();
    }
  },
);
