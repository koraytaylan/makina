/**
 * Integration tests for `src/daemon/persistence.ts`.
 *
 * Each test scopes its on-disk state to a fresh `Deno.makeTempDir`
 * directory; nothing leaks between cases. The tests cover:
 *
 *  - Round-trip equivalence: every saved task reappears verbatim from
 *    `loadAll`.
 *  - Concurrent saves serialize through the internal mutex (so the final
 *    file holds every record, none are dropped).
 *  - A leftover `<path>.tmp` from a simulated crash is ignored on startup
 *    and removed on the next successful save.
 *  - `loadAll` on a missing file returns an empty array.
 *  - `save` creates parent directories on demand.
 *  - `remove` deletes a single record without touching siblings.
 *  - Corrupt JSON in the live file surfaces an error rather than being
 *    silently dropped (no data loss without warning).
 */

import { assertEquals, assertRejects } from "@std/assert";
import { dirname, join } from "@std/path";
import { exists } from "@std/fs";

import { createPersistence } from "../../src/daemon/persistence.ts";
import {
  makeIssueNumber,
  makeRepoFullName,
  makeTaskId,
  type Task,
  type TaskId,
} from "../../src/types.ts";

/**
 * Build a minimal valid {@link Task} fixture. Only fields that are
 * meaningful to the test need to be overridden.
 */
function makeTask(overrides: Partial<Task> & Pick<Task, "id">): Task {
  return {
    repo: makeRepoFullName("koraytaylan/makina"),
    issueNumber: makeIssueNumber(7),
    state: "INIT",
    mergeMode: "squash",
    model: "claude-sonnet-4-6",
    iterationCount: 0,
    createdAtIso: "2026-04-26T12:00:00.000Z",
    updatedAtIso: "2026-04-26T12:00:00.000Z",
    ...overrides,
  };
}

/**
 * Allocate a temp dir and the path to its `state.json`. The directory
 * itself is removed at the end of every test through `using` cleanup.
 */
async function makeTempStorePath(): Promise<{
  readonly path: string;
  readonly cleanup: () => Promise<void>;
}> {
  const dir = await Deno.makeTempDir({ prefix: "makina-persistence-" });
  return {
    path: join(dir, "state.json"),
    cleanup: () => Deno.remove(dir, { recursive: true }),
  };
}

Deno.test("loadAll on missing file returns an empty array", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    const tasks = await persistence.loadAll();
    assertEquals(tasks, []);
  } finally {
    await cleanup();
  }
});

Deno.test("save then loadAll round-trips a single task verbatim", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    const task = makeTask({
      id: makeTaskId("task_round_trip_1"),
      iterationCount: 3,
      branchName: "makina/issue-7",
    });
    await persistence.save(task);
    const tasks = await persistence.loadAll();
    assertEquals(tasks.length, 1);
    assertEquals(tasks[0], task);
  } finally {
    await cleanup();
  }
});

Deno.test("save replaces a task with the same id (upsert)", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    const id = makeTaskId("task_upsert");
    await persistence.save(makeTask({ id, state: "INIT", iterationCount: 0 }));
    await persistence.save(makeTask({
      id,
      state: "DRAFTING",
      iterationCount: 1,
      updatedAtIso: "2026-04-26T12:01:00.000Z",
    }));
    const tasks = await persistence.loadAll();
    assertEquals(tasks.length, 1);
    assertEquals(tasks[0]?.state, "DRAFTING");
    assertEquals(tasks[0]?.iterationCount, 1);
  } finally {
    await cleanup();
  }
});

