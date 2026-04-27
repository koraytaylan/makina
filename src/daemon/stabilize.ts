/**
 * daemon/stabilize.ts — stabilize-loop sub-phase implementations.
 *
 * The supervisor (Wave 3) walks every task through `STABILIZING` after
 * the PR opens; the three sub-phases (`REBASE`, `CI`, `CONVERSATIONS`)
 * are the work the daemon does between "PR is open" and "ready to
 * merge". Each sub-phase is a standalone module so the per-phase logic
 * can be reasoned about, tested, and replaced independently of the
 * supervisor's state machine.
 *
 * **Wave 4 #15 ships the rebase phase.** The `CI` and `CONVERSATIONS`
 * phases are still stubs inside {@link createTaskSupervisor};
 * {@link runRebasePhase} is the single piece this module exports.
 *
 * **Rebase semantics.** After every push the supervisor must reconcile
 * the per-task feature branch with `<baseBranch>` so the PR stays
 * mergeable while sibling tasks are landing. The phase:
 *
 *  1. `git fetch origin <baseBranch>` inside the per-task worktree —
 *     refresh the remote's view of the base branch. Failures here are
 *     surfaced as {@link StabilizeRebaseError} for the supervisor to
 *     translate into a `FAILED` transition; they are not retryable.
 *  2. `git rebase origin/<baseBranch>` against the same worktree.
 *     Three observable outcomes:
 *
 *       - **Clean rebase** — exit 0; the phase resolves with kind
 *         `"clean"` and the supervisor advances to `CI`.
 *       - **Conflict** — exit non-zero with conflict markers in the
 *         work tree. The phase dispatches the agent runner with a
 *         deterministic conflict-context prompt and retries `git add
 *         -A && git rebase --continue` on the resulting state. Bounded
 *         by {@link MAX_TASK_ITERATIONS}; on exhaustion the phase
 *         resolves with kind `"needs-human"` and a list of unresolved
 *         conflicting files. The worktree is preserved.
 *       - **Fatal git error** (auth, refspec missing, etc.) — surfaced
 *         as {@link StabilizeRebaseError}.
 *
 * **Determinism for tests.** Every collaborator that reaches outside
 * the module is injected: the git invoker, the agent runner, and the
 * filesystem reader for conflict-marker extraction. Tests pass an
 * in-memory `gitInvoker` and the {@link MockAgentRunner} in
 * `tests/helpers/`; production code passes a `Deno.Command`-backed
 * default and the real {@link AgentRunner}. No `Deno.Command`,
 * `Deno.readTextFile`, or `Deno.env.get` is invoked outside the
 * default-factory functions at the bottom of this file (Lesson #3 from
 * the Wave 3 brief).
 *
 * **Cross-platform paths.** The default git invoker spawns the same
 * `git` binary on macOS and Linux; Windows is deferred per ADR-008. We
 * still branch on `Deno.build.os` when normalising the conflict-file
 * paths returned by `git diff --name-only --diff-filter=U` so the
 * supervisor's logging is consistent across hosts.
 *
 * **Branded ids.** The phase receives the {@link TaskId} and
 * {@link IssueNumber} as branded values from the supervisor; it never
 * mints them itself.
 *
 * See {@link https://github.com/koraytaylan/makina/blob/develop/docs/adrs/018-stabilize-rebase-conflict-loop.md ADR-018}
 * for the conflict-resolution contract and the bounded-iteration
 * rationale.
 *
 * @module
 */

import { getLogger } from "@std/log";
import { isAbsolute, join, SEPARATOR } from "@std/path";

import {
  MAX_TASK_ITERATIONS,
  STABILIZE_REBASE_CONFLICT_FILE_PREVIEW_CHARS,
  STABILIZE_REBASE_CONFLICT_PROMPT_HEAD,
  STABILIZE_REBASE_GIT_NULL_SUCCESS_EXIT_CODE,
} from "../constants.ts";
import type { AgentRunner, IssueNumber, TaskId } from "../types.ts";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/**
 * Result of one git invocation, as observed by {@link StabilizeGitInvoker}.
 *
 * The shape is intentionally narrow: {@link runRebasePhase} only branches
 * on `exitCode` and consumes the captured `stdout`/`stderr` for prompt
 * context and {@link StabilizeRebaseError} messages. Implementations
 * (production, tests) match this surface verbatim — no extra fields are
 * defined or read.
 */
