/**
 * daemon/worktree-manager.ts — per-repo bare clones and per-task git
 * worktrees, the way ADR-007 spells them out.
 *
 * The supervisor (Wave 3) hands the manager a `<repo, issueNumber>` and
 * receives back an isolated working directory on its own branch. Concurrent
 * agents never share a working tree, but they *do* share the underlying
 * object store via the bare clone — `git fetch` once per repo refreshes
 * objects for every worktree.
 *
 * **Layout** (all paths beneath the configured `workspace`):
 *
 * ```text
 * <workspace>/repos/<owner>__<name>.git/                  # bare clone, one per repo
 * <workspace>/worktrees/<owner>__<name>/issue-<n>/        # worktree, one per task
 * ```
 *
 * **Branch naming**: `makina/issue-<n>`. The branch is created at the
 * remote `HEAD` of the bare clone the first time a worktree for that issue
 * is requested, and reused if the worktree is recreated afterwards (so
 * unmerged work survives a daemon restart).
 *
 * **Concurrency**. `git worktree add` mutates the bare clone's `worktrees/`
 * metadata directory; running two of them in parallel for the same repo can
 * race. The manager serializes `git` invocations *per repository* with a
 * tiny per-repo promise chain, so the supervisor can spin off many tasks
 * across repos in parallel without coordinating.
 *
 * **Startup hygiene**. `pruneAll()` runs `git worktree prune` against every
 * existing bare clone so an interrupted daemon doesn't leak metadata
 * (entries pointing at directories that were `rm -rf`'d while the daemon
 * was down).
 *
 * **TaskId bookkeeping**. The W1 `WorktreeManager` interface gives
 * `createWorktreeForIssue(repo, issueNumber)` no `taskId` to associate
 * with the resulting worktree, but `removeWorktree(taskId)` needs one. The
 * manager keeps its own `taskId → path` map; the supervisor calls
 * {@link WorktreeManagerImpl.registerTaskId} after minting the task id.
 * Until a task id is registered, only the path-based teardown works (the
 * supervisor isn't expected to use that — it goes through the registered
 * id so logs read consistently).
 *
 * @module
 */

import { getLogger } from "@std/log";
import { dirname, join, resolve } from "@std/path";

import {
  type IssueNumber,
  type RepoFullName,
  type TaskId,
  type WorktreeManager,
} from "../types.ts";

/**
 * Narrow logger surface used by the worktree manager. The `@std/log`
 * `Logger` class satisfies this shape (its `warn` accepts a string), but
 * the narrower interface lets tests inject a recording double without
 * matching the SDK's full overload signature.
 */
export interface WorktreeManagerLogger {
  /** Emit a warning. */
  warn(message: string): void;
}

/** Constructor options for {@link createWorktreeManager}. */
export interface WorktreeManagerOptions {
  /**
   * Absolute path to the makina workspace directory. The manager creates
   * `repos/` and `worktrees/` subdirectories beneath this path on first
   * use. The directory must already exist (the loader created it).
   */
  readonly workspace: string;
  /**
   * Override the `git` executable. Defaults to the unqualified name `git`
   * which Deno resolves on `PATH`. Useful for tests that bind to a
   * specific binary.
   */
  readonly gitBinary?: string;
  /**
   * Logger used for non-fatal warnings — currently only
   * {@link WorktreeManagerImpl.pruneAll} skipping a corrupt bare directory.
   * Defaults to `getLogger()` from `@std/log` (the default-namespace
   * logger), adapted to the {@link WorktreeManagerLogger} surface.
   *
   * Tests inject a recording logger to assert warning behavior without
   * touching the global logger registry.
   */
  readonly logger?: WorktreeManagerLogger;
}

/**
 * One git invocation's outcome, surfaced when something goes wrong.
 *
 * Carries the command, exit code, and captured stderr so the supervisor
 * can render a precise error message rather than a bare exception. The
 * class is exported so callers can `instanceof`-check.
 */
