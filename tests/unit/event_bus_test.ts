/**
 * Unit tests for `src/daemon/event-bus.ts`. Covers:
 *
 *  - Exact-match delivery: a publish to `task:42` reaches a `task:42`
 *    subscriber but **not** a `task:99` subscriber.
 *  - Wildcard fan-out: a publish to `task:42` reaches both `task:42`
 *    subscribers and `*` subscribers.
 *  - Unsubscribe: an unsubscribed handler stops receiving; unsubscribe is
 *    idempotent.
 *  - Backpressure: a slow consumer overflowing the bounded queue drops
 *    further publishes and a single warning is logged per overflow
 *    episode (re-arming after a drain).
 *  - Handler-throws: an exception inside one subscriber's handler is
 *    isolated; sibling subscribers and subsequent publishes survive.
 *  - Construction: invalid `bufferSize` values are rejected.
 */

import { assertEquals, assertStrictEquals, assertThrows } from "@std/assert";
import { delay } from "@std/async";

import { createEventBus } from "../../src/daemon/event-bus.ts";
import { makeTaskId, type TaskEvent, type TaskId } from "../../src/types.ts";
import { EVENT_BUS_DEFAULT_BUFFER_SIZE } from "../../src/constants.ts";

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/** Recording logger that captures every `warn` call. */
function recordingLogger(): { warn: (message: string) => void; messages: string[] } {
  const messages: string[] = [];
  return {
    messages,
    warn(message: string): void {
      messages.push(message);
    },
  };
}

/** Construct a minimal valid {@link TaskEvent} for tests. */
function fakeEvent(
  taskId: TaskId,
  message = "tick",
): TaskEvent {
  return {
    taskId,
    atIso: "2026-04-26T00:00:00.000Z",
    kind: "log",
    data: { level: "info", message },
  };
}

/**
 * Yield enough microtasks for the event-bus pump to drain `events.length`
 * deliveries. The pump uses `await reader.read()`, which resolves on the
 * microtask queue, so a small `delay(0)` is enough to let the pump catch
 * up. We optionally take a `count` for sanity in flooding tests.
 */
async function flush(): Promise<void> {
  // Three turns of the microtask + macrotask cycle is enough for any
  // realistic burst the tests publish (under 1000 events). `delay(0)`
  // schedules a `setTimeout(0)` macrotask, which always lands after every
  // microtask the previous turn enqueued.
  await delay(0);
  await delay(0);
  await delay(0);
}

// ---------------------------------------------------------------------------
// Exact-match delivery
// ---------------------------------------------------------------------------

Deno.test("publish delivers to exact-match subscribers and skips others", async () => {
  const bus = createEventBus({ logger: recordingLogger() });
  const a = makeTaskId("task_a");
  const b = makeTaskId("task_b");
  const aSeen: TaskEvent[] = [];
  const bSeen: TaskEvent[] = [];
  const subA = bus.subscribe(a, (event) => aSeen.push(event));
  const subB = bus.subscribe(b, (event) => bSeen.push(event));

  bus.publish(fakeEvent(a, "to-a"));
  bus.publish(fakeEvent(b, "to-b"));
  bus.publish(fakeEvent(a, "to-a-again"));

  await flush();

  assertEquals(aSeen.length, 2);
  assertEquals(bSeen.length, 1);
  assertEquals((aSeen[0]?.data as { message: string }).message, "to-a");
  assertEquals((aSeen[1]?.data as { message: string }).message, "to-a-again");
  assertEquals((bSeen[0]?.data as { message: string }).message, "to-b");

  subA.unsubscribe();
  subB.unsubscribe();
});

// ---------------------------------------------------------------------------
// Wildcard fan-out
// ---------------------------------------------------------------------------

Deno.test("wildcard subscriber receives events for every task", async () => {
  const bus = createEventBus({ logger: recordingLogger() });
  const a = makeTaskId("task_a");
  const b = makeTaskId("task_b");
  const wildcardSeen: TaskEvent[] = [];
  const aSeen: TaskEvent[] = [];

  const subWild = bus.subscribe("*", (event) => wildcardSeen.push(event));
  const subA = bus.subscribe(a, (event) => aSeen.push(event));

  bus.publish(fakeEvent(a, "first"));
  bus.publish(fakeEvent(b, "second"));
  bus.publish(fakeEvent(a, "third"));

  await flush();

  assertEquals(wildcardSeen.length, 3);
  assertEquals(aSeen.length, 2);
  assertEquals(wildcardSeen.map((e) => (e.data as { message: string }).message), [
    "first",
    "second",
    "third",
  ]);
  assertEquals(aSeen.map((e) => (e.data as { message: string }).message), [
    "first",
    "third",
  ]);

  subWild.unsubscribe();
  subA.unsubscribe();
});