Deno.test("save converges duplicate ids in the on-disk store to one record", async () => {
  // Defence-in-depth: if the live file ever contains multiple records for
  // the same id (manual edit, corruption, legacy multi-writer history), a
  // single save must collapse them back to one entry — not rewrite every
  // duplicate with the new payload.
  const { path, cleanup } = await makeTempStorePath();
  try {
    await Deno.mkdir(dirname(path), { recursive: true });
    const id = makeTaskId("task_dup");
    const seeded = [
      makeTask({ id, state: "INIT", iterationCount: 0 }),
      makeTask({ id, state: "INIT", iterationCount: 0 }),
      makeTask({ id, state: "INIT", iterationCount: 0 }),
    ];
    await Deno.writeTextFile(path, JSON.stringify(seeded));
    const persistence = createPersistence({ path });
    await persistence.save(makeTask({
      id,
      state: "DRAFTING",
      iterationCount: 1,
      updatedAtIso: "2026-04-26T12:01:00.000Z",
    }));
    const tasks = await persistence.loadAll();
    assertEquals(tasks.length, 1);
    assertEquals(tasks[0]?.id, "task_dup");
    assertEquals(tasks[0]?.state, "DRAFTING");
    assertEquals(tasks[0]?.iterationCount, 1);
  } finally {
    await cleanup();
  }
});

Deno.test("save preserves siblings when adding a new task", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    const a = makeTask({ id: makeTaskId("task_a") });
    const b = makeTask({ id: makeTaskId("task_b") });
    await persistence.save(a);
    await persistence.save(b);
    const tasks = await persistence.loadAll();
    assertEquals(tasks.map((t) => t.id), ["task_a", "task_b"]);
  } finally {
    await cleanup();
  }
});

Deno.test("remove deletes a single record without touching siblings", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    const a = makeTask({ id: makeTaskId("task_a") });
    const b = makeTask({ id: makeTaskId("task_b") });
    const c = makeTask({ id: makeTaskId("task_c") });
    await persistence.save(a);
    await persistence.save(b);
    await persistence.save(c);
    await persistence.remove(b.id);
    const tasks = await persistence.loadAll();
    assertEquals(tasks.map((t) => t.id), ["task_a", "task_c"]);
  } finally {
    await cleanup();
  }
});

Deno.test("remove on an unknown id is a no-op (does not error)", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    await persistence.save(makeTask({ id: makeTaskId("task_alive") }));
    await persistence.remove(makeTaskId("task_ghost"));
    const tasks = await persistence.loadAll();
    assertEquals(tasks.length, 1);
    assertEquals(tasks[0]?.id, "task_alive");
  } finally {
    await cleanup();
  }
});

Deno.test("remove on a missing file is a no-op", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    await persistence.remove(makeTaskId("task_nothing"));
    const tasks = await persistence.loadAll();
    assertEquals(tasks, []);
  } finally {
    await cleanup();
  }
});

Deno.test("concurrent saves are serialized — every record survives", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    const ids: TaskId[] = [];
    const promises: Promise<void>[] = [];
    // Fire 16 saves with distinct ids without awaiting between them. If the
    // mutex is broken at least one rename will land on top of another's
    // read-modify-write window and we will see fewer than 16 records.
    for (let index = 0; index < 16; index += 1) {
      const id = makeTaskId(`task_concurrent_${index}`);
      ids.push(id);
      promises.push(persistence.save(makeTask({ id, iterationCount: index })));
    }
    await Promise.all(promises);
    const tasks = await persistence.loadAll();
    const seenIds = new Set(tasks.map((t) => t.id));
    assertEquals(seenIds.size, ids.length);
    for (const id of ids) {
      assertEquals(seenIds.has(id), true, `missing ${id}`);
    }
  } finally {
    await cleanup();
  }
});

