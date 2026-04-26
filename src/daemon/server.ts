/**
 * daemon/server.ts — Unix-domain socket listener and IPC dispatch loop.
 *
 * Wave 2 owns the wire side of the daemon: this module accepts TUI
 * connections over `Deno.listen({ transport: "unix", path })`, decodes the
 * NDJSON-style framed envelopes defined by `src/ipc/codec.ts`, and routes
 * each one to a pluggable handler. Two message types are handled
 * intrinsically:
 *
 *  - `ping` is replied to with a `pong` carrying {@link MAKINA_VERSION};
 *  - `subscribe { target }` registers a fan-out from a passed-in
 *    {@link EventBus} so the supervisor's events flow to the connected
 *    client without the client having to poll.
 *
 * Anything else (`command`, `prompt`, `unsubscribe`) is forwarded to the
 * `handlers` map on {@link DaemonServerOptions}; if no handler is
 * registered the daemon replies with `ack { ok: false, error:
 * "unimplemented" }` so the TUI gets a deterministic answer instead of a
 * silent timeout. Wave 3 wires the supervisor's command surface here; this
 * module's contract is the dispatch loop, not the commands.
 *
 * **Resilience.** A TUI that exits while the daemon is mid-write produces
 * a `BrokenPipe` (or `Interrupted` / `ConnectionReset` on macOS); these
 * are swallowed per-connection so a single misbehaving client cannot kill
 * the daemon. Stale socket files left behind by an unclean shutdown are
 * cleaned up before binding: the daemon first probes the path with
 * `Deno.connect`; if the file exists but no peer answers, it is unlinked
 * and the listener takes over.
 *
 * @module
 */

import { decode, encode, IpcCodecError } from "../ipc/codec.ts";
import {
  type AckPayload,
  type EventPayload,
  type MessageEnvelope,
  type PongPayload,
  type SubscribePayload,
} from "../ipc/protocol.ts";
import { MAKINA_VERSION } from "../constants.ts";
import {
  type EventBus,
  type EventSubscription,
  makeTaskId,
  type TaskEvent,
  type TaskId,
} from "../types.ts";

/**
 * Per-message handler signature. The handler receives the parsed envelope
 * and returns the {@link AckPayload} the daemon should reply with.
 *
 * Returning a rejected promise (or throwing synchronously) is treated as
 * an internal handler bug: the daemon converts the failure into
 * `ack { ok: false, error: <string> }` so the client never sees a
 * dropped frame.
 *
 * @template TEnvelope The narrowed envelope variant the handler accepts.
 */
export type DaemonHandler<TEnvelope extends MessageEnvelope = MessageEnvelope> = (
  envelope: TEnvelope,
) => Promise<AckPayload> | AckPayload;

/**
 * Map of message type → handler. Only the message types the daemon does
 * not handle intrinsically are looked up here (`command`, `prompt`,
 * `unsubscribe`). Missing entries fall back to `ack { ok: false, error:
 * "unimplemented" }`.
 *
 * Each entry is typed against the discriminated union in
 * `src/ipc/protocol.ts` so a wave-3 handler that wires `command` cannot
 * accidentally accept a `subscribe`.
 */
export interface DaemonHandlers {
  /** Optional override for `command` envelopes. */
  readonly command?: DaemonHandler<Extract<MessageEnvelope, { type: "command" }>>;
  /** Optional override for `prompt` envelopes. */
  readonly prompt?: DaemonHandler<Extract<MessageEnvelope, { type: "prompt" }>>;
  /** Optional override for `unsubscribe` envelopes. */
  readonly unsubscribe?: DaemonHandler<Extract<MessageEnvelope, { type: "unsubscribe" }>>;
}

/**
 * Construction-time configuration for {@link startDaemon}.
 */
