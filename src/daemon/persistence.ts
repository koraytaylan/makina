/**
 * daemon/persistence.ts — atomic JSON state store for the supervisor.
 *
 * The daemon must be able to crash at any byte boundary and come back up with
 * a coherent picture of every in-flight task. The contract for that is the
 * {@link Persistence} interface in `src/types.ts`; this module is the
 * production implementation.
 *
 * **Storage shape.** A single JSON file at the user-configured path. The
 * top-level value is an array of {@link Task} records — small, denormalized,
 * easy to inspect with `jq`. Wave 5+ may switch to a sharded layout once the
 * task count crosses the readability threshold; the interface is stable
 * either way.
 *
 * **Atomic write.** Every mutation rewrites the entire store using the
 * standard write-tmp + fsync + rename + dir-fsync dance:
 *
 *  1. Serialize the current in-memory snapshot to JSON.
 *  2. Open `<path>.tmp` (truncating any leftover from a previous crash).
 *  3. Write the bytes, then `fdatasync` the file descriptor so the kernel
 *     flushes them to disk before the rename.
 *  4. Atomically rename `<path>.tmp` over `<path>`.
 *  5. Open the parent directory and `fdatasync` it so the new directory
 *     entry (the post-rename name → inode mapping) is itself durable. On
 *     POSIX filesystems the rename's metadata can otherwise live in the
 *     directory's dirty page cache and be lost on power failure even after
 *     the file's data has been flushed.
 *
 * Because POSIX rename(2) within a filesystem is atomic, an observer (a
 * concurrent `loadAll`, the next daemon restart, an operator with `cat`)
 * sees either the previous full file or the new full file — never a torn
 * mix.
 *
 * **Concurrency.** A single internal mutex (a chained promise) serializes
 * `save` and `remove` so two callers cannot race the rename. `loadAll`
 * waits behind any in-flight write; this is conservative but the cost is
 * negligible against disk IO.
 *
 * **Crash recovery.** If `<path>.tmp` exists at startup it is the residue of
 * a crash mid-write — the rename never happened, so the live `<path>` is
 * still intact. We ignore the orphan on the read path; the next successful
 * `save` overwrites it. If the live `<path>` is missing entirely, the store
 * is empty — `loadAll` returns `[]` rather than failing.
 *
 * **`@std/fs` policy.** Issue #7 specifies "All file IO through `@std/fs`
 * where possible". The persistence layer is the documented exception:
 * `@std/fs` exposes neither the rename, the file-descriptor `syncData`, nor
 * a directory-tracking `mkdir` that would let us fsync each newly-created
 * parent (see `ensureDurableParentChain` below for why that matters). We
 * therefore call `Deno.readTextFile`, `Deno.rename`, `Deno.mkdir`, and
 * `Deno.open` directly. The rationale is captured in
 * {@link https://github.com/koraytaylan/makina/blob/develop/docs/adrs/014-persistence-durability-protocol.md ADR-014}.
 * Other modules without a hard durability contract should continue to use
 * `@std/fs`.
 *
 * @module
 */

import { dirname } from "@std/path";

import { MERGE_MODES, type Persistence, type Task, TASK_STATES, type TaskId } from "../types.ts";

/** Suffix appended to the live path to form the temp file used for atomic writes. */
const TEMP_FILE_SUFFIX = ".tmp";

/** JSON serialization indentation. Two spaces matches `deno fmt` defaults. */
const JSON_INDENT_SPACES = 2;

/**
 * Function shape used by {@link createPersistence} to open files and
 * directories on the write path. Structurally identical to
 * {@link Deno.open}; production wiring uses `Deno.open` itself, while
 * tests can inject a spy that records `syncData` calls without mutating
 * the global. The latter matters because `deno test --parallel` shares a
 * single process with every test file, and a swapped-out `Deno.open` is
 * a cross-test race waiting to happen.
 */
export type FsOpen = (
  path: string | URL,
  options?: Deno.OpenOptions,
) => Promise<Deno.FsFile>;