Deno.test("save creates parent directories on demand", async () => {
  const dir = await Deno.makeTempDir({ prefix: "makina-persistence-mkdir-" });
  const path = join(dir, "nested", "deeper", "state.json");
  try {
    const persistence = createPersistence({ path });
    await persistence.save(makeTask({ id: makeTaskId("task_nested") }));
    // The file (and therefore its parent chain) must exist.
    assertEquals(await exists(path, { isFile: true }), true);
    assertEquals(await exists(dirname(path), { isDirectory: true }), true);
    const tasks = await persistence.loadAll();
    assertEquals(tasks.length, 1);
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test("a leftover .tmp from a crashed write is ignored on startup", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    // Pre-populate the live file with one valid task.
    await Deno.mkdir(dirname(path), { recursive: true });
    const survived = makeTask({ id: makeTaskId("task_survived"), iterationCount: 1 });
    await Deno.writeTextFile(path, JSON.stringify([survived]));
    // Drop a corrupt `.tmp` next to it as if a previous process had crashed
    // mid-write — the bytes are not even valid JSON.
    const tempPath = `${path}.tmp`;
    await Deno.writeTextFile(tempPath, "{partial garbage from a crashed write");
    // A fresh persistence reads the live file and never touches the orphan.
    const persistence = createPersistence({ path });
    const tasks = await persistence.loadAll();
    assertEquals(tasks.length, 1);
    assertEquals(tasks[0]?.id, "task_survived");
    // The next successful save replaces the orphan with our own bytes — no
    // garbage left behind.
    await persistence.save(makeTask({ id: makeTaskId("task_new") }));
    assertEquals(await exists(tempPath), false);
    const after = await persistence.loadAll();
    assertEquals(after.length, 2);
  } finally {
    await cleanup();
  }
});

Deno.test("loadAll surfaces a corrupt live file rather than silently dropping data", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    await Deno.mkdir(dirname(path), { recursive: true });
    // Live file claims to be JSON but is truncated mid-array. We must not
    // pretend the store is empty — that would lose every recorded task.
    await Deno.writeTextFile(path, '[{"id":"task_x"');
    const persistence = createPersistence({ path });
    await assertRejects(
      () => persistence.loadAll(),
      Error,
      "is not valid JSON",
    );
  } finally {
    await cleanup();
  }
});

Deno.test("loadAll rejects an array element that is not a Task object", async () => {
  // Per-element validation exists so a corrupt or hand-edited store can't
  // hand the supervisor garbage typed as `Task`. A primitive in the array
  // must be rejected, not silently cast.
  const { path, cleanup } = await makeTempStorePath();
  try {
    await Deno.mkdir(dirname(path), { recursive: true });
    await Deno.writeTextFile(path, JSON.stringify(["not-a-task"]));
    const persistence = createPersistence({ path });
    await assertRejects(
      () => persistence.loadAll(),
      Error,
      "is not a Task object",
    );
  } finally {
    await cleanup();
  }
});

Deno.test("loadAll rejects an array element missing required Task fields", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    await Deno.mkdir(dirname(path), { recursive: true });
    await Deno.writeTextFile(path, JSON.stringify([{ id: "task_partial" }]));
    const persistence = createPersistence({ path });
    await assertRejects(
      () => persistence.loadAll(),
      Error,
      "missing required field",
    );
  } finally {
    await cleanup();
  }
});

Deno.test("loadAll rejects an element with the wrong type for a required field", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    await Deno.mkdir(dirname(path), { recursive: true });
    const malformed = {
      id: "task_typed_wrong",
      repo: "koraytaylan/makina",
      issueNumber: "seven",
      state: "INIT",
      mergeMode: "squash",
      model: "claude-sonnet-4-6",
      iterationCount: 0,
      createdAtIso: "2026-04-26T12:00:00.000Z",
      updatedAtIso: "2026-04-26T12:00:00.000Z",
    };
    await Deno.writeTextFile(path, JSON.stringify([malformed]));
    const persistence = createPersistence({ path });
    await assertRejects(
      () => persistence.loadAll(),
      Error,
      "issueNumber is not an integer",
    );
  } finally {
    await cleanup();
  }
});

Deno.test("loadAll rejects an element with an unknown TaskState", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    await Deno.mkdir(dirname(path), { recursive: true });
    const malformed = {
      id: "task_unknown_state",
      repo: "koraytaylan/makina",
      issueNumber: 7,
      state: "GLITCHED",
      mergeMode: "squash",
      model: "claude-sonnet-4-6",
      iterationCount: 0,
      createdAtIso: "2026-04-26T12:00:00.000Z",
      updatedAtIso: "2026-04-26T12:00:00.000Z",
    };
    await Deno.writeTextFile(path, JSON.stringify([malformed]));
    const persistence = createPersistence({ path });
    await assertRejects(
      () => persistence.loadAll(),
      Error,
      "is not a TaskState",
    );
  } finally {
    await cleanup();
  }
});

