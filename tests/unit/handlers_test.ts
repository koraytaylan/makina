/**
 * Unit tests for `src/daemon/handlers.ts`.
 *
 * The handler factory routes IPC `command` and `prompt` envelopes onto
 * a {@link TaskSupervisorImpl}. We exercise it with a tiny stub
 * supervisor (the real one is exercised by
 * `tests/integration/main_daemon_runtime_test.ts`); the tests target
 * the dispatcher itself: command-name routing, default-repo fallback,
 * malformed payload handling, the `merge` error mapping, the `status`
 * JSON projection, and the deterministic `not yet implemented`
 * affordance.
 *
 * The supervisor stub records every call so each test can pin the
 * exact shape the handler delegates to. No global mutation; no
 * filesystem or socket IO.
 */

import { assert, assertEquals } from "@std/assert";

import { createDaemonHandlers, type StatusTaskProjection } from "../../src/daemon/handlers.ts";
import { SupervisorError, type TaskSupervisorImpl } from "../../src/daemon/supervisor.ts";
import type { AckPayload, CommandPayload, MessageEnvelope } from "../../src/ipc/protocol.ts";
import {
  type IssueNumber,
  makeIssueNumber,
  makeRepoFullName,
  makeTaskId,
  type MergeMode,
  type RepoFullName,
  type Task,
  type TaskId,
} from "../../src/types.ts";

interface StartCall {
  readonly repo: RepoFullName;
  readonly issueNumber: IssueNumber;
  readonly mergeMode?: MergeMode;
  readonly model?: string;
}

class StubSupervisor implements TaskSupervisorImpl {
  readonly startCalls: StartCall[] = [];
  readonly mergeCalls: TaskId[] = [];
  readonly tasks: Task[] = [];
  startBehavior: "succeed" | "rejectAfterAck" = "succeed";
  mergeBehavior:
    | { readonly kind: "ok" }
    | { readonly kind: "supervisor-error"; readonly error: SupervisorError }
    | { readonly kind: "raw-error"; readonly error: Error } = { kind: "ok" };

  start(args: StartCall): Promise<Task> {
    this.startCalls.push(args);
    if (this.startBehavior === "rejectAfterAck") {
      return Promise.reject(new Error("scripted background failure"));
    }
    const id = makeTaskId(`task_${args.repo}_${args.issueNumber}`);
    const now = "2026-04-26T12:00:00.000Z";
    const task: Task = {
      id,
      repo: args.repo,
      issueNumber: args.issueNumber,
      state: "INIT",
      mergeMode: args.mergeMode ?? "squash",
      model: args.model ?? "claude-sonnet-4-6",
      iterationCount: 0,
      createdAtIso: now,
      updatedAtIso: now,
    };
    this.tasks.push(task);
    return Promise.resolve(task);
  }

  listTasks(): readonly Task[] {
    return [...this.tasks];
  }

  getTask(taskId: TaskId): Task | undefined {
    return this.tasks.find((task) => task.id === taskId);
  }

  mergeReadyTask(taskId: TaskId): Promise<Task> {
    this.mergeCalls.push(taskId);
    if (this.mergeBehavior.kind === "supervisor-error") {
      return Promise.reject(this.mergeBehavior.error);
    }
    if (this.mergeBehavior.kind === "raw-error") {
      return Promise.reject(this.mergeBehavior.error);
    }
    const task = this.getTask(taskId);
    if (task === undefined) {
      return Promise.reject(
        new SupervisorError("unknown-task", `no task found for id ${taskId}`),
      );
    }
    const merged: Task = { ...task, state: "MERGED" };
    return Promise.resolve(merged);
  }

  injectTask(task: Task): void {
    this.tasks.push(task);
  }
}

function commandEnvelope(
  payload: {
    readonly name: string;
    readonly args?: readonly string[];
    readonly repo?: RepoFullName;
    readonly issueNumber?: IssueNumber;
  },
  id: string = "1",
): Extract<MessageEnvelope, { type: "command" }> {
  const built: {
    name: string;
    args: readonly string[];
    repo?: RepoFullName;
    issueNumber?: IssueNumber;
  } = {
    name: payload.name,
    args: payload.args ?? [],
  };
  if (payload.repo !== undefined) built.repo = payload.repo;
  if (payload.issueNumber !== undefined) built.issueNumber = payload.issueNumber;
  return { id, type: "command", payload: built as CommandPayload };
}

