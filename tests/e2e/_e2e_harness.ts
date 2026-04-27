/**
 * tests/e2e/_e2e_harness.ts — shared scaffolding for the Wave 5
 * end-to-end suite.
 *
 * The three e2e tests (`happy_path_test.ts`, `ci_fail_recovery_test.ts`,
 * `review_comment_recovery_test.ts`) all need the same shape:
 *
 *  1. Read sandbox credentials from environment variables.
 *  2. If the gate (`MAKINA_E2E=1`) is off **or** any required variable
 *     is missing, register a `Deno.test` that prints a clear skip note
 *     and exits.
 *  3. Otherwise: build a synthetic `HOME` (`Deno.makeTempDir`), write a
 *     `config.json` pointed at the sandbox repo, spawn `main.ts daemon`
 *     against that workspace, open a `Deno.connect` socket session, and
 *     yield a `Harness` to the test body.
 *  4. Tear down: send `SIGTERM` to the daemon, wait for the socket to
 *     drain, remove the temp dir, and (if a sandbox PR was opened in the
 *     test body) attempt to clean it up.
 *
 * The harness is **not** in the production code path; it lives entirely
 * under `tests/e2e/` and is exempt from the production "no global
 * mutation in tests" rule because it never touches process-wide state
 * outside of the spawned child. Per-file isolation is preserved via
 * unique temp directories.
 *
 * **Why a separate file** rather than copy-pasting the spawn logic into
 * each test (the integration suite's pattern): the e2e tests are
 * intrinsically more expensive to build — each scenario needs a
 * sandbox-repo precondition, a polling helper that watches the event
 * bus for FSM transitions, and a robust teardown. Centralising the
 * boilerplate here keeps each test focused on its own scenario.
 *
 * @module
 */

import { decode, encode } from "../../src/ipc/codec.ts";
import {
  type AckPayload,
  type EventPayload,
  type MessageEnvelope,
} from "../../src/ipc/protocol.ts";
import {
  type IssueNumber,
  makeIssueNumber,
  makeRepoFullName,
  type RepoFullName,
} from "../../src/types.ts";

/**
 * Environment variable that gates the entire e2e suite.
 *
 * When unset (or any value other than `"1"`), every e2e test registers
 * a thin skip body so `deno task test` is fast and offline-safe. This
 * mirrors the convention used by the `MAKINA_E2E_*` family below.
 */
export const E2E_GATE_ENV = "MAKINA_E2E";

/**
 * Required environment variables when the gate is on. Any missing
 * variable causes the test to skip with a precise diagnostic naming
 * the variable.
 */
export const E2E_REQUIRED_ENV = [
  "MAKINA_E2E_APP_ID",
  "MAKINA_E2E_PRIVATE_KEY_PATH",
  "MAKINA_E2E_REPO",
  "MAKINA_E2E_INSTALLATION_ID",
] as const;

/** Type of {@link E2E_REQUIRED_ENV}'s elements. */
export type RequiredEnvName = (typeof E2E_REQUIRED_ENV)[number];

/**
 * Environment variable naming the sandbox issue to start the FSM
 * against. Each scenario reads a different variable so a single CI
 * invocation can run all three concurrently without aliasing.
 */
export const E2E_HAPPY_ISSUE_ENV = "MAKINA_E2E_HAPPY_ISSUE";
/** Issue number whose acceptance criteria deliberately fail CI. */
export const E2E_CI_FAIL_ISSUE_ENV = "MAKINA_E2E_CI_FAIL_ISSUE";
/** Issue number whose review-comment recovery scenario runs against. */
export const E2E_REVIEW_COMMENT_ISSUE_ENV = "MAKINA_E2E_REVIEW_COMMENT_ISSUE";

/**
 * Default upper bound on how long we wait for the supervisor to walk
 * the FSM to a terminal state, in milliseconds. The end-to-end paths
 * touch real CI which can take minutes; we bound the wait so a wedged
 * test does not stall the runner indefinitely.
 *
 * Override via `MAKINA_E2E_TIMEOUT_MS`. Tests doing extra-long CI work
 * (e.g. a fail-recovery loop with multiple iterations) read the
 * variable directly.
 */