export interface DaemonServerOptions {
  /** Filesystem path of the Unix-domain socket the daemon should bind. */
  readonly socketPath: string;
  /**
   * Optional dispatch overrides for non-intrinsic message types. Wave 3
   * wires the supervisor's command surface here; Wave 2 leaves it empty
   * and the daemon answers with `unimplemented`.
   */
  readonly handlers?: DaemonHandlers;
  /**
   * Optional event bus. When set, `subscribe` envelopes register a
   * fan-out from the bus to the connected client; when omitted, the
   * daemon replies with `ack { ok: false, error: "unimplemented" }` to
   * any `subscribe`.
   */
  readonly eventBus?: EventBus;
  /**
   * Optional version string surfaced in `pong`. Defaults to
   * {@link MAKINA_VERSION}; tests pin a fixed value to assert against.
   */
  readonly daemonVersion?: string;
  /**
   * Optional logger invoked once per non-fatal error (broken pipes,
   * malformed frames, handler throws). The default writes to
   * `console.error`; tests pass a recorder.
   */
  readonly onError?: (error: unknown, context: string) => void;
}

/**
 * Handle returned by {@link startDaemon}. Closing the handle stops
 * accepting new connections, drains the active ones, unsubscribes every
 * fan-out, and unlinks the socket file.
 */
export interface DaemonHandle {
  /** Filesystem path the daemon is bound to. */
  readonly socketPath: string;
  /**
   * Stop the listener and tear down active connections.
   *
   * Idempotent: a second call resolves immediately. Resolves once every
   * accept-loop iteration has unwound and the socket file has been
   * unlinked.
   */
  stop(): Promise<void>;
}

const UNIMPLEMENTED_ACK: AckPayload = { ok: false, error: "unimplemented" };

/**
 * Start a daemon listener on `opts.socketPath`.
 *
 * The function returns once the listener is bound; the accept loop runs
 * in the background until {@link DaemonHandle.stop} is called.
 *
 * @param opts Server configuration. See {@link DaemonServerOptions}.
 * @returns A {@link DaemonHandle} the caller uses to stop the server.
 * @throws Deno.errors.AddrInUse if another live process is already bound
 *   to `opts.socketPath`. Stale-socket cleanup runs first so a leftover
 *   file from a previous unclean shutdown does not surface as this error.
 *
 * @example
 * ```ts
 * const handle = await startDaemon({ socketPath: "/tmp/makina.sock" });
 * // ...
 * await handle.stop();
 * ```
 */
export async function startDaemon(opts: DaemonServerOptions): Promise<DaemonHandle> {
  const onError = opts.onError ?? defaultOnError;
  const daemonVersion = opts.daemonVersion ?? MAKINA_VERSION;

  await cleanupStaleSocket(opts.socketPath);

  const listener = Deno.listen({ transport: "unix", path: opts.socketPath });

  const activeConnections = new Set<Promise<void>>();
  let stopped = false;
  let stopPromise: Promise<void> | undefined;

  const acceptLoop = (async () => {
    while (!stopped) {
      let conn: Deno.Conn;
      try {
        conn = await listener.accept();
      } catch (error) {
        if (stopped) {
          return;
        }
        // BadResource is what Deno emits when `listener.close()` races the
        // accept; treat it as a normal shutdown signal.
        if (error instanceof Deno.errors.BadResource) {
          return;
        }
        onError(error, "accept");
        return;
      }
      const handled = handleConnection({
        conn,
        opts,
        daemonVersion,
        onError,
      })
        .catch((error) => onError(error, "connection"))
        .finally(() => {
          activeConnections.delete(handled);
        });
      activeConnections.add(handled);
    }
  })();

  const stop = (): Promise<void> => {
    if (stopPromise !== undefined) {
      return stopPromise;
    }
    stopPromise = (async () => {
      stopped = true;
      try {
        listener.close();
      } catch (error) {
        if (!(error instanceof Deno.errors.BadResource)) {
          onError(error, "listener-close");
        }
      }
      await acceptLoop;
      // Drain in-flight connections; each handler closes its own conn so
      // we just await every recorded promise.
      await Promise.allSettled(Array.from(activeConnections));
      try {
        await Deno.remove(opts.socketPath);
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) {
          onError(error, "socket-unlink");
        }
      }
    })();
    return stopPromise;
  };

  return { socketPath: opts.socketPath, stop };
}

