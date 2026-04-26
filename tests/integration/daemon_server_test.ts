/**
 * Integration tests for `src/daemon/server.ts`. Boots the server on a
 * `Deno.makeTempDir`-backed Unix socket, opens a real client over
 * `Deno.connect`, and round-trips encoded {@link MessageEnvelope}s.
 *
 * Coverage targets:
 *  - `ping → pong` round trip (with custom `daemonVersion`).
 *  - `subscribe { target: "*" }` → event fan-out for a published
 *    {@link TaskEvent}; the client receives a matching `event` envelope.
 *  - Stale socket cleanup: a non-listening file at the socket path is
 *    removed before {@link Deno.listen} runs, and an `AddrInUse` is
 *    raised when the socket is still owned by a live peer.
 *  - Broken pipe: a TUI that closes mid-write does not kill the daemon;
 *    a follow-up client can still round-trip.
 *  - Custom handler dispatch (`command`, `prompt`, `unsubscribe`).
 *  - Unknown subscriptions: `unsubscribe` for an unregistered id.
 *  - Daemon-only message types (`pong`, `event`, `ack`) sent by a client
 *    are rejected with an `ack`.
 *  - Custom handler errors are translated into `ack { ok: false }`.
 */

import { assertEquals, assertNotEquals, assertRejects } from "@std/assert";

import { decode, encode } from "../../src/ipc/codec.ts";
import { type AckPayload, type MessageEnvelope, type PongPayload } from "../../src/ipc/protocol.ts";
import {
  type EventBus,
  type EventSubscription,
  makeTaskId,
  type TaskEvent,
  type TaskId,
} from "../../src/types.ts";
import { startDaemon } from "../../src/daemon/server.ts";

/**
 * Minimal in-test {@link EventBus}. Wave 2's #8 ships the production
 * implementation; this test fixture mirrors the surface (`publish`,
 * `subscribe`) just enough to exercise the daemon's fan-out.
 */
class TestEventBus implements EventBus {
  private readonly handlers = new Map<
    TaskId | "*",
    Set<(event: TaskEvent) => void>
  >();

  publish(event: TaskEvent): void {
    for (const target of [event.taskId, "*"] as const) {
      const set = this.handlers.get(target);
      if (set === undefined) continue;
      for (const handler of [...set]) {
        try {
          handler(event);
        } catch {
          // Match production EventBus contract: handler throws are
          // contained.
        }
      }
    }
  }

  subscribe(
    target: TaskId | "*",
    handler: (event: TaskEvent) => void,
  ): EventSubscription {
    let set = this.handlers.get(target);
    if (set === undefined) {
      set = new Set();
      this.handlers.set(target, set);
    }
    set.add(handler);
    return {
      unsubscribe: () => {
        set?.delete(handler);
      },
    };
  }

  subscriberCount(target: TaskId | "*"): number {
    return this.handlers.get(target)?.size ?? 0;
  }
}

/**
 * Allocate a unique socket path under `Deno.makeTempDir`. Each test gets
 * its own directory so they can run in parallel without colliding.
 */
async function makeSocketPath(): Promise<string> {
  const dir = await Deno.makeTempDir({ prefix: "makina-daemon-" });
  return `${dir}/sock`;
}

/**
 * Open a Unix client connection and yield raw decoded envelopes alongside
 * a `send` helper for typed envelopes.
 */
async function connectClient(socketPath: string) {
  const conn = await Deno.connect({ transport: "unix", path: socketPath });
  const writer = conn.writable.getWriter();
  // Decode wraps the stream; the reader lock is held internally so we
  // never need to `getReader()` ourselves.
  const reader = (async function* () {
    for await (const envelope of decode(conn.readable)) {
      yield envelope;
    }
  })();
  return {
    conn,
    send: async (envelope: MessageEnvelope) => {
      await writer.write(encode(envelope));
    },
    next: async (): Promise<MessageEnvelope> => {
      const result = await reader.next();
      if (result.done) {
        throw new Error("connection closed before next envelope");
      }
      return result.value;
    },
    close: async () => {
      try {
        await writer.close();
      } catch {
        // Ignore.
      }
      try {
        conn.close();
      } catch {
        // Ignore.
      }
    },
  };
}

