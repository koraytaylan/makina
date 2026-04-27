/**
 * Integration test for the `/merge <task-id>` slash command, end-to-end
 * against the daemon server.
 *
 * Boots `startDaemon()` on a `Deno.makeTempDir`-backed Unix socket with
 * a real {@link createTaskSupervisor} (driving an
 * {@link InMemoryGitHubClient} + {@link MockAgentRunner} +
 * {@link RecordingWorktreeManager}). A custom `command` handler is wired
 * in that decodes the `/merge <task-id>` envelope, looks the supervisor
 * up, and replies with a precise `ack { ok }` per the brief:
 *
 *  - `command { name: "merge", args: [taskId] }` → calls
 *    `supervisor.mergeReadyTask(taskId)` and replies `ack { ok: true }`
 *    on success.
 *  - The same call against a task in a non-`READY_TO_MERGE` state
 *    must reply with `ack { ok: false, error: <descriptive> }`.
 *  - The same call against an unknown task id must reply with
 *    `ack { ok: false }`.
 *
 * The test connects a real client over `Deno.connect`, sends the
 * encoded `command` envelope, and asserts the replies.
 */

import { assert, assertEquals } from "@std/assert";
import { join } from "@std/path";

import { decode, encode } from "../../src/ipc/codec.ts";
import { type AckPayload, type MessageEnvelope } from "../../src/ipc/protocol.ts";
import {
  branchNameFor,
  createTaskSupervisor,
  DEFAULT_BASE_BRANCH,
  type SupervisorClock,
  SupervisorError,
  type SupervisorRandomSource,
  type TaskSupervisorImpl,
} from "../../src/daemon/supervisor.ts";
import { startDaemon } from "../../src/daemon/server.ts";
import { createEventBus } from "../../src/daemon/event-bus.ts";
import { createPersistence } from "../../src/daemon/persistence.ts";
import {
  type IssueNumber,
  makeIssueNumber,
  makeRepoFullName,
  makeTaskId,
  type RepoFullName,
  type TaskId,
} from "../../src/types.ts";
import type { StabilizeGitInvoker } from "../../src/daemon/stabilize.ts";
import type { WorktreeManagerImpl } from "../../src/daemon/worktree-manager.ts";
import { InMemoryGitHubClient } from "../helpers/in_memory_github_client.ts";
import { MockAgentRunner } from "../helpers/mock_agent_runner.ts";

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

class DeterministicClock implements SupervisorClock {
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
      bytes[i] = (i * 17) & 0xff;
    }
  }
}

class RecordingWorktreeManager implements WorktreeManagerImpl {
  readonly removed: TaskId[] = [];
  private readonly registered = new Map<TaskId, string>();