Deno.test("loadAll surfaces non-array root rather than silently coercing", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    await Deno.mkdir(dirname(path), { recursive: true });
    await Deno.writeTextFile(path, JSON.stringify({ tasks: [] }));
    const persistence = createPersistence({ path });
    await assertRejects(
      () => persistence.loadAll(),
      Error,
      "root is not an array",
    );
  } finally {
    await cleanup();
  }
});

Deno.test("save bubbles up corrupt-store errors instead of overwriting them", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    await Deno.mkdir(dirname(path), { recursive: true });
    await Deno.writeTextFile(path, "not json at all");
    const persistence = createPersistence({ path });
    await assertRejects(
      () => persistence.save(makeTask({ id: makeTaskId("task_blocked") })),
      Error,
      "is not valid JSON",
    );
  } finally {
    await cleanup();
  }
});

Deno.test("loadAll on a zero-byte live file returns an empty array", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    await Deno.mkdir(dirname(path), { recursive: true });
    await Deno.writeTextFile(path, "");
    const persistence = createPersistence({ path });
    const tasks = await persistence.loadAll();
    assertEquals(tasks, []);
  } finally {
    await cleanup();
  }
});

Deno.test("the live file is rewritten atomically — no .tmp lingers after a save", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    await persistence.save(makeTask({ id: makeTaskId("task_atomic") }));
    assertEquals(await exists(`${path}.tmp`), false);
    assertEquals(await exists(path, { isFile: true }), true);
  } finally {
    await cleanup();
  }
});

Deno.test("a failed save does not poison subsequent saves", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    await Deno.mkdir(dirname(path), { recursive: true });
    await Deno.writeTextFile(path, "this is not json");
    const persistence = createPersistence({ path });
    // First save fails because the existing file is not valid JSON.
    await assertRejects(
      () => persistence.save(makeTask({ id: makeTaskId("task_fail") })),
      Error,
    );
    // Repair the file by overwriting with an empty array; the next save
    // must succeed — the mutex chain must not be in a permanently rejected
    // state.
    await Deno.writeTextFile(path, "[]");
    await persistence.save(makeTask({ id: makeTaskId("task_recover") }));
    const tasks = await persistence.loadAll();
    assertEquals(tasks.length, 1);
    assertEquals(tasks[0]?.id, "task_recover");
  } finally {
    await cleanup();
  }
});

Deno.test("two persistence instances pointed at the same path see each other's writes", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const writer = createPersistence({ path });
    const reader = createPersistence({ path });
    await writer.save(makeTask({ id: makeTaskId("task_shared") }));
    const tasks = await reader.loadAll();
    assertEquals(tasks.length, 1);
    assertEquals(tasks[0]?.id, "task_shared");
  } finally {
    await cleanup();
  }
});

Deno.test("save persists every Task field including optional ones", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    const task: Task = {
      id: makeTaskId("task_full"),
      repo: makeRepoFullName("koraytaylan/makina"),
      issueNumber: makeIssueNumber(42),
      state: "STABILIZING",
      stabilizePhase: "CI",
      mergeMode: "rebase",
      model: "claude-opus-4-7",
      iterationCount: 5,
      prNumber: makeIssueNumber(101),
      branchName: "makina/issue-42",
      worktreePath: "/tmp/wt/42",
      sessionId: "session-abc",
      lastReviewAtIso: "2026-04-26T12:00:00.000Z",
      createdAtIso: "2026-04-26T11:00:00.000Z",
      updatedAtIso: "2026-04-26T12:00:00.000Z",
    };
    await persistence.save(task);
    const tasks = await persistence.loadAll();
    assertEquals(tasks.length, 1);
    assertEquals(tasks[0], task);
  } finally {
    await cleanup();
  }
});

Deno.test("after save the live file holds the new bytes (read postcondition)", async () => {
  // This pins the round-trip-via-disk invariant: after `save` resolves,
  // an external `Deno.readTextFile` on the live path must surface the
  // serialized payload. (We cannot observe `fdatasync` from user space —
  // see the spy-based test below for that — but we can prove the rename
  // landed and the bytes are JSON-decodable.)
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    const task = makeTask({ id: makeTaskId("task_postcondition"), iterationCount: 99 });
    await persistence.save(task);
    const raw = await Deno.readTextFile(path);
    const parsed = JSON.parse(raw) as readonly Task[];
    assertEquals(parsed.length, 1);
    assertEquals(parsed[0]?.iterationCount, 99);
  } finally {
    await cleanup();
  }
});