function promptEnvelope(
  taskId: string,
  text: string,
): Extract<MessageEnvelope, { type: "prompt" }> {
  return {
    id: "1",
    type: "prompt",
    payload: { taskId, text },
  };
}

const DEFAULT_REPO = makeRepoFullName("owner/default");
const ALT_REPO = makeRepoFullName("owner/alt");

interface Rig {
  readonly supervisor: StubSupervisor;
  readonly commandHandler: (
    envelope: Extract<MessageEnvelope, { type: "command" }>,
  ) => Promise<AckPayload> | AckPayload;
  readonly promptHandler: (
    envelope: Extract<MessageEnvelope, { type: "prompt" }>,
  ) => Promise<AckPayload> | AckPayload;
  readonly backgroundErrors: Array<{ readonly error: unknown; readonly context: string }>;
}

function newRig(behavior?: {
  readonly hasGitHubClientFor?: (repo: RepoFullName) => boolean;
}): Rig {
  const supervisor = new StubSupervisor();
  const backgroundErrors: Array<{ readonly error: unknown; readonly context: string }> = [];
  const handlers = createDaemonHandlers({
    supervisor,
    defaultRepo: DEFAULT_REPO,
    hasGitHubClientFor: behavior?.hasGitHubClientFor ??
      ((repo) => repo === DEFAULT_REPO || repo === ALT_REPO),
    onBackgroundError: (error, context) => {
      backgroundErrors.push({ error, context });
    },
  });
  if (handlers.command === undefined) {
    throw new Error("createDaemonHandlers returned no command handler");
  }
  if (handlers.prompt === undefined) {
    throw new Error("createDaemonHandlers returned no prompt handler");
  }
  return {
    supervisor,
    commandHandler: handlers.command,
    promptHandler: handlers.prompt,
    backgroundErrors,
  };
}

Deno.test("createDaemonHandlers: /issue starts a task with the default repo when --repo is omitted", async () => {
  const rig = newRig();
  const ack = await rig.commandHandler(
    commandEnvelope({ name: "issue", args: ["42"], issueNumber: makeIssueNumber(42) }),
  );

  assertEquals(ack, { ok: true });
  // Yield once so the background `start()` promise registers the call.
  await Promise.resolve();
  assertEquals(rig.supervisor.startCalls.length, 1);
  const call = rig.supervisor.startCalls[0];
  assert(call !== undefined);
  assertEquals(call.repo, DEFAULT_REPO);
  assertEquals(call.issueNumber, makeIssueNumber(42));
});

Deno.test("createDaemonHandlers: /issue uses the explicit --repo when supplied", async () => {
  const rig = newRig();
  const ack = await rig.commandHandler(
    commandEnvelope({
      name: "issue",
      args: ["7"],
      issueNumber: makeIssueNumber(7),
      repo: ALT_REPO,
    }),
  );

  assertEquals(ack, { ok: true });
  await Promise.resolve();
  const call = rig.supervisor.startCalls[0];
  assert(call !== undefined);
  assertEquals(call.repo, ALT_REPO);
});

Deno.test("createDaemonHandlers: /issue rejects when the repo has no GitHub installation", async () => {
  const unconfigured = makeRepoFullName("owner/no-install");
  const rig = newRig({
    hasGitHubClientFor: (repo) => repo === DEFAULT_REPO,
  });
  const ack = await rig.commandHandler(
    commandEnvelope({
      name: "issue",
      args: ["1"],
      issueNumber: makeIssueNumber(1),
      repo: unconfigured,
    }),
  );

  assertEquals(ack.ok, false);
  assertEquals(rig.supervisor.startCalls.length, 0);
});

Deno.test("createDaemonHandlers: /issue rejects when the issue number is missing", async () => {
  const rig = newRig();
  const ack = await rig.commandHandler(
    commandEnvelope({ name: "issue", args: [] }),
  );

  assertEquals(ack.ok, false);
  assertEquals(rig.supervisor.startCalls.length, 0);
});

