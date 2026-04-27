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
import { createEventBus } from "./src/daemon/event-bus.ts";
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
  // Wiring for the Wave 2 daemon subcommand.
  //
  // The config loader and the EventBus (#8) are both direct imports —
  // `src/daemon/event-bus.ts` is permanent on develop, so the earlier
  // dynamic-import + module-not-found fallback bought no value and just
  // added a complex error-narrowing branch (a parse failure, a thrown
  // top-level statement, or a permission error in the bus module would
  // have been silently demoted to "module missing" if the sniff guessed
  // wrong). A missing module now surfaces as a normal load failure.
  //
  // For the loader, a missing user `config.json` is a benign signal
  // ("user has not run `makina setup` yet") — we log a one-liner and
  // fall back to `${TMPDIR:-/tmp}/makina.sock` so the binary still boots
  // for smoke-testing. Any other config failure (permissions, malformed
  // JSON, schema-invalid) exits non-zero with the loader's diagnostic.
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

  const eventBus = createEventBus();

  const handle = await startDaemon({ socketPath, eventBus });
  console.error(`[daemon] listening on ${handle.socketPath}`);

  // Translate SIGINT/SIGTERM into a clean shutdown so a crashed daemon
  // does not leave a stale socket file behind. We always exit at the end
  // of the handler — the signal has already fired, the operator wants
  // the process gone, and any further work would race the OS-level
  // tear-down. A `handle.stop()` rejection (e.g. an in-flight tear-down
  // that fails to release the socket) is logged with a non-zero exit
  // code so it is visible to whatever supervisor restarted us; without
  // the try/catch the rejection would surface as an unhandled-promise
  // warning while the process kept running.
  const shutdown = async () => {
    try {
      await handle.stop();
      Deno.exit(0);
    } catch (error) {
      console.error(`[daemon] error during shutdown: ${formatError(error)}`);
      Deno.exit(1);
    }
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

/** Render an arbitrary error value to a single-line diagnostic. */
function formatError(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}