export interface GitInvocationResult {
  /** Process exit code; `0` is success, non-zero is failure. */
  readonly exitCode: number;
  /** Decoded stdout text. */
  readonly stdout: string;
  /** Decoded stderr text. */
  readonly stderr: string;
}

/**
 * Spawns `git <args>` against a worktree and returns the captured output.
 *
 * The default implementation backs onto `Deno.Command`; tests pass a
 * scripted double so the rebase loop is fully deterministic without
 * touching real git. The supervisor injects the production binding
 * through {@link StabilizeRebaseOptions.gitInvoker}.
 *
 * @example
 * ```ts
 * const invoker: StabilizeGitInvoker = async (args, opts) => {
 *   if (args[0] === "fetch") return { exitCode: 0, stdout: "", stderr: "" };
 *   throw new Error(`unscripted git ${args.join(" ")}`);
 * };
 * ```
 */
export type StabilizeGitInvoker = (
  args: readonly string[],
  options: { readonly cwd: string },
) => Promise<GitInvocationResult>;

/**
 * Reads the contents of a conflicted file in the worktree as UTF-8 text.
 *
 * The reader receives the file's absolute path. Wave 4's rebase phase
 * uses this to extract conflict markers from conflicted files for the
 * agent prompt. The default implementation backs onto
 * `Deno.readTextFile`; tests pass a scripted reader so the
 * conflict-context branch can be exercised without touching the
 * filesystem.
 *
 * @example
 * ```ts
 * const reader: ConflictFileReader = async (absolutePath) => {
 *   if (absolutePath.endsWith("README.md")) {
 *     return "<<<<<<< HEAD\nfoo\n=======\nbar\n>>>>>>> origin/main\n";
 *   }
 *   throw new Deno.errors.NotFound(`unscripted read: ${absolutePath}`);
 * };
 * ```
 */
export type ConflictFileReader = (absolutePath: string) => Promise<string>;

/**
 * Logger surface used by {@link runRebasePhase}.
 *
 * The narrower interface lets tests inject a recording double without
 * matching `@std/log`'s full overload signature. The runtime `Logger`
 * class satisfies this shape (its `info`/`warn` accept plain strings).
 */
export interface StabilizeLogger {
  /** Emit an informational line. */
  info(message: string): void;
  /** Emit a warning. */
  warn(message: string): void;
}

/**
 * Construction-time wiring for {@link runRebasePhase}.
 *
 * Every collaborator with side effects is injected so the supervisor's
 * production bindings and the unit-test in-memory bindings share the
 * same code path. Tests pass scripted invokers and an
 * in-memory {@link AgentRunner}; production passes the daemon's real
 * agent runner and a `Deno.Command`-backed git invoker.
 */
export interface StabilizeRebaseOptions {
  /** The task's branded id; passed through to agent invocations. */
  readonly taskId: TaskId;
  /**
   * The PR's issue number. Embedded into the agent prompt so the
   * conflict-context message is human-readable in logs.
   */
  readonly issueNumber: IssueNumber;
  /**
   * Absolute filesystem path of the per-task worktree. Used as `cwd`
   * for every git invocation and as the root for conflict-file reads.
   */
  readonly worktreePath: string;
  /**
   * Base branch the supervisor is rebasing onto, e.g. `"main"` or
   * `"develop"`. Caller-supplied because Wave 4 still hard-codes the
   * default at the supervisor layer; see
   * {@link DEFAULT_BASE_BRANCH} in `supervisor.ts`.
   */
  readonly baseBranch: string;
  /**
   * SDK-assigned session id to resume agent runs from, when the
   * supervisor has one persisted on the task record.
   */
  readonly sessionId?: string;
  /**
   * Anthropic model id for agent runs. Forwarded verbatim to
   * {@link AgentRunner.runAgent}.
   */
  readonly model: string;
  /** Agent runner used to resolve conflicts. */
  readonly agentRunner: AgentRunner;
  /**
   * Maximum number of agent-resolve iterations before the phase
   * surrenders to `NEEDS_HUMAN`. Defaults to {@link MAX_TASK_ITERATIONS}.
   * Tests override this to exercise exhaustion without queueing eight
   * agent runs.
   */
  readonly maxIterations?: number;
  /**
   * git invoker used for every command the phase executes. Defaults to
   * the production {@link defaultGitInvoker} which backs onto
   * `Deno.Command("git", …)`.
   */
  readonly gitInvoker?: StabilizeGitInvoker;
  /**
   * Conflict-file reader used to extract conflict markers for the
   * agent prompt. Defaults to {@link defaultConflictFileReader}, which
   * backs onto `Deno.readTextFile`.
   */
  readonly conflictFileReader?: ConflictFileReader;
  /**
   * Logger used for non-fatal warnings (an agent run dispatched, the
   * conflict-marker reader failing on one file). Defaults to
   * `getLogger()` from `@std/log`, adapted to the
   * {@link StabilizeLogger} surface.
   */
  readonly logger?: StabilizeLogger;
}