Deno.test("publish to a task reaches both task and wildcard subscribers", async () => {
  const bus = createEventBus({ logger: recordingLogger() });
  const id = makeTaskId("task_42");
  const taskSeen: TaskEvent[] = [];
  const wildcardSeen: TaskEvent[] = [];

  const subTask = bus.subscribe(id, (event) => taskSeen.push(event));
  const subWild = bus.subscribe("*", (event) => wildcardSeen.push(event));

  bus.publish(fakeEvent(id));
  await flush();

  assertEquals(taskSeen.length, 1);
  assertEquals(wildcardSeen.length, 1);
  assertStrictEquals(taskSeen[0]?.taskId, id);
  assertStrictEquals(wildcardSeen[0]?.taskId, id);

  subTask.unsubscribe();
  subWild.unsubscribe();
});

// ---------------------------------------------------------------------------
// Unsubscribe
// ---------------------------------------------------------------------------

Deno.test("unsubscribed handler stops receiving events", async () => {
  const bus = createEventBus({ logger: recordingLogger() });
  const id = makeTaskId("task_x");
  const seen: TaskEvent[] = [];

  const sub = bus.subscribe(id, (event) => seen.push(event));
  bus.publish(fakeEvent(id, "before"));
  await flush();
  assertEquals(seen.length, 1);

  sub.unsubscribe();
  bus.publish(fakeEvent(id, "after"));
  await flush();
  // No new event arrived after unsubscribe.
  assertEquals(seen.length, 1);
});

Deno.test("unsubscribe is idempotent", async () => {
  const bus = createEventBus({ logger: recordingLogger() });
  const id = makeTaskId("task_y");
  const seen: TaskEvent[] = [];

  const sub = bus.subscribe(id, (event) => seen.push(event));
  sub.unsubscribe();
  sub.unsubscribe(); // Second call is a no-op, must not throw.

  bus.publish(fakeEvent(id));
  await flush();
  assertEquals(seen.length, 0);
});

// ---------------------------------------------------------------------------
// Backpressure: bounded queue and slow-consumer drop
// ---------------------------------------------------------------------------

Deno.test("slow consumer drops events past bufferSize and warns once", async () => {
  const logger = recordingLogger();
  // bufferSize: 4 keeps the test fast; the publisher floods with 10
  // events while the handler blocks, so 6 events should drop.
  const bufferSize = 4;
  const bus = createEventBus({ bufferSize, logger });
  const id = makeTaskId("task_flood");

  // The handler blocks on a promise the test resolves later. Because the
  // pump awaits `handler(next.value)` only via the synchronous-throw
  // catch, a Promise-returning handler does NOT pause the pump — the
  // handler signature is sync. So instead we make the handler busy-wait
  // synchronously? No: that would block the pump too tightly. Use a
  // counter that records but never blocks; the slow-consumer scenario
  // we model is "publisher flooded faster than the pump can read", which
  // happens any time more than `bufferSize` events are enqueued before
  // the pump's next microtask turn.

  const seen: TaskEvent[] = [];
  const sub = bus.subscribe(id, (event) => {
    seen.push(event);
  });

  // Synchronous flood: the pump cannot drain between iterations of this
  // loop because we never yield to the microtask queue. So events
  // 0..bufferSize-1 land in the queue, events bufferSize..N-1 drop.
  const totalPublished = 10;
  for (let i = 0; i < totalPublished; i++) {
    bus.publish(fakeEvent(id, `m${i}`));
  }

  await flush();

  // Exactly bufferSize events made it through.
  assertEquals(seen.length, bufferSize, `expected ${bufferSize} delivered, got ${seen.length}`);
  // First bufferSize events are the ones queued; later ones dropped.
  assertEquals(
    seen.map((e) => (e.data as { message: string }).message),
    Array.from({ length: bufferSize }, (_, i) => `m${i}`),
  );

  // Exactly one warning per overflow episode.
  const overflowWarnings = logger.messages.filter((m) => m.includes("queue full"));
  assertEquals(
    overflowWarnings.length,
    1,
    `expected exactly 1 overflow warning, got ${overflowWarnings.length}: ${
      overflowWarnings.join(" | ")
    }`,
  );

  sub.unsubscribe();
});

