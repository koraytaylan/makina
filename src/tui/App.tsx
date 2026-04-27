/**
 * tui/App.tsx — top-level Ink component for the makina TUI.
 *
 * Holds the daemon connection (via {@link useDaemonConnection}), the
 * focused-task id, and the per-task event tally that drives the main
 * pane's "1 task active" copy. Composes the three baseline panes
 * (`Header`, `MainPane`, `StatusBar`).
 *
 * The exported {@link launch} function wires `main.ts` to a real
 * socket-backed client. Snapshot tests render {@link App} directly with
 * the in-memory double from `tests/helpers/in_memory_daemon_client.ts`.
 *
 * @module
 */

import { Box, render, Text, useApp } from "ink";
import { type ReactElement, useEffect, useMemo, useState } from "react";

import { Header } from "./components/Header.tsx";
import { MainPane } from "./components/MainPane.tsx";
import { StatusBar } from "./components/StatusBar.tsx";
import { type DaemonClient, SocketDaemonClient } from "./ipc-client.ts";
import { type ConnectionLifecycle, useDaemonConnection } from "./hooks/useDaemonConnection.ts";
import type { EventPayload } from "../ipc/protocol.ts";
import { makeTaskId, type TaskId } from "../types.ts";
import { MAKINA_VERSION, STATUS_BAR_TRUNCATION_WIDTH_CODE_UNITS } from "../constants.ts";

/**
 * Default Unix-socket path the daemon listens on.
 *
 * The path is intentionally local to {@link launch}; setup wizard /
 * config loader can supply a different one.
 */
const DEFAULT_DAEMON_SOCKET_PATH = `${Deno.env.get("HOME") ?? "/tmp"}/.makina/daemon.sock`;

/**
 * Props accepted by {@link App}.
 */
export interface AppProps {
  /**
   * The daemon client. Real socket-backed in production; the in-memory
   * double from `tests/helpers/in_memory_daemon_client.ts` in tests.
   */
  readonly client: DaemonClient & ConnectionLifecycle;
  /**
   * Initial focused-task id. Wave 3 wires the task switcher; until
   * then the value is provided externally for snapshot determinism.
   */
  readonly initialFocusedTaskId?: TaskId | undefined;
  /**
   * Optional override for the version string rendered in the header.
   * Defaults to {@link MAKINA_VERSION}.
   */
  readonly version?: string | undefined;
  /**
   * If `true`, hooks call `client.connect()` automatically on mount.
   * Set to `false` in tests that drive the lifecycle by hand.
   *
   * @default true
   */
  readonly autoConnect?: boolean | undefined;
}

/**
 * The top-level TUI component.
 *
 * Subscribes to the daemon's pushed events on mount; updates the active-
 * task tally and the most recent log/error message on every event.
 *
 * @param props See {@link AppProps}.
 * @returns The composed TUI tree.
 *
 * @example
 * ```tsx
 * <App client={new SocketDaemonClient(socketPath)} />
 * ```
 */
export function App(props: AppProps): ReactElement {
  const {
    client,
    initialFocusedTaskId,
    version = MAKINA_VERSION,
    autoConnect = true,
  } = props;
  const connection = useDaemonConnection({
    client,
    autoConnect,
  });
  const [knownTaskIds, setKnownTaskIds] = useState<ReadonlySet<TaskId>>(() => new Set());
  const [lastMessage, setLastMessage] = useState<string | undefined>(undefined);
  const [lastError, setLastError] = useState<string | undefined>(undefined);
  const [focusedTaskId, setFocusedTaskId] = useState<TaskId | undefined>(initialFocusedTaskId);

  useEffect(() => {
    const subscription = connection.subscribe((event) => {
      handleEvent(event, {
        setKnownTaskIds,
        setLastMessage,
        setLastError,
        setFocusedTaskId,
      });
    });
    return () => {
      subscription.unsubscribe();
    };
  }, [connection.subscribe]);

  const hint = useMemo(() => {
    switch (connection.status) {
      case "idle":
      case "connecting":
        return "Connecting to daemon…";
      case "disconnected":
        return "Daemon disconnected. Press R to reconnect (W3).";
      case "error":
        return `Connection error: ${connection.lastError ?? "unknown"}`;
      case "connected":
        return undefined;
    }
  }, [connection.status, connection.lastError]);

  return (
    <Box flexDirection="column">
      <Header
        version={version}
        status={connection.status}
        focusedTaskId={focusedTaskId}
      />
      <MainPane
        activeTaskCount={knownTaskIds.size}
        hint={hint}
      />
      <StatusBar
        message={lastMessage ?? "ready"}
        errorMessage={lastError ?? connection.lastError}
      />
    </Box>
  );
}

/**
 * Setters {@link handleEvent} reaches into when an event arrives. The
 * indirection keeps the event handler easy to unit-test: the production
 * call site passes React `useState` setters, while tests pass plain
 * closures that capture into local variables.
 *
 * `setFocusedTaskId` accepts a functional updater so the handler can
 * apply "auto-focus only when nothing is focused yet" without reading
 * the current focused-task value out of band — the React setter and
 * the test stubs both honor the closure-style update.
 */
