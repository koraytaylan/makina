/**
 * tui/hooks/useDaemonConnection.ts — React hook that manages the TUI's
 * single daemon connection.
 *
 * The hook accepts any {@link DaemonClient} (real socket-backed or the
 * in-memory test double from `tests/helpers/in_memory_daemon_client.ts`)
 * and exposes a `{ status, connect, disconnect, subscribe, send }`
 * surface that React components and the snapshot tests both code
 * against. State transitions:
 *
 * ```text
 * idle  → connecting → connected
 * idle  → connecting → error
 * connected → disconnected (manual)
 * connected → error (read-loop failure)
 * ```
 *
 * The hook itself is transport-agnostic: it does not care whether the
 * underlying client speaks over a Unix socket or in-memory. The optional
 * `connect`/`disconnect` arguments on the client interface are honored
 * when present so the snapshot tests can use the in-memory double
 * (which is "connected" the moment it is constructed) without changes.
 *
 * @module
 */

import {
  type Dispatch,
  type SetStateAction,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";

import type { DaemonClient, DaemonEventSubscription, DaemonReply } from "../ipc-client.ts";
import type { EventPayload, MessageEnvelope } from "../../ipc/protocol.ts";

/**
 * Lifecycle states the hook walks through.
 *
 * - `idle` — no connect attempt made yet.
 * - `connecting` — `connect()` is in flight.
 * - `connected` — the client is ready for `send`/`subscribe`.
 * - `disconnected` — the caller invoked `disconnect()` cleanly.
 * - `error` — connect or read-loop failed; `lastError` carries the
 *   message.
 */
export type DaemonConnectionStatus =
  | "idle"
  | "connecting"
  | "connected"
  | "disconnected"
  | "error";

/**
 * Error raised by {@link useDaemonConnection}'s `send` when invoked
 * before the lifecycle reaches `"connected"`. Surfaced as a rejection
 * so callers can distinguish a hook-side guard from an underlying
 * transport failure.
 */
export class DaemonHookError extends Error {
  /**
   * Construct a daemon-hook error.
   *
   * @param message Human-readable description of the violation.
   */
  constructor(message: string) {
    super(message);
    this.name = "DaemonHookError";
  }
}

/**
 * The `connect`/`close` half of the {@link DaemonClient} surface that
 * `useDaemonConnection` calls when present. Both are optional so the
 * in-memory double (which is connected on construction) just satisfies
 * the {@link DaemonClient} portion.
 */
export interface ConnectionLifecycle {
  /** Open the underlying transport. Must be idempotent. */
  connect?: () => Promise<void>;
  /** Tear the underlying transport down. Must be idempotent. */
  close?: () => Promise<void>;
}

/**
 * Public surface returned by {@link useDaemonConnection}.
 *
 * Components destructure `{ status, send, subscribe, connect,
 * disconnect, lastError }` and re-render on every status change.
 */
export interface DaemonConnectionApi {
  /** Current lifecycle state. */
  readonly status: DaemonConnectionStatus;
  /**
   * Last error message, when {@link DaemonConnectionApi.status} is
   * `"error"`. `undefined` otherwise.
   */
  readonly lastError: string | undefined;
  /**
   * Send a request envelope. Rejects with a {@link DaemonHookError} if
   * `status !== "connected"` at call time; otherwise the call is
   * forwarded to the underlying client and any transport-level error
   * surfaces as the client's own rejection (typically a
   * `DaemonClientError`).
   *
   * @param envelope The envelope to send.
   * @returns The daemon's reply.
   */
  readonly send: (envelope: MessageEnvelope) => Promise<DaemonReply>;
  /**
   * Subscribe to pushed event envelopes. Returns a handle whose
   * `unsubscribe()` stops further deliveries.
   *
   * @param handler Callback invoked synchronously per event.
   */
  readonly subscribe: (
    handler: (event: EventPayload) => void,
  ) => DaemonEventSubscription;
  /** Trigger a `connect()` on the underlying client (if the client supports it). */
  readonly connect: () => Promise<void>;
  /** Trigger a `close()` on the underlying client (if the client supports it). */
  readonly disconnect: () => Promise<void>;
}

/**
 * Options accepted by {@link useDaemonConnection}.
 */
export interface UseDaemonConnectionOptions {
  /** The daemon client (real or in-memory). */
  readonly client: DaemonClient & ConnectionLifecycle;
  /**
   * If `true`, the hook calls `client.connect()` on mount automatically.
   *
   * @default true
   */
  readonly autoConnect?: boolean;
}

/**
 * React hook that wires a {@link DaemonClient} into a component's render
 * cycle.
 *
 * Two key invariants hold across renders:
 *
 * - Calling `send`/`subscribe` against the returned object is stable
 *   — the function references survive re-renders, so consumers can
 *   include them in dependency arrays without triggering effect loops.
 * - The internal client reference always tracks the latest `options.client`
 *   so subsequent `send`/`subscribe` calls hit the new instance. The
 *   one-shot `autoConnect` effect only fires on mount, however, so swapping
 *   the client at runtime does **not** trigger a fresh `connect()` — call
 *   `disconnect()`/`connect()` from the consumer if a reactive re-wire is
 *   needed. In practice no current call site passes a reactive client, so
 *   the simpler mount-only contract is the one we ship.
 *
 * @param options Hook options. See {@link UseDaemonConnectionOptions}.
 * @returns The {@link DaemonConnectionApi} the component should render
 *   against.
 *
 * @example
 * ```tsx
 * import { useDaemonConnection } from "./useDaemonConnection.ts";
 *
 * function App({ client }: { client: DaemonClient }) {
 *   const { status } = useDaemonConnection({ client });
 *   return <Text>{status}</Text>;
 * }
 * ```
 */
export function useDaemonConnection(
  options: UseDaemonConnectionOptions,
): DaemonConnectionApi {
  const { client, autoConnect = true } = options;
  const [status, setStatus] = useState<DaemonConnectionStatus>(
    client.connect === undefined ? "connected" : "idle",
  );
  const [lastError, setLastError] = useState<string | undefined>(undefined);
  const clientRef = useRef(client);

  // Keep the ref pointed at the latest client so the stable callbacks
  // below always read the current instance.
  useEffect(() => {
    clientRef.current = client;
  }, [client]);

  const connect = useCallback(async () => {
    await openConnection(clientRef.current, setStatus, setLastError);
  }, []);

  const disconnect = useCallback(async () => {
    await closeConnection(clientRef.current, setStatus, setLastError);
  }, []);

  // `statusRef` mirrors `status` so the stable `send` callback can
  // enforce the "must be connected" guarantee without re-binding on
  // every status change (which would defeat the stable-reference
  // contract documented on `DaemonConnectionApi`).
  const statusRef = useRef(status);
  useEffect(() => {
    statusRef.current = status;
  }, [status]);

  const send = useCallback((envelope: MessageEnvelope): Promise<DaemonReply> => {
    if (statusRef.current !== "connected") {
      return Promise.reject(
        new DaemonHookError(
          `useDaemonConnection.send requires status === "connected"; got ${
            JSON.stringify(statusRef.current)
          }`,
        ),
      );
    }
    return clientRef.current.send(envelope);
  }, []);

  const subscribe = useCallback(
    (handler: (event: EventPayload) => void): DaemonEventSubscription => {
      return clientRef.current.subscribeEvents(handler);
    },
    [],
  );

  useEffect(() => {
    if (!autoConnect) {
      return;
    }
    if (clientRef.current.connect === undefined) {
      return;
    }
    let cancelled = false;
    void openConnection(clientRef.current, (next) => {
      if (!cancelled) {
        setStatus(next);
      }
    }, (next) => {
      if (!cancelled) {
        setLastError(next);
      }
    });
    return () => {
      cancelled = true;
    };
    // Mount-only by design: the dep array intentionally excludes
    // `client` so a re-render with a swapped instance does not trigger
    // a fresh `connect()`. `clientRef` is updated in the effect above
    // so manual `connect()`/`disconnect()` calls and `send`/`subscribe`
    // still hit the latest instance — but the auto-wire stays a
    // one-shot. No current consumer passes a reactive client; if one
    // ever needs that, drive the lifecycle from the consumer rather
    // than re-subscribing here.
  }, [autoConnect]);

  return { status, lastError, send, subscribe, connect, disconnect };
}

/**
 * Drive `client.connect()` and reflect the outcome through the supplied
 * setters. Extracted so {@link useDaemonConnection} can call the same
 * code path from `autoConnect` and from the manually-invoked `connect`
 * callback.
 *
 * @param client The daemon client to connect.
 * @param setStatus Setter for the lifecycle status.
 * @param setLastError Setter for the last-error message.
 */
async function openConnection(
  client: DaemonClient & ConnectionLifecycle,
  setStatus: Dispatch<SetStateAction<DaemonConnectionStatus>>,
  setLastError: Dispatch<SetStateAction<string | undefined>>,
): Promise<void> {
  if (client.connect === undefined) {
    setStatus("connected");
    setLastError(undefined);
    return;
  }
  setStatus("connecting");
  setLastError(undefined);
  try {
    await client.connect();
    setStatus("connected");
  } catch (error) {
    setStatus("error");
    setLastError(error instanceof Error ? error.message : String(error));
  }
}

/**
 * Drive `client.close()` and reflect the outcome through the supplied
 * setters. The lifecycle becomes `"disconnected"` on success and
 * `"error"` if `close()` itself rejects.
 *
 * @param client The daemon client to close.
 * @param setStatus Setter for the lifecycle status.
 * @param setLastError Setter for the last-error message.
 */
async function closeConnection(
  client: DaemonClient & ConnectionLifecycle,
  setStatus: Dispatch<SetStateAction<DaemonConnectionStatus>>,
  setLastError: Dispatch<SetStateAction<string | undefined>>,
): Promise<void> {
  if (client.close === undefined) {
    // Mirror the success path of the lifecycle-aware branch below: a
    // clean disconnect leaves no lingering error message even when
    // the underlying client lacks a `close` to call (in-memory
    // double, etc.). Without the reset, an earlier transition into
    // `"error"` would still surface its message in the status bar
    // after the caller asked to disconnect cleanly.
    setStatus("disconnected");
    setLastError(undefined);
    return;
  }
  try {
    await client.close();
    setStatus("disconnected");
    setLastError(undefined);
  } catch (error) {
    setStatus("error");
    setLastError(error instanceof Error ? error.message : String(error));
  }
}