export class GitCommandError extends Error {
  /**
   * Build a git-command error.
   *
   * @param message Human-readable description.
   * @param command The full argv that was invoked, including the binary.
   * @param exitCode Process exit code reported by Deno.
   * @param stderr Captured stderr text.
   */
  constructor(
    message: string,
    /** Full argv that was invoked, including the `git` binary. */
    public readonly command: readonly string[],
    /** Exit code reported by Deno. */
    public readonly exitCode: number,
    /** Captured stderr text. */
    public readonly stderr: string,
  ) {
    super(message);
    this.name = "GitCommandError";
  }
}

/**
 * Concrete return type of {@link createWorktreeManager}. Widens the W1
 * {@link WorktreeManager} contract with:
 *
 *  - a `remoteUrl` argument on {@link WorktreeManagerImpl.ensureBareClone}
 *    (the W3 supervisor injects the GitHub-App-authenticated URL there);
 *  - the bookkeeping calls the supervisor uses to map task ids to paths;
 *  - {@link WorktreeManagerImpl.pruneAll} for startup hygiene.
 *
 * The wider surface stays out of the cross-wave contract file so consumer
 * waves not in the supervisor (TUI, persistence) are not coupled to it.
 */
export interface WorktreeManagerImpl extends Omit<WorktreeManager, "ensureBareClone"> {
  /**
   * Ensure a bare clone of `repo` exists on disk, cloning from `remoteUrl`
   * if it does not. Idempotent: subsequent calls observe the existing
   * directory and short-circuit without invoking `git clone`.
   *
   * @param repo Repository identifier.
   * @param remoteUrl Fully-qualified clone URL. The supervisor builds this
   *   with a GitHub App installation token embedded in the userinfo so the
   *   bare clone can be created without a separate credential helper.
   * @returns Absolute filesystem path of the bare clone.
   */
  ensureBareClone(repo: RepoFullName, remoteUrl: string): Promise<string>;

  /**
   * Run `git worktree prune` against every bare clone present in the
   * workspace. Idempotent; safe to call many times. The supervisor calls
   * this once during boot, after the workspace is loaded but before any
   * worktree creation.
   *
   * @returns Resolves when every prune finishes.
   */
  pruneAll(): Promise<void>;

  /**
   * Associate `taskId` with a worktree path so a later
   * {@link WorktreeManager.removeWorktree} call can find it. The
   * supervisor calls this immediately after
   * {@link WorktreeManager.createWorktreeForIssue} returns.
   *
   * Re-registering the same id with a different path overwrites the prior
   * binding (handy when the supervisor re-creates a worktree after
   * resuming from persistence).
   *
   * @param taskId The task identifier to bind.
   * @param worktreePath Absolute filesystem path of the worktree.
   */
  registerTaskId(taskId: TaskId, worktreePath: string): void;

  /**
   * Look up the worktree path bound to `taskId`, or `undefined` if the
   * supervisor has not yet registered (or has already removed) it.
   *
   * @param taskId The task identifier to look up.
   * @returns The bound worktree path, if any.
   */
  worktreePathFor(taskId: TaskId): string | undefined;
}

/**
 * Construct a {@link WorktreeManagerImpl}.
 *
 * The factory does not perform any IO; it returns an object whose methods
 * lazily create the `repos/` and `worktrees/` subdirectories of `workspace`
 * the first time they are needed. This keeps construction cheap and makes
 * the manager easy to instantiate inside tests.
 *
 * @param opts Configuration.
 * @returns A {@link WorktreeManagerImpl} bound to `opts.workspace`.
 *
 * @example
 * ```ts
 * import { createWorktreeManager } from "./worktree-manager.ts";
 *
 * const manager = createWorktreeManager({ workspace: "/var/lib/makina" });
 * await manager.pruneAll();
 * await manager.ensureBareClone(repo, "https://github.com/owner/name.git");
 * const path = await manager.createWorktreeForIssue(repo, makeIssueNumber(42));
 * manager.registerTaskId(taskId, path);
 * ```
 */
