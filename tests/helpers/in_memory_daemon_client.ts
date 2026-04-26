/**
 * tests/helpers/in_memory_daemon_client.ts — in-process IPC client used by
 * the TUI shell tests.
 *
 * Wave 2's real `src/tui/ipc-client.ts` opens a Unix-domain socket to the
 * daemon, encodes outgoing messages with `src/ipc/codec.ts`, and decodes
 * the daemon's replies + pushed events back into typed envelopes. The
 * snapshot-style TUI tests do not need to exercise socket I/O — they need
 * the same surface area but driven entirely in-memory. This double
 * provides that surface.
 *
 * **Surface.**
 * - {@link InMemoryDaemonClient.send} — send a request envelope. Returns
 *   the {@link DaemonReply} envelope (`pong` for `ping`, `ack` otherwise).
 * - {@link InMemoryDaemonClient.subscribeEvents} — receive `event`
 *   envelopes pushed by the daemon. The handler is invoked synchronously
 *   per event in the order events were emitted.
 * - {@link InMemoryDaemonClient.simulateEvent} — fixture-only API for
 *   tests to inject an event as if the daemon had pushed it.
 *
 * Compiles against `src/ipc/protocol.ts` so a contract drift breaks
 * immediately.
 */

import {
  type AckPayload,
  type EventPayload,
  type MessageEnvelope,
  parseEnvelope,
  type PongPayload,
} from "../../src/ipc/protocol.ts";
import { MAKINA_VERSION } from "../../src/constants.ts";

/**
 * Envelopes the daemon emits as a reply to a client request. `pong`
 * answers `ping`; `ack` answers everything else.
 */
export type DaemonReply =
  | (Extract<MessageEnvelope, { type: "pong" }>)
  | (Extract<MessageEnvelope, { type: "ack" }>);

/**
 * Subscription handle returned by
 * {@link InMemoryDaemonClient.subscribeEvents}. `unsubscribe()` is
 * idempotent.
 */
export interface DaemonEventSubscription {
  /** Stop forwarding events to this handler. */
  unsubscribe(): void;
}

/**
 * Per-request handler invoked when the SUT calls `send()`. Tests register
 * one with {@link InMemoryDaemonClient.setRequestHandler} to script
 * non-trivial daemon behavior; the default handler just produces an
 * `ack { ok: true }` (or a `pong` for `ping`).
 */
export type RequestHandler = (
  envelope: MessageEnvelope,
) => Promise<AckPayload | PongPayload>;

/**
 * In-memory daemon double for TUI shell tests.
 */
export class InMemoryDaemonClient {
  private readonly eventHandlers = new Set<(event: EventPayload) => void>();
  private readonly sentEnvelopes: MessageEnvelope[] = [];
  private requestHandler: RequestHandler = defaultRequestHandler;

  /**
   * Override the default request handler.
   *
   * @param handler New handler; receives the parsed envelope and returns
   *   the payload of the reply. The envelope's `id` is forwarded to the
   *   reply automatically by {@link InMemoryDaemonClient.send}.
   */
  setRequestHandler(handler: RequestHandler): void {
    this.requestHandler = handler;
  }

  /**
   * Send a request envelope. Returns the daemon's reply.
   *
   * Validation failures arrive as a rejected promise (not a synchronous
   * throw) so callers always have a single error path to handle.
   *
   * @param envelope The envelope to send. Must validate against
   *   {@link parseEnvelope}.
   * @returns The reply envelope (`pong` for `ping`, `ack` otherwise).
   */
  async send(envelope: MessageEnvelope): Promise<DaemonReply> {
    const validation = parseEnvelope(envelope);
    if (!validation.success) {
      throw new Error(
        `envelope failed validation: ${
          validation.issues.map((issue) => `${issue.path.join(".")}: ${issue.message}`).join("; ")
        }`,
      );
    }
    const parsed = validation.data;
    this.sentEnvelopes.push(parsed);
    const reply = await this.requestHandler(parsed);
    if (parsed.type === "ping") {
      return { id: parsed.id, type: "pong", payload: reply as PongPayload };
    }
    return { id: parsed.id, type: "ack", payload: reply as AckPayload };
  }

  /**
   * Subscribe to event envelopes pushed by the daemon.
   *
   * @param handler Callback invoked synchronously per event.
   * @returns Subscription handle whose `unsubscribe()` stops further
   *   deliveries.
   */
  subscribeEvents(handler: (event: EventPayload) => void): DaemonEventSubscription {
    this.eventHandlers.add(handler);
    return {
      unsubscribe: () => {
        this.eventHandlers.delete(handler);
      },
    };
  }

  /**
   * Push an event to every active subscriber. Test-fixture API; not
   * present on the real socket-backed client.
   *
   * @param event Event payload as the daemon would have published it.
   */
  simulateEvent(event: EventPayload): void {
    for (const handler of this.eventHandlers) {
      handler(event);
    }
  }

  /**
   * Inspect the envelopes the SUT has sent.
   *
   * @returns A defensive copy of every envelope, in send order.
   */
  recordedSends(): readonly MessageEnvelope[] {
    return [...this.sentEnvelopes];
  }

  /**
   * Number of currently-active event subscriptions. Useful for asserting
   * unsubscribe correctness.
   *
   * @returns Subscription count.
   */
  activeSubscriptionCount(): number {
    return this.eventHandlers.size;
  }
}

const defaultRequestHandler: RequestHandler = (envelope) => {
  if (envelope.type === "ping") {
    return Promise.resolve<PongPayload>({ daemonVersion: MAKINA_VERSION });
  }
  return Promise.resolve<AckPayload>({ ok: true });
};