/**
 * Construct an atomic JSON-backed {@link Persistence}.
 *
 * The returned object owns the on-disk file at `opts.path`. Multiple
 * instances pointed at the same path will not coordinate with each other;
 * the daemon constructs exactly one per process.
 *
 * The file is created lazily on the first {@link Persistence.save} or
 * {@link Persistence.remove}. {@link Persistence.loadAll} on a missing
 * file returns `[]`, which is the correct semantics for a fresh daemon
 * install.
 *
 * @param opts.path Filesystem path of the live store. May be absolute or
 *   relative; relative paths are resolved against the daemon's current
 *   working directory by the underlying `Deno.readTextFile` / `Deno.rename`
 *   syscalls. The parent directory is created if it does not exist; the
 *   file itself is created on demand. Production callers pass an absolute
 *   path so the daemon's `cd` (or lack thereof) cannot move the store out
 *   from under it.
 * @param opts.open Optional override for `Deno.open`. Production callers
 *   leave this unset and get the real syscall; tests pass a spy so they
 *   can observe `syncData()` invocations without mutating `Deno.open`
 *   process-wide. The hook intercepts only `Deno.open` calls; the other
 *   filesystem syscalls used by this module — `Deno.readTextFile`,
 *   `Deno.mkdir`, and `Deno.rename` — go directly to the runtime because
 *   no test currently needs to observe them and the production behavior
 *   is identical with or without a spy.
 * @returns A {@link Persistence} bound to `opts.path`.
 *
 * @example
 * ```ts
 * import { createPersistence } from "./persistence.ts";
 *
 * const persistence = createPersistence({ path: "/var/lib/makina/state.json" });
 * const tasks = await persistence.loadAll();
 * for (const task of tasks) {
 *   console.log(task.id, task.state);
 * }
 * ```
 */
export function createPersistence(
  opts: { readonly path: string; readonly open?: FsOpen },
): Persistence {
  return new AtomicJsonPersistence(opts.path, opts.open ?? Deno.open);
}

/**
 * Internal implementation of {@link Persistence} backed by a single JSON
 * file with atomic write semantics. Exported only as a type via
 * {@link createPersistence}; instances must be obtained through the factory
 * so the construction invariants stay inside this module.
 */
class AtomicJsonPersistence implements Persistence {
  /** Absolute path of the live store. */
  private readonly livePath: string;
  /** Absolute path of the staging file used for atomic writes. */
  private readonly tempPath: string;
  /**
   * Injected `Deno.open` (or a test spy with the same shape). Used for the
   * temp-file write and the parent-directory open whose `syncData()` calls
   * make the rename durable.
   */
  private readonly open: FsOpen;
  /**
   * Tail of the serialization mutex chain. Every mutation appends itself by
   * `await`-ing the previous tail and assigning a new tail; concurrent
   * callers therefore queue rather than race the rename.
   *
   * The chain catches and discards rejection so a single failed save does
   * not poison every subsequent caller.
   */
  private writeChain: Promise<void> = Promise.resolve();

  /**
   * @param livePath Absolute path of the live JSON store.
   * @param open Function used to open files/directories on the write path.
   *   Defaults to {@link Deno.open}; tests inject a spy.
   */
  constructor(livePath: string, open: FsOpen) {
    this.livePath = livePath;
    this.tempPath = `${livePath}${TEMP_FILE_SUFFIX}`;
    this.open = open;
  }