Deno.test("save fsyncs the staged temp file and the parent directory", async () => {
  // The page-cache-immediate-read trick won't catch a missing `syncData`,
  // so we inject a spy `open` into the persistence factory and record
  // every `syncData` invocation. After one save we expect both the temp
  // file (`<path>.tmp`) and the parent directory to have been synced.
  //
  // Spy is per-instance — `Deno.open` is never mutated process-wide, so
  // this test is safe under `deno test --parallel`.
  const { path, cleanup } = await makeTempStorePath();
  try {
    const synced: string[] = [];
    const spyOpen: typeof Deno.open = async (
      target: string | URL,
      options?: Deno.OpenOptions,
    ) => {
      const file = await Deno.open(target, options);
      const targetPath = typeof target === "string" ? target : target.pathname;
      const originalSyncData = file.syncData.bind(file);
      file.syncData = async () => {
        synced.push(targetPath);
        await originalSyncData();
      };
      return file;
    };
    const persistence = createPersistence({ path, open: spyOpen });
    await persistence.save(makeTask({ id: makeTaskId("task_fsync_spy") }));
    // Both the staged temp file and the parent directory must have been
    // syncData()'d. Without the parent-dir sync, a power loss after the
    // rename could lose the directory entry update on POSIX filesystems.
    const tempPath = `${path}.tmp`;
    const parentDir = dirname(path);
    assertEquals(
      synced.includes(tempPath),
      true,
      `expected fsync on ${tempPath}, saw ${JSON.stringify(synced)}`,
    );
    assertEquals(
      synced.includes(parentDir),
      true,
      `expected fsync on ${parentDir}, saw ${JSON.stringify(synced)}`,
    );
  } finally {
    await cleanup();
  }
});

Deno.test("loadAll returns a frozen result", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    await persistence.save(makeTask({ id: makeTaskId("task_freeze") }));
    const tasks = await persistence.loadAll();
    assertEquals(Object.isFrozen(tasks), true);
  } finally {
    await cleanup();
  }
});

Deno.test("the same instance can interleave save and remove correctly", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    const a = makeTask({ id: makeTaskId("task_a") });
    const b = makeTask({ id: makeTaskId("task_b") });
    const c = makeTask({ id: makeTaskId("task_c") });
    // Issue every operation without awaiting; the mutex must apply them
    // in submission order.
    const operations = [
      persistence.save(a),
      persistence.save(b),
      persistence.remove(a.id),
      persistence.save(c),
      persistence.remove(b.id),
    ];
    await Promise.all(operations);
    const tasks = await persistence.loadAll();
    assertEquals(tasks.map((t) => t.id), ["task_c"]);
  } finally {
    await cleanup();
  }
});

Deno.test("two new ids are not collapsed even when the second arrives first to the rename", async () => {
  // Concurrent inserts of two distinct ids must both survive the
  // serialization, in submission order. (Regression guard for a buggy
  // mutex that drops the first writer's payload by reading the live file
  // before it lands.)
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    const first = persistence.save(makeTask({ id: makeTaskId("task_first") }));
    const second = persistence.save(makeTask({ id: makeTaskId("task_second") }));
    await Promise.all([first, second]);
    const tasks = await persistence.loadAll();
    assertEquals(tasks.map((t) => t.id), ["task_first", "task_second"]);
  } finally {
    await cleanup();
  }
});

Deno.test("save updates updatedAtIso semantics are caller's responsibility (no auto-mutation)", async () => {
  // Persistence does not stamp timestamps — the supervisor owns that. This
  // pins the invariant so we do not accidentally introduce auto-mutation.
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    const id = makeTaskId("task_stamp");
    const updatedAtIso = "2030-01-01T00:00:00.000Z";
    await persistence.save(makeTask({ id, updatedAtIso }));
    const tasks = await persistence.loadAll();
    assertEquals(tasks[0]?.updatedAtIso, updatedAtIso);
  } finally {
    await cleanup();
  }
});

