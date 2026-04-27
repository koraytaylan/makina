/**
 * daemon/handlers.ts — IPC command handlers for the production daemon.
 *
 * `main.ts daemon` constructs the supervisor, persistence, and worktree
 * collaborators, and then needs a `DaemonHandlers` map to route the
 * `command` and `prompt` IPC envelopes onto the supervisor's surface.
 * Extracting the handler factory into its own module gives that wiring a
 * unit-testable seam: production callers pass real collaborators,
 * integration tests pass in-memory doubles, and the routing logic itself
 * never has to grow a `Deno.*` syscall.
 *
 * **What the routes do today.**
 *
 *  - `command { name: "issue", … }` calls
 *    `supervisor.start({ repo, issueNumber, mergeMode })` (with
 *    `mergeMode` populated when the slash-command parser saw
 *    `--merge=<mode>`) and replies `ack { ok: true }` once the FSM
 *    has accepted the start request. The supervisor — not this
 *    handler — owns task identity and any defaulted run settings.
 *    The drive of the FSM continues in the background: the handler
 *    does **not** await the terminal state because `start()` walks
 *    every phase to a {@link "MERGED"} | {@link "NEEDS_HUMAN"} |
 *    {@link "FAILED"} landing (which can take minutes) and a
 *    synchronous IPC reply cannot block that long. The TUI observes
 *    the lifecycle through the event-bus subscription instead.
 *  - `command { name: "merge", … }` calls
 *    `supervisor.mergeReadyTask(taskId)`. Synchronous failures
 *    (`unknown-task`, `not-ready-to-merge`, `merge-in-flight`,
 *    `merge-precondition`) are reported through `ack { ok: false,
 *    error }`; FSM-internal failures are absorbed by the supervisor
 *    and surface only on the bus.
 *  - `command { name: "status", … }` projects the supervisor's
 *    in-memory task table onto a small JSON payload and replies
 *    `ack { ok: true }`. The TUI keeps its own copy via the bus, so
 *    this command is mostly a CLI affordance (and a smoke-test seam
 *    for the integration tests).
 *  - `command { name: "cancel" | "retry" | "logs" | "switch" |
 *    "repo" | "help" | "quit" | "daemon", … }` reply with `ok: false,
 *    error: "not yet implemented"` so the TUI sees a deterministic
 *    answer rather than a silent timeout. Wire each one up as the
 *    supervisor surface grows.
 *  - `prompt { taskId, text }` replies with `ok: false, error: "..."`
 *    until the supervisor exposes a per-task input channel. Documented
 *    as a follow-up.
 *  - Unknown command name → `ack { ok: false, error: "unknown
 *    command: <name>" }`.
 *
 * **Multi-repo lookup.** Today the supervisor takes a single
 * {@link GitHubClient}. The daemon, however, can speak to many repos
 * (one installation per repo). The handler factory takes a
 * `githubClientFor(repo)` lookup function so the wiring stays correct
 * across repos even though the supervisor itself only sees one client.
 * When the supervisor surface widens to accept a per-task lookup
 * (tracked as a follow-up), this module will pass the function through
 * verbatim instead of capturing a single client.
 *
 * @module
 */

import type { AckPayload, MessageEnvelope } from "../ipc/protocol.ts";
import { makeIssueNumber, makeRepoFullName, type RepoFullName, type Task } from "../types.ts";
import type { DaemonHandlers } from "./server.ts";
import { SupervisorError, type TaskSupervisorImpl } from "./supervisor.ts";

/**
 * Status payload returned by the `status` command. Mirrors the public
 * fields of {@link Task} that the TUI's task switcher already renders.
 *
 * Wave 5 keeps the projection small (state, repo, issue, merge mode,
 * timestamps) so the IPC `ack` body stays under the codec's frame cap;
 * later waves will add scrollback excerpts and per-phase counters when
 * the TUI grows the surface to consume them.
 */
