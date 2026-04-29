/**
 * tui/ipc-client.ts — Unix-domain-socket IPC client used by the TUI.
 *
 * **Surface.** The exported {@link DaemonClient} interface intentionally
 * mirrors the public surface of the in-process double in
 * `tests/helpers/in_memory_daemon_client.ts` (`send` and `subscribeEvents`
 * only — the test-only `simulateEvent`, `recordedSends`, and
 * `setRequestHandler` are deliberately absent here). The TUI hooks code
 * against the interface, never the concrete class, so swapping the real
 * socket-backed client for the in-memory double in tests is a one-line
 * change.
 *
 * **Wire format.** Each frame is `<decimal-length>\n<utf8-json>\n` per
 * `src/ipc/codec.ts`. The codec handles framing; this module only deals in
 * typed envelopes. Pushed `event` envelopes from the daemon are surfaced
 * through the `subscribeEvents` callback in arrival order; reply envelopes
 * (`pong` for `ping`, `ack` otherwise) are matched back to outstanding
 * `send` calls by the envelope `id`.
 *
 * **Lifecycle.** Construct → {@link SocketDaemonClient.connect} → use
 * `send`/`subscribeEvents` → {@link SocketDaemonClient.close}. `close()`
 * tears down the socket and rejects every in-flight `send` so the caller
 * never hangs waiting for a reply that will never arrive.
 *
 * @module
 */

import {
  type AckPayload,
  type EventPayload,
  type MessageEnvelope,
  parseEnvelope,
  type PongPayload,
} from "@makina/core";
import { decode, encode, IpcCodecError } from "@makina/core";

/**
 * Envelopes the daemon emits as a reply to a client request. `pong`
 * answers `ping`; `ack` answers everything else.
 *
 * Mirrors the type defined in `tests/helpers/in_memory_daemon_client.ts`
 * so consumers can be parametrized over the interface.
 */
export type DaemonReply =
  | (Extract<MessageEnvelope, { type: "pong" }>)
  | (Extract<MessageEnvelope, { type: "ack" }>);

/**
 * Subscription handle returned by
 * {@link DaemonClient.subscribeEvents}. `unsubscribe()` is idempotent.
 */
export interface DaemonEventSubscription {
  /** Stop forwarding events to this handler. */
  unsubscribe(): void;
}

/**
 * Public surface every daemon client (real socket-backed or in-memory
 * test double) must implement.
 *
 * @example
 * ```ts
 * const client: DaemonClient = new SocketDaemonClient("/tmp/makina.sock");
 * await client.connect();
 * const reply = await client.send({ id: "1", type: "ping", payload: {} });
 * ```
 */
export interface DaemonClient {
  /**
   * Send a request envelope and resolve with the daemon's reply.
   *
   * @param envelope The envelope to send. Must validate against the
   *   {@link parseEnvelope} schema; an invalid envelope rejects without
   *   touching the wire.
   * @returns The reply envelope (`pong` for `ping`, `ack` otherwise).
   */
  send(envelope: MessageEnvelope): Promise<DaemonReply>;
  /**
   * Subscribe to event envelopes pushed by the daemon. The handler is
   * invoked synchronously per event, in arrival order; throws inside
   * `handler` are swallowed (they cannot poison the read loop).
   *
   * @param handler Callback invoked per event.
   * @returns Subscription handle whose `unsubscribe()` stops further
   *   deliveries.
   */
  subscribeEvents(handler: (event: EventPayload) => void): DaemonEventSubscription;
}

/**
 * Error thrown by {@link SocketDaemonClient} when the underlying socket
 * cannot be opened, the daemon disconnects mid-request, or a reply
 * arrives that does not correspond to any outstanding `send`.
 */
export class DaemonClientError extends Error {
  /**
   * Construct a daemon-client error.
   *
   * @param message Human-readable description.
   * @param cause Underlying error, when one exists.
   */
  constructor(
    message: string,
    /** Underlying cause, when applicable. */
    public override readonly cause?: unknown,
  ) {
    super(message);
    this.name = "DaemonClientError";
  }
}

/**
 * Minimal duplex transport surface the client needs. The real
 * implementation uses `Deno.connect({ transport: "unix" })`; tests can
 * inject any object that quacks like one.
 */
export interface DuplexConnection {
  /** Readable byte stream from the peer. */
  readonly readable: ReadableStream<Uint8Array>;
  /** Writable byte stream to the peer. */
  readonly writable: WritableStream<Uint8Array>;
  /** Close both halves of the connection. */
  close(): void;
}

/**
 * Function that opens a {@link DuplexConnection}. Defaults to a
 * `Deno.connect`-backed Unix-socket connector; exposed for test
 * injection.
 */