Deno.test("loading an empty store after every record is removed yields []", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    const id = makeTaskId("task_lone");
    await persistence.save(makeTask({ id }));
    await persistence.remove(id);
    const tasks = await persistence.loadAll();
    assertEquals(tasks, []);
  } finally {
    await cleanup();
  }
});

Deno.test("remove returns ordering from disk after subsequent saves (no stale snapshot)", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    const a = makeTask({ id: makeTaskId("task_a") });
    const b = makeTask({ id: makeTaskId("task_b") });
    await persistence.save(a);
    await persistence.save(b);
    await persistence.remove(a.id);
    await persistence.save(makeTask({ id: makeTaskId("task_c") }));
    const tasks = await persistence.loadAll();
    assertEquals(tasks.map((t) => t.id), ["task_b", "task_c"]);
    // Tasks should round-trip equal even after the rewrite.
    assertEquals(tasks[0]?.id, "task_b");
    // The removed record really is gone — not merely "not equal to b".
    assertEquals(tasks.find((t) => t.id === "task_a"), undefined);
  } finally {
    await cleanup();
  }
});

Deno.test("save bubbles up corrupt non-array root rather than overwriting", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    await Deno.mkdir(dirname(path), { recursive: true });
    await Deno.writeTextFile(path, JSON.stringify({ a: 1 }));
    const persistence = createPersistence({ path });
    await assertRejects(
      () => persistence.save(makeTask({ id: makeTaskId("task_blocked") })),
      Error,
      "root is not an array",
    );
  } finally {
    await cleanup();
  }
});

Deno.test("save against a zero-byte existing file proceeds (treats it as empty)", async () => {
  const { path, cleanup } = await makeTempStorePath();
  try {
    await Deno.mkdir(dirname(path), { recursive: true });
    await Deno.writeTextFile(path, "");
    const persistence = createPersistence({ path });
    await persistence.save(makeTask({ id: makeTaskId("task_zero") }));
    const tasks = await persistence.loadAll();
    assertEquals(tasks.length, 1);
    assertEquals(tasks[0]?.id, "task_zero");
  } finally {
    await cleanup();
  }
});