export const E2E_DEFAULT_TIMEOUT_MILLISECONDS = 30 * 60 * 1_000;

/**
 * Result of {@link checkGate}: either the gate is open with every
 * required variable present, or the test should skip with a message
 * naming the missing piece.
 */
export type GateResult =
  | { readonly mode: "skip"; readonly reason: string }
  | { readonly mode: "run"; readonly env: ResolvedE2eEnv };

/**
 * Parsed view of the e2e environment variables, ready for the test
 * body to consume. Strings are passed through verbatim; numbers are
 * branded via the `make*` constructors so a malformed value fails
 * fast.
 */
export interface ResolvedE2eEnv {
  /** GitHub App id (numeric). */
  readonly appId: number;
  /** Filesystem path of the GitHub App private key (PEM). */
  readonly privateKeyPath: string;
  /** Sandbox `<owner>/<name>` pair. */
  readonly repo: RepoFullName;
  /** Installation id for the sandbox repo (numeric). */
  readonly installationId: number;
  /** Optional issue number for the happy path. */
  readonly happyIssue?: IssueNumber;
  /** Optional issue number for the CI-fail recovery scenario. */
  readonly ciFailIssue?: IssueNumber;
  /** Optional issue number for the review-comment recovery scenario. */
  readonly reviewCommentIssue?: IssueNumber;
  /** Optional max-wait override in milliseconds. */
  readonly timeoutMilliseconds: number;
}

/**
 * Parse the e2e environment. Returns either a skip reason or a fully
 * resolved {@link ResolvedE2eEnv} ready for the harness to consume.
 *
 * The function is pure: it only reads `Deno.env`, never spawns
 * processes or touches the filesystem. Each test calls it once and
 * branches.
 *
 * @returns Whether the test should run or skip.
 *
 * @example
 * ```ts
 * Deno.test("happy path", async () => {
 *   const gate = checkGate();
 *   if (gate.mode === "skip") {
 *     console.error(gate.reason);
 *     return;
 *   }
 *   // ... use gate.env
 * });
 * ```
 */
export function checkGate(): GateResult {
  if (Deno.env.get(E2E_GATE_ENV) !== "1") {
    return {
      mode: "skip",
      reason:
        `[e2e] skipped — set ${E2E_GATE_ENV}=1 to enable the end-to-end suite (see docs/development.md).`,
    };
  }
  const missing: RequiredEnvName[] = [];
  for (const name of E2E_REQUIRED_ENV) {
    const value = Deno.env.get(name);
    if (value === undefined || value.length === 0) {
      missing.push(name);
    }
  }
  if (missing.length > 0) {
    return {
      mode: "skip",
      reason: `[e2e] skipped — required env not set: ${missing.join(", ")}.`,
    };
  }
  const appId = Number(Deno.env.get("MAKINA_E2E_APP_ID"));
  const installationId = Number(Deno.env.get("MAKINA_E2E_INSTALLATION_ID"));
  if (!Number.isFinite(appId) || !Number.isInteger(appId) || appId <= 0) {
    return {
      mode: "skip",
      reason: `[e2e] skipped — MAKINA_E2E_APP_ID must be a positive integer.`,
    };
  }
  if (
    !Number.isFinite(installationId) || !Number.isInteger(installationId) ||
    installationId <= 0
  ) {
    return {
      mode: "skip",
      reason: `[e2e] skipped — MAKINA_E2E_INSTALLATION_ID must be a positive integer.`,
    };
  }
  let repo: RepoFullName;
  const repoRaw = Deno.env.get("MAKINA_E2E_REPO");
  if (repoRaw === undefined) {
    return { mode: "skip", reason: "[e2e] skipped — MAKINA_E2E_REPO missing." };
  }
  try {
    repo = makeRepoFullName(repoRaw);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return { mode: "skip", reason: `[e2e] skipped — MAKINA_E2E_REPO invalid: ${message}` };
  }

  const happyIssue = optionalIssueNumber(E2E_HAPPY_ISSUE_ENV);
  const ciFailIssue = optionalIssueNumber(E2E_CI_FAIL_ISSUE_ENV);
  const reviewCommentIssue = optionalIssueNumber(E2E_REVIEW_COMMENT_ISSUE_ENV);

  const timeoutRaw = Deno.env.get("MAKINA_E2E_TIMEOUT_MS");
  let timeoutMilliseconds = E2E_DEFAULT_TIMEOUT_MILLISECONDS;
  if (timeoutRaw !== undefined) {
    const parsed = Number(timeoutRaw);
    if (Number.isFinite(parsed) && Number.isInteger(parsed) && parsed > 0) {
      timeoutMilliseconds = parsed;
    }
  }

  // The PEM path was checked for non-emptiness above; we deliberately
  // do not stat it here to avoid coupling the gate to filesystem
  // permissions in a test runner sandbox. The daemon will reject a
  // missing file with its own diagnostic; the e2e test will then
  // surface the failure through the listen-line wait.
  const env: ResolvedE2eEnv = {
    appId,
    privateKeyPath: Deno.env.get("MAKINA_E2E_PRIVATE_KEY_PATH") ?? "",
    repo,
    installationId,
    timeoutMilliseconds,
  };
  if (happyIssue !== undefined) {
    return {
      mode: "run",
      env: { ...env, happyIssue, ciFailIssue, reviewCommentIssue } as ResolvedE2eEnv,
    };
  }
  return {
    mode: "run",
    env: { ...env, ciFailIssue, reviewCommentIssue } as ResolvedE2eEnv,
  };
}