  /**
   * Read the on-disk store and return every persisted task.
   *
   * If the live file does not exist, returns an empty array. A leftover
   * `<path>.tmp` from a crashed write is ignored — the live file (or its
   * absence) is the source of truth.
   *
   * @returns Frozen array of every task currently persisted, in the order
   *   the file holds them.
   * @throws If the file exists but is not valid JSON, the root is not an
   *   array, or any element is missing the required {@link Task} shape
   *   (object with at minimum `id`, `repo`, `issueNumber`, `state`,
   *   `mergeMode`, `model`, `iterationCount`, `createdAtIso`,
   *   `updatedAtIso`); this surfaces a corrupt store rather than silently
   *   handing the daemon malformed records typed as `Task`.
   */
  async loadAll(): Promise<readonly Task[]> {
    // Wait for any in-flight write so we never observe a partially serialized
    // in-memory snapshot, even though the on-disk file is itself atomic.
    await this.waitForChain();
    let bytes: string;
    try {
      bytes = await Deno.readTextFile(this.livePath);
    } catch (error) {
      if (error instanceof Deno.errors.NotFound) {
        return Object.freeze([]);
      }
      throw error;
    }
    const trimmed = bytes.trim();
    if (trimmed.length === 0) {
      // Treat a zero-byte file as an empty store. We never write zero bytes
      // ourselves, but a manual `truncate` against the path should not crash
      // the daemon on startup.
      return Object.freeze([]);
    }
    let parsed: unknown;
    try {
      parsed = JSON.parse(trimmed);
    } catch (error) {
      throw new Error(
        `persistence: ${this.livePath} is not valid JSON`,
        { cause: error },
      );
    }
    if (!Array.isArray(parsed)) {
      throw new Error(
        `persistence: ${this.livePath} root is not an array`,
      );
    }
    return Object.freeze(parsed.map((entry, index) => coerceTask(entry, index, this.livePath)));
  }

  /**
   * Persist `task`, overwriting any existing record with the same
   * {@link Task.id}.
   *
   * Reads the current store, replaces (or appends) the matching record, and
   * writes the result atomically. Concurrent calls are serialized through
   * an internal mutex so two writers cannot race the rename.
   *
   * @param task The task to persist.
   * @returns A promise that resolves once the bytes are durably on disk.
   *
   * @example
   * ```ts
   * await persistence.save({
   *   id: makeTaskId("task_2026-04-26T12-00-00_abc123"),
   *   repo: makeRepoFullName("koraytaylan/makina"),
   *   issueNumber: makeIssueNumber(7),
   *   state: "INIT",
   *   mergeMode: "squash",
   *   model: "claude-sonnet-4-6",
   *   iterationCount: 0,
   *   createdAtIso: new Date().toISOString(),
   *   updatedAtIso: new Date().toISOString(),
   * });
   * ```
   */
  save(task: Task): Promise<void> {
    return this.enqueue(async () => {
      const current = await this.readForMutation();
      const next = upsertTask(current, task);
      await this.writeAtomic(next);
    });
  }

  /**
   * Remove the persisted record for `taskId`.
   *
   * No-op if no record matches. Concurrent with {@link save}, ordered
   * through the same internal mutex.
   *
   * @param taskId Identifier of the task to forget.
   */
  remove(taskId: TaskId): Promise<void> {
    return this.enqueue(async () => {
      const current = await this.readForMutation();
      const next = current.filter((existing) => existing.id !== taskId);
      if (next.length === current.length) {
        // Nothing to do — leave the store untouched and skip the disk IO.
        return;
      }
      await this.writeAtomic(next);
    });
  }

  /**
   * Append `mutation` to the write chain so it runs after every previously
   * scheduled mutation completes (success or failure).
   */
  private enqueue(mutation: () => Promise<void>): Promise<void> {
    const next = this.writeChain.then(mutation, mutation);
    // Suppress UnhandledPromiseRejection on the chain itself — callers see
    // the failure through the awaited `next`, but the chain must continue.
    this.writeChain = next.then(noop, noop);
    return next;
  }

  /**
   * Resolve when the current write chain settles, regardless of outcome.
   * Used by {@link loadAll} to avoid reading mid-rename.
   */
  private async waitForChain(): Promise<void> {
    try {
      await this.writeChain;
    } catch {
      // The original caller already saw the rejection; we just need to
      // know the chain is no longer running.
    }
  }