/**
 * Probe `socketPath` to detect a stale socket left behind by a previous
 * unclean shutdown and unlink it so {@link Deno.listen} can rebind.
 *
 * The probe order is important:
 *  1. If the file does not exist, do nothing (the listener will create
 *     it).
 *  2. If `Deno.connect` succeeds, a live peer owns the socket — surface
 *     `Deno.errors.AddrInUse` so the caller sees a deterministic error.
 *  3. If `Deno.connect` fails (no listener answered), treat the file as
 *     stale and `Deno.remove` it.
 *
 * @param socketPath The candidate socket file to probe.
 */
async function cleanupStaleSocket(socketPath: string): Promise<void> {
  let stat: Deno.FileInfo;
  try {
    stat = await Deno.lstat(socketPath);
  } catch (error) {
    if (error instanceof Deno.errors.NotFound) {
      return;
    }
    throw error;
  }
  // Only probe regular sockets; refuse to remove a real file that happens
  // to share the path so we cannot be tricked into deleting unrelated
  // data.
  if (!stat.isSocket && !stat.isFile) {
    return;
  }
  try {
    const probe = await Deno.connect({ transport: "unix", path: socketPath });
    probe.close();
    throw new Deno.errors.AddrInUse(`socket already in use: ${socketPath}`);
  } catch (error) {
    if (error instanceof Deno.errors.AddrInUse) {
      throw error;
    }
    // No peer answered — the file is stale; remove it.
    try {
      await Deno.remove(socketPath);
    } catch (removeError) {
      if (!(removeError instanceof Deno.errors.NotFound)) {
        throw removeError;
      }
    }
  }
}

/**
 * Per-connection IPC loop. Decodes envelopes from `conn.readable`, routes
 * each one through {@link dispatch}, and writes replies and pushed events
 * back through `conn.writable`. Any subscriptions opened during the
 * connection are torn down on exit.
 */
async function handleConnection(args: {
  readonly conn: Deno.Conn;
  readonly opts: DaemonServerOptions;
  readonly daemonVersion: string;
  readonly onError: (error: unknown, context: string) => void;
}): Promise<void> {
  const { conn, opts, daemonVersion, onError } = args;
  const writer = conn.writable.getWriter();
  let writerReleased = false;
  const subscriptions = new Map<string, EventSubscription>();
  const writeMutex = new SerialMutex();

  /**
   * Serialize writes through a per-connection mutex. Multiple subscribed
   * events can be published concurrently; without serialization their
   * length-prefixed frames could interleave on the wire.
   */
  const sendEnvelope = (envelope: MessageEnvelope): Promise<void> =>
    writeMutex.run(async () => {
      if (writerReleased) {
        return;
      }
      try {
        await writer.write(encode(envelope));
      } catch (error) {
        if (isBrokenPipe(error)) {
          // Peer disappeared mid-write; silently drop the frame and let
          // the read loop unwind.
          return;
        }
        throw error;
      }
    });

  try {
    for await (const envelope of decode(conn.readable)) {
      try {
        await dispatch({
          envelope,
          opts,
          daemonVersion,
          subscriptions,
          sendEnvelope,
        });
      } catch (error) {
        onError(error, `dispatch:${envelope.type}`);
        await sendEnvelope({
          id: envelope.id,
          type: "ack",
          payload: { ok: false, error: errorMessage(error) },
        });
      }
    }
  } catch (error) {
    if (error instanceof IpcCodecError) {
      onError(error, "decode");
    } else if (!isBrokenPipe(error)) {
      onError(error, "connection-read");
    }
  } finally {
    for (const subscription of subscriptions.values()) {
      try {
        subscription.unsubscribe();
      } catch (error) {
        onError(error, "unsubscribe-on-close");
      }
    }
    subscriptions.clear();
    try {
      await writer.close();
    } catch {
      // Already closed (peer disconnect); fine.
    } finally {
      writerReleased = true;
    }
    try {
      conn.close();
    } catch {
      // Already closed; fine.
    }
  }
}

/**
 * Route a single envelope to its handler. Built-in dispatch:
 *
 *  - `ping` → reply with `pong { daemonVersion }`.
 *  - `subscribe { target }` → register an EventBus fan-out (or reply
 *    `unimplemented` when no bus was supplied).
 *  - `unsubscribe { subscriptionId }` → tear down a previously-registered
 *    fan-out (or fall back to `opts.handlers.unsubscribe`).
 *  - `command`, `prompt` → forward to `opts.handlers`; default is
 *    `unimplemented`.
 *  - Anything else is rejected with `unimplemented`.
 */
