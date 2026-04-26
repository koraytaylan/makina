/**
 * makina entry point.
 *
 * Argv-dispatch shell. The release pipeline (and the `build:smoke` CI step)
 * exercises the `--version` flag. The real subcommands wire in as their
 * Wave 2 branches land:
 *
 * - `daemon` — long-running supervisor (#9), already on develop.
 * - `setup` — first-run GitHub-App configuration wizard (#3), this PR.
 * - default → launch the Ink TUI (#10).
 *
 * The version constant is the single source of truth in `src/constants.ts`.
 */

import { ensureDir } from "@std/fs";
import { dirname } from "@std/path";

import { MAKINA_VERSION } from "./src/constants.ts";
import { expandHome } from "./src/config/load.ts";
import { startDaemon } from "./src/daemon/server.ts";
import {
  createStdioWizardIo,
  defaultConfigPath,
  runSetupWizard,
  SetupWizardError,
  type WizardGitHubClient,
  type WizardInstallation,
} from "./src/config/setup-wizard.ts";

if (Deno.args.includes("--version")) {
  console.log(MAKINA_VERSION);
  Deno.exit(0);
}

const subcommand = Deno.args[0];

if (subcommand === "daemon") {
  // Defensive wiring for the Wave 2 daemon subcommand.
  //
  // The real config loader (#3) and EventBus (#8) may or may not be on
  // `develop` when this branch lands; both are loaded via dynamic
  // `import()` so a still-missing sibling does not block the daemon
  // from starting. We carefully distinguish two outcomes:
  //
  //  1. **Module not found** (`ERR_MODULE_NOT_FOUND`): the sibling has
  //     not landed yet. Log a single `note:` line so operators know we
  //     fell back, then continue with:
  //       - a hardcoded `${TMPDIR:-/tmp}/makina.sock` socket path so
  //         the binary can boot for smoke-testing;
  //       - no event bus, which makes `subscribe` envelopes ack with
  //         `{ ok: false, error: "unimplemented" }` until #8 lands.
  //  2. **Anything else** (parse failure, permission error, throwing
  //     constructor, …): a real misconfiguration. Print the diagnostic
  //     to stderr and exit non-zero. Silently swallowing these would
  //     mean a typo'd `config.json` boots a daemon on `/tmp/makina.sock`
  //     and the operator has no signal that anything went wrong.
  //
  // TODO(#3): once #3 lands, the loadConfig branch is the only path —
  // drop the module-missing fallback.
  // TODO(#8): once #8 lands, the same applies to the event bus branch.
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
  } catch (error) {
    if (isModuleNotFoundError(error)) {
      console.error(
        `[daemon] note: src/config/load.ts not found; using fallback socket ${socketPath}`,
      );
    } else {
      console.error(`[daemon] failed to load configuration: ${formatError(error)}`);
      Deno.exit(1);
    }
  }

  let eventBus: import("./src/types.ts").EventBus | undefined;
  try {
    const busModule = await import("./src/daemon/event-bus.ts");
    const ctor = (busModule as { InProcessEventBus?: new () => unknown })
      .InProcessEventBus;
    if (typeof ctor === "function") {
      eventBus = new ctor() as import("./src/types.ts").EventBus;
    }
  } catch (error) {
    if (isModuleNotFoundError(error)) {
      console.error(
        "[daemon] note: src/daemon/event-bus.ts not found; subscribe will reply unimplemented",
      );
    } else {
      console.error(`[daemon] failed to initialise event bus: ${formatError(error)}`);
      Deno.exit(1);
    }
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
} else if (subcommand === "setup") {
  // The interactive wizard is fully implemented; what is **not** yet
  // wired in this PR is the App-level GitHub client that lists reachable
  // installations. That client lands with [W2-github-app-auth] (issue #4)
  // and the wizard already accepts it via injection. To avoid a UX where
  // the user types the App ID and key path only to hit a "not
  // implemented" error mid-flow, the subcommand gates itself behind the
  // `MAKINA_ALLOW_WIZARD_STUB=1` env var until #4 lands. Without the
  // flag we exit immediately with a pointer; with the flag the wizard
  // runs and the stub surfaces a clear failure on the discovery step.
  const allowStub = Deno.env.get("MAKINA_ALLOW_WIZARD_STUB") === "1";
  if (!allowStub) {
    console.error(
      "makina setup is not yet usable: the GitHub App client is wired in #4.",
    );
    console.error(
      "Track issue #4 ([W2-github-app-auth]); set MAKINA_ALLOW_WIZARD_STUB=1 to dry-run the wizard.",
    );
    Deno.exit(2);
  }
  // Stub used only for the `MAKINA_ALLOW_WIZARD_STUB=1` dry-run path. It
  // throws on the discovery step so contributors can exercise the
  // prompts without a real App client; the wizard's own tests inject
  // their own client and never touch this code.
  const stubClient: WizardGitHubClient = {
    getInstallations(): Promise<readonly WizardInstallation[]> {
      throw new Error(
        "Real GitHub App client is not yet wired. Track [W2-github-app-auth] (issue #4).",
      );
    },
  };
  try {
    const config = await runSetupWizard(createStdioWizardIo(stubClient));
    const targetPath = expandHome(defaultConfigPath());
    await ensureDir(dirname(targetPath));
    await Deno.writeTextFile(targetPath, `${JSON.stringify(config, null, 2)}\n`);
    console.log(`Wrote ${targetPath}.`);
    Deno.exit(0);
  } catch (error) {
    if (error instanceof SetupWizardError) {
      console.error(`setup failed: ${error.message}`);
      Deno.exit(1);
    }
    throw error;
  }
} else {
  // Default branch → launch the Ink-based TUI.
  await import("./src/tui/App.tsx").then((module) => module.launch());
}

/**
 * Whether `error` is the specific "module file does not exist" failure
 * that Deno's dynamic `import()` raises. We treat this as a benign
 * "sibling has not landed yet" signal; every other failure (a parse
 * error in the module, a thrown top-level statement, a permission
 * issue) is propagated by the caller so misconfiguration cannot silently
 * downgrade the daemon's behaviour.
 *
 * Deno surfaces this as a `TypeError` whose `code` is the Node-style
 * `ERR_MODULE_NOT_FOUND`. We sniff the `code` field defensively because
 * the constructor is `TypeError` (a base class shared with many other
 * runtime failures), and the message text is best-effort only.
 */
function isModuleNotFoundError(error: unknown): boolean {
  if (!(error instanceof TypeError)) {
    return false;
  }
  const code = (error as TypeError & { code?: unknown }).code;
  return code === "ERR_MODULE_NOT_FOUND";
}

/** Render an arbitrary error value to a single-line diagnostic. */
function formatError(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}
