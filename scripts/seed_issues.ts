/**
 * seed_issues.ts — create the wave-catalog GitHub issues for makina.
 *
 * Run once during Wave 0 (after the repo and labels exist). Idempotent: any
 * title that already exists in the repo is skipped, so re-running is safe.
 *
 * Usage:
 *   deno run -A scripts/seed_issues.ts                 # creates everything
 *   deno run -A scripts/seed_issues.ts --dry-run       # prints what it would do
 *
 * The catalog mirrors the §Issue catalog section of the plan. Each entry is
 * an elaborate self-contained brief so an agent can pick the issue up
 * without external context.
 */

import { parseArgs } from "@std/cli/parse-args";

interface IssueSpec {
  readonly title: string;
  readonly labels: readonly string[];
  readonly body: string;
}

const REPO = "koraytaylan/makina";

const DEFINITION_OF_DONE = `**Definition of Done** (always required, not optional):

- [ ] New exported symbols carry JSDoc with \`@param\`, \`@returns\`, \`@example\` where useful; \`deno doc --lint\` is green.
- [ ] Tests cover the new code; coverage is ≥ 80% lines AND ≥ 80% branches; in-memory doubles for any external collaborator land in the same PR as the contract they implement.
- [ ] Any new architectural choice ships with its ADR under \`docs/adrs/\`.
- [ ] User-facing behavior changes update \`README.md\` and the relevant \`docs/*.md\`.
- [ ] Commits follow Conventional Commits (\`<type>[(<scope>)][!]: <subject>\`); subject ≤ 72 chars.
- [ ] \`deno task ci\` is green on the branch.`;

function buildBody(opts: {
  dependsOn: string;
  goal: string;
  scope: string;
  files: string;
  inputs: string;
  outputs: string;
  acceptance: string;
  outOfScope: string;
  references: string;
}): string {
  return `## Depends on

${opts.dependsOn}

## Goal

${opts.goal}

## Scope

${opts.scope}

## Files

${opts.files}

## Inputs (contracts depended on)

${opts.inputs}

## Outputs

${opts.outputs}

## Acceptance criteria

${DEFINITION_OF_DONE}

**Issue-specific:**

${opts.acceptance}

## Out of scope

${opts.outOfScope}

## References

${opts.references}
`;
}

