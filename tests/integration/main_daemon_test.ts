/**
 * Integration tests for the `daemon` branch of `main.ts`.
 *
 * Covers the consumer-boundary contract that nested `~/`-prefixed path
 * fields in `config.json` are expanded before they are used (here:
 * `daemon.socketPath` is expanded before `Deno.listen({ transport:
 * "unix" })` binds it). A regression in `main.ts` that drops the
 * `expandHome(...)` call would manifest as the daemon trying to bind a
 * literal `~` path: this test pins the behavior end-to-end by spawning
 * the binary against a controlled `HOME` and asserting the socket file
 * lands at `<HOME>/.../daemon.sock`, never at any path containing a
 * literal `~`.
 *
 * The test does not exercise the IPC dispatch loop (that is the job of
 * `tests/integration/daemon_server_test.ts`); it only proves the path
 * gets expanded at the wiring boundary.
 */

import { assert, assertEquals } from "@std/assert";

const CONFIG_FILENAME = "config.json";
const SOCKET_FILENAME = "daemon.sock";
const STARTUP_LISTEN_TIMEOUT_MS = 10_000;
const SHUTDOWN_GRACE_MS = 1_000;

/**
 * Build a minimal-but-valid `config.json` with a `~/`-prefixed
 * `daemon.socketPath`. The wizard preserves the user-typed `~/...`
 * shape verbatim (path expansion happens at the daemon's binding
 * boundary in `main.ts`, not at config-load time), so this matches the
 * realistic shape of a freshly-written config.
 */
function configWithTildeSocket(): string {
  return JSON.stringify(
    {
      github: {
        appId: 1234567,
        privateKeyPath: "~/keys/app.pem",
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
      workspace: "~/workspace",
      daemon: { socketPath: `~/.local/state/makina/${SOCKET_FILENAME}`, autoStart: true },
      tui: { keybindings: { commandPalette: "ctrl+p", taskSwitcher: "ctrl+g" } },
    },
    null,
    2,
  );
}

/**
 * Wait for the daemon's "[daemon] listening on <path>" line so we know
 * `Deno.listen` has returned. We reach for stderr (not the socket file)
 * because the listen line is the only signal that the bind actually
 * succeeded — a regression that bound `~/...` literally would surface
 * as a permission/ENOENT failure rather than just an unexpanded path on
 * disk, and we want this test to fail loudly in that case.
 *
 * The reader returns when either (a) the listen line is matched —
 * resolving to the captured path — or (b) the deadline elapses, in
 * which case the accumulated stderr text is included in the rejection
 * message so a regression failure leaves a clear breadcrumb.
 */
async function waitForListenLine(
  stderr: ReadableStream<Uint8Array>,
): Promise<string> {
  const decoder = new TextDecoder();
  const reader = stderr.getReader();
  let buffer = "";
  let deadlineTimer: number | undefined;
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
      const match = buffer.match(/\[daemon\] listening on (.+)/);
      const captured = match?.[1];
      if (captured !== undefined) {
        return captured.trimEnd();
      }
    }
  } finally {
    if (deadlineTimer !== undefined) clearTimeout(deadlineTimer);
    reader.releaseLock();
  }
}

