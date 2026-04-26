/**
 * makina entry point.
 *
 * Argv-dispatch shell. Today this just exposes the `--version` flag so the
 * release pipeline (and the `build:smoke` CI step) can prove that a compiled
 * binary starts. The real subcommands (`daemon`, `setup`, default TUI) are
 * wired in by the Wave 2+ feature branches; this file is the only entrypoint
 * the eventual `deno compile` target produces, and it imports the version
 * string from `src/constants.ts` so both this stub and any future code share
 * a single source of truth.
 */

import { MAKINA_VERSION } from "./src/constants.ts";
import { startDaemon } from "./src/daemon/server.ts";

if (Deno.args.includes("--version")) {
  console.log(MAKINA_VERSION);
  Deno.exit(0);
}

if (Deno.args[0] === "daemon") {
  // Defensive wiring for the Wave 2 daemon subcommand.
  //
  // The real config loader (#3) and EventBus (#8) may or may not be on
  // `develop` when this branch lands; both are wrapped in a `try` so a
  // missing sibling does not block the daemon from starting. When the
  // sibling is absent we fall back to:
  //   - a hardcoded `${TMPDIR:-/tmp}/makina.sock` socket path so the
  //     binary can boot for smoke-testing;
  //   - no event bus, which makes `subscribe` envelopes ack with
  //     `{ ok: false, error: "unimplemented" }` until #8 lands.
  //
  // TODO(#3): replace the fallback socket path with
  // `loadConfig().daemon.socketPath`.
  // TODO(#8): replace the absent bus with an `InProcessEventBus`.
  const tmpDir = Deno.env.get("TMPDIR") ?? "/tmp";
  const fallbackSocketPath = `${tmpDir.replace(/\/$/, "")}/makina.sock`;

  let socketPath = fallbackSocketPath;
  try {
    const configModule = await import("./src/config/load.ts");
    if (
      typeof (configModule as { loadConfig?: unknown }).loadConfig === "function"
    ) {
      const loadConfig = (configModule as {
        loadConfig: () => Promise<{ daemon: { socketPath: string } }>;
      }).loadConfig;
      const config = await loadConfig();
      socketPath = config.daemon.socketPath;
    }
  } catch {
    // Sibling not landed yet; keep the fallback path.
  }

  let eventBus: import("./src/types.ts").EventBus | undefined;
  try {
    const busModule = await import("./src/daemon/event-bus.ts");
    const ctor = (busModule as { InProcessEventBus?: new () => unknown })
      .InProcessEventBus;
    if (typeof ctor === "function") {
      eventBus = new ctor() as import("./src/types.ts").EventBus;
    }
  } catch {
    // Sibling not landed yet; fall through with no bus.
  }

  const handle = await startDaemon(
    eventBus === undefined ? { socketPath } : { socketPath, eventBus },
  );
  console.error(`[daemon] listening on ${handle.socketPath}`);

  // Translate SIGINT/SIGTERM into a clean shutdown so a crashed daemon
  // does not leave a stale socket file behind.
  const shutdown = async () => {
    await handle.stop();
    Deno.exit(0);
  };
  Deno.addSignalListener("SIGINT", shutdown);
  Deno.addSignalListener("SIGTERM", shutdown);
} else {
  console.log("makina — agentic GitHub issue resolver");
  console.log("");
  console.log("Wave 1 skeleton. Most subcommands are not yet implemented.");
  console.log("Run `makina --version` to see the version.");
  console.log("See https://github.com/koraytaylan/makina for status.");
}