  /**
   * Read the live store from disk into a mutable array suitable for the
   * in-place upsert in {@link save}.
   */
  private async readForMutation(): Promise<Task[]> {
    let bytes: string;
    try {
      bytes = await Deno.readTextFile(this.livePath);
    } catch (error) {
      if (error instanceof Deno.errors.NotFound) {
        return [];
      }
      throw error;
    }
    const trimmed = bytes.trim();
    if (trimmed.length === 0) {
      return [];
    }
    let parsed: unknown;
    try {
      parsed = JSON.parse(trimmed);
    } catch (error) {
      throw new Error(
        `persistence: ${this.livePath} is not valid JSON`,
        { cause: error },
      );
    }
    if (!Array.isArray(parsed)) {
      throw new Error(
        `persistence: ${this.livePath} root is not an array`,
      );
    }
    return parsed.map((entry, index) => coerceTask(entry, index, this.livePath));
  }

  /**
   * Atomically replace the live file with the JSON serialization of
   * `tasks`.
   *
   * Sequence: durably ensure the parent chain → open `<path>.tmp`
   * (truncate) → write JSON → `fdatasync` the file descriptor → close →
   * rename over the live path → `fdatasync` every directory from the leaf
   * parent up to (and including) the highest newly-created ancestor's
   * parent. The rename is the commit point and the directory syncs make
   * both that commit and any freshly-created parent entries durable across
   * power loss — without them the new directory entry (or its containing
   * directory's entry, if we just created the directory) can live in dirty
   * page cache and be lost even though the file's data is on disk.
   */
  private async writeAtomic(tasks: readonly Task[]): Promise<void> {
    const parent = dirname(this.livePath);
    const created = await this.ensureDurableParentChain(parent);
    const json = JSON.stringify(tasks, null, JSON_INDENT_SPACES);
    const bytes = new TextEncoder().encode(json);
    const file = await this.open(this.tempPath, {
      write: true,
      create: true,
      truncate: true,
    });
    try {
      await writeAll(file, bytes);
      await file.syncData();
    } finally {
      file.close();
    }
    await Deno.rename(this.tempPath, this.livePath);
    // Always sync the leaf parent so the rename's directory entry is
    // durable. Then walk up the chain of directories we just created and
    // sync each of *their* parents so each new directory entry is also
    // durable. The set is deduplicated and ordered from leaf-most outward,
    // which matches the safe-to-fsync order on POSIX (a child must be
    // synced before its parent if both are dirty, though syncing in either
    // order is correct — we just want every link committed).
    const toSync = directoriesToFsync(parent, created);
    for (const dir of toSync) {
      await this.syncDirectory(dir);
    }
  }

  /**
   * Create every missing directory from the filesystem root down to
   * `leaf`, fsyncing each newly-created directory's *parent* so the new
   * entry is durable.
   *
   * Returns the absolute (or platform-rooted relative) paths of every
   * directory we actually had to create, ordered leaf-most first. If the
   * entire chain already existed, returns an empty array.
   *
   * Why this is not just `ensureDir + syncDirectory(leaf)`: when
   * `ensureDir` creates intermediate directories (e.g., creating
   * `<workspace>/persistence/` to host `state.json` when `<workspace>`
   * exists but `persistence/` does not), the new sub-directory's
   * directory entry lives in `<workspace>`'s dirty page cache. Without
   * fsyncing `<workspace>`, a power loss after the rename can lose the
   * `persistence/` entry — which loses the state file even though we
   * fsync'd it.
   */
  private async ensureDurableParentChain(leaf: string): Promise<string[]> {
    // Build the chain from leaf up to the root by repeated `dirname`.
    const chain: string[] = [];
    let current = leaf;
    // `dirname("/")` is "/" on POSIX; `dirname(".")` is "." — both are
    // fixed points, which is how we terminate. Defensive cap so a buggy
    // path does not loop forever.
    for (let i = 0; i < 4096; i += 1) {
      chain.push(current);
      const next = dirname(current);
      if (next === current) {
        break;
      }
      current = next;
    }
    // Walk root → leaf (reverse), creating each directory if missing.
    // Track which ones we actually created so we can fsync their parents
    // afterwards.
    const created: string[] = [];
    for (let i = chain.length - 1; i >= 0; i -= 1) {
      const dir = chain[i];
      if (dir === undefined) {
        continue;
      }
      try {
        await Deno.mkdir(dir);
        created.push(dir);
      } catch (error) {
        if (error instanceof Deno.errors.AlreadyExists) {
          continue;
        }
        throw error;
      }
    }
    // `created` is in root → leaf order; flip to leaf → root so callers
    // see the deepest directory first (matches the natural fsync order).
    return created.reverse();
  }