export interface StatusTaskProjection {
  /** Branded task identifier. */
  readonly id: string;
  /** Owner/name pair. */
  readonly repo: string;
  /** Issue number within the repo. */
  readonly issueNumber: number;
  /** Current FSM state. */
  readonly state: string;
  /** Active stabilize phase, when relevant. */
  readonly stabilizePhase?: string;
  /** Merge mode at task start. */
  readonly mergeMode: string;
  /** Anthropic model id used for runs. */
  readonly model: string;
  /** Iteration counter. */
  readonly iterationCount: number;
  /** Pull-request number once known. */
  readonly prNumber?: number;
  /** Per-task feature branch name. */
  readonly branchName?: string;
  /** Absolute filesystem path of the worktree. */
  readonly worktreePath?: string;
  /** Free-form rationale for a terminal state. */
  readonly terminalReason?: string;
  /** ISO-8601 task creation timestamp. */
  readonly createdAtIso: string;
  /** ISO-8601 most-recent-update timestamp. */
  readonly updatedAtIso: string;
}

/**
 * Options accepted by {@link createDaemonHandlers}.
 *
 * Production callers pass the real supervisor and a `githubClientFor`
 * lookup keyed by `RepoFullName`; tests pass in-memory doubles and a
 * function that returns whichever client is appropriate for the test
 * scenario.
 */
export interface DaemonHandlersOptions {
  /**
   * The supervisor that owns the per-issue FSM. The handlers route
   * `issue`, `merge`, and `status` commands directly onto its public
   * methods; other commands are not yet wired and reply with a
   * deterministic `ack { ok: false, error }`.
   */
  readonly supervisor: TaskSupervisorImpl;
  /**
   * Configured default repository to use when an `issue` command
   * arrives without an explicit `--repo` flag. Production wiring takes
   * this from `config.github.defaultRepo`, so handlers can always fall
   * back to a concrete repo rather than minting a task with no repo;
   * the type is therefore required (not optional). Tests pass any
   * branded {@link RepoFullName} the scenario needs.
   */
  readonly defaultRepo: RepoFullName;
  /**
   * Optional per-repo {@link GitHubClient} lookup. The supervisor today
   * holds a single client (the one supplied at construction); this
   * function exists so the wiring layer can plumb the right client per
   * task once the supervisor surface widens. Until then, the lookup is
   * called only to confirm that the repo is reachable — a missing entry
   * makes the `issue` command reply with `ack { ok: false }`.
   *
   * Pass a function whose body returns `undefined` for unknown repos so
   * the handler can give a precise error.
   */
  readonly hasGitHubClientFor?: (repo: RepoFullName) => boolean;
  /**
   * Optional sink for unhandled supervisor rejections raised by the
   * background `start()` walk after the IPC `ack { ok: true }` has
   * already been written. Defaults to a `console.error` line; tests
   * pass a recorder to assert against the rejection without relying on
   * stderr ordering.
   */
  readonly onBackgroundError?: (error: unknown, context: string) => void;
}

/**
 * Construct a {@link DaemonHandlers} map for the production daemon.
 *
 * The factory wires every IPC `command` and `prompt` envelope onto a
 * pure-function dispatcher; the dispatcher in turn calls the
 * supervisor. Returning a freshly-built `DaemonHandlers` (rather than
 * mutating an existing one) keeps the daemon-server module's typing
 * intact — `startDaemon({ ..., handlers })` consumes the map and
 * registers the closures verbatim.
 *
 * @param opts Wiring collaborators. See {@link DaemonHandlersOptions}.
 * @returns A {@link DaemonHandlers} ready to drop into
 *   {@link startDaemon}.
 *
 * @example
 * ```ts
 * import { createDaemonHandlers } from "./handlers.ts";
 * const handlers = createDaemonHandlers({
 *   supervisor,
 *   defaultRepo: makeRepoFullName(config.github.defaultRepo),
 *   hasGitHubClientFor: (repo) => clients.has(repo),
 * });
 * const handle = await startDaemon({ socketPath, eventBus, handlers });
 * ```
 */
