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
import { basename, fromFileUrl } from "@std/path";

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
 * caller may retry or surface the error to the user. The error
 * message embeds the tail of the daemon's stderr log when one is
 * available so the user has something to act on.
 */
export class DaemonStartTimeoutError extends Error {
  /**
   * Construct a start-timeout error referencing the probed socket
   * path, the elapsed budget, and (optionally) the tail of the
   * daemon's stderr log.
   *
   * @param socketPath The Unix-domain socket the launcher polled.
   * @param timeoutMs The bind deadline that elapsed without success.
   * @param logTail Tail of the daemon's stderr log file, when one
   *   was captured. Appended to the error message verbatim.
   */
  constructor(socketPath: string, timeoutMs: number, logTail?: string) {
    const base = `spawned daemon did not bind ${socketPath} within ${timeoutMs}ms`;
    const decorated = logTail !== undefined && logTail.length > 0
      ? `${base}\n--- daemon log tail ---\n${logTail}\n--- end log tail ---`
      : base;
    super(decorated);
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
  // 15s default. A `deno compile`d binary's cold start runs the
  // embedded archive's inflate-and-link step before the daemon
  // branch even runs, which is comfortably under 5s on a warm
  // FS but flirts with that budget on a cold one. The integration
  // tests use 10s and 30s for the same reason.
  const timeoutMs = opts.timeoutMs ?? 15_000;
  try {
    await waitForSocket(opts.socketPath, timeoutMs);
  } catch (error) {
    if (error instanceof DaemonStartTimeoutError) {
      const logTail = await readDaemonLogTail();
      throw new DaemonStartTimeoutError(opts.socketPath, timeoutMs, logTail);
    }
    throw error;
  }
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
 * The daemon's stdout is sent to `/dev/null` and its stderr is
 * redirected (truncate + write) to {@link daemonLogPath} via a
 * `sh -c "exec ... 2>file"` wrapper. Going through `sh` is what
 * lets the daemon write directly to the file even after the parent
 * (the TUI) exits — a `Deno.Command` `"piped"` stderr would block
 * once the parent stops draining it. The `exec` keyword makes `sh`
 * replace itself with the daemon, so no extra shell process lingers.
 *
 * The child is detached via `unref()` so the TUI can exit
 * independently; the daemon's own SIGINT/SIGTERM handlers
 * ({@link main.ts}) take over its lifecycle.
 */
function spawnDaemon(): void {
  const { execPath, args } = resolveDaemonInvocation();
  const logFile = daemonLogPath();
  const cmdline = [execPath, ...args].map(shellQuote).join(" ");
  const command = new Deno.Command("sh", {
    args: ["-c", `exec ${cmdline} >/dev/null 2>${shellQuote(logFile)}`],
    stdout: "null",
    stderr: "null",
    stdin: "null",
  });
  const child = command.spawn();
  child.unref();
}

/**
 * Filesystem path the daemon's stderr log lands at when the launcher
 * spawns it. Lives under `$TMPDIR` (with `/tmp` as the cross-platform
 * fallback) so the user can `tail -f` it for live diagnostics.
 */
export function daemonLogPath(): string {
  const tmpDir = (Deno.env.get("TMPDIR") ?? "/tmp").replace(/\/$/, "");
  return `${tmpDir}/makina-daemon.log`;
}

/**
 * Read the last `maxLines` lines of the daemon's stderr log, or
 * `undefined` if the log does not exist yet (a missing log is the
 * normal case when the launcher has never autospawned a daemon).
 *
 * Used by {@link ensureDaemonRunning} to enrich the timeout error
 * with whatever the daemon child managed to write before giving up.
 */
async function readDaemonLogTail(maxLines = 30): Promise<string | undefined> {
  let text: string;
  try {
    text = await Deno.readTextFile(daemonLogPath());
  } catch {
    return undefined;
  }
  const lines = text.split("\n").filter((line) => line.length > 0);
  if (lines.length === 0) {
    return undefined;
  }
  return lines.slice(-maxLines).join("\n");
}

/**
 * Single-quote `s` for use inside a `sh -c` argument: every embedded
 * single quote becomes `'\''` and the whole thing is wrapped in
 * single quotes. Sufficient for our two callers (an absolute exec
 * path and a fixed log path); not a general-purpose POSIX shell
 * escaper.
 */
function shellQuote(s: string): string {
  return `'${s.replaceAll("'", `'\\''`)}'`;
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
 * Detection: if `Deno.execPath()`'s basename is `deno` (the
 * canonical CLI name), we are running via `deno run`. Anything else
 * is the compiled binary. We deliberately do **not** key off
 * `Deno.mainModule`: a `deno compile`d binary extracts its embedded
 * archive into `${TMPDIR}/deno-compile-<name>/...` and reports that
 * path as `Deno.mainModule`, so `Deno.statSync(mainModule)` succeeds
 * inside a compiled binary too — relying on it caused a real
 * fork-bomb (the compiled binary spawned itself with `run -A
 * <extracted>/main.ts daemon`, which has no `run` subcommand and
 * fell back to the default TUI branch, which spawned another
 * "daemon" the same way…).
 *
 * Exported for unit-test reuse.
 */
export function resolveDaemonInvocation(): {
  readonly execPath: string;
  readonly args: readonly string[];
} {
  const execPath = Deno.execPath();
  if (isDenoRunMode(execPath)) {
    const scriptPath = mainModulePathOnDisk();
    if (scriptPath !== undefined) {
      return { execPath, args: ["run", "-A", scriptPath, "daemon"] };
    }
  }
  return { execPath, args: ["daemon"] };
}

/**
 * `true` iff `execPath`'s basename is `deno` (or `deno.exe` on
 * Windows) — i.e. we are running via `deno run`, not a compiled
 * binary.
 */
function isDenoRunMode(execPath: string): boolean {
  const name = basename(execPath);
  const stripped = name.endsWith(".exe") ? name.slice(0, -".exe".length) : name;
  return stripped === "deno";
}

/**
 * Return the on-disk path of `Deno.mainModule` when it points at a
 * real file. Used only after {@link isDenoRunMode} confirms we are
 * actually running via `deno run`; the stat is otherwise unreliable
 * because a compiled binary also extracts its `mainModule` to a
 * real path under `${TMPDIR}/deno-compile-<name>/...`.
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