  /**
   * `fdatasync` the directory at `path` so the most recent rename or
   * mkdir within it is durable across power loss.
   *
   * On POSIX filesystems a `rename(2)` or `mkdir(2)` updates the parent
   * directory's dirty page cache; the entry is not durable until the
   * directory itself is flushed. macOS, Linux ext4, XFS, and APFS all
   * permit opening a directory read-only and calling `fdatasync` on the
   * descriptor — Deno exposes that as `Deno.FsFile.syncData()`.
   *
   * Routed through the injected {@link FsOpen} so the same spy that
   * observes the temp-file sync also sees the directory sync.
   *
   * Any IO failure is propagated; a half-durable rename is worse than no
   * rename because callers think their write committed.
   */
  private async syncDirectory(path: string): Promise<void> {
    const dir = await this.open(path, { read: true });
    try {
      await dir.syncData();
    } finally {
      dir.close();
    }
  }
}

/**
 * Compute the set of directories whose `fdatasync` makes a `writeAtomic`
 * commit fully durable, ordered leaf-most first.
 *
 * Two distinct directory entries can be dirty after a `writeAtomic`:
 *
 *  1. The leaf parent directory of the live file — its rename's new
 *     entry lives in the leaf's dirty page cache.
 *  2. The parent of every directory that {@link AtomicJsonPersistence.ensureDurableParentChain}
 *     just created — each new directory's entry lives in *its* parent's
 *     dirty page cache.
 *
 * `created` from `ensureDurableParentChain` is leaf-most first, so the
 * parents we need to fsync are `dirname(created[0])`,
 * `dirname(created[1])`, etc., plus the leaf itself. Deduplicating keeps
 * us from fsyncing the same directory twice when the leaf parent was
 * itself one of the just-created directories (e.g., when the entire
 * persistence dir tree was new). The leaf-most-first ordering is the
 * natural shape — fsyncing the deepest directory first covers the
 * just-renamed file entry before we move outward to durably link in
 * the new directories themselves.
 */
