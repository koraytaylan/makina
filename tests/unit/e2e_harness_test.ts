/**
 * Unit tests for `tests/e2e/_e2e_harness.ts`'s read loop.
 *
 * The e2e harness opens a Unix socket to the daemon and runs an async
 * read loop that decodes IPC envelopes, dispatches them to either the
 * `eventQueue`/`eventWaiters` (for `event` envelopes) or the
 * `replyWaiters` (for `ack`/`pong`/...). The loop is the only thing
 * that resolves outstanding `send()` promises.
 *
 * This file regression-tests the **clean-close** path: when the peer
 * closes the socket at a frame boundary, `decode()` runs to EOF
 * without throwing. A previous version of the loop relied on the
 * `catch` block to fail pending waiters, so a clean close left
 * `replyWaiters` and `eventWaiters` hanging indefinitely. The fix is
 * to detect the EOF and reject every pending waiter with a typed
 * {@link HarnessError}`("connection-closed", ...)`.
 *
 * The test uses a real Unix socket on a temp directory rather than a
 * synthetic `ReadableStream` because the bug only surfaces when the
 * peer-side close model matches the production daemon's lifecycle —
 * a manually-controlled `ReadableStream.close()` happens to behave
 * the same way today, but the on-the-wire path is the contract.
 *
 * @module
 */

import { assertEquals, assertInstanceOf, assertRejects } from "@std/assert";

import { encode } from "../../src/ipc/codec.ts";
import { type EventPayload, type MessageEnvelope } from "../../src/ipc/protocol.ts";
import {
  HarnessError,
  type HarnessEventWaiter,
  type HarnessReplyWaiter,
  runHarnessReadLoop,
} from "../../tests/e2e/_e2e_harness.ts";

/**
 * Spin up a one-shot Unix socket server that writes the supplied
 * frames to the first peer that connects, then closes the connection
 * at a frame boundary.
 *
 * @returns The socket path and a promise that resolves when the
 *   server has accepted-and-closed exactly one peer.
 */
async function makeOneShotServer(
  frames: Uint8Array[],
): Promise<{ path: string; closed: Promise<void>; tempDir: string }> {
  const tempDir = await Deno.makeTempDir({
    dir: "/tmp",
    prefix: "makina-harness-readloop-",
  });
  const path = `${tempDir}/peer.sock`;
  const listener = Deno.listen({ transport: "unix", path });
  const closed = (async () => {
    try {
      const conn = await listener.accept();
      const writer = conn.writable.getWriter();
      try {
        for (const frame of frames) {
          await writer.write(frame);
        }
        await writer.close();
      } catch {
        // Peer dropped — drop our side too.
      }
      try {
        conn.close();
      } catch {
        // Already closed.
      }
    } finally {
      try {
        listener.close();
      } catch {
        // Already closed.
      }
    }
  })();
  return { path, closed, tempDir };
}

Deno.test(
  "runHarnessReadLoop rejects pending waiters with HarnessError on clean close",
  async () => {
    // Send one valid event, then close the connection. A pending
    // reply waiter (whose id never appeared on the wire) and a
    // pending event waiter (whose predicate never fires) should both
    // reject with HarnessError("connection-closed").
    const eventEnvelope: MessageEnvelope = {
      id: "synthetic-event-1",
      type: "event",
      payload: {
        taskId: "synthetic-task",
        atIso: new Date(0).toISOString(),
        kind: "log",
        data: { level: "info", message: "synthetic frame" },
      },
    };
    const server = await makeOneShotServer([encode(eventEnvelope)]);
    try {
      const conn = await Deno.connect({
        transport: "unix",
        path: server.path,
      });
      const eventQueue: EventPayload[] = [];
      const eventWaiters: HarnessEventWaiter[] = [];
      const replyWaiters = new Map<string, HarnessReplyWaiter>();

      // Install a reply waiter for an id that will never come.
      const replyPromise = new Promise<MessageEnvelope>((resolve, reject) => {
        replyWaiters.set("never-arrives", { resolve, reject });
      });
      // Install an event waiter whose predicate matches nothing.
      const eventPromise = new Promise<EventPayload>((resolve, reject) => {
        const timer = setTimeout(() => {
          reject(new Error("unit-test timer fired (should not happen)"));
        }, 30_000);
        eventWaiters.push({
          predicate: () => false,
          resolve,
          reject,
          timer,
        });
      });

      const loopDone = runHarnessReadLoop({
        readable: conn.readable,
        eventQueue,
        eventWaiters,
        replyWaiters,
      });

      // Both waiters should reject with HarnessError once the loop
      // observes the clean EOF.
      const replyError = await assertRejects(
        () => replyPromise,
        HarnessError,
      );
      assertEquals(replyError.kind, "connection-closed");

      const eventError = await assertRejects(
        () => eventPromise,
        HarnessError,
      );
      assertEquals(eventError.kind, "connection-closed");
      assertInstanceOf(eventError, Error);

      await loopDone;
      await server.closed;
      // The valid frame should have made it through before the close.
      assertEquals(eventQueue.length, 1);
      assertEquals(eventQueue[0]?.kind, "log");
      // Both waiter collections must be empty; the loop drained them.
      assertEquals(replyWaiters.size, 0);
      assertEquals(eventWaiters.length, 0);
    } finally {
      try {
        await Deno.remove(server.tempDir, { recursive: true });
      } catch {
        // Best-effort.
      }
    }
  },
);