export function createDaemonHandlers(
  opts: DaemonHandlersOptions,
): DaemonHandlers {
  const { supervisor, defaultRepo } = opts;
  const hasGitHubClientFor = opts.hasGitHubClientFor ?? (() => true);
  const onBackgroundError = opts.onBackgroundError ?? defaultBackgroundErrorSink;

  const handleCommand = async (
    envelope: Extract<MessageEnvelope, { type: "command" }>,
  ): Promise<AckPayload> => {
    const payload = envelope.payload;
    switch (payload.name) {
      case "issue":
        return await handleIssueCommand({
          payload,
          supervisor,
          defaultRepo,
          hasGitHubClientFor,
          onBackgroundError,
        });
      case "merge":
        return await handleMergeCommand({ payload, supervisor });
      case "status":
        return handleStatusCommand({ supervisor });
      // Commands with a parser route but no supervisor wiring yet. Reply
      // with a deterministic `not yet implemented` so the TUI's command
      // palette renders a tight error rather than a generic timeout.
      case "cancel":
      case "retry":
      case "logs":
      case "switch":
      case "repo":
      case "help":
      case "quit":
      case "daemon":
        return notImplementedAck(payload.name);
      default:
        return {
          ok: false,
          error: `unknown command: ${payload.name}`,
        };
    }
  };

  const handlePrompt = (
    _envelope: Extract<MessageEnvelope, { type: "prompt" }>,
  ): AckPayload => {
    // The supervisor does not expose a per-task input channel yet; the
    // handler returns a deterministic `not yet implemented` so the TUI's
    // per-task input pane shows a tight error instead of a silent
    // timeout. Wire the supervisor side as a follow-up.
    return {
      ok: false,
      error: "prompt forwarding to per-task agent input not yet implemented (follow-up).",
    };
  };

  return {
    command: handleCommand,
    prompt: handlePrompt,
  };
}

/**
 * Dispatcher for `command { name: "issue", ... }`.
 *
 * Resolves the repo (explicit `--repo` wins over the configured
 * default), runs the brand constructors so a malformed payload from a
 * non-TUI client cannot reach the FSM, and fires `supervisor.start()`
 * in the background. The background promise is observed via
 * `onBackgroundError` so a synchronous `start()` rejection is logged
 * (or recorded by a test) instead of becoming an unhandled rejection.
 *
 * @param args Resolved supervisor + payload context.
 * @returns The {@link AckPayload} to write back to the IPC client.
 */
async function handleIssueCommand(args: {
  readonly payload: Extract<MessageEnvelope, { type: "command" }>["payload"];
  readonly supervisor: TaskSupervisorImpl;
  readonly defaultRepo: RepoFullName;
  readonly hasGitHubClientFor: (repo: RepoFullName) => boolean;
  readonly onBackgroundError: (error: unknown, context: string) => void;
}): Promise<AckPayload> {
  const { payload, supervisor, defaultRepo, hasGitHubClientFor, onBackgroundError } = args;

  const issueNumber = payload.issueNumber;
  if (issueNumber === undefined) {
    return await Promise.resolve({
      ok: false,
      error: "/issue requires an issue number.",
    });
  }

  let repo: RepoFullName;
  if (payload.repo !== undefined) {
    repo = payload.repo;
  } else {
    repo = defaultRepo;
  }
  // Re-mint the brand so even a hand-crafted payload that bypassed the
  // TUI's parser gets a final validation pass before the supervisor
  // sees it. The brand cast is a TS-only artifact at runtime, so this
  // is the only way to guarantee the FSM never sees a malformed string.
  let validatedRepo: RepoFullName;
  try {
    validatedRepo = makeRepoFullName(repo);
  } catch (error) {
    return {
      ok: false,
      error: `invalid repo: ${errorMessage(error)}`,
    };
  }

  if (!hasGitHubClientFor(validatedRepo)) {
    // The predicate is also `false` when daemon GitHub auth itself is
    // unavailable (e.g. the App private key failed to load and the
    // wiring fed an empty registry through), so the diagnostic names
    // both possibilities and points operators at the right next step
    // for either case.
    return {
      ok: false,
      error:
        `GitHub access is unavailable for ${validatedRepo}: either no GitHub installation is configured for this repo, or daemon GitHub authentication is unavailable (for example, the app private key failed to load); run \`makina setup\` to register the installation, and verify the daemon's GitHub credentials are configured correctly.`,
    };
  }

  // Same brand-rebuild guard for the issue number; the parser already
  // validated it but a non-TUI client could send any integer.
  let validatedIssueNumber;
  try {
    validatedIssueNumber = makeIssueNumber(issueNumber);
  } catch (error) {
    return {
      ok: false,
      error: `invalid issue number: ${errorMessage(error)}`,
    };
  }

  // Pre-check the duplicate-start guard against the supervisor's
  // in-memory table. The supervisor itself enforces the same invariant
  // (and would throw `SupervisorErrorKind.duplicate-start`), but
  // detecting it here lets us surface the rejection in the IPC `ack`
  // rather than burying it in the background-error sink. Non-terminal
  // tasks for the same `(repo, issueNumber)` pair block; terminal ones
  // do not, matching the supervisor's own contract.
  const existing = supervisor.listTasks().find((task) =>
    task.repo === validatedRepo &&
    task.issueNumber === validatedIssueNumber &&
    !isTerminalTaskState(task.state)
  );
  if (existing !== undefined) {
    return {
      ok: false,
      error: `task already in flight for ${validatedRepo}#${validatedIssueNumber} ` +
        `(taskId=${existing.id}, state=${existing.state})`,
    };
  }

  // Forward the parsed merge mode (set by the slash-command parser when
  // the user typed `/issue <n> --merge=<mode>`) so operator intent is
  // not silently dropped. Omitting the field when undefined keeps the
  // supervisor's own default in charge.
  const startArgs: Parameters<typeof supervisor.start>[0] = payload.mergeMode === undefined
    ? { repo: validatedRepo, issueNumber: validatedIssueNumber }
    : {
      repo: validatedRepo,
      issueNumber: validatedIssueNumber,
      mergeMode: payload.mergeMode,
    };

  // The FSM walk runs in the background. We acknowledge the start
  // immediately and let the event bus stream every transition to the
  // TUI; awaiting here would tie the IPC reply latency to the merge
  // pipeline (minutes), which the codec's frame deadline would not
  // tolerate and which would make the palette feel hung.
  void supervisor
    .start(startArgs)
    .catch((error) => {
      onBackgroundError(error, `supervisor.start ${validatedRepo}#${validatedIssueNumber}`);
    });

  return { ok: true };
}