Deno.test("overflow warning re-arms after a successful drain", async () => {
  const logger = recordingLogger();
  const bufferSize = 2;
  const bus = createEventBus({ bufferSize, logger });
  const id = makeTaskId("task_re_arm");

  const seen: TaskEvent[] = [];
  const sub = bus.subscribe(id, (event) => {
    seen.push(event);
  });

  // First overflow burst.
  for (let i = 0; i < 5; i++) bus.publish(fakeEvent(id, `a${i}`));
  await flush();
  // After the drain the queue is empty and the warning flag is re-armed.
  assertEquals(seen.length, bufferSize); // 2 delivered out of the burst

  // Second overflow burst.
  for (let i = 0; i < 5; i++) bus.publish(fakeEvent(id, `b${i}`));
  await flush();
  assertEquals(seen.length, bufferSize * 2); // 2 more delivered = 4 total

  const overflowWarnings = logger.messages.filter((m) => m.includes("queue full"));
  // One warning per episode = two total.
  assertEquals(
    overflowWarnings.length,
    2,
    `expected 2 overflow warnings (one per episode), got ${overflowWarnings.length}`,
  );

  sub.unsubscribe();
});

Deno.test("non-overflowing publish stream produces no warnings", async () => {
  const logger = recordingLogger();
  const bus = createEventBus({ bufferSize: 8, logger });
  const id = makeTaskId("task_calm");

  const seen: TaskEvent[] = [];
  const sub = bus.subscribe(id, (event) => seen.push(event));

  for (let i = 0; i < 8; i++) bus.publish(fakeEvent(id, `n${i}`));
  await flush();

  assertEquals(seen.length, 8);
  assertEquals(logger.messages.length, 0);

  sub.unsubscribe();
});

// ---------------------------------------------------------------------------
// Handler-throws-don't-kill-the-bus
// ---------------------------------------------------------------------------

Deno.test("handler that throws does not poison the bus", async () => {
  const logger = recordingLogger();
  const bus = createEventBus({ logger });
  const id = makeTaskId("task_throwy");
  const goodSeen: TaskEvent[] = [];
  let throwCount = 0;

  const subThrow = bus.subscribe(id, (_event) => {
    throwCount++;
    throw new Error("handler exploded");
  });
  const subGood = bus.subscribe("*", (event) => goodSeen.push(event));

  bus.publish(fakeEvent(id, "first"));
  bus.publish(fakeEvent(id, "second"));
  bus.publish(fakeEvent(id, "third"));

  await flush();

  // The throwing handler ran for every event…
  assertEquals(throwCount, 3);
  // …and the sibling subscriber kept getting events the whole time.
  assertEquals(goodSeen.length, 3);
  // …and a warning was logged for each throw.
  const handlerWarnings = logger.messages.filter((m) => m.includes("handler threw"));
  assertEquals(handlerWarnings.length, 3);

  subThrow.unsubscribe();
  subGood.unsubscribe();

  // The bus survives a fresh publish after the throwing subscriber leaves.
  const lateSeen: TaskEvent[] = [];
  const subLate = bus.subscribe("*", (event) => lateSeen.push(event));
  bus.publish(fakeEvent(id, "late"));
  await flush();
  assertEquals(lateSeen.length, 1);
  subLate.unsubscribe();
});