Deno.test("startDaemon round-trips ping → pong", async () => {
  const socketPath = await makeSocketPath();
  const handle = await startDaemon({
    socketPath,
    daemonVersion: "0.0.0-test",
  });
  try {
    const client = await connectClient(socketPath);
    try {
      await client.send({ id: "1", type: "ping", payload: {} });
      const reply = await client.next();
      assertEquals(reply.type, "pong");
      assertEquals(reply.id, "1");
      assertEquals(
        (reply.payload as PongPayload).daemonVersion,
        "0.0.0-test",
      );
    } finally {
      await client.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon fans out a published event to a wildcard subscription", async () => {
  const socketPath = await makeSocketPath();
  const bus = new TestEventBus();
  const handle = await startDaemon({ socketPath, eventBus: bus });
  try {
    const client = await connectClient(socketPath);
    try {
      await client.send({
        id: "sub-1",
        type: "subscribe",
        payload: { target: "*" },
      });
      const ack = await client.next();
      assertEquals(ack.type, "ack");
      assertEquals((ack.payload as AckPayload).ok, true);

      // Give the bus subscription a tick to register, then publish.
      const taskId = makeTaskId("task_under_test");
      const taskEvent: TaskEvent = {
        taskId,
        atIso: "2026-04-26T12:00:00.000Z",
        kind: "log",
        data: { level: "info", message: "hello world" },
      };
      bus.publish(taskEvent);

      const event = await client.next();
      assertEquals(event.type, "event");
      assertEquals(event.id, "sub-1");
      if (event.type !== "event") throw new Error("unreachable");
      assertEquals(event.payload.taskId, "task_under_test");
      assertEquals(event.payload.kind, "log");
      if (event.payload.kind !== "log") throw new Error("unreachable");
      assertEquals(event.payload.data.message, "hello world");

      // Unsubscribe and verify the bus drops the handler.
      await client.send({
        id: "unsub-1",
        type: "unsubscribe",
        payload: { subscriptionId: "sub-1" },
      });
      const unsubAck = await client.next();
      assertEquals(unsubAck.type, "ack");
      assertEquals((unsubAck.payload as AckPayload).ok, true);
      assertEquals(bus.subscriberCount("*"), 0);
    } finally {
      await client.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon fans out a published event to a per-task subscription", async () => {
  const socketPath = await makeSocketPath();
  const bus = new TestEventBus();
  const handle = await startDaemon({ socketPath, eventBus: bus });
  try {
    const client = await connectClient(socketPath);
    try {
      await client.send({
        id: "sub-2",
        type: "subscribe",
        payload: { target: "task_specific" },
      });
      const ack = await client.next();
      assertEquals(ack.type, "ack");
      assertEquals((ack.payload as AckPayload).ok, true);

      // Publish an event for an unrelated task — should NOT fan out.
      bus.publish({
        taskId: makeTaskId("task_other"),
        atIso: "2026-04-26T12:00:00.000Z",
        kind: "log",
        data: { level: "info", message: "irrelevant" },
      });
      // Publish the matching event.
      bus.publish({
        taskId: makeTaskId("task_specific"),
        atIso: "2026-04-26T12:00:01.000Z",
        kind: "log",
        data: { level: "info", message: "matched" },
      });

      const event = await client.next();
      assertEquals(event.type, "event");
      if (event.type !== "event") throw new Error("unreachable");
      assertEquals(event.payload.taskId, "task_specific");
      if (event.payload.kind !== "log") throw new Error("unreachable");
      assertEquals(event.payload.data.message, "matched");
    } finally {
      await client.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon cleans up a stale socket file on startup", async () => {
  // Create a leftover regular file at the socket path; the daemon should
  // detect it as stale (no listener) and unlink it before binding.
  const dir = await Deno.makeTempDir({ prefix: "makina-stale-" });
  const socketPath = `${dir}/sock`;
  await Deno.writeTextFile(socketPath, "leftover");
  // Sanity: file is present.
  const before = await Deno.lstat(socketPath);
  assertEquals(before.isFile, true);

  const handle = await startDaemon({ socketPath });
  try {
    // After startup, the file is replaced by a bound socket.
    const after = await Deno.lstat(socketPath);
    assertEquals(after.isSocket, true);

    // And it is functional.
    const client = await connectClient(socketPath);
    try {
      await client.send({ id: "1", type: "ping", payload: {} });
      const reply = await client.next();
      assertEquals(reply.type, "pong");
    } finally {
      await client.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon refuses to bind when the socket is still in use", async () => {
  const socketPath = await makeSocketPath();
  const first = await startDaemon({ socketPath });
  try {
    await assertRejects(
      () => startDaemon({ socketPath }),
      Deno.errors.AddrInUse,
    );
  } finally {
    await first.stop();
  }
});

Deno.test("startDaemon survives a client that closes mid-conversation", async () => {
  const socketPath = await makeSocketPath();
  const errors: { error: unknown; context: string }[] = [];
  const handle = await startDaemon({
    socketPath,
    onError: (error, context) => {
      errors.push({ error, context });
    },
  });
  try {
    // First client: send a ping then immediately close, simulating a
    // TUI that quit before reading the reply.
    const client1 = await connectClient(socketPath);
    await client1.send({ id: "1", type: "ping", payload: {} });
    await client1.close();

    // Second client: round-trips fine.
    const client2 = await connectClient(socketPath);
    try {
      await client2.send({ id: "2", type: "ping", payload: {} });
      const reply = await client2.next();
      assertEquals(reply.type, "pong");
      assertEquals(reply.id, "2");
    } finally {
      await client2.close();
    }
  } finally {
    await handle.stop();
  }
  // Even if the daemon logged a broken-pipe error, the connection
  // unwound cleanly and the second client succeeded — that's the
  // contract we're verifying.
});

Deno.test("startDaemon drops events for a closed peer without crashing", async () => {
  const socketPath = await makeSocketPath();
  const bus = new TestEventBus();
  const handle = await startDaemon({ socketPath, eventBus: bus });
  try {
    const client = await connectClient(socketPath);
    await client.send({
      id: "sub-3",
      type: "subscribe",
      payload: { target: "*" },
    });
    const ack = await client.next();
    assertEquals(ack.type, "ack");
    // Close the client while a fan-out is queued.
    await client.close();

    // Publish many events; the daemon should swallow the broken pipes.
    for (let index = 0; index < 50; index += 1) {
      bus.publish({
        taskId: makeTaskId("task_x"),
        atIso: "2026-04-26T12:00:00.000Z",
        kind: "log",
        data: { level: "info", message: `n=${index}` },
      });
    }

    // Daemon is still alive: a fresh client round-trips.
    const fresh = await connectClient(socketPath);
    try {
      await fresh.send({ id: "after", type: "ping", payload: {} });
      const reply = await fresh.next();
      assertEquals(reply.type, "pong");
    } finally {
      await fresh.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon dispatches to a custom command handler", async () => {
  const socketPath = await makeSocketPath();
  const handle = await startDaemon({
    socketPath,
    handlers: {
      command: (envelope) => {
        return { ok: true, error: `executed:${envelope.payload.name}` };
      },
    },
  });
  try {
    const client = await connectClient(socketPath);
    try {
      await client.send({
        id: "cmd-1",
        type: "command",
        payload: { name: "issue", args: ["42"] },
      });
      const reply = await client.next();
      assertEquals(reply.type, "ack");
      assertEquals((reply.payload as AckPayload).ok, true);
      assertEquals((reply.payload as AckPayload).error, "executed:issue");
    } finally {
      await client.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon dispatches to a custom prompt handler", async () => {
  const socketPath = await makeSocketPath();
  const handle = await startDaemon({
    socketPath,
    handlers: {
      prompt: () => Promise.resolve({ ok: true }),
    },
  });
  try {
    const client = await connectClient(socketPath);
    try {
      await client.send({
        id: "p-1",
        type: "prompt",
        payload: { taskId: "task_x", text: "carry on" },
      });
      const reply = await client.next();
      assertEquals(reply.type, "ack");
      assertEquals((reply.payload as AckPayload).ok, true);
    } finally {
      await client.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon translates a thrown handler into ack { ok: false }", async () => {
  const socketPath = await makeSocketPath();
  const handle = await startDaemon({
    socketPath,
    handlers: {
      command: () => {
        throw new Error("boom");
      },
    },
    onError: () => {/* swallow for the test */},
  });
  try {
    const client = await connectClient(socketPath);
    try {
      await client.send({
        id: "err-1",
        type: "command",
        payload: { name: "broken", args: [] },
      });
      const reply = await client.next();
      assertEquals(reply.type, "ack");
      const payload = reply.payload as AckPayload;
      assertEquals(payload.ok, false);
      assertEquals(payload.error, "boom");
    } finally {
      await client.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon answers unimplemented when no handler is registered", async () => {
  const socketPath = await makeSocketPath();
  const handle = await startDaemon({ socketPath });
  try {
    const client = await connectClient(socketPath);
    try {
      await client.send({
        id: "no-handler",
        type: "command",
        payload: { name: "noop", args: [] },
      });
      const reply = await client.next();
      assertEquals(reply.type, "ack");
      const payload = reply.payload as AckPayload;
      assertEquals(payload.ok, false);
      assertEquals(payload.error, "unimplemented");
    } finally {
      await client.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon answers unimplemented for subscribe when no event bus is supplied", async () => {
  const socketPath = await makeSocketPath();
  const handle = await startDaemon({ socketPath });
  try {
    const client = await connectClient(socketPath);
    try {
      await client.send({
        id: "sub-no-bus",
        type: "subscribe",
        payload: { target: "*" },
      });
      const reply = await client.next();
      assertEquals(reply.type, "ack");
      const payload = reply.payload as AckPayload;
      assertEquals(payload.ok, false);
      assertEquals(payload.error, "unimplemented");
    } finally {
      await client.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon rejects an unsubscribe for an unknown subscription id", async () => {
  const socketPath = await makeSocketPath();
  const bus = new TestEventBus();
  const handle = await startDaemon({ socketPath, eventBus: bus });
  try {
    const client = await connectClient(socketPath);
    try {
      await client.send({
        id: "u-1",
        type: "unsubscribe",
        payload: { subscriptionId: "does-not-exist" },
      });
      const reply = await client.next();
      assertEquals(reply.type, "ack");
      const payload = reply.payload as AckPayload;
      assertEquals(payload.ok, false);
    } finally {
      await client.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon rejects a daemon-only envelope sent by a client", async () => {
  const socketPath = await makeSocketPath();
  const handle = await startDaemon({ socketPath });
  try {
    const client = await connectClient(socketPath);
    try {
      await client.send({
        id: "wrong-direction",
        type: "pong",
        payload: { daemonVersion: "x" },
      });
      const reply = await client.next();
      assertEquals(reply.type, "ack");
      const payload = reply.payload as AckPayload;
      assertEquals(payload.ok, false);
    } finally {
      await client.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon rejects a subscribe with an invalid target", async () => {
  const socketPath = await makeSocketPath();
  const bus = new TestEventBus();
  const handle = await startDaemon({ socketPath, eventBus: bus });
  try {
    const client = await connectClient(socketPath);
    try {
      // Whitespace-only target trips makeTaskId(); the daemon answers
      // with ack { ok: false }.
      await client.send({
        id: "sub-bad",
        type: "subscribe",
        payload: { target: "   " },
      });
      const reply = await client.next();
      assertEquals(reply.type, "ack");
      const payload = reply.payload as AckPayload;
      assertEquals(payload.ok, false);
    } finally {
      await client.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon rejects duplicate subscription ids on the same connection", async () => {
  const socketPath = await makeSocketPath();
  const bus = new TestEventBus();
  const handle = await startDaemon({ socketPath, eventBus: bus });
  try {
    const client = await connectClient(socketPath);
    try {
      await client.send({
        id: "dup",
        type: "subscribe",
        payload: { target: "*" },
      });
      const first = await client.next();
      assertEquals(first.type, "ack");
      assertEquals((first.payload as AckPayload).ok, true);

      // Re-using the same id is a client bug.
      await client.send({
        id: "dup",
        type: "subscribe",
        payload: { target: "task_y" },
      });
      const second = await client.next();
      assertEquals(second.type, "ack");
      assertEquals((second.payload as AckPayload).ok, false);
    } finally {
      await client.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon defers an unsubscribe to a custom handler when no local subscription exists", async () => {
  const socketPath = await makeSocketPath();
  let observed = false;
  const bus = new TestEventBus();
  const handle = await startDaemon({
    socketPath,
    eventBus: bus,
    handlers: {
      unsubscribe: () => {
        observed = true;
        return { ok: true };
      },
    },
  });
  try {
    const client = await connectClient(socketPath);
    try {
      await client.send({
        id: "u-defer",
        type: "unsubscribe",
        payload: { subscriptionId: "not-mine" },
      });
      const reply = await client.next();
      assertEquals(reply.type, "ack");
      assertEquals((reply.payload as AckPayload).ok, true);
      assertEquals(observed, true);
    } finally {
      await client.close();
    }
  } finally {
    await handle.stop();
  }
});

Deno.test("startDaemon stop() is idempotent and unlinks the socket file", async () => {
  const socketPath = await makeSocketPath();
  const handle = await startDaemon({ socketPath });
  await handle.stop();
  await handle.stop(); // No-op the second time.
  // Socket file is gone after a clean stop.
  await assertRejects(() => Deno.lstat(socketPath), Deno.errors.NotFound);
});

Deno.test("startDaemon ignores a non-socket regular file at the same path that is not a socket", async () => {
  // A directory at the path is neither isSocket nor isFile; the daemon
  // skips the unlink, lets Deno.listen surface its native error, and
  // does not blow up our cleanup helper.
  const dir = await Deno.makeTempDir({ prefix: "makina-dir-" });
  await assertRejects(() => startDaemon({ socketPath: dir }));
  // Tidy up.
  await Deno.remove(dir);
});

Deno.test("startDaemon stop() runs even before any connection arrives", async () => {
  // Sanity-check the accept-loop teardown path on an idle listener.
  const socketPath = await makeSocketPath();
  const handle = await startDaemon({ socketPath });
  await handle.stop();
  assertNotEquals(handle.socketPath, "");
});