/**
 * Whether `state` is one of the supervisor's terminal landings. Mirrors
 * the supervisor's own `isTerminal` predicate; duplicated here so the
 * handler module stays decoupled from `supervisor.ts`'s internals.
 */
function isTerminalTaskState(state: Task["state"]): boolean {
  return state === "MERGED" || state === "NEEDS_HUMAN" || state === "FAILED";
}

/**
 * Dispatcher for `command { name: "merge", ... }`.
 *
 * Validates the task-id token and forwards to
 * `supervisor.mergeReadyTask`. The supervisor's
 * {@link SupervisorError} discriminator tells the operator
 * exactly why a `/merge` was rejected (unknown task, not at
 * `READY_TO_MERGE`, in-flight, precondition).
 *
 * @param args Supervisor + payload context.
 * @returns The {@link AckPayload} to write back to the IPC client.
 */
async function handleMergeCommand(args: {
  readonly payload: Extract<MessageEnvelope, { type: "command" }>["payload"];
  readonly supervisor: TaskSupervisorImpl;
}): Promise<AckPayload> {
  const { payload, supervisor } = args;
  const taskIdToken = payload.args[0];
  if (taskIdToken === undefined || taskIdToken.length === 0) {
    return { ok: false, error: "/merge requires <task-id>." };
  }
  // The supervisor accepts a branded `TaskId`; we mint via the
  // `getTask`-driven projection rather than casting so a malformed id
  // becomes a clean `unknown-task` error instead of a runtime crash.
  let foundTaskId;
  try {
    foundTaskId = supervisor.listTasks().find((task) => task.id === taskIdToken)?.id;
  } catch (error) {
    return {
      ok: false,
      error: `failed to read tasks: ${errorMessage(error)}`,
    };
  }
  if (foundTaskId === undefined) {
    return { ok: false, error: `unknown task id: ${taskIdToken}` };
  }

  try {
    await supervisor.mergeReadyTask(foundTaskId);
    return { ok: true };
  } catch (error) {
    if (error instanceof SupervisorError) {
      return { ok: false, error: error.message };
    }
    return { ok: false, error: errorMessage(error) };
  }
}