const issues: readonly IssueSpec[] = [
  {
    title: "[W0-bootstrap] Bootstrap project skeleton, remote, and issue catalog",
    labels: ["wave:0", "type:scaffolding", "agent:in-progress"],
    body: buildBody({
      dependsOn: "Nothing — this is the entry point.",
      goal:
        "Land a green-CI skeleton on `main`, create the public remote, branch `develop`, apply protections, seed labels, and create every other issue in this catalog. From the moment this issue closes, all subsequent waves can be picked up by parallel agents.",
      scope:
        "Wave 0 of the plan, end to end: local file scaffolding (deno.json, workflows, hooks, scripts, docs, ADRs); `git init` + initial commit `chore: bootstrap project skeleton`; `gh repo create koraytaylan/makina --public`; push `main`; branch + push `develop`; branch protections on both branches; seed wave/type/agent labels; dry-run `v0.0.0-rc.0` release and clean up; create every other issue in the catalog; open the long-lived release-tracking issue.",
      files:
        "Everything under §File layout in the plan, except the wave-1+ source files. See the initial commit for the full list.",
      inputs: "Nothing.",
      outputs:
        "A working repo at https://github.com/koraytaylan/makina with green CI, a successful dry-run release artifact, all issues from this catalog created, and `develop` checked out as the integration branch.",
      acceptance: `- [ ] \`deno task ci\` is green on the initial commit.
- [ ] Dry-run \`v0.0.0-rc.0\` release succeeds (notes + 4 binaries + \`SHASUMS256.txt\`) and is then deleted.
- [ ] \`gh issue list --limit 100\` returns 20 successor issues with the labels declared in this catalog.
- [ ] Direct push to \`main\` is rejected by branch protection.
- [ ] Direct push to \`develop\` is rejected by branch protection.
- [ ] \`gh label list\` returns the full wave/type/agent label set.`,
      outOfScope: "Any wave-1+ source code; this issue is exclusively about scaffolding.",
      references: "Plan §Repository setup, §Wave 0; ADR-001, ADR-004, ADR-009.",
    }),
  },
  {
    title: "[W1-contracts] Define foundational typed contracts and in-memory doubles",
    labels: ["wave:1", "type:contract", "agent:claimable"],
    body: buildBody({
      dependsOn: "`[W0-bootstrap]`",
      goal:
        "Establish immutable typed interfaces so Wave 2 modules can be implemented in parallel against frozen contracts. Ship the in-memory doubles alongside the interfaces so consumer-side tests can run before any provider is implemented.",
      scope:
        "Production: `src/types.ts` (Task, TaskState, TaskEvent, branded ids); `src/constants.ts` (every named numeric constant; identifier carries the unit, e.g. `SETTLING_WINDOW_MILLISECONDS`); `src/config/schema.ts` (zod schema for config.json with helpful error paths); `src/ipc/protocol.ts` (zod schemas per IPC message type); `src/ipc/codec.ts` (NDJSON framing with length-prefix validation). Test infrastructure: `tests/helpers/in_memory_github_client.ts` (scriptable timeline), `tests/helpers/in_memory_github_auth.ts` (token mint stub), `tests/helpers/in_memory_daemon_client.ts` (in-process IPC for the TUI shell), `tests/helpers/mock_agent_runner.ts` (scripted `SDKMessage` streams). Tests: `tests/unit/types_test.ts`, `tests/unit/ipc_codec_test.ts`, `tests/unit/config_schema_test.ts`. Removes the Wave 0 placeholders `src/welcome.ts` and `tests/unit/sanity_test.ts`.",
      files:
        "- src/types.ts\n- src/constants.ts\n- src/config/schema.ts\n- src/ipc/protocol.ts\n- src/ipc/codec.ts\n- tests/helpers/in_memory_github_client.ts\n- tests/helpers/in_memory_github_auth.ts\n- tests/helpers/in_memory_daemon_client.ts\n- tests/helpers/mock_agent_runner.ts\n- tests/unit/{types,ipc_codec,config_schema}_test.ts\n- (removes) src/welcome.ts, tests/unit/sanity_test.ts",
      inputs: "None — this issue defines the contracts.",
      outputs:
        "All the typed interfaces every later wave imports. The in-memory doubles let Wave 2 branches run their tests immediately without coordinating implementation order.",
      acceptance:
        `- [ ] Codec rejects malformed frames (length mismatch, partial frames, oversized frames).
- [ ] Config schema rejects out-of-range values with helpful zod paths in the error message.
- [ ] In-memory doubles compile against their interfaces and have at least one no-op test each.
- [ ] All the named constants are unit-suffixed; the linter trips on bare numeric literals in the new modules.`,
      outOfScope:
        "Implementations that USE these contracts (those are Wave 2 issues). Real GitHub or Claude calls (those land in Wave 3+).",
      references: "Plan §Architecture, §Configuration, §IPC protocol; ADR-004.",
    }),
  },
  {
    title: "[W2-config-loader] Implement config loader and `setup` wizard",
    labels: ["wave:2", "type:module", "agent:blocked"],
    body: buildBody({
      dependsOn: "`[W1-contracts]`",
      goal:
        "Provide a typed loader for the user's config file and an interactive first-run wizard that produces a valid one.",
      scope:
        "`src/config/load.ts` (parse JSON, expand `~/`, validate via the W1 zod schema, surface zod paths in error messages). `src/config/setup-wizard.ts` (stdin-driven prompts for App ID, private-key path, default-repo discovery via the GitHub App's installations endpoint). Wired into `main.ts setup`.",
      files:
        "- src/config/load.ts\n- src/config/setup-wizard.ts\n- main.ts (wire `setup` subcommand)\n- tests/unit/config_load_test.ts\n- tests/integration/setup_wizard_test.ts",
      inputs:
        "- `Config` schema from `src/config/schema.ts` (W1)\n- `InMemoryGitHubClient` from `tests/helpers/in_memory_github_client.ts` (W1) for the wizard's installation discovery test",
      outputs:
        "- `loadConfig(path: string): Promise<Config>`\n- `runSetupWizard(io): Promise<Config>` that the `setup` subcommand invokes.",
      acceptance: `- [ ] Loader expands \`~/\` and reports the failing zod path on invalid config.
- [ ] Wizard happy path tested end-to-end with scripted stdin against the in-memory GitHub client.
- [ ] Wizard rejects a missing private-key file with a helpful error.
- [ ] \`deno task setup\` runs against an empty home dir and produces a valid config.`,
      outOfScope: "GitHub App authentication (that is `[W2-github-app-auth]`).",
      references: "Plan §Configuration; ADR-003.",
    }),
  },
  {
    title: "[W2-github-app-auth] Implement GitHub App auth (JWT + installation tokens)",
    labels: ["wave:2", "type:module", "agent:blocked"],
    body: buildBody({
      dependsOn: "`[W1-contracts]`",
      goal:
        "Mint installation tokens via `@octokit/auth-app` with a correct expiry-aware cache, exposed as the `GitHubAuth` interface from W1.",
      scope:
        "`src/github/app-auth.ts` exposing `createGitHubAppAuth(opts): GitHubAuth`. Uses `@octokit/auth-app` for JWT signing and installation-token exchange. Caches each installation's token until `expires_at - 60_000 ms`.",
      files: "- src/github/app-auth.ts\n- tests/unit/app_auth_test.ts",
      inputs:
        "- `GitHubAuth` interface from `src/types.ts` (W1)\n- npm dependency `@octokit/auth-app`",
      outputs:
        "- `createGitHubAppAuth(opts: { appId, privateKey, installationId }): GitHubAuth` returning a `getInstallationToken(): Promise<string>`",
      acceptance: `- [ ] Token cache hit, miss, and near-expiry refresh all covered by tests.
- [ ] Errors from \`@octokit/auth-app\` propagate with a clear message.
- [ ] Private-key file IO is mocked; no real network in unit tests.`,
      outOfScope: "High-level GitHub API methods (those land in `[W2-github-client]`).",
      references: "Plan §Architecture (GitHubClient); ADR-005.",
    }),
  },
  {
    title: "[W2-github-client] Implement high-level GitHubClient over `@octokit/core`",
    labels: ["wave:2", "type:module", "agent:blocked"],
    body: buildBody({
      dependsOn:
        "`[W1-contracts]` (uses the `GitHubAuth` interface; tests use the in-memory auth double)",
      goal:
        "A single typed client exposing every GitHub method the supervisor needs, honoring rate-limit and retry headers.",
      scope:
        "`src/github/client.ts` exposing `getIssue`, `createPullRequest`, `requestReviewers`, `getCombinedStatus`, `listCheckRuns`, `getCheckRunLogs`, `listReviews`, `listReviewComments`, `mergePullRequest`. Built on `@octokit/core` with the `GitHubAuth` strategy injected. Honors `Retry-After` and `X-RateLimit-Reset`.",
      files: "- src/github/client.ts\n- tests/unit/github_client_test.ts",
      inputs:
        "- `GitHubAuth` interface from `src/types.ts` (W1)\n- in-memory auth double from W1\n- npm dependency `@octokit/core`",
      outputs:
        "- `GitHubClient` class implementing the interface from W1, ready to be consumed by the supervisor in Wave 3.",
      acceptance:
        `- [ ] Each method has a happy-path + 429 retry + 5xx propagation test using Octokit's mock fetch.
- [ ] No real network in unit tests.
- [ ] Rate-limit headers respected; the test for that asserts the delay before retry.`,
      outOfScope:
        "GraphQL surface (deferred to `[W4-stabilize-conversations]` if `resolveReviewThread` warrants `@octokit/graphql`).",
      references: "Plan §Architecture (GitHubClient), §Lifecycle.",
    }),
  },
  {
    title: "[W2-worktree-manager] Implement WorktreeManager (bare clone + per-task worktrees)",
    labels: ["wave:2", "type:module", "agent:blocked"],
    body: buildBody({
      dependsOn: "`[W1-contracts]`",
      goal:
        "Manage a bare clone per repo and a per-task worktree on its own branch, so concurrent agents never share a working directory.",
      scope:
        "`src/daemon/worktree-manager.ts` exposing `ensureBareClone(repo)`, `createWorktreeForIssue(repo, issueNumber)`, `removeWorktree(taskId)`. All `git` invocations through `Deno.Command`. Branch naming `makina/issue-<n>`. Bare clone path `<workspace>/repos/<owner>__<name>.git`. Worktree path `<workspace>/worktrees/<owner>__<name>/issue-<n>`.",
      files: "- src/daemon/worktree-manager.ts\n- tests/integration/worktree_manager_test.ts",
      inputs: "- types from W1\n- `git` CLI on `PATH`",
      outputs:
        "- `WorktreeManager` interface from `src/types.ts` (W1) implementation; ready for the supervisor (W3).",
      acceptance:
        `- [ ] Integration tests against \`Deno.makeTempDir\`-backed repos cover concurrent worktree creation, removal, and the preserve-on-NEEDS_HUMAN policy.
- [ ] \`git worktree prune\` runs on manager startup so an interrupted daemon does not leak metadata.
- [ ] Worktree paths exactly match the convention so users can navigate to one for manual intervention.`,
      outOfScope:
        "Cloning private repos using App auth (W3 supervisor wires the auth in via remote URLs).",
      references: "Plan §Architecture (WorktreeManager); ADR-007.",
    }),
  },
  {
    title: "[W2-persistence] Implement atomic JSON state store",
    labels: ["wave:2", "type:module", "agent:blocked"],
    body: buildBody({
      dependsOn: "`[W1-contracts]`",
      goal: "Durable, crash-safe storage of task state with replay on daemon restart.",
      scope:
        "`src/daemon/persistence.ts` with `loadAll(): Promise<Task[]>` and `save(task: Task): Promise<void>`. Atomic write: serialize → write to `.tmp` → fsync → rename. All file IO through `@std/fs`.",
      files: "- src/daemon/persistence.ts\n- tests/integration/persistence_test.ts",
      inputs: "- `Task` from `src/types.ts` (W1)\n- `@std/fs` (jsr)",
      outputs:
        "- `Persistence` interface implementation; the supervisor (W3) uses it after every state transition.",
      acceptance:
        `- [ ] Tests use \`Deno.makeTempDir\`; cover concurrent writes (serialized internally), partial-write recovery (corrupt \`.tmp\` ignored), and round-trip equivalence.
- [ ] No data loss on simulated kill mid-write.`,
      outOfScope: "Schema migrations (out of scope until v1 ships).",
      references: "Plan §Architecture (Persistence).",
    }),
  },
  {
    title: "[W2-event-bus] Implement typed in-process event bus",
    labels: ["wave:2", "type:module", "agent:blocked"],
    body: buildBody({
      dependsOn: "`[W1-contracts]`",
      goal:
        "Pub/sub for task events with topic addressing and bounded backpressure, used by both the supervisor (publisher) and the daemon server (forwarder).",
      scope:
        "`src/daemon/event-bus.ts` exposing `publish(taskId, event)`, `subscribe(taskId | '*', handler)`, `unsubscribe(handle)`. Subscribers receive events through a bounded `ReadableStream`; slow consumers drop with a warning logged via `@std/log`.",
      files: "- src/daemon/event-bus.ts\n- tests/unit/event_bus_test.ts",
      inputs: "- `TaskEvent` from `src/types.ts` (W1)",
      outputs: "- `EventBus` ready for the daemon and supervisor.",
      acceptance:
        `- [ ] Wildcard fan-out works (a publish to a task is observed by both \`task:<id>\` and \`task:*\` subscribers).
- [ ] Unsubscribe cleanly removes the handler.
- [ ] Bounded-stream behavior under a flooding publisher (slow consumer drops, warning logged once per occurrence).`,
      outOfScope:
        "Cross-process pub/sub (the daemon server fans events out over the IPC layer; that is `[W2-daemon-server]`).",
      references: "Plan §Architecture (EventBus).",
    }),
  },
  {
    title: "[W2-daemon-server] Implement daemon Unix socket server + dispatch",
    labels: ["wave:2", "type:module", "agent:blocked"],
    body: buildBody({
      dependsOn: "`[W1-contracts]`",
      goal:
        "Accept TUI clients over a Unix socket, decode NDJSON, dispatch to a pluggable command handler. No supervisor wiring yet; the supervisor lands in Wave 3 and is injected here.",
      scope:
        "`src/daemon/server.ts` exposing `startDaemon({ socketPath, handlers })`. Handlers for `ping` and `subscribe` only at this stage; `command`/`prompt` return `ack { ok: false, error: 'unimplemented' }`. Cleans up stale sockets on start.",
      files:
        "- src/daemon/server.ts\n- main.ts (wire `daemon` subcommand)\n- tests/integration/daemon_server_test.ts",
      inputs:
        "- IPC protocol from `src/ipc/protocol.ts` (W1)\n- IPC codec from `src/ipc/codec.ts` (W1)",
      outputs:
        "- `startDaemon(opts)` accepts a TUI connection, exchanges `ping` ↔ `pong`, and forwards subscribed events.",
      acceptance:
        `- [ ] Integration test boots the server on a temp-dir socket and round-trips \`ping → pong\`.
- [ ] A synthetic event published into a \`task:*\` subscription reaches the client.
- [ ] Stale socket from a previous run is removed on startup.`,
      outOfScope: "Real command handling (Wave 3); supervisor integration (Wave 3).",
      references: "Plan §Architecture (Process model, IPC).",
    }),
  },
  {
    title: "[W2-tui-shell] Implement TUI shell components and Ink-on-Deno feasibility gate",
    labels: ["wave:2", "type:module", "agent:blocked"],
    body: buildBody({
      dependsOn:
        "`[W1-contracts]` (uses the IPC protocol from W1; tests use the in-memory daemon double)",
      goal:
        "A working Ink-rendered shell that connects to the daemon and renders state. **Doubles as the Ink-on-Deno feasibility gate** — if Yoga's WASM or `node:tty` raw-mode does not work under Deno 2.7, escalate to ADR-010 (TBD; Node-side Ink subprocess fallback) rather than push the risk to later waves.",
      scope:
        "`src/tui/App.tsx`, `src/tui/components/{Header,MainPane,StatusBar}.tsx`, `src/tui/ipc-client.ts`, `src/tui/hooks/useDaemonConnection.ts`. Snapshot tests via `@std/testing/snapshot` against `ink-testing-library` (or equivalent rendered string).",
      files:
        "- src/tui/App.tsx\n- src/tui/components/Header.tsx\n- src/tui/components/MainPane.tsx\n- src/tui/components/StatusBar.tsx\n- src/tui/ipc-client.ts\n- src/tui/hooks/useDaemonConnection.ts\n- main.ts (default → TUI launch)\n- tests/unit/tui_shell_test.ts",
      inputs:
        "- IPC protocol from `src/ipc/protocol.ts` (W1)\n- in-memory daemon double from `tests/helpers/in_memory_daemon_client.ts` (W1)",
      outputs:
        "- A renderable Ink app that connects to a daemon over a Unix socket and shows Header + empty MainPane + StatusBar.",
      acceptance:
        `- [ ] \`deno run -A main.ts\` launches the TUI, connects to a running daemon, renders correctly.
- [ ] Snapshot tests cover initial render and "1 task active" state.
- [ ] If the Ink-on-Deno feasibility gate fails, ship ADR-010 (escalation) in this PR instead of working around silently.`,
      outOfScope: "Command palette and task switcher (Wave 3); slash-command parser (Wave 3).",
      references: "Plan §Architecture (TUI), §Risks (Ink under Deno); ADR-001.",
    }),
  },
  {
    title: "[W3-agent-runner] Implement AgentRunner wrapping `@anthropic-ai/claude-agent-sdk`",
    labels: ["wave:3", "type:integration", "agent:blocked"],
    body: buildBody({
      dependsOn: "`[W1-contracts]`, `[W2-worktree-manager]`",
      goal:
        "Stream `SDKMessage`s from `query()` into the event bus tagged with `taskId`, with session resumption per task so model context carries across CI/review/rebase iterations.",
      scope:
        "`src/daemon/agent-runner.ts` exposing `runAgent({ taskId, worktreePath, sessionId?, prompt }): AsyncIterable<SDKMessage>`. Auto-discovers `claude` via `which`. Persists `sessionId` after the first run (the supervisor stores it). System-prompt addendum instructs the agent to author commits in `<type>(#<issueNumber>): <subject>` format.",
      files:
        "- src/daemon/agent-runner.ts\n- tests/unit/agent_runner_test.ts (uses MockAgentRunner)\n- tests/integration/agent_runner_real_test.ts (skip if `claude` not on PATH)",
      inputs:
        "- npm dependency `@anthropic-ai/claude-agent-sdk`\n- `WorktreeManager` from W2\n- types from W1",
      outputs: "- `AgentRunner` interface implementation; the supervisor consumes it in Wave 3.",
      acceptance:
        `- [ ] Unit tests use \`MockAgentRunner\` to verify the supervisor side of the integration.
- [ ] One integration test exercises the real SDK with a stubbed prompt against a temp worktree (skip if \`claude\` not on PATH).
- [ ] System-prompt addendum is asserted by snapshot.
- [ ] \`pathToClaudeCodeExecutable\` discovery works on a system with \`claude\` installed at a non-default path.`,
      outOfScope:
        "Hooks, custom MCP tools, structured outputs (deferred until a concrete need lands).",
      references: "Plan §Architecture (AgentRunner).",
    }),
  },
  {
    title: "[W3-supervisor-skeleton] Implement TaskSupervisor end-to-end against in-memory doubles",
    labels: ["wave:3", "type:integration", "agent:blocked"],
    body: buildBody({
      dependsOn: "Every `[W2-*]` issue, plus `[W3-agent-runner]`",
      goal:
        "Drive a task through `INIT → CLONING_WORKTREE → DRAFTING → COMMITTING → PUSHING → PR_OPEN → READY_TO_MERGE → MERGED` end-to-end against `InMemoryGitHubClient` + `MockAgentRunner`. Stub bodies for the three `STABILIZING` sub-phases — Wave 4 fills them in.",
      scope:
        "`src/daemon/supervisor.ts` (state machine + transition logging + persistence on every transition + Copilot reviewer requested on PR open). Wires together every Wave 2 module.",
      files: "- src/daemon/supervisor.ts\n- tests/integration/supervisor_end_to_end_test.ts",
      inputs:
        "- All Wave 2 modules (GitHubClient, WorktreeManager, Persistence, EventBus, AgentRunner, daemon server's command handler interface)\n- Types from W1",
      outputs:
        "- `TaskSupervisor` ready to be wired into the daemon server (replacing the `unimplemented` ack from W2).",
      acceptance:
        `- [ ] Integration test runs \`/issue 1\` end-to-end in-memory with the full supervisor.
- [ ] Every transition appears on the event bus.
- [ ] Persistence replay reconstructs the task identically.
- [ ] Copilot is requested as reviewer immediately on PR open.
- [ ] \`STABILIZING\` sub-phases are stubbed and immediately transition to \`READY_TO_MERGE\` (Wave 4 implements them).`,
      outOfScope: "Real stabilize loop sub-phases (Wave 4); polling (`[W3-poller]`).",
      references: "Plan §Architecture (TaskSupervisor), §Lifecycle.",
    }),
  },
  {
    title: "[W3-poller] Implement Poller with backoff and rate-limit honoring",
    labels: ["wave:3", "type:integration", "agent:blocked"],
    body: buildBody({
      dependsOn: "`[W2-github-client]`",
      goal:
        "One timer per task in any `STABILIZING` sub-phase; backoff respects `Retry-After` and `X-RateLimit-Reset`. Synthetic clock supported via DI for testability.",
      scope:
        "`src/daemon/poller.ts` exposing `poll({ taskId, intervalMs, fetcher, onResult, clock? })`. Cancels cleanly on `AbortSignal`.",
      files: "- src/daemon/poller.ts\n- tests/unit/poller_test.ts (synthetic clock)",
      inputs: "- `GitHubClient` from W2\n- types from W1",
      outputs:
        "- `Poller` ready to drive the stabilize loop's CI and conversations sub-phases (Wave 4).",
      acceptance: `- [ ] Tests with synthetic clock cover steady-state cadence.
- [ ] Backoff honors a 429 response with \`Retry-After\`.
- [ ] Backoff honors \`X-RateLimit-Reset\` when remaining=0.
- [ ] Clean cancellation on \`AbortSignal.abort()\`.`,
      outOfScope: "What gets polled (the stabilize loop in Wave 4 wires the fetchers in).",
      references: "Plan §Architecture (Poller); ADR-006.",
    }),
  },
  {
    title: "[W3-tui-overlays] Implement CommandPalette, TaskSwitcher, slash-command parser",
    labels: ["wave:3", "type:integration", "agent:blocked"],
    body: buildBody({
      dependsOn: "`[W2-tui-shell]`",
      goal:
        "Toggleable command palette + task switcher overlays + slash-command parser. Free-form input forwards to the focused task's session; `/` invokes commands.",
      scope:
        "`src/tui/components/CommandPalette.tsx`, `src/tui/components/TaskSwitcher.tsx`, `src/tui/hooks/useFocusedTask.ts`, slash-command parser with autocomplete and history. Default keybindings `Ctrl+P` / `Ctrl+G`; configurable via `tui.keybindings` in config.",
      files:
        "- src/tui/components/CommandPalette.tsx\n- src/tui/components/TaskSwitcher.tsx\n- src/tui/hooks/useFocusedTask.ts\n- src/tui/slash-command-parser.ts\n- tests/unit/tui_overlays_test.ts\n- tests/unit/slash_command_parser_test.ts",
      inputs:
        "- Components from `[W2-tui-shell]`\n- IPC protocol for forwarding commands to the daemon",
      outputs: "- A TUI that supports every slash command from §Slash commands.",
      acceptance: `- [ ] Snapshot tests for palette open/closed/filtered states.
- [ ] Snapshot tests for switcher with 0, 1, and N tasks.
- [ ] Parser tests cover every command in §Slash commands of the plan.
- [ ] Keybindings honor the config file.`,
      outOfScope: "Lifecycle behavior of any specific command (handled by their wave).",
      references: "Plan §TUI shape, §Slash commands.",
    }),
  },
  {
    title: "[W4-stabilize-rebase] Implement stabilize loop — rebase phase",
    labels: ["wave:4", "type:stabilize", "agent:blocked"],
    body: buildBody({
      dependsOn: "`[W3-supervisor-skeleton]`",
      goal:
        "After each push, fetch + rebase onto base; on conflict, dispatch agent with conflict context. Bounded by `maxIterationsPerTask`. Transition to `NEEDS_HUMAN` on exhaustion.",
      scope:
        "Rebase implementation under `src/daemon/stabilize.ts` (replacing the Wave 3 stub). Conflict context passed to the agent: list of conflicted files, conflict markers in each, latest base-branch SHA.",
      files:
        "- src/daemon/stabilize.ts (rebase phase)\n- tests/unit/stabilize_rebase_test.ts (scripted-timeline against InMemoryGitHubClient)\n- tests/integration/stabilize_rebase_test.ts",
      inputs: "- supervisor + WorktreeManager from prior waves",
      outputs: "- Stabilize-loop rebase phase that the supervisor runs after every push.",
      acceptance:
        `- [ ] Scripted-timeline tests cover clean rebase, conflict-then-resolve, and exhaustion → \`NEEDS_HUMAN\`.
- [ ] Worktree is preserved on \`NEEDS_HUMAN\` so the user can inspect.`,
      outOfScope: "CI and conversations phases (other Wave 4 issues).",
      references: "Plan §Lifecycle.",
    }),
  },
  {
    title: "[W4-stabilize-ci] Implement stabilize loop — CI phase",
    labels: ["wave:4", "type:stabilize", "agent:blocked"],
    body: buildBody({
      dependsOn: "`[W3-supervisor-skeleton]`, `[W3-poller]`",
      goal:
        "Poll commit status + check runs; on red, fetch failing-job logs trimmed to a configurable byte budget, dispatch the agent.",
      scope:
        "CI implementation under `src/daemon/stabilize.ts`. Log fetching trimmed to `lifecycle.ciLogByteBudget` (configurable; default 100 KB). Logs are summarized for the agent prompt.",
      files:
        "- src/daemon/stabilize.ts (ci phase)\n- tests/unit/stabilize_ci_test.ts\n- tests/integration/stabilize_ci_test.ts",
      inputs: "- supervisor + Poller + GitHubClient from prior waves",
      outputs: "- Stabilize-loop CI phase that the supervisor runs after a clean rebase.",
      acceptance:
        `- [ ] Scripted-timeline tests cover green-on-first-poll, red-then-fixed, and perpetual-red → \`NEEDS_HUMAN\`.
- [ ] Log-byte budget enforced; over-budget logs are trimmed at a sensible boundary.`,
      outOfScope: "Rebase and conversations phases.",
      references: "Plan §Lifecycle.",
    }),
  },
  {
    title: "[W4-stabilize-conversations] Implement stabilize loop — conversations phase",
    labels: ["wave:4", "type:stabilize", "agent:blocked"],
    body: buildBody({
      dependsOn: "`[W3-supervisor-skeleton]`, `[W3-poller]`",
      goal:
        "Group new review comments + reviews since `lastReviewAt`; dispatch agent; resolve threads via GraphQL `resolveReviewThread`; re-request Copilot review on every push.",
      scope:
        "Conversations implementation under `src/daemon/stabilize.ts`. Introduces `@octokit/graphql` only if the surface area justifies it; otherwise issues GraphQL through `@octokit/core`. ADR-010 (or update ADR-004) records the decision.",
      files:
        "- src/daemon/stabilize.ts (conversations phase)\n- src/github/client.ts (extend with GraphQL `resolveReviewThread`)\n- tests/unit/stabilize_conversations_test.ts\n- tests/integration/stabilize_conversations_test.ts\n- (maybe) docs/adrs/010-…md",
      inputs: "- supervisor + Poller + GitHubClient from prior waves",
      outputs: "- Stabilize-loop conversations phase that the supervisor runs after a clean CI.",
      acceptance:
        `- [ ] Scripted-timeline tests cover new-comments-then-resolved, no-new-comments → no work, and re-request after every push.
- [ ] Resolved threads are GraphQL-resolved (verified in the InMemoryGitHubClient).
- [ ] If \`@octokit/graphql\` is added, ADR-010 lands in this PR.`,
      outOfScope: "Rebase and CI phases.",
      references: "Plan §Lifecycle.",
    }),
  },
  {
    title: "[W4-merge-modes] Implement all merge modes + cleanup + `/merge` command",
    labels: ["wave:4", "type:stabilize", "agent:blocked"],
    body: buildBody({
      dependsOn: "`[W3-supervisor-skeleton]`",
      goal:
        "All three `mergeMode` branches (`squash`/`rebase`/`manual`); cleanup behavior per `preserveWorktreeOnMerge`; `READY_TO_MERGE` state; `/merge` command for manual mode.",
      scope:
        "Merge implementation under `src/daemon/stabilize.ts`. Slash-command wiring for `/merge` (rejects on non-`READY_TO_MERGE` tasks).",
      files:
        "- src/daemon/stabilize.ts (merge step)\n- src/tui/slash-command-parser.ts (extend with `/merge`)\n- tests/unit/merge_modes_test.ts\n- tests/integration/merge_modes_test.ts",
      inputs: "- supervisor + GitHubClient from prior waves",
      outputs:
        "- The supervisor reaches `MERGED` for tasks configured with auto-merge; sits at `READY_TO_MERGE` for manual mode until `/merge`.",
      acceptance: `- [ ] Tests cover each merge mode against InMemoryGitHubClient.
- [ ] Cleanup verified against a temp-dir worktree.
- [ ] \`/merge\` rejected for non-\`READY_TO_MERGE\` tasks.`,
      outOfScope: "Other stabilize-loop phases.",
      references: "Plan §Lifecycle, §Slash commands.",
    }),
  },
  {
    title: "[W5-end-to-end-suite] End-to-end suite + final docs pass",
    labels: ["wave:5", "type:release", "agent:blocked"],
    body: buildBody({
      dependsOn: "Every `[W4-*]` issue",
      goal: "Real-GitHub end-to-end suite, opt-in via env flag; final pass on README/docs/ADRs.",
      scope:
        "`tests/e2e/` against a sandbox repo with the App installed; documented as a release gate. Final pass on every `docs/*.md` and ADR; remove any wave-noted forward references.",
      files: "- tests/e2e/**/*.ts\n- README.md\n- docs/**/*.md",
      inputs: "- A sandbox repo with the App installed",
      outputs:
        "- A repeatable e2e suite the release pipeline can gate on; user-facing docs that read coherently to a first-time visitor.",
      acceptance:
        `- [ ] e2e suite green against the sandbox repo for happy path + CI-fail recovery + review-comment recovery.
- [ ] No forward references like "Wave N stub" remain in user-facing docs.
- [ ] README quick-start accurately reflects the shipped behavior.`,
      outOfScope: "The actual release tag (that is `[W5-release-v0.1.0]`).",
      references: "Plan §Verification scenarios 3–5.",
    }),
  },
  {
    title: "[W5-release-v0.1.0] Cut the first real release",
    labels: ["wave:5", "type:release", "agent:blocked"],
    body: buildBody({
      dependsOn: "`[W5-end-to-end-suite]`",
      goal:
        "Merge `develop` → `main`; tag `v0.1.0`; observe `release.yml` ship the first real release.",
      scope:
        "Release PR (`develop` → `main`); tag; post-release smoke test on the published darwin-arm64 binary.",
      files: "- (none — this is a release ceremony, not code)",
      inputs: "- Every Wave 4/5 issue closed",
      outputs:
        "- A real GitHub release at https://github.com/koraytaylan/makina/releases/v0.1.0 with notes + 4 binaries + checksums.",
      acceptance: `- [ ] Real GitHub release published with notes + 4 binaries + checksums.
- [ ] \`makina --version\` against the published darwin-arm64 binary returns \`v0.1.0\`.
- [ ] Happy-path verification scenario passes against a real issue on the sandbox repo.
- [ ] The long-lived release-tracking issue (\`chore: release v0.1.0\`) is closed by the workflow.`,
      outOfScope: "Anything beyond v0.1.0.",
      references: "Plan §Conventional Commits + automated releases, §Verification.",
    }),
  },
  {
    title: "chore: release v0.1.0",
    labels: ["wave:5", "type:release", "agent:blocked"],
    body: buildBody({
      dependsOn: "Every `[W4-*]` and `[W5-*]` issue. Stays `agent:blocked` until they all close.",
      goal:
        "Single source of truth for v0.1.0 release status. Auto-closed by `release.yml` posting the release URL on tag push.",
      scope:
        "Tracking only — no code. Comments accumulate as wave issues close; the release workflow appends the release URL on completion.",
      files: "- (none)",
      inputs: "- The Wave 4/5 issues closing.",
      outputs: "- A closed issue with the release URL in a comment.",
      acceptance: `- [ ] Every Wave 4/5 issue is closed before this is closed.
- [ ] Closed automatically by \`release.yml\` with a comment containing the release URL.`,
      outOfScope: "Any actual implementation work.",
      references: "Plan §Issue catalog.",
    }),
  },
];

