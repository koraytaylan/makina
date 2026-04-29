/**
 * Integration tests for the production daemon runtime in the
 * `daemon` branch of `main.ts` (#43).
 *
 * Spawns the binary against a synthetic workspace + `config.json` and
 * exercises the end-to-end IPC surface:
 *
 *  1. The daemon binds the configured Unix socket and answers
 *     `ping → pong` with the runtime version.
 *  2. `/issue <n>` produces an `ack { ok: true }`. The supervisor
 *     starts an FSM walk in the background; we observe it through the
 *     event bus subscription. The mock GitHub installation has no real
 *     auth so the FSM lands in `FAILED` quickly — the test asserts only
 *     that the supervisor accepted the start (a `state-changed` event
 *     arrives) and that the daemon's IPC layer surfaces the right
 *     acknowledgement.
 *  3. `/status` reflects the started task's projection. The `ack` body
 *     carries the task table on the structured `data` field (per the
 *     IPC contract; `error` is reserved for failure descriptions); the
 *     test parses it and verifies the `(repo, issueNumber)` pair routed
 *     correctly.
 *  4. An unknown command name yields `ack { ok: false, error: "unknown
 *     command: ..." }`.
 *
 * The Claude subprocess is never spawned: the supervisor's FSM never
 * reaches the DRAFTING phase because the GitHub-app private key in
 * the synthetic config is a deterministic stub that the production
 * `@octokit/auth-app` rejects. The test does not depend on that exact
 * failure mode — it only relies on (a) the daemon binding the socket,
 * (b) the IPC dispatch routing commands to the supervisor, and
 * (c) the event bus delivering the supervisor's first transition.
 */

import { assert, assertEquals } from "@std/assert";

import { decode, encode } from "@makina/core";
import {
  type AckPayload,
  type EventPayload,
  type MessageEnvelope,
  type PongPayload,
} from "@makina/core";
import { makeIssueNumber } from "@makina/core";

const STARTUP_LISTEN_TIMEOUT_MS = 30_000;
const SHUTDOWN_GRACE_MS = 2_000;
const REPLY_TIMEOUT_MS = 10_000;
const SOCKET_FILENAME = "daemon.sock";
const CONFIG_FILENAME = "config.json";

/**
 * A throwaway PEM key that conforms to the schema (non-empty path)
 * and produces a deterministic load failure inside the GitHub auth
 * factory. We do not exercise the GitHub auth path in this test;
 * the FSM only reaches the GitHub layer if a task is started, and
 * the test asserts only that the daemon's IPC surface routed the
 * command correctly. The string is a syntactic but non-functional
 * RSA private key; whether it parses is irrelevant to the assertions.
 */
const STUB_PEM = `-----BEGIN RSA PRIVATE KEY-----
MIIBOgIBAAJBAKj34GkxFhD90vcNLYLInFEX6Ppy1tPf9Cnzj4p4WGeKLs1Pt8Qu
KUpRKfFLfRYC9AIKjbJTWit+CqvjWYzvQwECAwEAAQJAIJLixBy2qpFoS4DSmoEm
o3qGy0t6z09AIJtH+5OeRV1be+N4cDYJKffGzDa88vQENZiRm0GRq6a+HPGQMd2k
TQIhAKMSvzIBnni7ot/OSie2TmJLY4SwTQAevXysE2RbFDYdAiEBCUEaRQnMnbp7
9mxDXDf6AU0cN/RPBjb9qSHDcWZHGzUCIG2Es59z8ugGrDY+pxLQnwfotadxd+Uy
v/Ow5T0q5gIJAiEAyS4RaI9YG8EWx/2w0T67ZUVAw8eOMB6BIUg0Xcu+3okCIBOs
/5OiPgoTdSy7bcF9IGpSE8ZgGKzgYQVZeN97YE00
-----END RSA PRIVATE KEY-----
`;

interface RuntimeFixture {
  readonly home: string;
  readonly workspace: string;
  readonly socketPath: string;
  readonly configPath: string;
  cleanup(): Promise<void>;
}

/** Build a self-contained `HOME` with workspace, key, and `config.json`. */
async function makeFixture(): Promise<RuntimeFixture> {
  const home = await Deno.makeTempDir({ dir: "/tmp", prefix: "makina-w5-" });
  const configDir = Deno.build.os === "darwin"
    ? `${home}/Library/Application Support/makina`
    : `${home}/.config/makina`;
  await Deno.mkdir(configDir, { recursive: true });
  const workspace = `${home}/workspace`;
  await Deno.mkdir(workspace, { recursive: true });
  const keyDir = `${home}/keys`;
  await Deno.mkdir(keyDir, { recursive: true });
  const keyPath = `${keyDir}/app.pem`;
  await Deno.writeTextFile(keyPath, STUB_PEM);
  const socketDir = `${home}/run`;
  await Deno.mkdir(socketDir, { recursive: true });
  const socketPath = `${socketDir}/${SOCKET_FILENAME}`;
  const configPath = `${configDir}/${CONFIG_FILENAME}`;
  const config = {
    github: {
      appId: 1234567,
      privateKeyPath: keyPath,
      installations: { "owner/repo": 9876543 },
      defaultRepo: "owner/repo",
    },
    agent: {
      model: "claude-sonnet-4-6",
      permissionMode: "acceptEdits",
      maxIterationsPerTask: 4,
    },
    lifecycle: {
      mergeMode: "squash",
      settlingWindowMilliseconds: 60_000,
      pollIntervalMilliseconds: 30_000,
      preserveWorktreeOnMerge: false,
    },
    workspace,
    daemon: { socketPath, autoStart: true },
    tui: { keybindings: { commandPalette: "ctrl+p", taskSwitcher: "ctrl+g" } },
  };
  await Deno.writeTextFile(configPath, JSON.stringify(config, null, 2));
  return {
    home,
    workspace,
    socketPath,
    configPath,
    cleanup: () => Deno.remove(home, { recursive: true }),
  };
}