  ensureBareClone(_repo: RepoFullName, _remoteUrl: string): Promise<string> {
    return Promise.resolve("/tmp/makina-fake/bare.git");
  }
  createWorktreeForIssue(
    _repo: RepoFullName,
    issueNumber: IssueNumber,
  ): Promise<string> {
    return Promise.resolve(`/tmp/makina-fake/wt/issue-${issueNumber}`);
  }
  removeWorktree(taskId: TaskId): Promise<void> {
    this.removed.push(taskId);
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

/**
 * Wire a `command` handler that maps `/merge <task-id>` to
 * `supervisor.mergeReadyTask()`. Production wiring lives in `main.ts`
 * once the daemon's full command dispatcher lands; this test
 * encapsulates the same routing so the integration assertion runs
 * end-to-end against the real socket.
 *
 * Other commands (`issue`, `status`, …) are intentionally
 * unimplemented here — the integration target is the `/merge` path.
 */
function buildCommandHandler(supervisor: TaskSupervisorImpl) {
  return async (
    envelope: Extract<MessageEnvelope, { type: "command" }>,
  ): Promise<AckPayload> => {
    const { name, args } = envelope.payload;
    if (name !== "merge") {
      return { ok: false, error: `unsupported command: /${name}` };
    }
    const rawTaskId = args[0];
    if (rawTaskId === undefined) {
      return { ok: false, error: "/merge requires <task-id>" };
    }
    let taskId: TaskId;
    try {
      taskId = makeTaskId(rawTaskId);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      return { ok: false, error: message };
    }
    try {
      // Inspect the resulting task: `mergeReadyTask` returns the
      // post-merge record without throwing for FSM-internal failures
      // (a non-mergeable PR escalates to `NEEDS_HUMAN`; a transient
      // API fault lands at `FAILED`). Reporting these as
      // `{ ok: false, error: ... }` lets the client distinguish
      // "merged" from "escalated-to-human" via the ack alone, in
      // addition to the supervisor's `state-changed` event stream.
      const merged = await supervisor.mergeReadyTask(taskId);
      if (merged.state === "MERGED") {
        return { ok: true };
      }
      const reason = merged.terminalReason ?? merged.state;
      return {
        ok: false,
        error: `task ${taskId} did not merge (final state: ${merged.state}): ${reason}`,
      };
    } catch (error) {
      if (error instanceof SupervisorError) {
        return { ok: false, error: error.message };
      }
      throw error;
    }
  };
}

interface MergeCommandRig {
  readonly socketPath: string;
  readonly supervisor: TaskSupervisorImpl;
  readonly githubClient: InMemoryGitHubClient;
  readonly worktreeManager: RecordingWorktreeManager;
  readonly agentRunner: MockAgentRunner;
  readonly repo: RepoFullName;
  readonly issueNumber: IssueNumber;
  cleanup(): Promise<void>;
}

/**
 * Always-clean rebase stub. The /merge command tests exercise the daemon's
 * IPC layer; the stabilize-rebase phase has its own dedicated coverage in
 * `tests/unit/stabilize_rebase_test.ts`. We short-circuit the three calls
 * a clean rebase makes — fetch, rev-parse, and rebase — so the merge
 * command tests reach READY_TO_MERGE without needing a real worktree.
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

async function makeRig(): Promise<MergeCommandRig> {
  const dir = await Deno.makeTempDir({ prefix: "makina-merge-cmd-" });
  const socketPath = join(dir, "sock");
  const githubClient = new InMemoryGitHubClient();
  const worktreeManager = new RecordingWorktreeManager();
  const agentRunner = new MockAgentRunner();
  const supervisor = createTaskSupervisor({
    githubClient,
    worktreeManager,
    persistence: createPersistence({ path: join(dir, "state.json") }),
    eventBus: createEventBus(),
    agentRunner,
    cloneUrlFor: () => "file:///does-not-matter",
    clock: new DeterministicClock(),
    randomSource: new FixedRandomSource(),
    preserveWorktreeOnMerge: false,
    gitInvoker: ALWAYS_CLEAN_REBASE_INVOKER,
  });
  return {
    socketPath,
    supervisor,
    githubClient,
    worktreeManager,
    agentRunner,
    repo: makeRepoFullName("koraytaylan/makina"),
    issueNumber: makeIssueNumber(7),
    cleanup: () => Deno.remove(dir, { recursive: true }),
  };
}

async function connectClient(socketPath: string) {
  const conn = await Deno.connect({ transport: "unix", path: socketPath });
  const writer = conn.writable.getWriter();
  const reader = (async function* () {
    for await (const envelope of decode(conn.readable)) {
      yield envelope;
    }
  })();
  return {
    send: async (envelope: MessageEnvelope) => {
      await writer.write(encode(envelope));
    },
    next: async (): Promise<MessageEnvelope> => {
      const result = await reader.next();
      if (result.done) {
        throw new Error("connection closed before next envelope");
      }
      return result.value;
    },
    close: async () => {
      try {
        await writer.close();
      } catch { /* ignore */ }
      try {
        conn.close();
      } catch { /* ignore */ }
    },
  };
}