export type ConnectionOpener = () => Promise<DuplexConnection>;

/**
 * Socket-backed {@link DaemonClient} implementation.
 *
 * Wave 2's daemon (issue #9) listens on a Unix-domain socket; this
 * client opens that socket, encodes outgoing envelopes through
 * {@link encode}, and decodes pushed envelopes through {@link decode}.
 * Reply correlation is by envelope `id`.
 */
export class SocketDaemonClient implements DaemonClient {
  private readonly socketPath: string;
  private readonly opener: ConnectionOpener;
  private connection: DuplexConnection | undefined;
  private writer: WritableStreamDefaultWriter<Uint8Array> | undefined;
  private reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  private readonly pending = new Map<
    string,
    { resolve: (reply: DaemonReply) => void; reject: (error: unknown) => void }
  >();
  private readonly eventHandlers = new Set<(event: EventPayload) => void>();
  private readLoopPromise: Promise<void> | undefined;
  /**
   * Set by {@link SocketDaemonClient.close} so subsequent operations
   * fail fast. Distinct from the read-loop terminating on its own (a
   * peer disconnect): an unexpected end leaves `closed === false` so a
   * caller can call {@link SocketDaemonClient.connect} again to
   * recover, while an explicit `close()` is permanent.
   */
  private closed = false;
  /**
   * Tracks whether the read loop is currently consuming frames. Goes
   * `true` in {@link SocketDaemonClient.connect}, back to `false` in
   * the read loop's `finally`, and lets `connect()` distinguish "I am
   * already streaming" (no-op) from "the previous stream ended; open
   * a fresh socket" (reconnect).
   */
  private streaming = false;

  /**
   * Construct a socket-backed client.
   *
   * @param socketPath Filesystem path of the Unix-domain socket the
   *   daemon is listening on.
   * @param opener Optional connection opener; defaults to
   *   `Deno.connect({ transport: "unix", path: socketPath })`. Tests
   *   inject this to drive the client without real socket I/O.
   *
   * @example
   * ```ts
   * const client = new SocketDaemonClient("/tmp/makina.sock");
   * await client.connect();
   * ```
   */
  constructor(socketPath: string, opener?: ConnectionOpener) {
    this.socketPath = socketPath;
    this.opener = opener ?? defaultUnixConnector(socketPath);
  }

  /**
   * Open the underlying connection and start the read loop.
   *
   * - A second call while the read loop is still consuming frames is
   *   a no-op (the existing socket is reused).
   * - A call after {@link SocketDaemonClient.close} rejects with a
   *   {@link DaemonClientError} (an explicit close is permanent).
   * - A call after the read loop ended on its own (peer disconnect)
   *   reopens a fresh socket and restarts the read loop. This is the
   *   recovery path the {@link useDaemonConnection} hook walks when a
   *   reconnect button is pressed in Wave 3.
   *
   * @throws {DaemonClientError} If the client was explicitly closed
   *   or if the underlying socket cannot be opened.
   */
  async connect(): Promise<void> {
    if (this.streaming) {
      // Already streaming — `connect()` is idempotent in this state.
      return;
    }
    if (this.closed) {
      throw new DaemonClientError("client has been closed");
    }
    // Defensively clear any leftover handles from a previous read
    // loop that ended on its own (peer disconnect). The loop's
    // `finally` releases the locks for us; this just nulls the
    // refs so the new opener can install fresh ones.
    this.connection = undefined;
    this.writer = undefined;
    this.reader = undefined;
    this.readLoopPromise = undefined;
    try {
      this.connection = await this.opener();
    } catch (error) {
      throw new DaemonClientError(
        `failed to connect to daemon socket ${JSON.stringify(this.socketPath)}`,
        error,
      );
    }
    this.writer = this.connection.writable.getWriter();
    this.reader = this.connection.readable.getReader();
    this.streaming = true;
    this.readLoopPromise = this.runReadLoop(this.reader);
  }