/**
 * Outcome of one {@link runRebasePhase} call.
 *
 * The phase intentionally exposes a small discriminant union rather than
 * a free-form record so the supervisor's switch statement reads
 * exhaustively. `clean` advances to `CI`; `needs-human` lands the task
 * in `NEEDS_HUMAN` with the conflicting files preserved on disk.
 */
export type RebasePhaseResult =
  | {
    /** The rebase completed without conflicts. */
    readonly kind: "clean";
    /** Number of agent iterations spent. Always `0` on a clean rebase. */
    readonly iterations: number;
    /**
     * Latest SDK-assigned session id observed across the agent runs
     * dispatched by this phase, if any. `undefined` on a zero-iteration
     * clean rebase (no agent ran), or when the runner never emitted a
     * session id. The supervisor persists this back onto the task so
     * subsequent stabilize phases can resume the same session.
     */
    readonly sessionId?: string;
  }
  | {
    /**
     * The rebase exhausted the iteration budget without resolving
     * every conflict. The supervisor should land the task in
     * `NEEDS_HUMAN` and surface the file list to the operator.
     */
    readonly kind: "needs-human";
    /** Worktree-relative paths of files still carrying conflict markers. */
    readonly conflictingFiles: readonly string[];
    /** Number of agent iterations spent. */
    readonly iterations: number;
    /**
     * Latest SDK-assigned session id observed across the agent runs
     * dispatched by this phase, if any. The supervisor persists this
     * back onto the task even on `needs-human` so an operator-driven
     * resume can pick up where the budget exhausted.
     */
    readonly sessionId?: string;
  };

/**
 * Closed taxonomy of operation tags surfaced on
 * {@link StabilizeRebaseError.operation}.
 *
 * Operators, tests, and the supervisor (which embeds the tag in the
 * task's `terminalReason` as `stabilize-rebase-<operation>`) all rely
 * on these literal values; widening the union would silently accept
 * typos. The supervisor additionally emits `"precondition"` from
 * `runRebaseSubPhase` when entering `STABILIZING(REBASE)` without a
 * worktree path, so it is included here for completeness even though
 * it never originates inside this module.
 *
 *  - `"fetch"` — `git fetch origin <refspec>` failed.
 *  - `"rebase-start"` — initial `git rebase <ref>` exited non-zero
 *    without unmerged files (a non-conflict failure).
 *  - `"add"` — `git add -A` after the agent settled failed.
 *  - `"rebase-continue"` — `git rebase --continue` exited non-zero
 *    without unmerged files.
 *  - `"diff-conflicts"` — `git diff --name-only --diff-filter=U`
 *    failed.
 *  - `"agent-resolve"` — the agent runner threw while resolving a
 *    conflict iteration.
 *  - `"validate"` — the user-supplied options bag is malformed.
 *  - `"precondition"` — supervisor-side guard (the rebase sub-phase
 *    was entered without `task.worktreePath`).
 */
export type StabilizeRebaseOperation =
  | "fetch"
  | "rebase-start"
  | "add"
  | "rebase-continue"
  | "diff-conflicts"
  | "agent-resolve"
  | "validate"
  | "precondition";

/**
 * Domain error raised when the rebase phase cannot make progress for
 * reasons other than conflict-resolution exhaustion.
 *
 * Examples: `git fetch` rejects the refspec; `git rebase --continue`
 * exits with a non-conflict failure; the default git invoker fails to
 * spawn the binary. Wraps the underlying cause via the standard
 * `Error.cause` chain so log readers can recover the original
 * stack/exit-code/stderr.
 *
 * The supervisor catches this and lands the task in `FAILED` with
 * `terminalReason` derived from the message.
 *
 * @example
 * ```ts
 * try {
 *   await runRebasePhase(opts);
 * } catch (error) {
 *   if (error instanceof StabilizeRebaseError) {
 *     supervisor.fail(task, error, error.operation);
 *   } else {
 *     throw error;
 *   }
 * }
 * ```
 */
