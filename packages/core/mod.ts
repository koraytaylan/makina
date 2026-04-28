/**
 * Public surface for `@makina/core`.
 *
 * Re-exports every module the orchestration engine ships. The CLI consumes
 * this via Deno workspace local linking; a future SaaS web app will consume
 * the same surface via JSR. Adapters (`Persistence`, `WorktreeManager`,
 * `AgentRunner`, `GitHubClient`) ship as concrete defaults that downstream
 * deployments can swap by injecting a different implementation of the
 * matching interface.
 */

export * from "./src/constants.ts";
export * from "./src/types.ts";

export * from "./src/config/schema.ts";
export * from "./src/config/load.ts";

export * from "./src/daemon/agent-runner.ts";
export * from "./src/daemon/event-bus.ts";
export * from "./src/daemon/handlers.ts";
export * from "./src/daemon/persistence.ts";
export * from "./src/daemon/poller.ts";
export * from "./src/daemon/server.ts";
export * from "./src/daemon/stabilize.ts";
export * from "./src/daemon/supervisor.ts";
export * from "./src/daemon/worktree-manager.ts";

export * from "./src/github/app-auth.ts";
// The wizard-side AppClient defines its own InstallationAuthResult interface
// that intentionally diverges from the one in app-auth.ts (different shape,
// different lifecycle). Both names live in their own module; we re-export
// app-client's surface explicitly here, omitting the colliding name. Anyone
// who needs the wizard variant imports it directly from
// "@makina/core/src/github/app-client.ts" or via a future named sub-export.
export {
  type AppAuthResult,
  type AppClient,
  type AppClientAuthHook,
  type AppClientAuthStrategy,
  type AppInstallation,
  createAppClient,
  type CreateAppClientOptions,
  DEFAULT_USER_AGENT,
  GitHubAppClientError,
  type InstallationRepository,
} from "./src/github/app-client.ts";
export * from "./src/github/client.ts";

export * from "./src/ipc/codec.ts";
export * from "./src/ipc/protocol.ts";