Deno.test("async handler whose promise rejects is caught and logged (no unhandled rejection)", async () => {
  const logger = recordingLogger();
  const bus = createEventBus({ logger });
  const id = makeTaskId("task_async_reject");
  const goodSeen: TaskEvent[] = [];

  // TypeScript allows passing an `async` function where `(event) => void`
  // is expected (Promise<void> is assignable to void). The bus must
  // detect the returned Promise and attach a `.catch` so the rejection
  // is funneled through the same warn-and-continue path as sync throws.
  const subAsync = bus.subscribe(
    id,
    (async (_event) => {
      // The `await` makes the throw happen on a microtask boundary, which
      // is the realistic shape of an async handler that does any I/O
      // before failing. Without an `await`, lint's `require-await` would
      // flag this — and the test would still pass thanks to the
      // PromiseLike detection branch — but this is closer to what the
      // bus is actually defending against.
      await Promise.resolve();
      throw new Error("async handler exploded");
    }) as (event: TaskEvent) => void,
  );
  const subGood = bus.subscribe("*", (event) => goodSeen.push(event));

  bus.publish(fakeEvent(id, "first"));
  bus.publish(fakeEvent(id, "second"));

  await flush();

  // Sibling subscriber kept seeing events.
  assertEquals(goodSeen.length, 2);
  // One warn per rejection, carrying the rejection-specific phrase.
  const rejectWarnings = logger.messages.filter((m) => m.includes("handler rejected"));
  assertEquals(
    rejectWarnings.length,
    2,
    `expected 2 rejection warnings, got ${rejectWarnings.length}: ${logger.messages.join(" | ")}`,
  );
  assertEquals(rejectWarnings[0]?.includes("async handler exploded"), true);

  subAsync.unsubscribe();
  subGood.unsubscribe();
});

Deno.test("async handler that resolves cleanly does not log a warning", async () => {
  const logger = recordingLogger();
  const bus = createEventBus({ logger });
  const id = makeTaskId("task_async_ok");
  const seen: TaskEvent[] = [];

  const sub = bus.subscribe(
    id,
    (async (event) => {
      await Promise.resolve();
      seen.push(event);
    }) as (event: TaskEvent) => void,
  );

  bus.publish(fakeEvent(id));
  await flush();

  assertEquals(seen.length, 1);
  assertEquals(logger.messages.length, 0);

  sub.unsubscribe();
});

Deno.test("handler returning a thenable that rejects is treated like an async handler", async () => {
  const logger = recordingLogger();
  const bus = createEventBus({ logger });
  const id = makeTaskId("task_thenable");

  // A bare thenable (not a real Promise) still trips the PromiseLike
  // detection branch, exercising the duck-typed `then`-as-function check
  // in `isPromiseLike` rather than the `instanceof Promise` shortcut.
  const sub = bus.subscribe(
    id,
    (() => {
      return {
        then(_onFulfilled: unknown, onRejected: (reason: unknown) => void): void {
          onRejected(new Error("thenable boom"));
        },
      };
    }) as (event: TaskEvent) => void,
  );

  bus.publish(fakeEvent(id));
  await flush();

  const rejectWarnings = logger.messages.filter((m) => m.includes("handler rejected"));
  assertEquals(rejectWarnings.length, 1);
  assertEquals(rejectWarnings[0]?.includes("thenable boom"), true);

  sub.unsubscribe();
});

Deno.test("handler that throws a non-Error is logged with String() coercion", async () => {
  const logger = recordingLogger();
  const bus = createEventBus({ logger });
  const id = makeTaskId("task_weird_throw");

  const sub = bus.subscribe(id, () => {
    // Throwing a literal exercises the non-Error branch in stringifyError.
    // This is intentional even though `no-throw-literal` would normally
    // flag it — we are deliberately testing the bus's resilience to
    // misbehaving handlers.
    // deno-lint-ignore no-throw-literal
    throw "stringly typed";
  });

  bus.publish(fakeEvent(id));
  await flush();

  const handlerWarnings = logger.messages.filter((m) => m.includes("handler threw"));
  assertEquals(handlerWarnings.length, 1);
  // The warning carries the coerced string rather than `[object Object]`.
  assertEquals(handlerWarnings[0]?.includes("stringly typed"), true);

  sub.unsubscribe();
});

// ---------------------------------------------------------------------------
// Subscribe/unsubscribe during delivery
// ---------------------------------------------------------------------------