/**
 * Read an optional issue-number environment variable and brand it.
 *
 * Returns `undefined` when the variable is unset or empty so the
 * caller can branch without re-reading the variable.
 *
 * @param name Variable name to read.
 * @returns The branded {@link IssueNumber} or `undefined`.
 */
function optionalIssueNumber(name: string): IssueNumber | undefined {
  const raw = Deno.env.get(name);
  if (raw === undefined || raw.length === 0) return undefined;
  const parsed = Number(raw);
  if (!Number.isFinite(parsed) || !Number.isInteger(parsed) || parsed <= 0) {
    return undefined;
  }
  return makeIssueNumber(parsed);
}

/**
 * Live socket session: the spawned daemon, a connected client, and the
 * helpers each scenario needs to drive the FSM and observe transitions.
 *
 * Returned by {@link bootHarness}. The {@link Harness.cleanup} method
 * is called by the test body's `finally` so the daemon and temp
 * directory are always released even if the test rejects.
 */
export interface Harness {
  /** Spawned daemon child handle. */
  readonly child: Deno.ChildProcess;
  /** Synthetic `HOME` directory the daemon was launched against. */
  readonly home: string;
  /** Resolved socket path inside the synthetic HOME. */
  readonly socketPath: string;
  /** The branded sandbox repo name. */
  readonly repo: RepoFullName;
  /**
   * Send an envelope and resolve when the matching reply arrives. The
   * `id` is enforced unique by the harness; reply correlation is by
   * envelope id, matching the production client.
   *
   * @param envelope The envelope to send.
   * @returns The decoded reply envelope.
   */
  send(envelope: MessageEnvelope): Promise<MessageEnvelope>;
  /**
   * Wait for the next event matching `predicate`, or reject when the
   * supervisor emits a terminal `state-changed` event whose `toState`
   * matches a terminal value. Each scenario passes its own predicate
   * (e.g. "the task reached `READY_TO_MERGE`").
   *
   * @param predicate Returns `true` when the awaited event arrives.
   * @param timeoutMs Bound on the wait, in milliseconds.
   * @returns The matching event payload.
   */
  waitForEvent(
    predicate: (event: EventPayload) => boolean,
    timeoutMs: number,
  ): Promise<EventPayload>;
  /**
   * Tear down the daemon and remove the temp directory. Safe to call
   * multiple times.
   */
  cleanup(): Promise<void>;
}

