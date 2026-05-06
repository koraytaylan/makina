/**
 * daemon-launcher.ts — ensure the daemon is listening before the TUI
 * tries to connect.
 *
 * `makina` is two cooperating processes (TUI + daemon). When the user
 * launches the TUI we either find a daemon already on the socket and
 * return immediately, or — when `daemon.autoStart` is `true` — fork a
 * detached daemon and poll until its socket is bound.
 *
 * The launcher is invocation-mode aware: a `deno compile`d binary
 * spawns `<self> daemon`; a `deno run` invocation spawns
 * `deno run -A <mainModule> daemon` so the workspace import map
 * (`@makina/core` is a workspace link, not a published JSR/npm
 * package) resolves identically in the child.
 *
 * @module
 */
import { fromFileUrl } from "@std/path";

/**
 * Options accepted by {@link ensureDaemonRunning}.
 */
export interface EnsureDaemonRunningOptions {
  /** Unix-domain socket path the daemon binds. */
  readonly socketPath: string;
  /**
   * If `true`, spawn the daemon when no listener is found.
   * If `false`, surface a {@link DaemonNotRunningError} instead.
   */
  readonly autoStart: boolean;
  /**
   * How long to wait for a freshly-spawned daemon to bind its socket
   * before giving up. Default: 5000 ms.
   */
  readonly timeoutMs?: number;
  /**
   * Override for the spawn step. Defaults to {@link spawnDaemon}, which
   * forks a detached `makina daemon` (or `deno run main.ts daemon`)
   * child. Tests inject a stub that does not spawn a real process —
   * typically one that immediately binds the socket so the
   * `waitForSocket` poll succeeds without a real daemon.
   */
  readonly spawn?: () => void;
}

/**
 * Thrown when no daemon is listening on the socket and `autoStart` is
 * `false`. The TUI surfaces this as a one-line diagnostic and exits
 * non-zero — distinct from {@link DaemonStartTimeoutError}, which
 * indicates the spawn raced past its budget.
 */
export class DaemonNotRunningError extends Error {
  /**
   * Construct a not-running error referencing the probed socket path.
   *
   * @param socketPath The Unix-domain socket the launcher probed.
   */
  constructor(socketPath: string) {
    super(
      `no daemon listening on ${socketPath}; either start it with ` +
        "`makina daemon` or set `daemon.autoStart` to `true` in config.json",
    );
    this.name = "DaemonNotRunningError";
  }
}

/**
 * Thrown when an autospawned daemon does not bind its socket within
 * the configured timeout. The child has already been detached; the
 * caller may retry or surface the error to the user.
 */
export class DaemonStartTimeoutError extends Error {
  /**
   * Construct a start-timeout error referencing the probed socket path
   * and the elapsed budget.
   *
   * @param socketPath The Unix-domain socket the launcher polled.
   * @param timeoutMs The bind deadline that elapsed without success.
   */
  constructor(socketPath: string, timeoutMs: number) {
    super(
      `spawned daemon did not bind ${socketPath} within ${timeoutMs}ms`,
    );
    this.name = "DaemonStartTimeoutError";
  }
}

/**
 * Ensure a daemon is listening on `opts.socketPath`. Returns once the
 * socket accepts connections.
 *
 * Behavior:
 *
 * 1. Probe the socket with a one-shot `Deno.connect`. On success, the
 *    daemon is alive and we return immediately.
 * 2. If the probe fails and `opts.autoStart` is `false`, throw
 *    {@link DaemonNotRunningError}.
 * 3. Otherwise spawn a detached daemon (see {@link spawnDaemon}) and
 *    poll the socket every 50 ms until it accepts a connection or
 *    `opts.timeoutMs` elapses (default 5000 ms).
 *
 * @param opts See {@link EnsureDaemonRunningOptions}.
 * @throws {DaemonNotRunningError} When no daemon is up and autostart
 *   is disabled.
 * @throws {DaemonStartTimeoutError} When the spawned daemon misses the
 *   bind deadline.
 */