function directoriesToFsync(leafParent: string, created: readonly string[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  const push = (path: string): void => {
    if (seen.has(path)) {
      return;
    }
    seen.add(path);
    out.push(path);
  };
  // Always sync the leaf parent — that is the directory whose entry
  // table changed when we renamed the temp file over the live file.
  push(leafParent);
  // Then sync the parent of every newly-created directory (where its own
  // directory entry now lives), walking outward from leaf to root.
  for (const dir of created) {
    push(dirname(dir));
  }
  return out;
}

/**
 * Drop every record in `current` whose {@link Task.id} matches `incoming`,
 * then append `incoming` once.
 *
 * Filtering (rather than in-place replace) means that if the on-disk store
 * ever ends up with multiple records sharing one id — manual edit, file
 * corruption, or a legacy multi-writer history — a single save converges
 * the array back to one record per id rather than rewriting every duplicate
 * with the new payload. Returns a new array; `current` is not mutated.
 */
function upsertTask(current: readonly Task[], incoming: Task): Task[] {
  const next = current.filter((existing) => existing.id !== incoming.id);
  next.push(incoming);
  return next;
}

/**
 * Write `bytes` to `file` until the buffer is fully drained.
 *
 * `Deno.FsFile.write` may write fewer bytes than requested; the loop
 * advances through the buffer and re-issues writes until everything has
 * been accepted. Mirrors `@std/io` `writeAll` without pulling the import.
 */
async function writeAll(file: Deno.FsFile, bytes: Uint8Array): Promise<void> {
  let offset = 0;
  while (offset < bytes.byteLength) {
    const written = await file.write(bytes.subarray(offset));
    if (written <= 0) {
      throw new Error(
        `persistence: short write to staging file (offset=${offset}, size=${bytes.byteLength})`,
      );
    }
    offset += written;
  }
}

/** Sentinel no-op used by the write chain to swallow unhandled rejections. */
function noop(): void {
  // Intentionally empty.
}

/**
 * Set of required {@link Task} field names checked by {@link coerceTask}.
 *
 * We deliberately enforce the persistence-relevant minimum — every field
 * the supervisor reads at restore time must be present and well-typed.
 * Optional fields (`prNumber`, `branchName`, `worktreePath`, `sessionId`,
 * `lastReviewAtIso`, `stabilizePhase`, `terminalReason`) are not checked
 * here; they are validated by the consumers that actually read them.
 */
const REQUIRED_TASK_FIELDS = [
  "id",
  "repo",
  "issueNumber",
  "state",
  "mergeMode",
  "model",
  "iterationCount",
  "createdAtIso",
  "updatedAtIso",
] as const satisfies readonly (keyof Task)[];

/**
 * Validate that `entry` matches the {@link Task} shape closely enough to
 * be handed to the daemon as a `Task`.
 *
 * The check is intentionally narrow — we verify presence and primitive
 * type of every required persistence field, plus the discriminant unions
 * (`state`, `mergeMode`) that the supervisor switches on. We do not
 * re-validate brand invariants (`makeTaskId` etc.); a value that survived
 * a previous serialization must have already passed those.
 *
 * @throws If `entry` is not an object, is missing a required field, or
 *   carries the wrong type for one. The error names the live path and
 *   element index so an operator can locate the bad record with `jq`.
 */
function coerceTask(entry: unknown, index: number, livePath: string): Task {
  if (entry === null || typeof entry !== "object" || Array.isArray(entry)) {
    throw new Error(
      `persistence: ${livePath}[${index}] is not a Task object`,
    );
  }
  const record = entry as Record<string, unknown>;
  for (const field of REQUIRED_TASK_FIELDS) {
    if (!(field in record)) {
      throw new Error(
        `persistence: ${livePath}[${index}] is missing required field "${field}"`,
      );
    }
  }
  if (typeof record.id !== "string" || record.id.length === 0) {
    throw new Error(
      `persistence: ${livePath}[${index}].id is not a non-empty string`,
    );
  }
  if (typeof record.repo !== "string" || record.repo.length === 0) {
    throw new Error(
      `persistence: ${livePath}[${index}].repo is not a non-empty string`,
    );
  }
  if (typeof record.issueNumber !== "number" || !Number.isInteger(record.issueNumber)) {
    throw new Error(
      `persistence: ${livePath}[${index}].issueNumber is not an integer`,
    );
  }
  if (typeof record.iterationCount !== "number" || !Number.isInteger(record.iterationCount)) {
    throw new Error(
      `persistence: ${livePath}[${index}].iterationCount is not an integer`,
    );
  }
  if (typeof record.model !== "string") {
    throw new Error(
      `persistence: ${livePath}[${index}].model is not a string`,
    );
  }
  if (typeof record.createdAtIso !== "string") {
    throw new Error(
      `persistence: ${livePath}[${index}].createdAtIso is not a string`,
    );
  }
  if (typeof record.updatedAtIso !== "string") {
    throw new Error(
      `persistence: ${livePath}[${index}].updatedAtIso is not a string`,
    );
  }
  if (
    typeof record.state !== "string" ||
    !(TASK_STATES as readonly string[]).includes(record.state)
  ) {
    throw new Error(
      `persistence: ${livePath}[${index}].state ${JSON.stringify(record.state)} is not a TaskState`,
    );
  }
  if (
    typeof record.mergeMode !== "string" ||
    !(MERGE_MODES as readonly string[]).includes(record.mergeMode)
  ) {
    throw new Error(
      `persistence: ${livePath}[${index}].mergeMode ${
        JSON.stringify(record.mergeMode)
      } is not a MergeMode`,
    );
  }
  return record as unknown as Task;
}