async function gh(
  args: readonly string[],
  stdin?: string,
): Promise<{ ok: boolean; stdout: string; stderr: string }> {
  const command = new Deno.Command("gh", {
    args: [...args],
    stdin: stdin === undefined ? "null" : "piped",
    stdout: "piped",
    stderr: "piped",
  });
  const child = command.spawn();
  if (stdin !== undefined) {
    const writer = child.stdin.getWriter();
    await writer.write(new TextEncoder().encode(stdin));
    await writer.close();
  }
  const { code, stdout, stderr } = await child.output();
  return {
    ok: code === 0,
    stdout: new TextDecoder().decode(stdout),
    stderr: new TextDecoder().decode(stderr),
  };
}

async function listExistingTitles(): Promise<Set<string>> {
  const result = await gh([
    "issue",
    "list",
    "--repo",
    REPO,
    "--state",
    "all",
    "--limit",
    "200",
    "--json",
    "title",
  ]);
  if (!result.ok) {
    console.error("Failed to list existing issues:", result.stderr);
    Deno.exit(1);
  }
  const parsed = JSON.parse(result.stdout) as Array<{ title: string }>;
  return new Set(parsed.map((entry) => entry.title));
}

async function createIssue(spec: IssueSpec, dryRun: boolean): Promise<void> {
  const args = [
    "issue",
    "create",
    "--repo",
    REPO,
    "--title",
    spec.title,
    "--body-file",
    "-",
    ...spec.labels.flatMap((label) => ["--label", label]),
  ];
  if (dryRun) {
    console.log(`Would create: ${spec.title}  [${spec.labels.join(", ")}]`);
    return;
  }
  const result = await gh(args, spec.body);
  if (!result.ok) {
    console.error(`Failed to create "${spec.title}":`);
    console.error(result.stderr);
    Deno.exit(1);
  }
  console.log(`Created: ${spec.title} → ${result.stdout.trim()}`);
}

const args = parseArgs(Deno.args, { boolean: ["dry-run"] });
const dryRun = args["dry-run"] === true;

const existing = await listExistingTitles();
let createdCount = 0;
let skippedCount = 0;

for (const spec of issues) {
  if (existing.has(spec.title)) {
    console.log(`Skip (already exists): ${spec.title}`);
    skippedCount += 1;
    continue;
  }
  await createIssue(spec, dryRun);
  if (!dryRun) {
    createdCount += 1;
  }
}

console.log("");
console.log(
  `Done. Created: ${createdCount}; skipped: ${skippedCount}; total in catalog: ${issues.length}.`,
);