export function createWorktreeManager(
  opts: WorktreeManagerOptions,
): WorktreeManagerImpl {
  if (opts.workspace.length === 0) {
    throw new RangeError("workspace path cannot be empty");
  }
  const workspace = opts.workspace;
  const gitBinary = opts.gitBinary ?? "git";
  const logger = opts.logger ?? defaultLogger();

  const reposRoot = join(workspace, "repos");
  const worktreesRoot = join(workspace, "worktrees");

  /**
   * Per-repository serialization queue. `git worktree add` and `git
   * worktree remove` mutate the bare clone's `worktrees/` metadata
   * directory; running two of them in parallel for the same repo can
   * race. We serialize *per repo*, not globally, so unrelated repos run
   * in parallel.
   */
  const repoLocks = new Map<string, Promise<unknown>>();

  /**
   * Bookkeeping for {@link WorktreeManagerImpl.removeWorktree}. The
   * supervisor calls {@link WorktreeManagerImpl.registerTaskId} after
   * minting the task id and stashing the path on the persistent task
   * record. Re-registering with a different path is allowed (recovery
   * after a daemon crash recreates the binding from disk).
   */
  const taskIdToPath = new Map<TaskId, string>();

  /**
   * Acquire the per-repo lock around `work`, so that only one mutating
   * operation per bare clone is in flight at a time. Errors propagate
   * through the chain unchanged but do not poison subsequent calls — the
   * lock only awaits *settlement*, not success.
   *
   * Lock entries are removed from the map once the chain quiesces. We
   * compare-and-swap on the stored sentinel: only the operation whose
   * sentinel still occupies the slot at settlement time clears it. If a
   * later caller chained on top, the slot belongs to that newer chain and
   * must not be touched. This keeps `repoLocks` proportional to *active*
   * repositories rather than the cumulative set ever seen — important for
   * long-lived daemons that touch many repos.
   */
  function withRepoLock<T>(repoKey: string, work: () => Promise<T>): Promise<T> {
    const previous = repoLocks.get(repoKey) ?? Promise.resolve();
    const next = previous
      .catch(() => undefined)
      .then(() => work());
    // Store the cleaned promise so a failure on one task doesn't leak
    // into the next one's `await`. Capture the exact sentinel we install
    // so the cleanup branch can verify nothing chained on top before it
    // removes the entry.
    const sentinel: Promise<unknown> = next.catch(() => undefined);
    repoLocks.set(repoKey, sentinel);
    sentinel.then(() => {
      // Compare-and-swap: only delete if this chain is still the tail. A
      // subsequent `withRepoLock(repoKey, …)` would have replaced the
      // entry with its own sentinel, in which case ours is stale and
      // there is nothing to clean up.
      if (repoLocks.get(repoKey) === sentinel) {
        repoLocks.delete(repoKey);
      }
    });
    return next;
  }

  function bareClonePath(repo: RepoFullName): string {
    return join(reposRoot, `${repoSlug(repo)}.git`);
  }

  function worktreePath(repo: RepoFullName, issueNumber: IssueNumber): string {
    return join(worktreesRoot, repoSlug(repo), `issue-${issueNumber}`);
  }

  function branchName(issueNumber: IssueNumber): string {
    return `makina/issue-${issueNumber}`;
  }

  async function runGit(
    args: readonly string[],
    cwd?: string,
  ): Promise<{ stdout: string; stderr: string }> {
    const options: Deno.CommandOptions = {
      args: [...args],
      stdout: "piped",
      stderr: "piped",
    };
    if (cwd !== undefined) {
      options.cwd = cwd;
    }
    const command = new Deno.Command(gitBinary, options);
    const result = await command.output();
    const stdout = new TextDecoder().decode(result.stdout);
    const stderr = new TextDecoder().decode(result.stderr);
    if (!result.success) {
      // Redact credentials from any URL-shaped argv positions before the
      // error escapes — the supervisor injects GitHub App installation
      // tokens into clone URLs (`https://x-access-token:<token>@github.com/...`)
      // and a bare exception would otherwise route them straight into logs
      // and telemetry. We keep the host + path so the failure is still
      // diagnosable.
      const sanitizedArgs = args.map(redactUrlCredentials);
      const sanitizedStderr = redactUrlCredentials(stderr);
      throw new GitCommandError(
        `git ${sanitizedArgs.join(" ")} failed (exit ${result.code})`,
        [gitBinary, ...sanitizedArgs],
        result.code,
        sanitizedStderr,
      );
    }
    return { stdout, stderr };
  }

  async function pathExists(path: string): Promise<boolean> {
    try {
      await Deno.lstat(path);
      return true;
    } catch (error) {
      if (error instanceof Deno.errors.NotFound) {
        return false;
      }
      throw error;
    }
  }

  async function ensureBareClone(
    repo: RepoFullName,
    remoteUrl: string,
  ): Promise<string> {
    if (remoteUrl.length === 0) {
      throw new RangeError("remoteUrl cannot be empty");
    }
    const target = bareClonePath(repo);
    return await withRepoLock(target, async () => {
      if (await pathExists(target)) {
        // Sanity-check that what's there is actually a git directory; a
        // half-cloned leftover from a crashed run shouldn't be silently
        // treated as success.
        if (!(await pathExists(join(target, "HEAD")))) {
          throw new Error(
            `bare clone at ${target} exists but has no HEAD; remove it manually`,
          );
        }
        return target;
      }
      await Deno.mkdir(reposRoot, { recursive: true });
      await runGit(["clone", "--bare", "--quiet", remoteUrl, target]);
      return target;
    });
  }

  async function createWorktreeForIssue(
    repo: RepoFullName,
    issueNumber: IssueNumber,
  ): Promise<string> {
    const bareDir = bareClonePath(repo);
    if (!(await pathExists(bareDir))) {
      throw new Error(
        `bare clone for ${repo} not found at ${bareDir}; ` +
          `call ensureBareClone first`,
      );
    }
    const target = worktreePath(repo, issueNumber);
    const branch = branchName(issueNumber);
    return await withRepoLock(bareDir, async () => {
      if (await pathExists(target)) {
        // Someone (probably us, after a daemon restart) already has the
        // worktree on disk. Trust it: the supervisor preserves worktrees
        // across NEEDS_HUMAN/FAILED, and recreating it would clobber any
        // in-flight edits.
        return target;
      }
      await Deno.mkdir(dirname(target), { recursive: true });
      // If the branch already exists in the bare clone, reuse it; else
      // create it pointing at `HEAD`. The two-step is needed because
      // `git worktree add -b` fails if the branch is already there.
      const existing = await branchExists(bareDir, branch);
      const args = existing
        ? ["worktree", "add", "--quiet", target, branch]
        : ["worktree", "add", "--quiet", "-b", branch, target, "HEAD"];
      await runGit(args, bareDir);
      return target;
    });
  }

  async function branchExists(bareDir: string, branch: string): Promise<boolean> {
    try {
      await runGit(
        ["show-ref", "--verify", "--quiet", `refs/heads/${branch}`],
        bareDir,
      );
      return true;
    } catch (error) {
      // `git show-ref --verify --quiet` exits 1 when the ref does not
      // exist. Any other non-zero exit is a real error worth surfacing.
      if (error instanceof GitCommandError && error.exitCode === 1) {
        return false;
      }
      throw error;
    }
  }

  async function removeWorktree(taskId: TaskId): Promise<void> {
    const path = taskIdToPath.get(taskId);
    if (path === undefined) {
      // Idempotent: tearing down an already-removed task is a no-op.
      return;
    }
    // Path-traversal guard. `removeWorktree` performs recursive deletes,
    // and a registration whose path escapes the configured workspace
    // would let a corrupt persistence layer (or a malicious task record)
    // wipe arbitrary directories on disk. Any registration outside
    // `worktreesRoot` is a programming error: refuse it loudly and leave
    // the bookkeeping intact so the operator can investigate. The check
    // is intentionally strict — we *only* delete inside `worktreesRoot`,
    // never under `reposRoot`, never the workspace root itself, never
    // anywhere else on the filesystem.
    if (!isUnderWorktreesRoot(path)) {
      throw new Error(
        `refusing to remove worktree at ${path}: ` +
          `path is not under the configured workspace (${worktreesRoot}). ` +
          `This guards against path-traversal via corrupt task registrations.`,
      );
    }
    // Locate the bare clone for this worktree path so we can serialize
    // against other operations on the same repo.
    const repoKey = bareCloneForWorktreePath(path);
    await withRepoLock(repoKey ?? path, async () => {
      // Re-check bare-clone usability *inside* the lock. A check before
      // the lock would race against a concurrent `removeWorktree` (or an
      // out-of-band `rm -rf`) that wipes the bare clone between the
      // check and the locked section: we'd then take the git path and
      // throw, breaking the idempotency guarantee. The bare clone may be
      // gone (a user `rm -rf`'d the workspace, or its bookkeeping was
      // lost). Talking to git is pointless then: the metadata to prune
      // lives *inside* the bare clone, so its absence already means
      // there is nothing to reclaim. Fall back to a manual rm — still
      // bounded by the prefix guard above — so the API stays idempotent
      // on corrupted state.
      const bareCloneUsable = repoKey !== null &&
        (await pathExists(join(repoKey, "HEAD")));
      if (!bareCloneUsable) {
        if (await pathExists(path)) {
          await Deno.remove(path, { recursive: true });
        }
        taskIdToPath.delete(taskId);
        return;
      }
      // `repoKey` is non-null because `bareCloneUsable` requires it.
      const bareDir = repoKey as string;
      if (await pathExists(path)) {
        try {
          await runGit(["worktree", "remove", "--force", path], bareDir);
        } catch (error) {
          // If `git worktree remove` failed (e.g. because the worktree
          // metadata is corrupt), fall back to a manual rm followed by
          // `git worktree prune` so bookkeeping stays consistent. The
          // prune itself is best-effort: if the bare clone vanished
          // mid-flight we treat that as "nothing to reclaim" rather than
          // re-throwing past the idempotency guarantee.
          if (await pathExists(path)) {
            await Deno.remove(path, { recursive: true });
          }
          if (await pathExists(join(bareDir, "HEAD"))) {
            await runGit(["worktree", "prune"], bareDir);
          }
          if (!(error instanceof GitCommandError)) {
            throw error;
          }
        }
      } else {
        // The directory is gone but git may still hold metadata for it.
        // Bare clone may have vanished concurrently — only prune if it's
        // still there to be pruned.
        if (await pathExists(join(bareDir, "HEAD"))) {
          await runGit(["worktree", "prune"], bareDir);
        }
      }
      taskIdToPath.delete(taskId);
    });
  }

  /**
   * True when `path` sits strictly under `<worktreesRoot>/` after lexical
   * normalization. We *resolve* `path` before checking — a corrupt
   * registration like `<worktreesRoot>/repo/../../../outside` passes a raw
   * `startsWith` test but `Deno.remove()` would happily traverse the `..`
   * segments and wipe directories outside the workspace. Resolving first
   * collapses the `..` so the prefix check sees the real target.
   *
   * The check is purely lexical (no symlink resolution): the supervisor
   * never creates symlinked worktree paths, and a real-filesystem
   * `realpath` would fail for paths whose leaf doesn't exist yet. The
   * trailing-separator prefix (`<worktreesRoot>/`) keeps siblings like
   * `<worktreesRoot>-suffix` out of bounds, and the explicit equality
   * check rejects the root itself.
   */
  function isUnderWorktreesRoot(path: string): boolean {
    const normalizedRoot = resolve(worktreesRoot);
    const normalized = resolve(path);
    if (normalized === normalizedRoot) {
      return false;
    }
    const prefix = normalizedRoot + "/";
    return normalized.startsWith(prefix);
  }

  /**
   * Recover the bare-clone path that a worktree path lives under. Returns
   * `null` if the worktree path doesn't sit under our `worktrees/` root —
   * that shouldn't happen via the manager's own APIs (the prefix guard in
   * {@link isUnderWorktreesRoot} runs first), but we tolerate it to keep
   * `removeWorktree` idempotent on weird state. We resolve `path` first
   * for the same reason `isUnderWorktreesRoot` does: a stored path that
   * embeds `..` segments must be normalized before we slice the slug.
   */
  function bareCloneForWorktreePath(path: string): string | null {
    const normalizedRoot = resolve(worktreesRoot);
    const normalized = resolve(path);
    const prefix = normalizedRoot + "/";
    if (!normalized.startsWith(prefix)) {
      return null;
    }
    const remainder = normalized.slice(prefix.length);
    const slash = remainder.indexOf("/");
    if (slash <= 0) {
      return null;
    }
    const slug = remainder.slice(0, slash);
    return join(reposRoot, `${slug}.git`);
  }

  async function pruneAll(): Promise<void> {
    if (!(await pathExists(reposRoot))) {
      return;
    }
    const tasks: Promise<void>[] = [];
    for await (const entry of Deno.readDir(reposRoot)) {
      if (!entry.isDirectory || !entry.name.endsWith(".git")) {
        continue;
      }
      const bareDir = join(reposRoot, entry.name);
      // `pruneAll` is startup hygiene, called once before any worktree
      // creation. A single corrupt entry (a partial clone that never
      // wrote `HEAD`, or a manually-dropped `*.git` folder) must not
      // abort pruning the rest — that would prevent the daemon from
      // starting on an otherwise-recoverable workspace. Catch the
      // per-directory `GitCommandError`, warn, and move on. Errors
      // outside `runGit` (filesystem, lock primitive) still propagate.
      tasks.push(
        withRepoLock(bareDir, async () => {
          try {
            await runGit(["worktree", "prune"], bareDir);
          } catch (error) {
            if (!(error instanceof GitCommandError)) {
              throw error;
            }
            logger.warn(
              `worktree-manager: skipping prune of ${bareDir} ` +
                `(git exited ${error.exitCode}: ${error.stderr.trim()})`,
            );
          }
        }),
      );
    }
    await Promise.all(tasks);
  }

  function registerTaskId(taskId: TaskId, path: string): void {
    if (path.length === 0) {
      throw new RangeError("worktree path cannot be empty");
    }
    taskIdToPath.set(taskId, path);
  }

  function worktreePathFor(taskId: TaskId): string | undefined {
    return taskIdToPath.get(taskId);
  }

  return {
    ensureBareClone,
    createWorktreeForIssue,
    removeWorktree,
    pruneAll,
    registerTaskId,
    worktreePathFor,
  };
}

