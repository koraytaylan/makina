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
 * The subset of {@link MessageEnvelope} variants that are
 * client→daemon **requests** the in-memory client knows how to script
 * replies for. Excludes `pong`, `ack`, and `event` (which are server→
 * client envelopes the daemon emits, never receives).
 */
export type RequestEnvelope = Extract<
  MessageEnvelope,
  { type: "ping" | "subscribe" | "unsubscribe" | "command" | "prompt" }
>;

/**
 * The {@link RequestEnvelope} variant that maps to a `pong` reply.
 */
export type PingEnvelope = Extract<RequestEnvelope, { type: "ping" }>;

/**
 * The {@link RequestEnvelope} variants that map to an `ack` reply
 * (everything that is a request but not `ping`).
 */
export type AckEnvelope = Exclude<RequestEnvelope, { type: "ping" }>;

/**
 * Per-request handler invoked when the SUT calls `send()`. Tests register
 * one with {@link InMemoryDaemonClient.setRequestHandler} to script
 * non-trivial daemon behavior; the default handler just produces an
 * `ack { ok: true }` (or a `pong` for `ping`).
 *
 * The signature is a callable interface with two overloads so the
 * payload type tracks the envelope's `type` discriminant: a `ping`
 * envelope must be answered with a {@link PongPayload}; every other
 * request envelope must be answered with an {@link AckPayload}. This
 * pushes the previous runtime casts inside {@link
 * InMemoryDaemonClient.send} out to the handler boundary so the
 * test-double's contract is self-checking.
 *
 * A test handler that returns the wrong shape for a given envelope
 * type now fails to type-check, instead of silently shipping a stray
 * runtime cast.
 */
export interface RequestHandler {
  /**
   * Reply to a `ping` envelope.
   *
   * @param envelope The `ping` envelope to reply to.
   * @returns A promise resolving to the `pong` payload.
   */
  (envelope: PingEnvelope): Promise<PongPayload>;
  /**
   * Reply to any non-`ping` request envelope.
   *
   * @param envelope The request envelope to reply to.
   * @returns A promise resolving to the `ack` payload.
   */
  (envelope: AckEnvelope): Promise<AckPayload>;
}

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
    if (parsed.type === "ping") {
      // The `RequestHandler` overload selected here returns
      // `Promise<PongPayload>`; no runtime cast required.
      const payload = await this.requestHandler(parsed);
      return { id: parsed.id, type: "pong", payload };
    }
    if (
      parsed.type === "subscribe" ||
      parsed.type === "unsubscribe" ||
      parsed.type === "command" ||
      parsed.type === "prompt"
    ) {
      // The `AckEnvelope` overload returns `Promise<AckPayload>` for
      // every non-`ping` request type. A `pong`/`ack`/`event` envelope
      // would not be a valid client→daemon request and is rejected
      // below.
      const payload = await this.requestHandler(parsed);
      return { id: parsed.id, type: "ack", payload };
    }
    throw new Error(
      `InMemoryDaemonClient.send: ${parsed.type} envelopes flow daemon→client only`,
    );
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

// The default handler must satisfy both overloads. Implementing both
// signatures with one body and a runtime branch is the standard TS
// pattern for overloaded callables.
function defaultRequestHandler(envelope: PingEnvelope): Promise<PongPayload>;
function defaultRequestHandler(envelope: AckEnvelope): Promise<AckPayload>;
function defaultRequestHandler(
  envelope: RequestEnvelope,
): Promise<PongPayload | AckPayload> {
  if (envelope.type === "ping") {
    return Promise.resolve<PongPayload>({ daemonVersion: MAKINA_VERSION });
  }
  return Promise.resolve<AckPayload>({ ok: true });
}