/**
 * Structured success payload for `/status`. Carried on
 * {@link AckPayload.data} so the IPC contract's `error` field stays
 * reserved for failure descriptions (per
 * {@link AckPayload}'s JSDoc) — embedding JSON in `error` while
 * `ok: true` would let a careless client treat status responses as
 * errors.
 *
 * Exported so client-side code (today: the integration test; tomorrow:
 * the TUI's `/status` renderer) can narrow `ack.data` to a single
 * known shape rather than re-deriving it from the wire.
 */
export interface StatusResponseData {
  /** Snapshot of every task the supervisor knows about. */
  readonly tasks: readonly StatusTaskProjection[];
}

/**
 * Dispatcher for `command { name: "status", ... }`.
 *
 * Projects every supervisor-tracked task onto the small
 * {@link StatusTaskProjection} payload and returns it on the
 * {@link AckPayload.data} field as a structured
 * {@link StatusResponseData} object. The IPC contract reserves
 * `error` for failure descriptions (i.e. `ok: false`), so success
 * payloads ride on `data` instead. The TUI keeps its own copy via
 * the bus; this command is mostly a CLI affordance and a smoke-test
 * seam for the integration tests.
 *
 * @param args Supervisor context.
 * @returns The {@link AckPayload} to write back to the IPC client.
 */
function handleStatusCommand(args: {
  readonly supervisor: TaskSupervisorImpl;
}): AckPayload {
  const tasks = args.supervisor.listTasks();
  const projection = tasks.map(projectTaskForStatus);
  const data: StatusResponseData = { tasks: projection };
  return {
    ok: true,
    data,
  };
}

/**
 * Project a {@link Task} onto the public-facing
 * {@link StatusTaskProjection} shape.
 *
 * Branded fields lose their phantom type marker (the brand is a
 * compile-time artifact); the projection is the on-wire shape consumers
 * read.
 */
function projectTaskForStatus(task: Task): StatusTaskProjection {
  const projection: {
    id: string;
    repo: string;
    issueNumber: number;
    state: string;
    stabilizePhase?: string;
    mergeMode: string;
    model: string;
    iterationCount: number;
    prNumber?: number;
    branchName?: string;
    worktreePath?: string;
    terminalReason?: string;
    createdAtIso: string;
    updatedAtIso: string;
  } = {
    id: task.id as string,
    repo: task.repo as string,
    issueNumber: task.issueNumber as number,
    state: task.state,
    mergeMode: task.mergeMode,
    model: task.model,
    iterationCount: task.iterationCount,
    createdAtIso: task.createdAtIso,
    updatedAtIso: task.updatedAtIso,
  };
  if (task.stabilizePhase !== undefined) {
    projection.stabilizePhase = task.stabilizePhase;
  }
  if (task.prNumber !== undefined) {
    projection.prNumber = task.prNumber as number;
  }
  if (task.branchName !== undefined) {
    projection.branchName = task.branchName;
  }
  if (task.worktreePath !== undefined) {
    projection.worktreePath = task.worktreePath;
  }
  if (task.terminalReason !== undefined) {
    projection.terminalReason = task.terminalReason;
  }
  return projection;
}

/**
 * Build the deterministic "not yet implemented" reply for commands
 * the parser knows but the daemon has not wired through to the
 * supervisor yet. Centralised so the message stays stable across
 * commands and tests can match against a single string.
 */
function notImplementedAck(name: string): AckPayload {
  return {
    ok: false,
    error: `/${name}: not yet implemented`,
  };
}

/**
 * Render an arbitrary error value to a single-line diagnostic.
 *
 * Mirrors the helper in `main.ts` but is duplicated here so the
 * handler module stays free of cross-file imports beyond the supervisor
 * surface and the IPC contract.
 */
function errorMessage(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

/**
 * Default sink for background-task rejections.
 *
 * Writes a short stderr line consistent with the daemon's other
 * `[daemon] ...` messages so an operator running `journalctl -u makina`
 * sees a coherent log stream. Tests inject a recorder.
 */
function defaultBackgroundErrorSink(error: unknown, context: string): void {
  console.error(`[daemon] ${context}: ${errorMessage(error)}`);
}
