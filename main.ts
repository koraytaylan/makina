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
import { ConfigLoadError, expandHome, loadConfig } from "./src/config/load.ts";
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
  // The config loader is now a direct import (this PR lands it). The
  // EventBus (#8) is still loaded via dynamic `import()` because we
  // distinguish two outcomes for *that* sibling:
  //
  //  1. **Module not found** (`ERR_MODULE_NOT_FOUND`): the sibling has
  //     not landed yet. Log a single `note:` line and continue with no
  //     event bus, which makes `subscribe` envelopes ack with
  //     `{ ok: false, error: "unimplemented" }`.
  //  2. **Anything else** (parse failure, permission error, throwing
  //     constructor, …): a real misconfiguration. Print the diagnostic
  //     to stderr and exit non-zero. Silently swallowing these would
  //     mean a typo'd `config.json` boots a daemon on `/tmp/makina.sock`
  //     and the operator has no signal that anything went wrong.
  //
  // For the loader, a missing user `config.json` is also a benign signal
  // ("user has not run `makina setup` yet") — we log a one-liner and
  // fall back to `${TMPDIR:-/tmp}/makina.sock` so the binary still boots
  // for smoke-testing. Any other config failure (permissions, malformed
  // JSON, schema-invalid) exits non-zero with the loader's diagnostic.
  //
  // TODO(#8): once #8 lands, the event-bus branch becomes a direct
  // import and the module-missing fallback can be dropped.
  const tmpDir = Deno.env.get("TMPDIR") ?? "/tmp";
  const fallbackSocketPath = `${tmpDir.replace(/\/$/, "")}/makina.sock`;

  let socketPath = fallbackSocketPath;
  try {
    // `defaultConfigPath()` returns a `~/`-prefixed string; the loader
    // expands it before reading. The wizard writes to the same path, so
    // a successful `makina setup` is what populates this file.
    const config = await loadConfig(defaultConfigPath());
    // Per the loader's path-expansion contract, nested path fields
    // (including `daemon.socketPath`) are returned verbatim — each
    // consumer is responsible for expanding `~/` at its own boundary.
    // This is that boundary: `Deno.listen({ transport: "unix", path })`
    // would otherwise bind a literal `~/...` string.
    socketPath = expandHome(config.daemon.socketPath);
  } catch (error) {
    // The "not-found" branch is the "no setup yet" path; everything
    // else is a real misconfiguration the operator must fix. Narrow on
    // `ConfigLoadError` first so an unrelated error that happens to
    // expose a `kind` field cannot masquerade as a missing file.
    if (error instanceof ConfigLoadError && error.kind === "not-found") {
      console.error(
        `[daemon] note: no config.json yet (run \`makina setup\`); using fallback socket ${socketPath}`,
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
  // The interactive wizard is fully implemented and runs unconditionally.
  // What is **not** yet wired in this PR is the App-level GitHub client
  // that lists reachable installations; that lands with
  // [W2-github-app-auth] (issue #4). Until then, the wizard fails with a
  // clear "not yet implemented" message at the discovery step — the user
  // has typed their App ID and key path, but they get a single-line
  // diagnostic pointing at #4 rather than a stack trace.
  //
  // The wizard's own tests inject their own `WizardGitHubClient` and
  // never touch this code path; the in-memory doubles are isolated from
  // production wiring by design.
  const stubClient: WizardGitHubClient = {
    getInstallations(): Promise<readonly WizardInstallation[]> {
      throw new Error(
        "GitHub App auth not yet implemented (#4 — [W2-github-app-auth]).",
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