  /**
   * Send a request envelope and resolve with the daemon's reply.
   *
   * @param envelope The envelope to send.
   * @returns The reply envelope (`pong` for `ping`, `ack` otherwise).
   * @throws {DaemonClientError} If the client is not connected, the
   *   envelope is malformed, or the connection terminates before a
   *   reply arrives.
   */
  send(envelope: MessageEnvelope): Promise<DaemonReply> {
    // Check `closed` before the connection-state guard so a `send` after
    // an explicit `close()` rejects with the documented permanent-close
    // error instead of the recoverable "not connected" message — close()
    // tears down `connection`/`writer`, so both branches would otherwise
    // fire and the less-specific one would win the race.
    if (this.closed) {
      return Promise.reject(new DaemonClientError("client has been closed"));
    }
    if (this.connection === undefined || this.writer === undefined) {
      return Promise.reject(new DaemonClientError("not connected"));
    }
    let bytes: Uint8Array;
    try {
      bytes = encode(envelope);
    } catch (error) {
      return Promise.reject(
        error instanceof IpcCodecError
          ? new DaemonClientError(error.message, error)
          : new DaemonClientError("failed to encode envelope", error),
      );
    }
    if (this.pending.has(envelope.id)) {
      return Promise.reject(
        new DaemonClientError(`duplicate envelope id ${JSON.stringify(envelope.id)}`),
      );
    }
    const reply = new Promise<DaemonReply>((resolve, reject) => {
      this.pending.set(envelope.id, { resolve, reject });
    });
    this.writer.write(bytes).catch((error) => {
      const slot = this.pending.get(envelope.id);
      if (slot !== undefined) {
        this.pending.delete(envelope.id);
        slot.reject(new DaemonClientError("failed to write to daemon socket", error));
      }
    });
    return reply;
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
   * Tear down the connection. Outstanding `send` promises reject with a
   * {@link DaemonClientError}; the read loop exits cleanly. Idempotent.
   *
   * Once called, the client is permanently dead — subsequent
   * {@link SocketDaemonClient.connect} calls reject. To recover from a
   * peer disconnect without losing the right to reconnect, let the
   * read loop terminate on its own and then call `connect()` again.
   */
  async close(): Promise<void> {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.streaming = false;
    const reason = new DaemonClientError("client closed before reply");
    for (const [id, slot] of this.pending) {
      slot.reject(reason);
      this.pending.delete(id);
    }
    if (this.writer !== undefined) {
      // Fire-and-forget abort; awaiting can hang when the peer has
      // gone away mid-write, and we have no use for the resolution.
      // The slot rejection above already woke every consumer.
      this.writer.abort(reason).catch(() => {});
      try {
        this.writer.releaseLock();
      } catch {
        // Already released.
      }
      this.writer = undefined;
    }
    // Cancel the reader so the read loop terminates promptly.
    // Without the cancel the loop would keep waiting for the peer to
    // close its writable, which can hang shutdown for the lifetime
    // of the daemon process.
    if (this.reader !== undefined) {
      try {
        await this.reader.cancel();
      } catch {
        // Already cancelled or terminated; nothing to do.
      }
      try {
        this.reader.releaseLock();
      } catch {
        // Already released.
      }
      this.reader = undefined;
    }
    if (this.connection !== undefined) {
      try {
        this.connection.close();
      } catch {
        // Connection already closed.
      }
      this.connection = undefined;
    }
    if (this.readLoopPromise !== undefined) {
      await this.readLoopPromise.catch(() => {});
      this.readLoopPromise = undefined;
    }
  }

  /**
   * Decode envelopes off the read stream and dispatch them. Replies
   * resolve outstanding `send` promises by id; events fan out to every
   * subscriber.
   *
   * Uses the codec's stateful decoder by wrapping the owned reader in
   * a fresh {@link ReadableStream} so we keep the framing logic in
   * one place ({@link decode}) without giving up the ability to
   * cancel the underlying reader from {@link SocketDaemonClient.close}.
   *
   * @param reader The owned reader for the peer's byte stream.
   */
  private async runReadLoop(reader: ReadableStreamDefaultReader<Uint8Array>): Promise<void> {
    try {
      // Re-expose the reader as a ReadableStream so `decode` can chunk
      // through it normally. Cancelling the underlying reader (in
      // `close`) makes this stream end, which in turn ends the
      // for-await below.
      const stream = new ReadableStream<Uint8Array>({
        async pull(controller) {
          try {
            const { value, done } = await reader.read();
            if (done) {
              controller.close();
              return;
            }
            if (value !== undefined) {
              controller.enqueue(value);
            }
          } catch (error) {
            controller.error(error);
          }
        },
        cancel: async (reason) => {
          try {
            await reader.cancel(reason);
          } catch {
            // Already cancelled.
          }
        },
      });
      for await (const envelope of decode(stream)) {
        if (envelope.type === "event") {
          this.dispatchEvent(envelope.payload);
          continue;
        }
        if (envelope.type === "ack" || envelope.type === "pong") {
          this.resolveReply(envelope as DaemonReply);
          continue;
        }
        // Any other envelope type from the daemon is a protocol violation.
        // Reject every outstanding send so the caller surfaces it.
        this.failAllPending(
          new DaemonClientError(`unexpected envelope type from daemon: ${envelope.type}`),
        );
      }
      // Loop exited normally — the peer reached EOF without sending a
      // reply for at least the still-pending ids. Fail those promises
      // so callers do not hang waiting for a reply that will never
      // arrive. `close()` already drains `pending` itself, so this is
      // only meaningful for an unsolicited peer disconnect; it is a
      // no-op when the read loop ends because of a deliberate
      // `close()`.
      if (!this.closed) {
        this.failAllPending(new DaemonClientError("daemon disconnected before responding"));
      }
    } catch (error) {
      if (!this.closed) {
        const reason = error instanceof Error
          ? new DaemonClientError(error.message, error)
          : new DaemonClientError(String(error));
        this.failAllPending(reason);
      }
    } finally {
      // The stream ended (peer disconnect or explicit `close()`).
      // Tear down the local handles so a subsequent `connect()` opens
      // fresh ones — without this the early-return at the top of
      // `connect()` would treat the dead connection as still alive
      // and silently no-op. `closed` is left untouched here: only
      // `close()` should mark the client permanently dead.
      this.streaming = false;
      if (this.writer !== undefined) {
        try {
          this.writer.releaseLock();
        } catch {
          // Already released (close() ran in parallel).
        }
        this.writer = undefined;
      }
      if (this.reader !== undefined) {
        try {
          this.reader.releaseLock();
        } catch {
          // Already released.
        }
        this.reader = undefined;
      }
      if (this.connection !== undefined) {
        try {
          this.connection.close();
        } catch {
          // Already closed.
        }
        this.connection = undefined;
      }
    }
  }

  /**
   * Resolve the pending `send` whose envelope id matches the reply.
   *
   * @param reply The decoded reply envelope.
   */
  private resolveReply(reply: DaemonReply): void {
    const slot = this.pending.get(reply.id);
    if (slot === undefined) {
      // Unsolicited reply — protocol violation; surface to all pending.
      this.failAllPending(
        new DaemonClientError(`reply for unknown envelope id ${JSON.stringify(reply.id)}`),
      );
      return;
    }
    this.pending.delete(reply.id);
    slot.resolve(reply);
  }

  /**
   * Fan out a pushed `event` payload to every active subscriber.
   * Handler exceptions are swallowed (they cannot poison the loop).
   *
   * @param event The pushed event payload.
   */
  private dispatchEvent(event: EventPayload): void {
    for (const handler of this.eventHandlers) {
      try {
        handler(event);
      } catch {
        // Handler errors are not the read loop's problem.
      }
    }
  }

  /**
   * Reject every in-flight `send` with `reason` and clear the slot map.
   *
   * @param reason The rejection cause.
   */
  private failAllPending(reason: DaemonClientError): void {
    for (const [id, slot] of this.pending) {
      slot.reject(reason);
      this.pending.delete(id);
    }
  }
}

/**
 * Build the default {@link ConnectionOpener} that opens a
 * Unix-domain-socket connection to `socketPath` via `Deno.connect`.
 *
 * Factored out so {@link SocketDaemonClient} is constructible without
 * granting `--allow-net` permission at module-evaluation time.
 *
 * @param socketPath The Unix-socket path.
 * @returns A connector that opens the socket on demand.
 */
function defaultUnixConnector(socketPath: string): ConnectionOpener {
  return async () => {
    const conn = await Deno.connect({ transport: "unix", path: socketPath });
    return {
      readable: conn.readable,
      writable: conn.writable,
      close: () => {
        try {
          conn.close();
        } catch {
          // Already closed.
        }
      },
    };
  };
}

/**
 * Helper for tests and the in-memory adapter: wrap the test double in
 * the {@link parseEnvelope} round-trip the real socket-backed client
 * does naturally. This guarantees a test that passes against the
 * in-memory client also passes the wire validators.
 *
 * @param raw An arbitrary JSON value claiming to be a reply envelope.
 * @returns The validated {@link DaemonReply}.
 * @throws {DaemonClientError} If validation fails.
 */
export function validateReplyEnvelope(raw: unknown): DaemonReply {
  const result = parseEnvelope(raw);
  if (!result.success) {
    throw new DaemonClientError(
      `reply envelope failed validation: ${
        result.issues.map((issue) => `${issue.path.join(".")}: ${issue.message}`).join("; ")
      }`,
    );
  }
  if (result.data.type !== "ack" && result.data.type !== "pong") {
    throw new DaemonClientError(`expected ack or pong; got ${result.data.type}`);
  }
  return result.data as DaemonReply;
}

/** Re-export so consumers can narrow on the reply payloads. */
export type { AckPayload, EventPayload, PongPayload };