/**
 * Adapt the default-namespace `@std/log` logger to the narrow
 * {@link WorktreeManagerLogger} surface. `getLogger()` returns a `Logger`
 * whose `warn` overload accepts a plain string, but TypeScript needs a
 * thin adapter to project the SDK's shape onto our interface.
 */
function defaultLogger(): WorktreeManagerLogger {
  const inner = getLogger();
  return {
    warn(message: string): void {
      inner.warn(message);
    },
  };
}

/**
 * Convert a `<owner>/<name>` repo identifier into a single filesystem
 * segment by replacing the `/` separator with `__`. We use `__` rather
 * than `--` so hyphens in GitHub owner/repository names remain unchanged.
 *
 * The substitution is **best-effort**, not a reversible escape: a name
 * that happens to embed `__` would alias another, and {@link makeRepoFullName}
 * does not enforce a no-double-underscore invariant (W1 contract). The slug
 * is purely for laying out files on disk; callers who need to recover the
 * `<owner>/<name>` pair must keep it stored alongside, not parse it back.
 *
 * @param repo The repository name.
 * @returns A filesystem-safe slug.
 */
function repoSlug(repo: RepoFullName): string {
  return repo.replace("/", "__");
}

/**
 * Strip `userinfo` (`user`, `user:password`, or a bare token) from any
 * URL-shaped substring inside `text`, replacing it with the literal
 * `REDACTED` so the host + path remain debuggable. Used by `runGit` to
 * sanitize argv and stderr before they enter a {@link GitCommandError}:
 * the W3 supervisor injects GitHub App installation tokens as the
 * userinfo part of the clone URL (`https://x-access-token:<token>@…`),
 * and we must not leak those into logs/telemetry on a `git clone`
 * failure.
 *
 * The regex is intentionally conservative: it matches `scheme://user[:pw]@`
 * sequences. URLs without userinfo, SSH-style `git@host:` strings (no
 * credential to redact), and arbitrary non-URL text pass through unchanged.
 *
 * @param text Argv element or stderr buffer that may contain a URL.
 * @returns `text` with any `userinfo` segments rewritten to `REDACTED`.
 */
function redactUrlCredentials(text: string): string {
  // Match `scheme://userinfo@`. The userinfo grammar in RFC 3986 allows
  // unreserved chars + a few sub-delims + `:` + percent-encodings; we use
  // a permissive `[^@\s/]+` to avoid pulling in URL parsing for what is
  // ultimately a redaction routine.
  return text.replace(/([a-zA-Z][a-zA-Z0-9+.\-]*:\/\/)[^@\s/]+@/g, "$1REDACTED@");
}