Deno.test("loadAll re-throws non-NotFound IO errors instead of swallowing them", async () => {
  // Pointing the live file at a directory triggers an IO error on read
  // that is **not** NotFound; the load path must surface it rather than
  // pretend the store is empty.
  const dir = await Deno.makeTempDir({ prefix: "makina-persistence-iofail-" });
  try {
    const persistence = createPersistence({ path: dir });
    await assertRejects(
      () => persistence.loadAll(),
      Error,
    );
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test("loadAll waits for an in-flight failing save and rejects on the corrupt JSON it observes", async () => {
  // Regression guard for the catch-all in waitForChain: a failed mutation
  // must not poison a concurrent loadAll via the shared chain. The load
  // queues behind the failing save, then runs its own read against the
  // still-corrupt live file; it must reject on its own JSON parse rather
  // than on the swallowed save failure.
  const { path, cleanup } = await makeTempStorePath();
  try {
    await Deno.mkdir(dirname(path), { recursive: true });
    await Deno.writeTextFile(path, "this is not json");
    const persistence = createPersistence({ path });
    const failingSave = persistence.save(makeTask({ id: makeTaskId("task_fail") }));
    // Kick off the load before awaiting the save — both share the chain.
    const loadPromise = persistence.loadAll();
    await assertRejects(() => failingSave, Error);
    // The load itself rejects (because the file is still corrupt) but must
    // do so via its own JSON parse, not via the swallowed save failure.
    await assertRejects(() => loadPromise, Error, "is not valid JSON");
  } finally {
    await cleanup();
  }
});

Deno.test("save against a zero-byte existing file uses empty-store path", async () => {
  // Mirrors the zero-byte branch in readForMutation: a manually truncated
  // live file must allow the next save to overwrite cleanly.
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    await persistence.save(makeTask({ id: makeTaskId("task_a") }));
    // Overwrite the live file with a zero-byte file mid-flight, then save.
    await Deno.writeTextFile(path, "");
    await persistence.save(makeTask({ id: makeTaskId("task_b") }));
    const tasks = await persistence.loadAll();
    assertEquals(tasks.map((t) => t.id), ["task_b"]);
  } finally {
    await cleanup();
  }
});

Deno.test("save fsyncs every newly-created directory in a deep parent chain", async () => {
  // Regression guard for the durability of the directory chain itself.
  // When `writeAtomic` has to create multiple intermediate directories
  // (e.g., `<tempdir>/a/b/c/state.json` where only `<tempdir>` exists),
  // the new sub-directory entries live in their parents' dirty page
  // caches. A power loss after the rename can lose the entire chain even
  // though the file's data is on disk. The fix fsyncs every directory
  // whose entry table changed: the leaf parent (where the rename
  // committed) and the parent of every directory we just created (where
  // each new directory's entry was committed).
  //
  // Spy is per-instance — `Deno.open` is never mutated process-wide, so
  // this test is safe under `deno test --parallel`.
  const root = await Deno.makeTempDir({ prefix: "makina-persistence-deep-" });
  const path = join(root, "a", "b", "c", "state.json");
  try {
    const synced: string[] = [];
    const spyOpen: typeof Deno.open = async (
      target: string | URL,
      options?: Deno.OpenOptions,
    ) => {
      const file = await Deno.open(target, options);
      const targetPath = typeof target === "string" ? target : target.pathname;
      const originalSyncData = file.syncData.bind(file);
      file.syncData = async () => {
        synced.push(targetPath);
        await originalSyncData();
      };
      return file;
    };
    const persistence = createPersistence({ path, open: spyOpen });
    await persistence.save(makeTask({ id: makeTaskId("task_deep") }));
    // Every directory whose entry table changed must have been
    // syncData()'d:
    //  - <root>/a/b/c (the rename's parent — committed `state.json`)
    //  - <root>/a/b   (committed the new `c/` entry)
    //  - <root>/a     (committed the new `b/` entry)
    //  - <root>       (committed the new `a/` entry)
    const expected = [
      join(root, "a", "b", "c"),
      join(root, "a", "b"),
      join(root, "a"),
      root,
    ];
    for (const dir of expected) {
      assertEquals(
        synced.includes(dir),
        true,
        `expected fsync on ${dir}, saw ${JSON.stringify(synced)}`,
      );
    }
    // The save must of course have actually landed.
    const tasks = await persistence.loadAll();
    assertEquals(tasks.length, 1);
    assertEquals(tasks[0]?.id, "task_deep");
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});

Deno.test("save fsyncs only the leaf parent when the parent chain already exists", async () => {
  // Counterpart to the deep-chain test: when the parent chain is fully
  // present (no `mkdir` calls happen), the only directory whose entry
  // table changed is the leaf parent itself. We must not redundantly
  // fsync ancestors we did not write to — that would be wasted IO.
  const { path, cleanup } = await makeTempStorePath();
  try {
    const synced: string[] = [];
    const spyOpen: typeof Deno.open = async (
      target: string | URL,
      options?: Deno.OpenOptions,
    ) => {
      const file = await Deno.open(target, options);
      const targetPath = typeof target === "string" ? target : target.pathname;
      const originalSyncData = file.syncData.bind(file);
      file.syncData = async () => {
        synced.push(targetPath);
        await originalSyncData();
      };
      return file;
    };
    const persistence = createPersistence({ path, open: spyOpen });
    await persistence.save(makeTask({ id: makeTaskId("task_shallow") }));
    const parent = dirname(path);
    // The leaf parent must be fsynced.
    assertEquals(
      synced.includes(parent),
      true,
      `expected fsync on ${parent}, saw ${JSON.stringify(synced)}`,
    );
    // Every directory we fsynced must be either the temp file or the
    // leaf parent — never an unrelated ancestor.
    const tempPath = `${path}.tmp`;
    for (const target of synced) {
      const allowed = target === tempPath || target === parent;
      assertEquals(
        allowed,
        true,
        `unexpected fsync on ${target}; allowed only ${tempPath} or ${parent}`,
      );
    }
  } finally {
    await cleanup();
  }
});