Deno.test("main.ts daemon: expands ~/ in daemon.socketPath before binding", async () => {
  // Synthetic HOME so the test does not depend on the real user's
  // filesystem layout. The config inside is `~/.local/state/makina/...`
  // — once expanded, it must materialise under THIS directory.
  //
  // We pin the parent dir to `/tmp` (rather than letting
  // `Deno.makeTempDir` default to `$TMPDIR`, which on macOS is a long
  // `/var/folders/.../T/` path) because Unix-domain socket paths are
  // capped at `SUN_LEN` (~104 bytes on macOS, 108 on Linux). The
  // expanded socket path must fit comfortably or `Deno.listen` rejects
  // before we get to assert anything.
  const fakeHome = await Deno.makeTempDir({ dir: "/tmp", prefix: "mh-" });
  try {
    // Lay down a config under the path `defaultConfigPath()` resolves
    // for this platform (macOS gets `~/Library/Application Support/...`,
    // every other POSIX target gets `~/.config/...`).
    const configDir = Deno.build.os === "darwin"
      ? `${fakeHome}/Library/Application Support/makina`
      : `${fakeHome}/.config/makina`;
    await Deno.mkdir(configDir, { recursive: true });
    await Deno.writeTextFile(
      `${configDir}/${CONFIG_FILENAME}`,
      configWithTildeSocket(),
    );
    // The daemon expects the socket directory to exist. The wizard
    // (`runSetupWizard`) does not create it today; production installs
    // create it via `mkdir -p`. Here we set it up by hand so the
    // spawned daemon can bind without bootstrap noise.
    const socketDir = `${fakeHome}/.local/state/makina`;
    await Deno.mkdir(socketDir, { recursive: true });
    const expectedSocketPath = `${socketDir}/${SOCKET_FILENAME}`;

    // Build the spawn env carefully: we override HOME so the daemon
    // resolves `~/...` against `fakeHome`, but we MUST keep Deno's own
    // cache pointed at the real user's `DENO_DIR`. Otherwise the
    // spawned binary tries to re-download every dep into the synthetic
    // HOME and the listen line never appears within the test deadline.
    const spawnEnv: Record<string, string> = { HOME: fakeHome };
    const denoDir = Deno.env.get("DENO_DIR");
    if (denoDir !== undefined) {
      spawnEnv.DENO_DIR = denoDir;
    } else {
      // No explicit DENO_DIR: Deno's default lives under the parent's
      // HOME, so we must reach for the parent's HOME too. Read it once
      // and pin it.
      const parentHome = Deno.env.get("HOME");
      if (parentHome !== undefined) {
        spawnEnv.DENO_DIR = `${parentHome}/Library/Caches/deno`;
      }
    }
    // PATH and friends are needed for Deno itself to locate sub-tools;
    // pass through what the parent has so the spawn behaves like a
    // normal `deno run`.
    for (const passthrough of ["PATH", "TMPDIR", "LANG"]) {
      const value = Deno.env.get(passthrough);
      if (value !== undefined) spawnEnv[passthrough] = value;
    }

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
    let stderrCancelled = false;

    try {
      const listenLineOnSocketPath = await waitForListenLine(child.stderr);

      // 1. The listen line itself must contain the EXPANDED path
      //    (no stray `~`), and must equal the expected absolute path.
      assertEquals(
        listenLineOnSocketPath,
        expectedSocketPath,
        `daemon advertised the wrong socket path: ${listenLineOnSocketPath}`,
      );
      assert(
        !listenLineOnSocketPath.includes("~"),
        `daemon advertised an unexpanded ~ in: ${listenLineOnSocketPath}`,
      );

      // 2. The socket file must exist at the expanded path.
      const stat = await Deno.lstat(expectedSocketPath);
      assert(stat.isSocket, `expected a socket at ${expectedSocketPath}`);

      // 3. Belt-and-braces: a literal `~` directory must NOT have been
      //    created in the cwd by the regression we are guarding
      //    against.
      const literalTildeDir = `${Deno.cwd()}/~`;
      let literalTildeExists = false;
      try {
        await Deno.lstat(literalTildeDir);
        literalTildeExists = true;
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
      assert(
        !literalTildeExists,
        `daemon created a literal ~ directory at ${literalTildeDir}; ` +
          `~/ was not expanded before binding`,
      );
    } finally {
      // Send SIGTERM and wait for the process to clean up its socket.
      try {
        child.kill("SIGTERM");
      } catch (_) { /* already gone */ }
      // Cancel stderr so the pipe doesn't back-pressure shutdown and so
      // Deno's resource sanitiser does not flag a leaked stream. The
      // listen-line reader released the lock; cancel can reclaim now.
      if (!stderrCancelled) {
        try {
          await child.stderr.cancel();
          stderrCancelled = true;
        } catch (_) { /* already drained */ }
      }
      // Bound the wait so a wedged child cannot hang the test. Clear
      // the timer no matter who wins the race, otherwise Deno's
      // resource sanitiser flags it as a leaked timer.
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
      } catch (_) { /* already gone */ }
      await child.status;
    }
  } finally {
    await Deno.remove(fakeHome, { recursive: true });
  }
});