Deno.test("createDaemonHandlers: /issue rejects a duplicate non-terminal task with a precise error", async () => {
  const rig = newRig();
  // Inject a non-terminal task so the duplicate guard fires.
  rig.supervisor.injectTask({
    id: makeTaskId("task_existing"),
    repo: DEFAULT_REPO,
    issueNumber: makeIssueNumber(99),
    state: "DRAFTING",
    mergeMode: "squash",
    model: "claude-sonnet-4-6",
    iterationCount: 0,
    createdAtIso: "2026-04-26T11:00:00.000Z",
    updatedAtIso: "2026-04-26T11:00:00.000Z",
  });

  const ack = await rig.commandHandler(
    commandEnvelope({
      name: "issue",
      args: ["99"],
      issueNumber: makeIssueNumber(99),
    }),
  );
  assertEquals(ack.ok, false);
  assertEquals(rig.supervisor.startCalls.length, 0);
});

Deno.test("createDaemonHandlers: /issue allows starting a fresh task when prior was MERGED", async () => {
  const rig = newRig();
  rig.supervisor.injectTask({
    id: makeTaskId("task_done"),
    repo: DEFAULT_REPO,
    issueNumber: makeIssueNumber(5),
    state: "MERGED",
    mergeMode: "squash",
    model: "claude-sonnet-4-6",
    iterationCount: 1,
    createdAtIso: "2026-04-26T11:00:00.000Z",
    updatedAtIso: "2026-04-26T11:30:00.000Z",
  });

  const ack = await rig.commandHandler(
    commandEnvelope({
      name: "issue",
      args: ["5"],
      issueNumber: makeIssueNumber(5),
    }),
  );
  assertEquals(ack, { ok: true });
});

Deno.test("createDaemonHandlers: /issue forwards background failures to onBackgroundError", async () => {
  const rig = newRig();
  rig.supervisor.startBehavior = "rejectAfterAck";

  const ack = await rig.commandHandler(
    commandEnvelope({
      name: "issue",
      args: ["3"],
      issueNumber: makeIssueNumber(3),
    }),
  );
  assertEquals(ack, { ok: true });
  // Microtask lets the background promise reject and the catch fire.
  await new Promise((resolve) => setTimeout(resolve, 0));
  assertEquals(rig.backgroundErrors.length, 1);
  const recorded = rig.backgroundErrors[0];
  assert(recorded !== undefined);
  assertEquals(
    (recorded.error as Error).message,
    "scripted background failure",
  );
});

Deno.test("createDaemonHandlers: /merge forwards a known task to the supervisor", async () => {
  const rig = newRig();
  const id = makeTaskId("task_merge_target");
  rig.supervisor.injectTask({
    id,
    repo: DEFAULT_REPO,
    issueNumber: makeIssueNumber(11),
    state: "READY_TO_MERGE",
    mergeMode: "manual",
    model: "claude-sonnet-4-6",
    iterationCount: 1,
    createdAtIso: "2026-04-26T11:00:00.000Z",
    updatedAtIso: "2026-04-26T11:30:00.000Z",
  });

  const ack = await rig.commandHandler(
    commandEnvelope({ name: "merge", args: [id as string] }),
  );
  assertEquals(ack, { ok: true });
  assertEquals(rig.supervisor.mergeCalls, [id]);
});

Deno.test("createDaemonHandlers: /merge rejects an unknown task id", async () => {
  const rig = newRig();
  const ack = await rig.commandHandler(
    commandEnvelope({ name: "merge", args: ["task_does_not_exist"] }),
  );
  assertEquals(ack.ok, false);
  assertEquals(rig.supervisor.mergeCalls.length, 0);
});

Deno.test("createDaemonHandlers: /merge rejects when no task id is supplied", async () => {
  const rig = newRig();
  const ack = await rig.commandHandler(
    commandEnvelope({ name: "merge", args: [] }),
  );
  assertEquals(ack.ok, false);
  assertEquals(rig.supervisor.mergeCalls.length, 0);
});