/**
 * Boot a harness against the resolved e2e environment.
 *
 * Builds a synthetic HOME, writes a `config.json` keyed at the
 * sandbox repo, spawns the daemon, waits for the listen line, opens a
 * client socket, and subscribes to wildcard events.
 *
 * @param env The resolved environment (see {@link checkGate}).
 * @returns A {@link Harness} whose `cleanup` must be awaited from the
 *   caller's `finally`.
 *
 * @example
 * ```ts
 * const gate = checkGate();
 * if (gate.mode === "skip") return;
 * const harness = await bootHarness(gate.env);
 * try {
 *   // ... drive the daemon
 * } finally {
 *   await harness.cleanup();
 * }
 * ```
 */
export async function bootHarness(env: ResolvedE2eEnv): Promise<Harness> {
  const home = await Deno.makeTempDir({ dir: "/tmp", prefix: "makina-e2e-" });
  try {
    const configDir = Deno.build.os === "darwin"
      ? `${home}/Library/Application Support/makina`
      : `${home}/.config/makina`;
    await Deno.mkdir(configDir, { recursive: true });
    const workspace = `${home}/workspace`;
    await Deno.mkdir(workspace, { recursive: true });
    const socketDir = `${home}/run`;
    await Deno.mkdir(socketDir, { recursive: true });
    const socketPath = `${socketDir}/daemon.sock`;
    const configPath = `${configDir}/config.json`;
    const config = {
      github: {
        appId: env.appId,
        privateKeyPath: env.privateKeyPath,
        installations: { [env.repo as string]: env.installationId },
        defaultRepo: env.repo as string,
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
    await Deno.writeTextFile(configPath, `${JSON.stringify(config, null, 2)}\n`);

    const spawnEnv = buildSpawnEnv(home);
    const command = new Deno.Command(Deno.execPath(), {
      args: ["run", "-A", "main.ts", "daemon"],
      env: spawnEnv,
      clearEnv: true,
      cwd: Deno.cwd(),
      stdout: "null",
      stderr: "piped",
      stdin: "null",
    });
    const child = command.spawn();

    await waitForListenLine(child.stderr, 30_000);

    // The listen line confirms `Deno.listen` returned; the kernel may
    // still be racing the inode entry on macOS' tmpfs. Wait briefly
    // for the file to materialise before opening the client.
    for (let attempt = 0; attempt < 20; attempt += 1) {
      try {
        await Deno.lstat(socketPath);
        break;
      } catch {
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
    }
    const conn = await Deno.connect({ transport: "unix", path: socketPath });
    const writer = conn.writable.getWriter();
    const eventQueue: EventPayload[] = [];
    const eventWaiters: Array<{
      predicate: (event: EventPayload) => boolean;
      resolve: (event: EventPayload) => void;
      reject: (error: Error) => void;
      timer: number;
    }> = [];
    const replyWaiters = new Map<
      string,
      { resolve: (envelope: MessageEnvelope) => void; reject: (error: Error) => void }
    >();

    // Read loop: dispatch every decoded envelope.
    const readLoop = (async () => {
      try {
        for await (const envelope of decode(conn.readable)) {
          if (envelope.type === "event") {
            eventQueue.push(envelope.payload as EventPayload);
            // Drain any waiter whose predicate now matches.
            for (let i = eventWaiters.length - 1; i >= 0; i -= 1) {
              const waiter = eventWaiters[i];
              if (waiter === undefined) continue;
              if (waiter.predicate(envelope.payload as EventPayload)) {
                clearTimeout(waiter.timer);
                eventWaiters.splice(i, 1);
                waiter.resolve(envelope.payload as EventPayload);
              }
            }
            continue;
          }
          const slot = replyWaiters.get(envelope.id);
          if (slot !== undefined) {
            replyWaiters.delete(envelope.id);
            slot.resolve(envelope);
          }
        }
      } catch (error) {
        const cause = error instanceof Error ? error : new Error(String(error));
        for (const slot of replyWaiters.values()) slot.reject(cause);
        replyWaiters.clear();
        for (const waiter of eventWaiters) {
          clearTimeout(waiter.timer);
          waiter.reject(cause);
        }
        eventWaiters.length = 0;
      }
    })();

    const send = async (envelope: MessageEnvelope): Promise<MessageEnvelope> => {
      if (replyWaiters.has(envelope.id)) {
        throw new Error(`duplicate envelope id ${JSON.stringify(envelope.id)}`);
      }
      const reply = new Promise<MessageEnvelope>((resolve, reject) => {
        replyWaiters.set(envelope.id, { resolve, reject });
      });
      await writer.write(encode(envelope));
      return reply;
    };

    const waitForEvent = (
      predicate: (event: EventPayload) => boolean,
      timeoutMs: number,
    ): Promise<EventPayload> => {
      // Drain anything already queued before installing the waiter so
      // an event the test missed cannot be lost behind a slow predicate.
      for (let i = 0; i < eventQueue.length; i += 1) {
        const event = eventQueue[i];
        if (event !== undefined && predicate(event)) {
          eventQueue.splice(i, 1);
          return Promise.resolve(event);
        }
      }
      return new Promise<EventPayload>((resolve, reject) => {
        const timer = setTimeout(() => {
          // Remove ourselves from the waiter list before rejecting so a
          // late-arriving event after the timeout does not double-fire.
          const idx = eventWaiters.findIndex((w) => w.timer === timer);
          if (idx >= 0) eventWaiters.splice(idx, 1);
          reject(new Error(`waitForEvent timed out after ${timeoutMs}ms`));
        }, timeoutMs);
        eventWaiters.push({ predicate, resolve, reject, timer });
      });
    };

    // Subscribe to wildcard so every supervisor event lands in our queue.
    const subscribeReply = await send({
      id: "harness-subscribe",
      type: "subscribe",
      payload: { target: "*" },
    });
    if (subscribeReply.type !== "ack") {
      throw new Error(
        `harness subscribe expected ack, got ${subscribeReply.type}`,
      );
    }
    const ack = subscribeReply.payload as AckPayload;
    if (!ack.ok) {
      throw new Error(`harness subscribe rejected: ${ack.error ?? "(no detail)"}`);
    }

    let cleaned = false;
    const cleanup = async (): Promise<void> => {
      if (cleaned) return;
      cleaned = true;
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
      try {
        child.kill("SIGTERM");
      } catch {
        // Already gone.
      }
      try {
        await child.stderr.cancel();
      } catch {
        // Already drained.
      }
      let shutdownTimer: number | undefined;
      const shutdownTimeout = new Promise<void>((resolve) => {
        shutdownTimer = setTimeout(resolve, 2_000);
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
      await readLoop;
      try {
        await Deno.remove(home, { recursive: true });
      } catch {
        // Best-effort cleanup; a wedged FS leaves the dir behind for
        // the operator to inspect.
      }
    };

    return {
      child,
      home,
      socketPath,
      repo: env.repo,
      send,
      waitForEvent,
      cleanup,
    };
  } catch (error) {
    // Boot failed — make sure the temp dir is removed.
    try {
      await Deno.remove(home, { recursive: true });
    } catch {
      // Ignore.
    }
    throw error;
  }
}

/**
 * Build the environment a spawned `deno run` invocation needs.
 *
 * Mirrors the helper in `tests/integration/main_daemon_runtime_test.ts`:
 * pin `HOME` to the synthetic dir, share `DENO_DIR` with the parent so
 * the spawned binary does not re-download every dep, and pass through a
 * minimal POSIX env so the runtime can locate sub-tools.
 *
 * @param fakeHome The synthetic HOME directory to pin.
 * @returns The env map for `Deno.Command`'s `env` option.
 */
function buildSpawnEnv(fakeHome: string): Record<string, string> {
  const out: Record<string, string> = { HOME: fakeHome };
  const denoDir = Deno.env.get("DENO_DIR");
  if (denoDir !== undefined) {
    out.DENO_DIR = denoDir;
  } else {
    const parentHome = Deno.env.get("HOME");
    if (parentHome !== undefined) {
      out.DENO_DIR = Deno.build.os === "darwin"
        ? `${parentHome}/Library/Caches/deno`
        : `${parentHome}/.cache/deno`;
    }
  }
  for (const passthrough of ["PATH", "TMPDIR", "LANG"]) {
    const value = Deno.env.get(passthrough);
    if (value !== undefined) out[passthrough] = value;
  }
  return out;
}

/**
 * Wait for the daemon's "[daemon] listening on ..." line on stderr. We
 * key off the listen line (rather than the socket file) because a
 * regression that bound the socket but failed to emit the line would
 * leave the test hung; the line guarantees `Deno.listen` returned and
 * any failure surfaces as a parsable diagnostic in the rejection
 * message.
 *
 * @param stderr The daemon's stderr stream.
 * @param timeoutMs Bound on the wait, in milliseconds.
 */
async function waitForListenLine(
  stderr: ReadableStream<Uint8Array>,
  timeoutMs: number,
): Promise<void> {
  const decoder = new TextDecoder();
  const reader = stderr.getReader();
  let buffer = "";
  let deadlineTimer: number | undefined;
  try {
    const deadlinePromise = new Promise<never>((_, reject) => {
      deadlineTimer = setTimeout(() => {
        reject(
          new Error(
            `[e2e] daemon never logged "[daemon] listening on ..." within ` +
              `${timeoutMs}ms; stderr was:\n${buffer}`,
          ),
        );
      }, timeoutMs);
    });
    while (true) {
      const result = await Promise.race([reader.read(), deadlinePromise]);
      if (result.done) {
        throw new Error(
          `[e2e] daemon stderr closed before logging the listen line; stderr was:\n${buffer}`,
        );
      }
      buffer += decoder.decode(result.value, { stream: true });
      if (buffer.includes("[daemon] listening on ")) {
        return;
      }
    }
  } finally {
    if (deadlineTimer !== undefined) clearTimeout(deadlineTimer);
    reader.releaseLock();
  }
}

/**
 * Convenience helper: register a `Deno.test` whose body skips with a
 * clear note when the gate is closed.
 *
 * Each scenario file calls this once. The skip note goes through
 * `console.error` (visible in `deno test --quiet` output) and the
 * test resolves successfully so coverage reports stay clean.
 *
 * @param name Test name displayed by Deno's test runner.
 * @param scenarioIssueEnv Environment variable naming the per-scenario
 *   issue number; if unset, the scenario also skips.
 * @param body The actual test body, called only when the gate is open
 *   and the scenario issue is set. Receives the resolved environment
 *   and the live harness; `cleanup` runs from a `finally` here.
 *
 * @example
 * ```ts
 * registerE2eTest("e2e: happy path", "MAKINA_E2E_HAPPY_ISSUE", async (env, harness) => {
 *   // ... use harness.send / harness.waitForEvent
 * });
 * ```
 */
export function registerE2eTest(
  name: string,
  scenarioIssueEnv: string,
  body: (env: ResolvedE2eEnv, harness: Harness) => Promise<void>,
): void {
  Deno.test({
    name,
    // The harness opens a real socket and spawns a child; let Deno's
    // sanitizers flag any leak. If a future scenario needs to disable
    // a sanitizer, it should do so locally rather than in this helper.
    fn: async () => {
      const gate = checkGate();
      if (gate.mode === "skip") {
        console.error(gate.reason);
        return;
      }
      const issueRaw = Deno.env.get(scenarioIssueEnv);
      if (issueRaw === undefined || issueRaw.length === 0) {
        console.error(
          `[e2e] ${name}: skipped — ${scenarioIssueEnv} not set.`,
        );
        return;
      }
      const harness = await bootHarness(gate.env);
      try {
        await body(gate.env, harness);
      } finally {
        await harness.cleanup();
      }
    },
  });
}