async function dispatch(args: {
  readonly envelope: MessageEnvelope;
  readonly opts: DaemonServerOptions;
  readonly daemonVersion: string;
  readonly subscriptions: Map<string, EventSubscription>;
  readonly sendEnvelope: (envelope: MessageEnvelope) => Promise<void>;
}): Promise<void> {
  const { envelope, opts, daemonVersion, subscriptions, sendEnvelope } = args;
  switch (envelope.type) {
    case "ping": {
      const reply: PongPayload = { daemonVersion };
      await sendEnvelope({ id: envelope.id, type: "pong", payload: reply });
      return;
    }
    case "subscribe": {
      const ack = registerSubscription({
        envelope,
        opts,
        subscriptions,
        sendEnvelope,
      });
      await sendEnvelope({ id: envelope.id, type: "ack", payload: ack });
      return;
    }
    case "unsubscribe": {
      const ack = removeSubscription({ envelope, subscriptions, opts });
      // If a custom handler was supplied and we did not own the
      // subscription locally, defer to it.
      if (ack === undefined) {
        await runCustomHandler({
          envelope,
          handler: opts.handlers?.unsubscribe,
          sendEnvelope,
        });
        return;
      }
      await sendEnvelope({ id: envelope.id, type: "ack", payload: ack });
      return;
    }
    case "command": {
      await runCustomHandler({
        envelope,
        handler: opts.handlers?.command,
        sendEnvelope,
      });
      return;
    }
    case "prompt": {
      await runCustomHandler({
        envelope,
        handler: opts.handlers?.prompt,
        sendEnvelope,
      });
      return;
    }
    case "pong":
    case "ack":
    case "event": {
      // These flow daemon→client; a client sending one is misbehaving.
      await sendEnvelope({
        id: envelope.id,
        type: "ack",
        payload: { ok: false, error: `unexpected message type from client: ${envelope.type}` },
      });
      return;
    }
  }
}

/**
 * Subscribe to the {@link EventBus} on behalf of the connected client.
 *
 * The envelope's `id` is used as the subscription identifier so a
 * later `unsubscribe { subscriptionId }` can find it again.
 */
function registerSubscription(args: {
  readonly envelope: Extract<MessageEnvelope, { type: "subscribe" }>;
  readonly opts: DaemonServerOptions;
  readonly subscriptions: Map<string, EventSubscription>;
  readonly sendEnvelope: (envelope: MessageEnvelope) => Promise<void>;
}): AckPayload {
  const { envelope, opts, subscriptions, sendEnvelope } = args;
  const bus = opts.eventBus;
  if (bus === undefined) {
    return UNIMPLEMENTED_ACK;
  }
  if (subscriptions.has(envelope.id)) {
    return { ok: false, error: `subscription id already in use: ${envelope.id}` };
  }
  const target = decodeSubscribeTarget(envelope.payload);
  if (target === undefined) {
    return { ok: false, error: `invalid subscribe target: ${envelope.payload.target}` };
  }
  const subscription = bus.subscribe(target, (taskEvent: TaskEvent) => {
    const eventEnvelope: MessageEnvelope = {
      id: envelope.id,
      type: "event",
      payload: taskEventToPayload(taskEvent),
    };
    // Fire-and-forget; sendEnvelope swallows broken pipes and the
    // catch keeps the bus's publish loop free of exceptions.
    sendEnvelope(eventEnvelope).catch(() => {
      // Per-event delivery failures are logged inside sendEnvelope's
      // caller; nothing else to do here.
    });
  });
  subscriptions.set(envelope.id, subscription);
  return { ok: true };
}

/**
 * Remove a subscription previously registered under
 * `envelope.payload.subscriptionId`.
 *
 * Returns the {@link AckPayload} to write back, or `undefined` if no
 * matching local subscription exists (the caller may then defer to a
 * custom handler).
 */