Deno.test("subscribe inside a handler does not affect the current publish", async () => {
  const bus = createEventBus({ logger: recordingLogger() });
  const id = makeTaskId("task_dyn");
  const lateSeen: TaskEvent[] = [];

  // The handler subscribes a *new* subscriber the first time it runs.
  // The new subscriber must not retroactively receive the in-flight
  // event but must receive subsequent publishes.
  let lateSub: { unsubscribe(): void } | undefined;
  const sub = bus.subscribe(id, (_event) => {
    if (!lateSub) {
      lateSub = bus.subscribe("*", (event) => lateSeen.push(event));
    }
  });

  bus.publish(fakeEvent(id, "before"));
  await flush();
  // Late subscriber missed the in-flight event.
  assertEquals(lateSeen.length, 0);

  bus.publish(fakeEvent(id, "after"));
  await flush();
  assertEquals(lateSeen.length, 1);
  assertEquals((lateSeen[0]?.data as { message: string }).message, "after");

  sub.unsubscribe();
  lateSub?.unsubscribe();
});

Deno.test("unsubscribe inside a handler stops the subscriber from receiving future publishes", async () => {
  const bus = createEventBus({ logger: recordingLogger() });
  const id = makeTaskId("task_self_unsub");
  let count = 0;

  const sub: { unsubscribe(): void } = bus.subscribe(id, (_event) => {
    count++;
    sub.unsubscribe();
  });

  // First publish — handler will run and self-unsubscribe.
  bus.publish(fakeEvent(id, "first"));
  await flush();
  assertEquals(count, 1, "handler should have run exactly once for the first publish");

  // Second publish happens after the unsubscribe took effect; the
  // subscriber is no longer in the set so the publish skips it. This
  // verifies that unsubscribe takes effect for *new* publishes — events
  // already buffered before unsubscribe still drain (they were committed
  // to the queue at publish time).
  bus.publish(fakeEvent(id, "second"));
  await flush();
  assertEquals(count, 1, "handler must not run for events published after unsubscribe");
});

// ---------------------------------------------------------------------------
// Construction-time validation
// ---------------------------------------------------------------------------

Deno.test("createEventBus rejects non-positive bufferSize", () => {
  assertThrows(
    () => createEventBus({ bufferSize: 0 }),
    RangeError,
    "bufferSize",
  );
  assertThrows(
    () => createEventBus({ bufferSize: -1 }),
    RangeError,
    "bufferSize",
  );
  assertThrows(
    () => createEventBus({ bufferSize: 1.5 }),
    RangeError,
    "bufferSize",
  );
  assertThrows(
    () => createEventBus({ bufferSize: Number.NaN }),
    RangeError,
    "bufferSize",
  );
});

Deno.test("createEventBus uses EVENT_BUS_DEFAULT_BUFFER_SIZE when bufferSize omitted", async () => {
  const logger = recordingLogger();
  const bus = createEventBus({ logger });
  const id = makeTaskId("task_default");
  const seen: TaskEvent[] = [];
  const sub = bus.subscribe(id, (event) => seen.push(event));

  // Publishing exactly the default capacity must not warn.
  for (let i = 0; i < EVENT_BUS_DEFAULT_BUFFER_SIZE; i++) {
    bus.publish(fakeEvent(id, `c${i}`));
  }
  await flush();
  assertEquals(seen.length, EVENT_BUS_DEFAULT_BUFFER_SIZE);
  assertEquals(logger.messages.length, 0);

  sub.unsubscribe();
});

Deno.test("createEventBus with no options uses the default @std/log logger", async () => {
  // Exercises the `defaultLogger()` factory branch so coverage reflects
  // it. We can't easily assert on the global logger's output without
  // reaching into the std/log registry, but we can prove the bus runs
  // end-to-end without injection — any uncaught exception inside the
  // adapter would propagate out of `createEventBus` or the publish path.
  const bus = createEventBus();
  const id = makeTaskId("task_no_opts");
  const seen: TaskEvent[] = [];
  const sub = bus.subscribe(id, (event) => seen.push(event));
  bus.publish(fakeEvent(id, "no-opts"));
  await flush();
  assertEquals(seen.length, 1);
  sub.unsubscribe();
});

Deno.test("publish after unsubscribe is a silent no-op (does not throw)", async () => {
  const bus = createEventBus({ logger: recordingLogger() });
  const id = makeTaskId("task_post_unsub");
  const seen: TaskEvent[] = [];
  const sub = bus.subscribe(id, (event) => seen.push(event));
  sub.unsubscribe();
  // After unsubscribe, the subscriber is removed from the active set, so
  // `publish` skips it. The handler must not run and the publish must
  // not throw.
  bus.publish(fakeEvent(id));
  await flush();
  assertEquals(seen.length, 0);
});