Deno.test("createDaemonHandlers: /merge surfaces SupervisorError messages verbatim", async () => {
  const rig = newRig();
  const id = makeTaskId("task_drafting");
  rig.supervisor.injectTask({
    id,
    repo: DEFAULT_REPO,
    issueNumber: makeIssueNumber(12),
    state: "DRAFTING",
    mergeMode: "squash",
    model: "claude-sonnet-4-6",
    iterationCount: 0,
    createdAtIso: "2026-04-26T11:00:00.000Z",
    updatedAtIso: "2026-04-26T11:30:00.000Z",
  });
  rig.supervisor.mergeBehavior = {
    kind: "supervisor-error",
    error: new SupervisorError(
      "not-ready-to-merge",
      `task ${id} is not at READY_TO_MERGE (current state: DRAFTING)`,
    ),
  };

  const ack = await rig.commandHandler(
    commandEnvelope({ name: "merge", args: [id as string] }),
  );
  assertEquals(ack.ok, false);
  assertEquals(
    ack.error,
    `task ${id} is not at READY_TO_MERGE (current state: DRAFTING)`,
  );
});

Deno.test("createDaemonHandlers: /merge wraps non-supervisor errors", async () => {
  const rig = newRig();
  const id = makeTaskId("task_wrapped_failure");
  rig.supervisor.injectTask({
    id,
    repo: DEFAULT_REPO,
    issueNumber: makeIssueNumber(13),
    state: "READY_TO_MERGE",
    mergeMode: "manual",
    model: "claude-sonnet-4-6",
    iterationCount: 1,
    createdAtIso: "2026-04-26T11:00:00.000Z",
    updatedAtIso: "2026-04-26T11:30:00.000Z",
  });
  rig.supervisor.mergeBehavior = {
    kind: "raw-error",
    error: new Error("network blip"),
  };

  const ack = await rig.commandHandler(
    commandEnvelope({ name: "merge", args: [id as string] }),
  );
  assertEquals(ack.ok, false);
  assertEquals(ack.error, "network blip");
});

Deno.test("createDaemonHandlers: /status projects every task and embeds JSON in the ack", async () => {
  const rig = newRig();
  rig.supervisor.injectTask({
    id: makeTaskId("task_one"),
    repo: DEFAULT_REPO,
    issueNumber: makeIssueNumber(101),
    state: "PR_OPEN",
    mergeMode: "squash",
    model: "claude-sonnet-4-6",
    iterationCount: 1,
    prNumber: makeIssueNumber(202),
    branchName: "makina/issue-101",
    worktreePath: "/tmp/wt/issue-101",
    createdAtIso: "2026-04-26T11:00:00.000Z",
    updatedAtIso: "2026-04-26T11:30:00.000Z",
  });

  const ack = await rig.commandHandler(commandEnvelope({ name: "status" }));
  assertEquals(ack.ok, true);
  assert(ack.error !== undefined);
  const decoded = JSON.parse(ack.error) as {
    readonly tasks: readonly StatusTaskProjection[];
  };
  assertEquals(decoded.tasks.length, 1);
  const projection = decoded.tasks[0];
  assert(projection !== undefined);
  assertEquals(projection.id, "task_one");
  assertEquals(projection.repo, DEFAULT_REPO as string);
  assertEquals(projection.issueNumber, 101);
  assertEquals(projection.state, "PR_OPEN");
  assertEquals(projection.prNumber, 202);
  assertEquals(projection.branchName, "makina/issue-101");
  assertEquals(projection.worktreePath, "/tmp/wt/issue-101");
});

Deno.test("createDaemonHandlers: /cancel|/retry|/logs|/switch|/repo|/help|/quit|/daemon all reply 'not yet implemented'", async () => {
  const rig = newRig();
  for (const name of ["cancel", "retry", "logs", "switch", "repo", "help", "quit", "daemon"]) {
    const ack = await rig.commandHandler(commandEnvelope({ name, args: [] }));
    assertEquals(ack.ok, false);
    assertEquals(ack.error, `/${name}: not yet implemented`);
  }
});

Deno.test("createDaemonHandlers: unknown command name yields a precise error", async () => {
  const rig = newRig();
  const ack = await rig.commandHandler(
    commandEnvelope({ name: "totally-bogus-name" }),
  );
  assertEquals(ack.ok, false);
  assertEquals(ack.error, "unknown command: totally-bogus-name");
});

Deno.test("createDaemonHandlers: prompt envelope replies 'not yet implemented'", async () => {
  const rig = newRig();
  const ack = await rig.promptHandler(
    promptEnvelope("task_id_anything", "say hi"),
  );
  assertEquals(ack.ok, false);
  assertEquals(
    ack.error,
    "prompt forwarding to per-task agent input not yet implemented (follow-up).",
  );
});