function removeSubscription(args: {
  readonly envelope: Extract<MessageEnvelope, { type: "unsubscribe" }>;
  readonly subscriptions: Map<string, EventSubscription>;
  readonly opts: DaemonServerOptions;
}): AckPayload | undefined {
  const { envelope, subscriptions, opts } = args;
  const subscription = subscriptions.get(envelope.payload.subscriptionId);
  if (subscription === undefined) {
    if (opts.handlers?.unsubscribe !== undefined) {
      return undefined;
    }
    return { ok: false, error: `no such subscription: ${envelope.payload.subscriptionId}` };
  }
  subscription.unsubscribe();
  subscriptions.delete(envelope.payload.subscriptionId);
  return { ok: true };
}

/**
 * Run a custom dispatch handler (or fall back to `unimplemented`) and
 * write the resulting `ack` envelope.
 */
async function runCustomHandler<TEnvelope extends MessageEnvelope>(args: {
  readonly envelope: TEnvelope;
  readonly handler: DaemonHandler<TEnvelope> | undefined;
  readonly sendEnvelope: (envelope: MessageEnvelope) => Promise<void>;
}): Promise<void> {
  const { envelope, handler, sendEnvelope } = args;
  if (handler === undefined) {
    await sendEnvelope({ id: envelope.id, type: "ack", payload: UNIMPLEMENTED_ACK });
    return;
  }
  let ack: AckPayload;
  try {
    ack = await handler(envelope);
  } catch (error) {
    ack = { ok: false, error: errorMessage(error) };
  }
  await sendEnvelope({ id: envelope.id, type: "ack", payload: ack });
}

/**
 * Convert a {@link SubscribePayload}'s `target` into the
 * {@link EventBus.subscribe} argument shape (`TaskId` or `"*"`). Returns
 * `undefined` when the target string fails the brand constructor.
 */
function decodeSubscribeTarget(payload: SubscribePayload): TaskId | "*" | undefined {
  if (payload.target === "*") {
    return "*";
  }
  try {
    return makeTaskId(payload.target);
  } catch {
    return undefined;
  }
}

/**
 * Project a {@link TaskEvent} into the {@link EventPayload} shape the
 * envelope schema requires. The two shapes are deliberately
 * structurally-identical at the field level (`taskId`, `atIso`, `kind`,
 * `data`); this helper exists to make the brand cast explicit.
 */
function taskEventToPayload(event: TaskEvent): EventPayload {
  // The brand on `taskId` is a TS-only artifact; at runtime it is a
  // string, which is what `EventPayload` expects.
  return {
    taskId: event.taskId,
    atIso: event.atIso,
    kind: event.kind,
    data: event.data,
  } as EventPayload;
}

/**
 * Whether `error` is one of the half-dozen "peer dropped the connection
 * mid-write" shapes. Treated as benign — the read loop will see EOF on
 * the next iteration and the connection will unwind cleanly.
 */
function isBrokenPipe(error: unknown): boolean {
  return (
    error instanceof Deno.errors.BrokenPipe ||
    error instanceof Deno.errors.ConnectionReset ||
    error instanceof Deno.errors.ConnectionAborted ||
    error instanceof Deno.errors.NotConnected ||
    error instanceof Deno.errors.Interrupted
  );
}

/**
 * Best-effort string projection of an arbitrary error value, used in
 * `ack { ok: false, error }` payloads.
 */
function errorMessage(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

/**
 * Default error sink. Writes a short diagnostic line to stderr; tests
 * pass a recorder.
 */
function defaultOnError(error: unknown, context: string): void {
  const message = errorMessage(error);
  console.error(`[daemon] ${context}: ${message}`);
}

/**
 * Tiny serial mutex. `run(fn)` queues `fn` after every previously-queued
 * task; the queue resolves in FIFO order even when callers `await`
 * out-of-order.
 *
 * Used per-connection to prevent interleaved length-prefixed frames on
 * the wire when concurrent producers (the read loop and the event bus
 * fan-out) race the writer.
 */
class SerialMutex {
  private tail: Promise<unknown> = Promise.resolve();

  /**
   * Run `task` after every previously-queued task on this mutex.
   *
   * @param task The work to serialize.
   * @returns A promise that resolves with `task`'s result.
   */
  run<T>(task: () => Promise<T>): Promise<T> {
    const next = this.tail.then(task, task);
    // Swallow rejections on the chain so a thrown task does not poison
    // future runs.
    this.tail = next.catch(() => undefined);
    return next;
  }
}