export async function ensureDaemonRunning(
  opts: EnsureDaemonRunningOptions,
): Promise<void> {
  if (await isSocketAlive(opts.socketPath)) {
    return;
  }
  if (!opts.autoStart) {
    throw new DaemonNotRunningError(opts.socketPath);
  }
  const spawn = opts.spawn ?? spawnDaemon;
  spawn();
  const timeoutMs = opts.timeoutMs ?? 5000;
  await waitForSocket(opts.socketPath, timeoutMs);
}

/**
 * Probe `socketPath` to see whether a peer is currently accepting on
 * it. Exported for unit-test reuse so tests can drive
 * {@link ensureDaemonRunning} alongside a known socket state.
 */
export async function probeSocket(socketPath: string): Promise<boolean> {
  return await isSocketAlive(socketPath);
}

/**
 * Probe `socketPath` to see whether a peer is currently accepting on
 * it. Returns `true` only when `Deno.connect` succeeds (the connection
 * is closed immediately after).
 */
async function isSocketAlive(socketPath: string): Promise<boolean> {
  let conn: Deno.UnixConn | undefined;
  try {
    conn = await Deno.connect({ transport: "unix", path: socketPath });
    return true;
  } catch {
    return false;
  } finally {
    if (conn !== undefined) {
      try {
        conn.close();
      } catch {
        // Already closed.
      }
    }
  }
}

/**
 * Poll `socketPath` until a connection succeeds or `timeoutMs` elapses.
 *
 * The poll cadence is 50 ms — fast enough that the TUI's "Connecting…"
 * flash is brief, slow enough to avoid spinning the CPU while the
 * daemon's bare-clone fetches finish before the listener binds.
 */
async function waitForSocket(socketPath: string, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await isSocketAlive(socketPath)) {
      return;
    }
    await new Promise<void>((resolve) => setTimeout(resolve, 50));
  }
  throw new DaemonStartTimeoutError(socketPath, timeoutMs);
}

/**
 * Spawn a detached daemon child, mirroring the current invocation
 * mode (`deno compile`d binary or `deno run` script).
 *
 * The child is fully detached — stdio handles are nulled and `unref()`
 * is called so the parent process (the TUI) can exit independently.
 * The daemon's own SIGINT/SIGTERM handlers ({@link main.ts}) take
 * over its lifecycle from there.
 */
function spawnDaemon(): void {
  const { execPath, args } = resolveDaemonInvocation();
  const command = new Deno.Command(execPath, {
    args: [...args],
    stdout: "null",
    stderr: "null",
    stdin: "null",
  });
  const child = command.spawn();
  child.unref();
}

/**
 * Resolve how to spawn the daemon for the current invocation.
 *
 * - **Compiled binary**: `Deno.execPath()` is the makina binary itself
 *   and accepts the `daemon` arg directly.
 * - **`deno run`**: `Deno.execPath()` is the deno binary; we spawn it
 *   with `run -A <mainModule> daemon` so the same workspace
 *   (`deno.json` autodiscovery) resolves the `@makina/core` import.
 *
 * Detection: if `Deno.mainModule` resolves to a real file on disk,
 * we are in `deno run` mode. A `deno compile`d binary's `mainModule`
 * is an internal virtual path that does not stat.
 *
 * Exported for unit-test reuse.
 */
export function resolveDaemonInvocation(): {
  readonly execPath: string;
  readonly args: readonly string[];
} {
  const execPath = Deno.execPath();
  const scriptPath = mainModulePathOnDisk();
  if (scriptPath !== undefined) {
    return { execPath, args: ["run", "-A", scriptPath, "daemon"] };
  }
  return { execPath, args: ["daemon"] };
}

/**
 * Return the on-disk path of `Deno.mainModule` when it points at a
 * real file (i.e. running via `deno run`); `undefined` otherwise
 * (i.e. running a `deno compile`d binary, where mainModule is an
 * internal virtual path).
 */
function mainModulePathOnDisk(): string | undefined {
  let url: URL;
  try {
    url = new URL(Deno.mainModule);
  } catch {
    return undefined;
  }
  if (url.protocol !== "file:") {
    return undefined;
  }
  let path: string;
  try {
    path = fromFileUrl(url);
  } catch {
    return undefined;
  }
  try {
    const stat = Deno.statSync(path);
    return stat.isFile ? path : undefined;
  } catch {
    return undefined;
  }
}