export class StabilizeRebaseError extends Error {
  /** Discriminator visible in stack traces and `error.name === ...` checks. */
  override readonly name = "StabilizeRebaseError";
  /**
   * Closed-taxonomy label of the operation that failed; see
   * {@link StabilizeRebaseOperation} for the full list. Surfaced
   * verbatim by the supervisor in the task's `terminalReason` as
   * `stabilize-rebase-<operation>`.
   */
  public readonly operation: StabilizeRebaseOperation;

  /**
   * Build a stabilize-rebase error.
   *
   * @param message Human-readable description.
   * @param operation Operation tag from {@link StabilizeRebaseOperation}.
   * @param options Optional standard `cause` carrying the underlying
   *   exception (a `Deno` error, an `AgentRunnerError`, etc.).
   */
  constructor(
    message: string,
    operation: StabilizeRebaseOperation,
    options?: { readonly cause?: unknown },
  ) {
    super(message, options);
    this.operation = operation;
  }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/**
 * Run the rebase sub-phase of the stabilize loop.
 *
 * Walks the per-task worktree through `git fetch origin <baseBranch>`
 * and `git rebase origin/<baseBranch>`. On conflicts, dispatches the
 * agent runner with a deterministic conflict-context prompt
 * ({@link buildConflictPrompt}) up to {@link MAX_TASK_ITERATIONS} times.
 * Each iteration runs `git add -A && git rebase --continue` after the
 * agent settles. On exhaustion, attempts `git rebase --abort` so a
 * follow-up `git status` is sane, and resolves with
 * `kind: "needs-human"` plus the unresolved conflicting files. The
 * worktree is **always** preserved.
 *
 * @param opts Configuration. See {@link StabilizeRebaseOptions}.
 * @returns Resolves with a {@link RebasePhaseResult} describing the
 *   outcome.
 * @throws StabilizeRebaseError when a non-conflict git operation fails
 *   (auth, missing refspec, malformed worktree). The supervisor catches
 *   this and transitions the task to `FAILED` with the operation tag in
 *   `terminalReason`.
 *
 * @example
 * ```ts
 * import { runRebasePhase } from "./stabilize.ts";
 *
 * const result = await runRebasePhase({
 *   taskId, issueNumber, worktreePath, baseBranch: "main",
 *   model, agentRunner,
 * });
 * if (result.kind === "needs-human") {
 *   supervisor.transitionToNeedsHuman(task, result.conflictingFiles);
 * }
 * ```
 */
export async function runRebasePhase(
  opts: StabilizeRebaseOptions,
): Promise<RebasePhaseResult> {
  validateOptions(opts);
  const gitInvoker = opts.gitInvoker ?? defaultGitInvoker;
  const conflictFileReader = opts.conflictFileReader ?? defaultConflictFileReader;
  const logger = opts.logger ?? defaultLogger();
  const maxIterations = opts.maxIterations ?? MAX_TASK_ITERATIONS;
  const cwd = opts.worktreePath;
  // We rebase against an explicit `refs/remotes/origin/<baseBranch>`
  // ref. The fetch invocation below uses an explicit refspec so the
  // ref is populated even when the worktree's gitdir is a bare clone
  // configured without `remote.origin.fetch` (which is how the W3
  // {@link WorktreeManagerImpl} sets up the per-repo bare clones; see
  // `daemon/worktree-manager.ts`). Without the explicit refspec, the
  // bare clone's `git fetch origin main` updates `FETCH_HEAD` but does
  // not write `refs/remotes/origin/main`, and `git rebase
  // origin/main` then fails with "fatal: invalid upstream
  // 'origin/main'".
  const remoteTrackingRef = `refs/remotes/origin/${opts.baseBranch}`;
  const fetchRefspec = `${opts.baseBranch}:${remoteTrackingRef}`;

  // Step 1: refresh the remote's view of the base branch. A failure
  // here cannot be papered over by the agent, so we surface it as a
  // domain error the supervisor lands in FAILED.
  const fetchResult = await invokeGit(
    gitInvoker,
    ["fetch", "origin", fetchRefspec],
    cwd,
    "fetch",
  );
  if (fetchResult.exitCode !== STABILIZE_REBASE_GIT_NULL_SUCCESS_EXIT_CODE) {
    throw new StabilizeRebaseError(
      `git fetch origin ${fetchRefspec} failed (exit ${fetchResult.exitCode}): ${fetchResult.stderr.trim()}`,
      "fetch",
    );
  }

  // Step 2: try the rebase. A clean rebase exits 0 and the phase is
  // done. A non-zero exit puts us into the conflict-resolution loop.
  const rebaseStart = await invokeGit(
    gitInvoker,
    ["rebase", remoteTrackingRef],
    cwd,
    "rebase-start",
  );
  if (rebaseStart.exitCode === STABILIZE_REBASE_GIT_NULL_SUCCESS_EXIT_CODE) {
    return { kind: "clean", iterations: 0 };
  }

  // Step 3: conflict-resolution loop. Each iteration:
  //   a) collect the conflicted files via `git diff --name-only --diff-filter=U`
  //   b) build the conflict-context prompt and run the agent
  //   c) `git add -A` to stage whatever the agent edited
  //   d) `git rebase --continue` and inspect the exit code.
  //
  // We carry a `currentSessionId` so the agent's session resumes across
  // iterations, mirroring the supervisor's drafting loop.
  let currentSessionId = opts.sessionId;
  const startingSessionId = opts.sessionId;
  let iterations = 0;
  while (iterations < maxIterations) {
    iterations += 1;
    const conflictingFiles = await listConflictingFiles(gitInvoker, cwd);
    if (conflictingFiles.length === 0) {
      // The rebase exited non-zero but git reports no unmerged files —
      // typical when the rebase aborted earlier in the loop and the
      // worktree is clean. Treat as a fatal failure rather than spin on
      // an empty conflict set.
      throw new StabilizeRebaseError(
        `git rebase exited ${rebaseStart.exitCode} but no conflicting files reported: ${rebaseStart.stderr.trim()}`,
        "rebase-start",
      );
    }
    const prompt = await buildConflictPrompt({
      issueNumber: opts.issueNumber,
      baseBranch: opts.baseBranch,
      conflictingFiles,
      worktreePath: cwd,
      reader: conflictFileReader,
      logger,
    });
    logger.info(
      `stabilize-rebase: dispatching agent for ${conflictingFiles.length} conflicting file(s) ` +
        `on task ${opts.taskId} (iteration ${iterations}/${maxIterations})`,
    );
    currentSessionId = await runAgentForConflict({
      agentRunner: opts.agentRunner,
      taskId: opts.taskId,
      worktreePath: cwd,
      prompt,
      model: opts.model,
      sessionId: currentSessionId,
    });

    // Stage everything the agent edited. `git add -A` is non-failing
    // unless the worktree is missing entirely, which would have failed
    // the fetch above.
    const addResult = await invokeGit(gitInvoker, ["add", "-A"], cwd, "add");
    if (addResult.exitCode !== STABILIZE_REBASE_GIT_NULL_SUCCESS_EXIT_CODE) {
      throw new StabilizeRebaseError(
        `git add -A failed (exit ${addResult.exitCode}): ${addResult.stderr.trim()}`,
        "add",
      );
    }

    // Continue the rebase. Three observable outcomes:
    //   - exit 0: clean — the phase is done.
    //   - exit non-zero with unmerged files: keep iterating.
    //   - exit non-zero with no unmerged files: a non-conflict
    //     git-rebase failure. Surface as fatal — the agent cannot
    //     recover from it.
    const continueResult = await invokeGit(
      gitInvoker,
      ["rebase", "--continue"],
      cwd,
      "rebase-continue",
    );
    if (continueResult.exitCode === STABILIZE_REBASE_GIT_NULL_SUCCESS_EXIT_CODE) {
      return cleanResult(iterations, currentSessionId, startingSessionId);
    }
    const stillConflicting = await listConflictingFiles(gitInvoker, cwd);
    if (stillConflicting.length === 0) {
      throw new StabilizeRebaseError(
        `git rebase --continue exited ${continueResult.exitCode} ` +
          `with no unmerged files: ${continueResult.stderr.trim()}`,
        "rebase-continue",
      );
    }
    // Loop again with the next iteration; the conflict set may have
    // shrunk or rotated to a different file list.
  }

  // Iteration budget exhausted. Capture the final conflict set, then
  // try to abort the rebase so an operator inspecting the worktree
  // sees a stable state. An abort failure is logged but does not mask
  // the NEEDS_HUMAN signal — the worktree is preserved either way.
  const finalConflicts = await listConflictingFiles(gitInvoker, cwd);
  // `--abort` is best-effort cleanup: a thrown invoker (binary missing,
  // permission denied) must not mask the NEEDS_HUMAN signal we already
  // have. Log and continue.
  let abortResult: GitInvocationResult | undefined;
  try {
    abortResult = await gitInvoker(["rebase", "--abort"], { cwd });
  } catch (caught) {
    logger.warn(
      `stabilize-rebase: git rebase --abort threw for task ${opts.taskId}: ${
        stringifyError(caught)
      }`,
    );
  }
  if (
    abortResult !== undefined &&
    abortResult.exitCode !== STABILIZE_REBASE_GIT_NULL_SUCCESS_EXIT_CODE
  ) {
    logger.warn(
      `stabilize-rebase: git rebase --abort failed for task ${opts.taskId} ` +
        `(exit ${abortResult.exitCode}): ${abortResult.stderr.trim()}`,
    );
  }
  return needsHumanResult(
    finalConflicts,
    iterations,
    currentSessionId,
    startingSessionId,
  );
}

/**
 * Build a `kind: "clean"` result, omitting `sessionId` when the agent
 * runs never produced a new session id (so the supervisor doesn't write
 * `task.sessionId` redundantly).
 */
function cleanResult(
  iterations: number,
  observedSessionId: string | undefined,
  startingSessionId: string | undefined,
): RebasePhaseResult {
  return observedSessionId !== undefined && observedSessionId !== startingSessionId
    ? { kind: "clean", iterations, sessionId: observedSessionId }
    : { kind: "clean", iterations };
}

/**
 * Build a `kind: "needs-human"` result, omitting `sessionId` when no
 * new session id was observed across the iteration budget.
 */
function needsHumanResult(
  conflictingFiles: readonly string[],
  iterations: number,
  observedSessionId: string | undefined,
  startingSessionId: string | undefined,
): RebasePhaseResult {
  return observedSessionId !== undefined && observedSessionId !== startingSessionId
    ? {
      kind: "needs-human",
      conflictingFiles,
      iterations,
      sessionId: observedSessionId,
    }
    : { kind: "needs-human", conflictingFiles, iterations };
}

/**
 * Spawn `git <args>` against `cwd`, wrapping any thrown error from the
 * invoker (binary missing, sandbox-permission deny, EPIPE on a closed
 * stdio handle, …) as a {@link StabilizeRebaseError} tagged with
 * `operation`. Non-zero exit codes are *not* treated as throws here;
 * the caller is expected to inspect the {@link GitInvocationResult} so
 * the conflict-aware branches in the rebase loop can act on
 * `exitCode !== 0` without losing access to stderr.
 */
async function invokeGit(
  gitInvoker: StabilizeGitInvoker,
  args: readonly string[],
  cwd: string,
  operation: StabilizeRebaseOperation,
): Promise<GitInvocationResult> {
  try {
    return await gitInvoker(args, { cwd });
  } catch (caught) {
    throw new StabilizeRebaseError(
      `git ${args.join(" ")} threw before producing an exit code: ${stringifyError(caught)}`,
      operation,
      { cause: caught },
    );
  }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/**
 * Fetch the list of files with unresolved merge conflicts via
 * `git diff --name-only --diff-filter=U`. The diff filter `U` selects
 * "unmerged" entries (files with conflict markers post-rebase).
 *
 * The output is one path per line; we trim trailing whitespace and
 * filter empty lines so a stray newline at end-of-output does not
 * produce a phantom entry. Returned paths are left exactly as git
 * prints them, which means forward slashes may appear even on Windows.
 * Path-separator normalisation, when needed for filesystem reads, is
 * performed at the call site (see {@link joinWorktreePath}).
 */
async function listConflictingFiles(
  gitInvoker: StabilizeGitInvoker,
  cwd: string,
): Promise<readonly string[]> {
  const result = await invokeGit(
    gitInvoker,
    ["diff", "--name-only", "--diff-filter=U"],
    cwd,
    "diff-conflicts",
  );
  if (result.exitCode !== STABILIZE_REBASE_GIT_NULL_SUCCESS_EXIT_CODE) {
    throw new StabilizeRebaseError(
      `git diff --name-only --diff-filter=U failed (exit ${result.exitCode}): ${result.stderr.trim()}`,
      "diff-conflicts",
    );
  }
  const lines = result.stdout.split("\n").map((line) => line.trim());
  return lines.filter((line) => line.length > 0);
}

/**
 * Build the conflict-context prompt for the agent.
 *
 * The prompt embeds the issue number, the base branch, and the head of
 * each conflicting file (capped at
 * {@link STABILIZE_REBASE_CONFLICT_FILE_PREVIEW_CHARS} per file). The
 * agent is told it is mid-rebase, must resolve the conflict markers,
 * and must not run any git commands itself — the daemon will continue
 * the rebase once the agent settles.
 *
 * If reading a particular file fails (e.g. it has been deleted by the
 * rebase), we log a warning and surface the failure inline in the
 * prompt rather than aborting the whole phase. The agent is expected
 * to handle a missing file by deciding whether to `rm` or recreate it.
 */
async function buildConflictPrompt(args: {
  readonly issueNumber: IssueNumber;
  readonly baseBranch: string;
  readonly conflictingFiles: readonly string[];
  readonly worktreePath: string;
  readonly reader: ConflictFileReader;
  readonly logger: StabilizeLogger;
}): Promise<string> {
  const { issueNumber, baseBranch, conflictingFiles, worktreePath, reader, logger } = args;
  const sections: string[] = [];
  sections.push(STABILIZE_REBASE_CONFLICT_PROMPT_HEAD);
  sections.push("");
  sections.push(`Issue: #${issueNumber}`);
  sections.push(`Base branch: ${baseBranch}`);
  sections.push(`Conflicting files (${conflictingFiles.length}):`);
  for (const path of conflictingFiles) {
    sections.push(`  - ${path}`);
  }
  sections.push("");
  for (const path of conflictingFiles) {
    sections.push(`### ${path}`);
    sections.push("");
    try {
      const content = await reader(joinWorktreePath(worktreePath, path));
      const trimmed = content.length <= STABILIZE_REBASE_CONFLICT_FILE_PREVIEW_CHARS
        ? content
        : `${content.slice(0, STABILIZE_REBASE_CONFLICT_FILE_PREVIEW_CHARS)}\n...[truncated]`;
      sections.push("```");
      sections.push(trimmed);
      sections.push("```");
    } catch (caught) {
      logger.warn(
        `stabilize-rebase: could not read conflicted file ${path}: ${stringifyError(caught)}`,
      );
      sections.push(`(could not read file: ${stringifyError(caught)})`);
    }
    sections.push("");
  }
  return sections.join("\n");
}

/**
 * Drive one agent run for a conflict iteration and return the SDK
 * session id observed across the run (or the previous session id if
 * the SDK never emitted one).
 *
 * The runner contract yields {@link AgentRunner} messages but does not
 * expose the SDK session id directly; we rely on the structural
 * `(message as { sessionId?: string }).sessionId` projection used by
 * the supervisor's drafting loop. When the test double yields plain
 * messages without a `sessionId` field, the previous session id flows
 * through unchanged.
 */
async function runAgentForConflict(args: {
  readonly agentRunner: AgentRunner;
  readonly taskId: TaskId;
  readonly worktreePath: string;
  readonly prompt: string;
  readonly model: string;
  readonly sessionId: string | undefined;
}): Promise<string | undefined> {
  let observedSessionId = args.sessionId;
  const runArgs = args.sessionId === undefined
    ? {
      taskId: args.taskId,
      worktreePath: args.worktreePath,
      prompt: args.prompt,
      model: args.model,
    }
    : {
      taskId: args.taskId,
      worktreePath: args.worktreePath,
      prompt: args.prompt,
      model: args.model,
      sessionId: args.sessionId,
    };
  try {
    for await (const message of args.agentRunner.runAgent(runArgs)) {
      const candidate = (message as { sessionId?: string }).sessionId;
      if (typeof candidate === "string" && candidate.length > 0) {
        observedSessionId = candidate;
      }
    }
  } catch (caught) {
    throw new StabilizeRebaseError(
      `agent dispatch for conflict resolution failed: ${stringifyError(caught)}`,
      "agent-resolve",
      { cause: caught },
    );
  }
  return observedSessionId;
}

/**
 * Validate the user-supplied options bag at entry. Surface domain
 * errors with `operation: "validate"` so failures are distinguishable
 * from the git/agent failures further down the path.
 */
function validateOptions(opts: StabilizeRebaseOptions): void {
  if (opts.worktreePath.length === 0) {
    throw new StabilizeRebaseError(
      "StabilizeRebaseOptions.worktreePath must be a non-empty path",
      "validate",
    );
  }
  if (!isAbsolute(opts.worktreePath)) {
    throw new StabilizeRebaseError(
      `StabilizeRebaseOptions.worktreePath must be absolute (got ${
        JSON.stringify(opts.worktreePath)
      })`,
      "validate",
    );
  }
  if (opts.baseBranch.length === 0) {
    throw new StabilizeRebaseError(
      "StabilizeRebaseOptions.baseBranch must be a non-empty string",
      "validate",
    );
  }
  if (opts.model.length === 0) {
    throw new StabilizeRebaseError(
      "StabilizeRebaseOptions.model must be a non-empty string",
      "validate",
    );
  }
  if (opts.maxIterations !== undefined) {
    if (!Number.isInteger(opts.maxIterations) || opts.maxIterations < 1) {
      throw new StabilizeRebaseError(
        `StabilizeRebaseOptions.maxIterations must be a positive integer; got ${opts.maxIterations}`,
        "validate",
      );
    }
  }
}

/**
 * Join `worktreePath` with `relative` using the host separator. `git`
 * always emits forward slashes regardless of host; on Windows we have
 * to translate them so {@link Deno.readTextFile} resolves the path. On
 * macOS/Linux the join is a noop because `SEPARATOR === "/"` already.
 *
 * The branch on `Deno.build.os` (Lesson #4) is intentionally
 * conservative — Wave 1 deferred Windows support per ADR-008, but the
 * branch makes the rebase phase forward-compatible without relying on
 * `path.fromFileUrl` or other higher-level abstractions that would
 * pull a heavier dependency footprint.
 */
function joinWorktreePath(worktreePath: string, relative: string): string {
  if (Deno.build.os === "windows") {
    const normalised = relative.replace(/\//g, SEPARATOR);
    return join(worktreePath, normalised);
  }
  return join(worktreePath, relative);
}

/** Stringify an unknown caught value for inclusion in error messages. */
function stringifyError(caught: unknown): string {
  if (caught instanceof Error) {
    return `${caught.name}: ${caught.message}`;
  }
  return String(caught);
}

// ---------------------------------------------------------------------------
// Default factories
// ---------------------------------------------------------------------------

/**
 * Default git invoker: spawns `git <args>` via `Deno.Command` with the
 * supplied `cwd` and captures stdout/stderr.
 *
 * Pulled out as a separate factory so the rebase phase's main body
 * holds no `Deno.Command` reference — that lives entirely in this
 * function and is bypassed in every test that injects a `gitInvoker`.
 *
 * @param args Argv to pass to `git`. The literal `"git"` is supplied
 *   by this function and must not appear in the array.
 * @param options Per-invocation overrides. `cwd` is the worktree path.
 * @returns The captured invocation result.
 */
export async function defaultGitInvoker(
  args: readonly string[],
  options: { readonly cwd: string },
): Promise<GitInvocationResult> {
  const command = new Deno.Command("git", {
    args: [...args],
    cwd: options.cwd,
    stdout: "piped",
    stderr: "piped",
  });
  const result = await command.output();
  return {
    exitCode: result.code,
    stdout: new TextDecoder().decode(result.stdout),
    stderr: new TextDecoder().decode(result.stderr),
  };
}

/**
 * Default conflict-file reader: backs onto `Deno.readTextFile` with
 * UTF-8 decoding. Pulled out as a separate factory so the rebase phase
 * holds no `Deno.readTextFile` reference outside this binding (Lesson
 * #3).
 *
 * @param path Absolute path of the conflicted file to read.
 * @returns The file contents as a UTF-8 string.
 */
export function defaultConflictFileReader(path: string): Promise<string> {
  return Deno.readTextFile(path);
}

/**
 * Adapt the default-namespace `@std/log` logger to the narrow
 * {@link StabilizeLogger} surface. `getLogger()` returns a `Logger`
 * whose `info`/`warn` overloads accept plain strings, but TypeScript
 * needs a thin adapter to project the SDK's shape onto our interface.
 */
function defaultLogger(): StabilizeLogger {
  const inner = getLogger();
  return {
    info(message: string): void {
      inner.info(message);
    },
    warn(message: string): void {
      inner.warn(message);
    },
  };
}
