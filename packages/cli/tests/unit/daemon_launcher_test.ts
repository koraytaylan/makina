/**
 * Unit tests for `src/daemon-launcher.ts`.
 *
 * The launcher orchestrates three concerns:
 *
 *   1. **Already running** — when a daemon is already listening on the
 *      socket, `ensureDaemonRunning` returns immediately without
 *      spawning anything.
 *   2. **Autostart disabled** — when no socket is bound and
 *      `autoStart === false`, the launcher surfaces a
 *      {@link DaemonNotRunningError} so the TUI can print a diagnostic
 *      and exit non-zero.
 *   3. **Autospawn** — when no socket is bound and `autoStart === true`,
 *      the launcher invokes the injected `spawn` callback and polls the
 *      socket until it binds (or until `timeoutMs` elapses, in which
 *      case it throws {@link DaemonStartTimeoutError}).
 *
 * The injected `spawn` seam keeps these tests hermetic: no real
 * `makina daemon` child is forked. The "spawn" simulates a daemon
 * binding the socket on a delay (or never binding, for the timeout
 * path).
 */

import { assertEquals, assertRejects } from "@std/assert";
import { join } from "@std/path";

import {
  DaemonNotRunningError,
  DaemonStartTimeoutError,
  ensureDaemonRunning,
  probeSocket,
  resolveDaemonInvocation,
} from "../../src/daemon-launcher.ts";

/**
 * Build an isolated socket path inside a fresh temp dir. Returns the
 * path and a cleanup callback the test must invoke at the end.
 */
async function freshSocketPath(): Promise<{
  readonly path: string;
  readonly cleanup: () => Promise<void>;
}> {
  const dir = await Deno.makeTempDir({ prefix: "makina-launcher-test-" });
  return {
    path: join(dir, "daemon.sock"),
    cleanup: async () => {
      await Deno.remove(dir, { recursive: true }).catch(() => {});
    },
  };
}

Deno.test("ensureDaemonRunning: returns immediately when a daemon is already listening", async () => {
  const { path, cleanup } = await freshSocketPath();
  const listener = Deno.listen({ transport: "unix", path });
  let spawnCalls = 0;
  try {
    await ensureDaemonRunning({
      socketPath: path,
      autoStart: true,
      spawn: () => {
        spawnCalls += 1;
      },
    });
    assertEquals(spawnCalls, 0);
  } finally {
    listener.close();
    await cleanup();
  }
});

Deno.test("ensureDaemonRunning: throws DaemonNotRunningError when autoStart is false", async () => {
  const { path, cleanup } = await freshSocketPath();
  try {
    await assertRejects(
      () =>
        ensureDaemonRunning({
          socketPath: path,
          autoStart: false,
        }),
      DaemonNotRunningError,
    );
  } finally {
    await cleanup();
  }
});

Deno.test("ensureDaemonRunning: invokes spawn and resolves once the socket binds", async () => {
  const { path, cleanup } = await freshSocketPath();
  let listener: Deno.UnixListener | undefined;
  let spawnCalls = 0;
  try {
    await ensureDaemonRunning({
      socketPath: path,
      autoStart: true,
      timeoutMs: 2000,
      spawn: () => {
        spawnCalls += 1;
        // Simulate a daemon that binds the socket asynchronously.
        const handle = setTimeout(() => {
          listener = Deno.listen({ transport: "unix", path });
        }, 100);
        // The timer must not block process exit if the test fails.
        const maybeUnref = handle as unknown as { unref?: () => void };
        if (typeof maybeUnref.unref === "function") {
          maybeUnref.unref();
        }
      },
    });
    assertEquals(spawnCalls, 1);
  } finally {
    listener?.close();
    await cleanup();
  }
});

Deno.test("ensureDaemonRunning: throws DaemonStartTimeoutError when spawn never binds", async () => {
  const { path, cleanup } = await freshSocketPath();
  try {
    await assertRejects(
      () =>
        ensureDaemonRunning({
          socketPath: path,
          autoStart: true,
          timeoutMs: 150,
          spawn: () => {
            // Pretend we spawned a daemon, but never bind the socket.
          },
        }),
      DaemonStartTimeoutError,
    );
  } finally {
    await cleanup();
  }
});

Deno.test("probeSocket: true when a peer is listening, false otherwise", async () => {
  const { path, cleanup } = await freshSocketPath();
  try {
    assertEquals(await probeSocket(path), false);
    const listener = Deno.listen({ transport: "unix", path });
    try {
      assertEquals(await probeSocket(path), true);
    } finally {
      listener.close();
    }
  } finally {
    await cleanup();
  }
});

Deno.test("resolveDaemonInvocation: returns a real exec path and non-empty args", () => {
  const resolved = resolveDaemonInvocation();
  assertEquals(typeof resolved.execPath, "string");
  assertEquals(resolved.execPath.length > 0, true);
  // The last arg is always the daemon subcommand, regardless of mode.
  assertEquals(resolved.args[resolved.args.length - 1], "daemon");
});