export interface EventSetters {
  /** Setter for the known-tasks set used by `MainPane`. */
  readonly setKnownTaskIds: (
    update: (previous: ReadonlySet<TaskId>) => ReadonlySet<TaskId>,
  ) => void;
  /** Setter for the most-recent log message rendered in the status bar. */
  readonly setLastMessage: (next: string | undefined) => void;
  /** Setter for the most-recent error message rendered in the status bar. */
  readonly setLastError: (next: string | undefined) => void;
  /**
   * Setter for the focused task id rendered in the header.
   *
   * Receives a functional updater so {@link handleEvent} can preserve
   * an existing focus instead of clobbering it on every event.
   */
  readonly setFocusedTaskId: (
    update: (previous: TaskId | undefined) => TaskId | undefined,
  ) => void;
}

/**
 * Apply a single pushed event to the App's state.
 *
 * @param event The event payload.
 * @param setters The state setters captured by {@link App}.
 */
export function handleEvent(event: EventPayload, setters: EventSetters): void {
  let taskId: TaskId | undefined;
  try {
    taskId = makeTaskId(event.taskId);
  } catch {
    // A malformed task id should not crash the renderer; surface it as
    // an error and ignore the event otherwise.
    setters.setLastError(`event with invalid task id: ${event.taskId}`);
    return;
  }
  setters.setKnownTaskIds((previous) => {
    if (previous.has(taskId)) {
      return previous;
    }
    const next = new Set(previous);
    next.add(taskId);
    return next;
  });
  // Auto-focus the first task we hear about so the header's focus pill
  // stops saying "no task focused" once events arrive. Subsequent
  // events leave the focused task alone — Wave 3's task switcher will
  // own focus changes — so the focus does not jump around as new
  // tasks appear.
  setters.setFocusedTaskId((previous) => previous ?? taskId);
  switch (event.kind) {
    case "log":
      setters.setLastMessage(`[${event.data.level}] ${event.data.message}`);
      break;
    case "state-changed":
      setters.setLastMessage(
        `${taskId}: ${event.data.fromState} → ${event.data.toState}`,
      );
      break;
    case "agent-message":
      setters.setLastMessage(
        `agent ${event.data.role}: ${
          truncate(event.data.text, STATUS_BAR_TRUNCATION_WIDTH_CODE_UNITS)
        }`,
      );
      break;
    case "github-call":
      setters.setLastMessage(
        `github ${event.data.method} ${event.data.endpoint}`,
      );
      break;
    case "error":
      setters.setLastError(event.data.message);
      break;
  }
}

/**
 * Truncate `text` to at most `limit` UTF-16 code units, appending an
 * ellipsis when shortened. Used by {@link handleEvent} so a long agent
 * message does not overflow the status bar.
 *
 * The cut is by code units, not graphemes — a surrogate pair or a
 * grapheme cluster that straddles the boundary will be split. The
 * status bar consumes the result through Ink's text renderer, which
 * tolerates a stray half-surrogate (it renders as the replacement
 * character), so the render never crashes; the worst case is one
 * malformed code point at the cut. Callers that need cluster-aware
 * truncation should reach for `Intl.Segmenter` themselves.
 *
 * @param text The text to truncate.
 * @param limit The maximum length, in UTF-16 code units.
 * @returns The truncated string.
 */
function truncate(text: string, limit: number): string {
  if (text.length <= limit) {
    return text;
  }
  return `${text.slice(0, Math.max(0, limit - 1))}…`;
}

/**
 * Inner component used by {@link launch} to register a Ctrl+C exit
 * handler against Ink's app context. Kept separate so {@link App}
 * remains a pure render component (and therefore trivial to test).
 */
function CtrlCExit(): ReactElement {
  const app = useApp();
  useEffect(() => {
    const onSignal = () => {
      app.exit();
    };
    Deno.addSignalListener("SIGINT", onSignal);
    return () => {
      try {
        Deno.removeSignalListener("SIGINT", onSignal);
      } catch {
        // Already removed.
      }
    };
  }, [app]);
  return <Text></Text>;
}

/**
 * Production launch entrypoint wired in by `main.ts`.
 *
 * Builds a {@link SocketDaemonClient} pointing at the default daemon
 * socket and renders the {@link App} into Ink's default `process.stdout`
 * stream. The resolved promise settles when the app exits (Ctrl+C or
 * `useApp().exit()`).
 *
 * @param overrides Optional overrides for the socket path and the
 *   client (the latter exists so an integration test can drive the real
 *   render with a non-default client).
 * @returns A promise that settles when the app exits.
 *
 * @example
 * ```ts
 * import { launch } from "./src/tui/App.tsx";
 * await launch();
 * ```
 */
export async function launch(overrides?: {
  readonly socketPath?: string;
  readonly client?: DaemonClient & ConnectionLifecycle;
}): Promise<void> {
  const socketPath = overrides?.socketPath ?? DEFAULT_DAEMON_SOCKET_PATH;
  const client = overrides?.client ?? new SocketDaemonClient(socketPath);
  const instance = render(
    <Box flexDirection="column">
      <App client={client} />
      <CtrlCExit />
    </Box>,
    { exitOnCtrlC: false, patchConsole: false },
  );
  try {
    await instance.waitUntilExit();
  } finally {
    if (typeof client.close === "function") {
      await client.close().catch(() => {
        // Already closed; nothing to do.
      });
    }
  }
}