function scriptParkedTask(rig: MergeCommandRig, prNumber: IssueNumber): void {
  rig.githubClient.queueGetIssue({
    kind: "value",
    value: {
      number: rig.issueNumber,
      title: "T",
      body: "B",
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
  rig.agentRunner.queueRun({
    messages: [{ role: "assistant", text: "ok" }],
  });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

Deno.test(
  "/merge against a READY_TO_MERGE manual task auto-merges and replies ack { ok: true }",
  async () => {
    const rig = await makeRig();
    try {
      scriptParkedTask(rig, makeIssueNumber(50));
      // After /merge: a single mergePullRequest reply.
      rig.githubClient.queueMergePullRequest({
        kind: "value",
        value: undefined,
      });
      const parked = await rig.supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "manual",
      });
      assertEquals(parked.state, "READY_TO_MERGE");

      const handle = await startDaemon({
        socketPath: rig.socketPath,
        handlers: {
          command: buildCommandHandler(rig.supervisor),
        },
      });
      try {
        const client = await connectClient(rig.socketPath);
        try {
          await client.send({
            id: "merge-1",
            type: "command",
            payload: { name: "merge", args: [parked.id] },
          });
          const reply = await client.next();
          assertEquals(reply.type, "ack");
          const payload = reply.payload as AckPayload;
          assertEquals(payload.ok, true);
        } finally {
          await client.close();
        }
      } finally {
        await handle.stop();
      }

      // The supervisor merged the parked task.
      const finalTask = rig.supervisor.getTask(parked.id);
      assertEquals(finalTask?.state, "MERGED");
      const mergeCall = rig.githubClient
        .recordedCalls()
        .find((call) => call.method === "mergePullRequest");
      assert(mergeCall !== undefined);
      // Worktree was cleaned up.
      assertEquals(rig.worktreeManager.removed, [parked.id]);
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "/merge against a non-READY_TO_MERGE task replies ack { ok: false } with a clear error",
  async () => {
    const rig = await makeRig();
    try {
      // Drafting fails so the task lands in FAILED.
      rig.githubClient.queueGetIssue({
        kind: "error",
        error: new Error("404"),
      });
      const failed = await rig.supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "manual",
      });
      assertEquals(failed.state, "FAILED");

      const handle = await startDaemon({
        socketPath: rig.socketPath,
        handlers: {
          command: buildCommandHandler(rig.supervisor),
        },
      });
      try {
        const client = await connectClient(rig.socketPath);
        try {
          await client.send({
            id: "merge-bad",
            type: "command",
            payload: { name: "merge", args: [failed.id] },
          });
          const reply = await client.next();
          assertEquals(reply.type, "ack");
          const payload = reply.payload as AckPayload;
          assertEquals(payload.ok, false);
          assert(payload.error !== undefined);
          assert(payload.error.includes("READY_TO_MERGE"));
          // No GitHub merge call was attempted.
          assert(
            !rig.githubClient
              .recordedCalls()
              .some((call) => call.method === "mergePullRequest"),
          );
        } finally {
          await client.close();
        }
      } finally {
        await handle.stop();
      }
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "/merge against an unknown task id replies ack { ok: false }",
  async () => {
    const rig = await makeRig();
    try {
      const handle = await startDaemon({
        socketPath: rig.socketPath,
        handlers: {
          command: buildCommandHandler(rig.supervisor),
        },
      });
      try {
        const client = await connectClient(rig.socketPath);
        try {
          await client.send({
            id: "merge-unknown",
            type: "command",
            payload: { name: "merge", args: ["task_does_not_exist"] },
          });
          const reply = await client.next();
          assertEquals(reply.type, "ack");
          const payload = reply.payload as AckPayload;
          assertEquals(payload.ok, false);
          assert(payload.error !== undefined);
          assert(payload.error.includes("no task found"));
        } finally {
          await client.close();
        }
      } finally {
        await handle.stop();
      }
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "/merge with no <task-id> replies ack { ok: false }",
  async () => {
    const rig = await makeRig();
    try {
      const handle = await startDaemon({
        socketPath: rig.socketPath,
        handlers: {
          command: buildCommandHandler(rig.supervisor),
        },
      });
      try {
        const client = await connectClient(rig.socketPath);
        try {
          await client.send({
            id: "merge-empty",
            type: "command",
            payload: { name: "merge", args: [] },
          });
          const reply = await client.next();
          assertEquals(reply.type, "ack");
          const payload = reply.payload as AckPayload;
          assertEquals(payload.ok, false);
          assert(payload.error !== undefined);
          assert(payload.error.includes("requires"));
        } finally {
          await client.close();
        }
      } finally {
        await handle.stop();
      }
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "/merge surfaces a NEEDS_HUMAN escalation as ack { ok: false } naming the final state",
  async () => {
    const rig = await makeRig();
    try {
      scriptParkedTask(rig, makeIssueNumber(70));
      const notMergeable = new Error("Pull Request is not mergeable") as
        & Error
        & { status?: number };
      notMergeable.status = 405;
      rig.githubClient.queueMergePullRequest({
        kind: "error",
        error: notMergeable,
      });
      const parked = await rig.supervisor.start({
        repo: rig.repo,
        issueNumber: rig.issueNumber,
        mergeMode: "manual",
      });
      assertEquals(parked.state, "READY_TO_MERGE");

      const handle = await startDaemon({
        socketPath: rig.socketPath,
        handlers: {
          command: buildCommandHandler(rig.supervisor),
        },
      });
      try {
        const client = await connectClient(rig.socketPath);
        try {
          await client.send({
            id: "merge-not",
            type: "command",
            payload: { name: "merge", args: [parked.id] },
          });
          const reply = await client.next();
          assertEquals(reply.type, "ack");
          const payload = reply.payload as AckPayload;
          // The merge step itself ran (the supervisor took ownership)
          // but the task landed in NEEDS_HUMAN, not MERGED. The
          // handler reports `ok: false` with an error string that
          // names the final state so the client can distinguish
          // "merged" from "escalate-to-human" without subscribing to
          // the event stream.
          assertEquals(payload.ok, false);
          assert(payload.error !== undefined);
          assert(payload.error.includes("NEEDS_HUMAN"));
        } finally {
          await client.close();
        }
      } finally {
        await handle.stop();
      }

      const finalTask = rig.supervisor.getTask(parked.id);
      assertEquals(finalTask?.state, "NEEDS_HUMAN");
    } finally {
      await rig.cleanup();
    }
  },
);