/**
 * Spawn the daemon under a synthetic HOME and wait for the
 * "[daemon] listening on ..." line so we know the runtime wiring
 * succeeded and the socket is ready.
 */
async function spawnDaemon(fixture: RuntimeFixture): Promise<{
  readonly child: Deno.ChildProcess;
  readonly stop: () => Promise<void>;
}> {
  const spawnEnv: Record<string, string> = { HOME: fixture.home };
  const denoDir = Deno.env.get("DENO_DIR");
  if (denoDir !== undefined) {
    spawnEnv.DENO_DIR = denoDir;
  } else {
    const parentHome = Deno.env.get("HOME");
    if (parentHome !== undefined) {
      spawnEnv.DENO_DIR = Deno.build.os === "darwin"
        ? `${parentHome}/Library/Caches/deno`
        : `${parentHome}/.cache/deno`;
    }
  }
  for (const passthrough of ["PATH", "TMPDIR", "LANG"]) {
    const value = Deno.env.get(passthrough);
    if (value !== undefined) spawnEnv[passthrough] = value;
  }

  const command = new Deno.Command(Deno.execPath(), {
    args: ["run", "-A", "packages/cli/main.ts", "daemon"],
    env: spawnEnv,
    clearEnv: true,
    cwd: Deno.cwd(),
    stdout: "null",
    stderr: "piped",
    stdin: "null",
  });
  const child = command.spawn();

  // Wait for the "[daemon] listening on" line.
  const decoder = new TextDecoder();
  const reader = child.stderr.getReader();
  let buffer = "";
  let deadlineTimer: number | undefined;
  let stderrCancelled = false;
  try {
    const deadlinePromise = new Promise<never>((_, reject) => {
      deadlineTimer = setTimeout(() => {
        reject(
          new Error(
            `daemon never logged "[daemon] listening on ..." within ` +
              `${STARTUP_LISTEN_TIMEOUT_MS}ms; stderr was:\n${buffer}`,
          ),
        );
      }, STARTUP_LISTEN_TIMEOUT_MS);
    });
    while (true) {
      const result = await Promise.race([reader.read(), deadlinePromise]);
      if (result.done) {
        throw new Error(
          `daemon stderr closed before logging the listen line; stderr was:\n${buffer}`,
        );
      }
      buffer += decoder.decode(result.value, { stream: true });
      if (buffer.includes("[daemon] listening on ")) {
        break;
      }
    }
  } finally {
    if (deadlineTimer !== undefined) clearTimeout(deadlineTimer);
    reader.releaseLock();
  }

  const stop = async () => {
    try {
      child.kill("SIGTERM");
    } catch {
      // Already gone.
    }
    if (!stderrCancelled) {
      try {
        await child.stderr.cancel();
        stderrCancelled = true;
      } catch {
        // Already drained.
      }
    }
    let shutdownTimer: number | undefined;
    const shutdownTimeout = new Promise<void>((resolve) => {
      shutdownTimer = setTimeout(resolve, SHUTDOWN_GRACE_MS);
    });
    try {
      await Promise.race([child.status, shutdownTimeout]);
    } finally {
      if (shutdownTimer !== undefined) clearTimeout(shutdownTimer);
    }
    try {
      child.kill("SIGKILL");
    } catch {
      // Already gone.
    }
    await child.status;
  };

  return { child, stop };
}

/**
 * Open a Unix-domain client connection to the running daemon and
 * yield encoded envelopes alongside `send` / `next` helpers. Mirrors
 * the helper in `daemon_server_test.ts` so the test reads identically
 * to the existing integration suite.
 */
async function connectClient(socketPath: string) {
  // Wait briefly for the socket file to appear on disk; the daemon
  // logs "listening" right after `Deno.listen` returns, but we still
  // race the kernel committing the inode entry on macOS' tmpfs in
  // some test runners.
  for (let attempt = 0; attempt < 10; attempt += 1) {
    try {
      await Deno.lstat(socketPath);
      break;
    } catch {
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  }
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
      let timer: number | undefined;
      const timeout = new Promise<never>((_, reject) => {
        timer = setTimeout(() => {
          reject(
            new Error(
              `client did not receive an envelope within ${REPLY_TIMEOUT_MS}ms`,
            ),
          );
        }, REPLY_TIMEOUT_MS);
      });
      try {
        const result = await Promise.race([reader.next(), timeout]);
        if (result.done) {
          throw new Error("connection closed before next envelope");
        }
        return result.value;
      } finally {
        if (timer !== undefined) clearTimeout(timer);
      }
    },
    close: async () => {
      try {
        await writer.close();
      } catch {
        // Ignore.
      }
      try {
        conn.close();
      } catch {
        // Ignore.
      }
    },
  };
}

