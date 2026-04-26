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
 * standard write-tmp + fsync + rename dance:
 *
 *  1. Serialize the current in-memory snapshot to JSON.
 *  2. Open `<path>.tmp` (truncating any leftover from a previous crash).
 *  3. Write the bytes, then `fdatasync` the file descriptor so the kernel
 *     flushes them to disk before the rename returns.
 *  4. Atomically rename `<path>.tmp` over `<path>`.
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
 * @module
 */

import { dirname } from "@std/path";
import { ensureDir } from "@std/fs";

import { type Persistence, type Task, type TaskId } from "../types.ts";

/** Suffix appended to the live path to form the temp file used for atomic writes. */
const TEMP_FILE_SUFFIX = ".tmp";

/** JSON serialization indentation. Two spaces matches `deno fmt` defaults. */
const JSON_INDENT_SPACES = 2;

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
 * @param opts.path Absolute filesystem path of the live store. The
 *   parent directory is created if it does not exist; the file itself is
 *   created on demand.
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
export function createPersistence(opts: { readonly path: string }): Persistence {
  return new AtomicJsonPersistence(opts.path);
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
   */
  constructor(livePath: string) {
    this.livePath = livePath;
    this.tempPath = `${livePath}${TEMP_FILE_SUFFIX}`;
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
   * @throws If the file exists but is not valid JSON or does not match the
   *   expected array-of-tasks shape; this surfaces a corrupt store rather
   *   than silently dropping data.
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
    return Object.freeze(parsed as Task[]);
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
    return parsed as Task[];
  }

  /**
   * Atomically replace the live file with the JSON serialization of
   * `tasks`.
   *
   * Sequence: ensure parent → open `<path>.tmp` (truncate) → write JSON →
   * `fdatasync` the file descriptor → close → rename over the live path.
   * The rename is the commit point.
   */
  private async writeAtomic(tasks: readonly Task[]): Promise<void> {
    await ensureDir(dirname(this.livePath));
    const json = JSON.stringify(tasks, null, JSON_INDENT_SPACES);
    const bytes = new TextEncoder().encode(json);
    const file = await Deno.open(this.tempPath, {
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
  }
}

/**
 * Replace the matching record in `current` with `incoming`, or append
 * `incoming` if no record carries the same {@link Task.id}.
 *
 * Returns a new array; `current` is not mutated.
 */
function upsertTask(current: readonly Task[], incoming: Task): Task[] {
  const next: Task[] = [];
  let replaced = false;
  for (const existing of current) {
    if (existing.id === incoming.id) {
      next.push(incoming);
      replaced = true;
    } else {
      next.push(existing);
    }
  }
  if (!replaced) {
    next.push(incoming);
  }
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
