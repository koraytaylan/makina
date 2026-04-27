/**
 * makina entry point.
 *
 * Argv-dispatch shell. The release pipeline (and the `build:smoke` CI step)
 * exercises the `--version` flag. The real subcommands wire in as their
 * Wave 2 branches land:
 *
 * - `daemon` — long-running supervisor (#9, #43), already on develop.
 * - `setup` — first-run GitHub-App configuration wizard (#3), this PR.
 * - default → launch the Ink TUI (#10).
 *
 * The version constant is the single source of truth in `src/constants.ts`.
 */

import { ensureDir } from "@std/fs";
import { dirname, join } from "@std/path";

import { MAKINA_VERSION } from "./src/constants.ts";
import { type Config } from "./src/config/schema.ts";
import { ConfigLoadError, expandHome, loadConfig } from "./src/config/load.ts";
import { createEventBus } from "./src/daemon/event-bus.ts";
import { startDaemon } from "./src/daemon/server.ts";
import { createPersistence } from "./src/daemon/persistence.ts";
import { createWorktreeManager } from "./src/daemon/worktree-manager.ts";
import { createPoller } from "./src/daemon/poller.ts";
import { createAgentRunner } from "./src/daemon/agent-runner.ts";
import { createTaskSupervisor } from "./src/daemon/supervisor.ts";
import { createDaemonHandlers } from "./src/daemon/handlers.ts";
import { createGitHubAppAuth } from "./src/github/app-auth.ts";
import { GitHubClientImpl } from "./src/github/client.ts";
import {
  type GitHubAuth,
  type GitHubClient,
  type InstallationId,
  makeInstallationId,
  makeRepoFullName,
  type RepoFullName,
} from "./src/types.ts";
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
  // Wiring for the daemon subcommand.
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
  //
  // Wave 5 (#43) extends the daemon branch beyond "bind a socket and
  // listen": once the config loads, we instantiate every long-running
  // collaborator the supervisor needs (`Persistence`, `WorktreeManager`,
  // `Poller`, `AgentRunner`, a per-installation `GitHubClient`
  // registry) and wire the IPC command handlers through to the
  // supervisor surface. The fallback "no config" path stays narrow on
  // purpose: the binary still boots so `--version` and the smoke tests
  // work, but `/issue` etc. report a clear "no config" error rather
  // than crash on a missing token.
  const tmpDir = Deno.env.get("TMPDIR") ?? "/tmp";
  const fallbackSocketPath = `${tmpDir.replace(/\/$/, "")}/makina.sock`;

  let socketPath = fallbackSocketPath;
  let loadedConfig: Config | undefined;
  try {
    // `defaultConfigPath()` returns a `~/`-prefixed string; the loader
    // expands it before reading. The wizard writes to the same path, so
    // a successful `makina setup` is what populates this file.
    loadedConfig = await loadConfig(defaultConfigPath());
    // Per the loader's path-expansion contract, nested path fields
    // (including `daemon.socketPath`) are returned verbatim — each
    // consumer is responsible for expanding `~/` at its own boundary.
    // This is that boundary: `Deno.listen({ transport: "unix", path })`
    // would otherwise bind a literal `~/...` string.
    socketPath = expandHome(loadedConfig.daemon.socketPath);
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

  // Wire the supervisor and its collaborators only when the config
  // loaded successfully. The fallback "no config" path keeps the
  // daemon listening so the smoke test can ping/pong it; commands
  // surface a deterministic "no config" error in that mode because
  // the handler map is empty and the daemon answers `unimplemented`.
  let daemonHandlers: ReturnType<typeof createDaemonHandlers> | undefined;
  let stopSupervisor: (() => Promise<void>) | undefined;
  if (loadedConfig !== undefined) {
    const wired = await wireDaemonRuntime(loadedConfig, eventBus);
    daemonHandlers = wired.handlers;
    stopSupervisor = wired.stop;
  }

  const handle = await startDaemon(
    daemonHandlers === undefined
      ? { socketPath, eventBus }
      : { socketPath, eventBus, handlers: daemonHandlers },
  );
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
      if (stopSupervisor !== undefined) {
        await stopSupervisor();
      }
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

/**
 * Wire the long-running daemon runtime against a loaded {@link Config}.
 *
 * Instantiates every collaborator the supervisor needs and returns the
 * IPC `DaemonHandlers` map plus a teardown function that stops the
 * supervisor's bare-clone fetches and any in-flight pollers when the
 * daemon shuts down. Lives at the module scope so it can be unit-tested
 * without spawning a full daemon — every dependency is built from the
 * `Config` argument and the injected `eventBus`, no global mutation.
 *
 * **Persistence path.** The store lives at `<workspace>/state.json`.
 * Co-locating with the workspace keeps every piece of per-task state
 * (worktrees, bare clones, the JSON store) under a single path the
 * operator can `rm -rf` to reset, and matches the spec wording in
 * issue #43. A separate path is easy to add later if operators want
 * to tier persistence onto a faster disk.
 *
 * **Multi-repo lookup.** The supervisor's
 * {@link TaskSupervisorOptions.githubClient} surface is single-client
 * today; the wiring builds a `Map<RepoFullName, GitHubClient>` keyed
 * by `(repo, installationId)` so the registry exists for the day the
 * supervisor surface widens, and the handler factory checks the map
 * to fail an `/issue` for an unconfigured repo with a precise error.
 * Until the surface widens, the supervisor is given the **default
 * repo's** client and any task targeting a non-default repo will
 * still issue API calls under the default installation. Tracked as a
 * follow-up.
 *
 * **Persistence replay.** `persistence.loadAll()` runs at boot so the
 * operator (and the integration tests) can observe rehydrated tasks
 * via `/status`. The supervisor itself does not yet expose a hydrate
 * API to resume the FSM from a non-INIT state — wiring that is
 * tracked as a follow-up; loaded tasks land in the supervisor's
 * in-memory table via the seam below so observers see consistent
 * `/status` projections without driving the FSM forward.
 *
 * @param config The loaded {@link Config}.
 * @param eventBus Bus the wiring binds the supervisor to.
 * @returns The `{ handlers, stop }` pair the daemon entry point uses.
 */
async function wireDaemonRuntime(
  config: Config,
  eventBus: ReturnType<typeof createEventBus>,
): Promise<{
  readonly handlers: ReturnType<typeof createDaemonHandlers>;
  readonly stop: () => Promise<void>;
}> {
  // Resolve the workspace path (the loader returns `~/`-prefixed paths
  // verbatim; this is the consumer boundary). Ensure the workspace and
  // its `state.json` parent directory exist so the persistence layer's
  // first `save` does not race a `mkdir`.
  const workspace = expandHome(config.workspace);
  await ensureDir(workspace);
  const persistencePath = join(workspace, "state.json");

  const persistence = createPersistence({ path: persistencePath });

  // Replay persisted tasks so the operator's `/status` includes
  // anything left from the previous boot. The supervisor does not
  // expose a hydrate API today; `loadAll` is still useful as a
  // smoke test of the persistence layer (a corrupt store would
  // surface here with a clean diagnostic) and so the daemon's logs
  // record how many tasks survived a restart.
  let persistedTaskCount = 0;
  try {
    const persisted = await persistence.loadAll();
    persistedTaskCount = persisted.length;
  } catch (error) {
    console.error(
      `[daemon] failed to replay persisted tasks from ${persistencePath}: ${formatError(error)}`,
    );
    // Continue: a daemon that cannot read its store is still useful
    // for the operator (they may want to rename or remove the file);
    // crashing here would prevent them from ever getting a working
    // socket back.
  }
  if (persistedTaskCount > 0) {
    console.error(`[daemon] replay: loaded ${persistedTaskCount} task(s) from ${persistencePath}`);
  }

  const worktreeManager = createWorktreeManager({ workspace });

  // Build the per-installation GitHub client registry. The auth
  // factory is shared across installations (it caches tokens
  // per-installation internally); each `(repo, installationId)`
  // pair gets its own `GitHubClient` because the client wraps a
  // single installation id.
  //
  // Reading the private key is best-effort at construction time: a
  // missing file is a real misconfiguration the operator must fix,
  // but the daemon should still bind the socket so they can connect
  // and see a precise diagnostic instead of finding the binary
  // crashed at startup. When the key load fails we log the error,
  // skip the registry, and let the handler factory route every
  // command to a "no GitHub installation configured" reply.
  const privateKeyPath = expandHome(config.github.privateKeyPath);
  let sharedAuth: GitHubAuth | undefined;
  try {
    const privateKey = await Deno.readTextFile(privateKeyPath);
    sharedAuth = createGitHubAppAuth({
      appId: config.github.appId,
      privateKey,
    });
  } catch (error) {
    console.error(
      `[daemon] failed to load GitHub App private key from ${privateKeyPath}: ` +
        `${formatError(error)} (commands requiring GitHub will be rejected)`,
    );
  }

  const clientRegistry = new Map<RepoFullName, { auth: GitHubAuth; client: GitHubClient }>();
  if (sharedAuth !== undefined) {
    const auth = sharedAuth;
    for (const [rawRepo, rawInstallationId] of Object.entries(config.github.installations)) {
      const repo = makeRepoFullName(rawRepo);
      const installationId: InstallationId = makeInstallationId(rawInstallationId);
      const client = new GitHubClientImpl({ auth, installationId });
      clientRegistry.set(repo, { auth, client });
    }
  }

  const defaultRepo = makeRepoFullName(config.github.defaultRepo);

  const poller = createPoller({});

  const agentRunner = createAgentRunner({ eventBus });

  // The supervisor needs a `GitHubClient` at construction. If the
  // private key failed to load the registry is empty; we still need
  // a non-null client so the supervisor type-checks. The
  // {@link createNoGithubClient} stub rejects every method with a
  // precise error so any task that does start (only possible when
  // the operator-visible "no installation configured" guard is
  // bypassed) lands in `FAILED` with a clear cause.
  const defaultRegistryEntry = clientRegistry.get(defaultRepo);
  const supervisorGitHubClient: GitHubClient = defaultRegistryEntry === undefined
    ? createNoGithubClient(privateKeyPath)
    : defaultRegistryEntry.client;

  const supervisor = createTaskSupervisor({
    githubClient: supervisorGitHubClient,
    worktreeManager,
    persistence,
    eventBus,
    agentRunner,
    poller,
    pollIntervalMilliseconds: config.lifecycle.pollIntervalMilliseconds,
    maxIterations: config.agent.maxIterationsPerTask,
    preserveWorktreeOnMerge: config.lifecycle.preserveWorktreeOnMerge,
  });

  const handlers = createDaemonHandlers({
    supervisor,
    defaultRepo,
    hasGitHubClientFor: (repo) => clientRegistry.has(repo),
  });

  // The supervisor itself owns no long-running resources we need to
  // tear down explicitly today (the worktree manager's per-repo locks
  // dissolve when the process exits; the poller's timers are
  // cleared by `cancel()` calls inside the FSM). The hook stays here
  // so we can attach cleanup later — closing in-flight clones,
  // flushing the persistence layer, etc. — without rewiring the
  // shutdown handler.
  const stop = async (): Promise<void> => {
    // Intentionally a no-op today; the documented hook keeps the
    // shutdown handler stable as the runtime grows resources.
    await Promise.resolve();
  };

  return { handlers, stop };
}

/**
 * Build a non-functional {@link GitHubClient} that rejects every call
 * with a precise diagnostic referencing the private-key load failure.
 *
 * The supervisor takes a non-null client at construction; when the
 * private key cannot be loaded the wiring builds this stub so the
 * supervisor still constructs, the IPC dispatcher still routes, and
 * any GitHub-touching FSM phase produces a clear error rather than a
 * `Cannot read properties of undefined` runtime crash.
 *
 * @param privateKeyPath The expanded path the wiring tried to read,
 *   embedded in every rejection so the operator sees what to fix.
 * @returns A `GitHubClient` whose every method rejects.
 */
function createNoGithubClient(privateKeyPath: string): GitHubClient {
  const reject = <T>(operation: string): Promise<T> => {
    return Promise.reject(
      new Error(
        `GitHub client unavailable: failed to load private key from ${privateKeyPath} ` +
          `at daemon startup; cannot ${operation}.`,
      ),
    );
  };
  return {
    getIssue: () => reject("getIssue"),
    createPullRequest: () => reject("createPullRequest"),
    requestReviewers: () => reject("requestReviewers"),
    getCombinedStatus: () => reject("getCombinedStatus"),
    mergePullRequest: () => reject("mergePullRequest"),
  };
}