Deno.test("main.ts daemon runtime: ping/pong, /issue, /status, unknown command", async () => {
  const fixture = await makeFixture();
  let stop: (() => Promise<void>) | undefined;
  try {
    const spawned = await spawnDaemon(fixture);
    stop = spawned.stop;

    const client = await connectClient(fixture.socketPath);
    try {
      // 1. Ping/pong — proves the runtime is up and the IPC surface
      //    answers.
      await client.send({ id: "ping-1", type: "ping", payload: {} });
      const pong = await client.next();
      assertEquals(pong.type, "pong");
      assertEquals(pong.id, "ping-1");
      assert(
        (pong.payload as PongPayload).daemonVersion.length > 0,
        "pong daemonVersion should be non-empty",
      );

      // 2. Subscribe to the wildcard so we can observe supervisor
      //    transitions raised by the next /issue command.
      await client.send({
        id: "sub-1",
        type: "subscribe",
        payload: { target: "*" },
      });
      const subAck = await client.next();
      assertEquals(subAck.type, "ack");
      assertEquals((subAck.payload as AckPayload).ok, true);

      // 3. Unknown command name → deterministic error.
      await client.send({
        id: "bogus-1",
        type: "command",
        payload: { name: "totally-bogus", args: [] },
      });
      const bogusAck = await client.next();
      assertEquals(bogusAck.type, "ack");
      assertEquals((bogusAck.payload as AckPayload).ok, false);
      assert(
        (bogusAck.payload as AckPayload).error?.includes("unknown command"),
        `expected unknown-command error, got: ${(bogusAck.payload as AckPayload).error}`,
      );

      // 4. /issue 42 against the configured default repo.
      await client.send({
        id: "issue-1",
        type: "command",
        payload: { name: "issue", args: ["42"], issueNumber: makeIssueNumber(42) },
      });

      // The next envelope from the daemon may be either the `ack` for
      // /issue (handlers reply synchronously, but the supervisor's
      // first `state-changed` event also fires synchronously inside
      // `start()` before we yield to the event loop). Drain both, in
      // whichever order they arrive.
      let issueAck: AckPayload | undefined;
      let firstStateChange: EventPayload | undefined;
      for (let i = 0; i < 4 && (issueAck === undefined || firstStateChange === undefined); i += 1) {
        const envelope = await client.next();
        if (envelope.type === "ack" && envelope.id === "issue-1") {
          issueAck = envelope.payload as AckPayload;
          continue;
        }
        if (envelope.type === "event" && envelope.id === "sub-1") {
          firstStateChange = envelope.payload as EventPayload;
          continue;
        }
      }
      assertEquals(issueAck?.ok, true, `unexpected issue ack: ${JSON.stringify(issueAck)}`);
      assert(
        firstStateChange !== undefined,
        "expected a state-changed event for the started task",
      );
      assertEquals(
        firstStateChange.kind,
        "state-changed",
        "first event should be a state transition",
      );
      assert(
        firstStateChange.taskId.startsWith("task_"),
        `task id should be branded: ${firstStateChange.taskId}`,
      );

      // 5. /status reflects the new task in the JSON projection.
      await client.send({
        id: "status-1",
        type: "command",
        payload: { name: "status", args: [] },
      });
      // Drain envelopes until the matching ack appears: the supervisor
      // is racing the FSM forward and may publish more events between
      // requests.
      let statusAck: AckPayload | undefined;
      for (let i = 0; i < 64 && statusAck === undefined; i += 1) {
        const envelope = await client.next();
        if (envelope.type === "ack" && envelope.id === "status-1") {
          statusAck = envelope.payload as AckPayload;
        }
      }
      assert(statusAck !== undefined, "did not receive status ack");
      assertEquals(statusAck.ok, true);
      assertEquals(
        statusAck.error,
        undefined,
        "status ack must not overload `error` (contract: error is for failures)",
      );
      assert(statusAck.data !== undefined, "status ack should carry tasks on `data`");
      const parsed = statusAck.data as {
        readonly tasks: ReadonlyArray<{
          readonly id: string;
          readonly repo: string;
          readonly issueNumber: number;
          readonly state: string;
        }>;
      };
      assert(parsed.tasks.length >= 1, "status should report at least one task");
      const issueTask = parsed.tasks.find((t) => t.repo === "owner/repo" && t.issueNumber === 42);
      assert(
        issueTask !== undefined,
        `status missing the issue we started; got: ${JSON.stringify(statusAck.data)}`,
      );
    } finally {
      await client.close();
    }
  } finally {
    if (stop !== undefined) {
      await stop();
    }
    await fixture.cleanup();
  }
});
