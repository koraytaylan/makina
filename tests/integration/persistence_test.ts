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

import { assertEquals, assertNotEquals, assertRejects } from "@std/assert";
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

Deno.test("save fsyncs the staged bytes before the rename commits", async () => {
  // We cannot directly observe `fdatasync` from user space, but we can
  // observe its postcondition: after `save` returns, an immediate read of
  // the live file must surface the bytes we just wrote (no dirty-page
  // deferred-flush window where another reader sees stale content).
  const { path, cleanup } = await makeTempStorePath();
  try {
    const persistence = createPersistence({ path });
    const task = makeTask({ id: makeTaskId("task_fsync"), iterationCount: 99 });
    await persistence.save(task);
    const raw = await Deno.readTextFile(path);
    const parsed = JSON.parse(raw) as readonly Task[];
    assertEquals(parsed.length, 1);
    assertEquals(parsed[0]?.iterationCount, 99);
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
    assertNotEquals(tasks.find((t) => t.id === "task_a"), b);
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

Deno.test("loadAll waits for an in-flight failed save without itself rejecting", async () => {
  // Regression guard for the catch-all in waitForChain: a failed mutation
  // must not poison a concurrent loadAll. The load arrives before the
  // failure has been observed by anybody else.
  const { path, cleanup } = await makeTempStorePath();
  try {
    await Deno.mkdir(dirname(path), { recursive: true });
    await Deno.writeTextFile(path, "this is not json");
    const persistence = createPersistence({ path });
    const failingSave = persistence.save(makeTask({ id: makeTaskId("task_fail") }));
    // Kick off the load before awaiting the save — both share the chain.
    const loadPromise = persistence.loadAll();
    await assertRejects(() => failingSave, Error);
    // The load itself must reject (because the file is still corrupt) but
    // must do so via its own JSON parse, not via the swallowed save
    // failure.
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
