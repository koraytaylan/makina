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

import { Box, render, useInput } from "ink";
import { type ReactElement, useCallback, useEffect, useMemo, useState } from "react";

import { CommandPalette } from "./components/CommandPalette.tsx";
import { Header } from "./components/Header.tsx";
import { MainPane } from "./components/MainPane.tsx";
import { StatusBar } from "./components/StatusBar.tsx";
import { TaskSwitcher } from "./components/TaskSwitcher.tsx";
import { type DaemonClient, type DaemonReply, SocketDaemonClient } from "./ipc-client.ts";
import { type ConnectionLifecycle, useDaemonConnection } from "./hooks/useDaemonConnection.ts";
import { useFocusedTask } from "./hooks/useFocusedTask.ts";
import { matchesKeybinding } from "./keybindings.ts";
import type { CommandPayload, EventPayload, MessageEnvelope } from "@makina/core";
import { makeTaskId, type TaskId } from "@makina/core";
import {
  COMMAND_PALETTE_HISTORY_LIMIT,
  DEFAULT_COMMAND_PALETTE_KEYBINDING,
  DEFAULT_TASK_SWITCHER_KEYBINDING,
  MAKINA_VERSION,
  STATUS_BAR_TRUNCATION_WIDTH_CODE_UNITS,
} from "@makina/core";

/**
 * Subset of TUI keybindings the App reads. Only the overlay toggles
 * are needed here; future keybindings (focus next task, send prompt,
 * …) extend this interface alongside their config schema entries.
 */
export interface AppKeybindings {
  /** Chord that toggles the command-palette overlay. */
  readonly commandPalette: string;
  /** Chord that toggles the task-switcher overlay. */
  readonly taskSwitcher: string;
}

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
   * Initial focused-task id. Forwarded to {@link useFocusedTask} so the
   * Header's focus pill renders deterministically from snapshot tests.
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
  /**
   * Optional keybindings override. Defaults to the constants
   * mirrored from `src/config/schema.ts`. Production callers should
   * forward the loaded `Config.tui.keybindings`; snapshot tests pin
   * the defaults.
   *
   * @default { commandPalette: "ctrl+p", taskSwitcher: "ctrl+g" }
   */
  readonly keybindings?: AppKeybindings | undefined;
  /**
   * If `true`, Ink's `useInput` hooks at the App level are wired up.
   * Snapshot tests render with `inputEnabled={false}` so Ink does
   * not race the Deno test sanitizer with stdin readers.
   *
   * @default true
   */
  readonly inputEnabled?: boolean | undefined;
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
    keybindings = {
      commandPalette: DEFAULT_COMMAND_PALETTE_KEYBINDING,
      taskSwitcher: DEFAULT_TASK_SWITCHER_KEYBINDING,
    },
    inputEnabled = true,
  } = props;
  const connection = useDaemonConnection({
    client,
    autoConnect,
  });
  const focus = useFocusedTask({
    source: client,
    initialFocusedTaskId,
  });
  const [knownTaskIds, setKnownTaskIds] = useState<ReadonlySet<TaskId>>(() => new Set());
  const [lastMessage, setLastMessage] = useState<string | undefined>(undefined);
  const [lastError, setLastError] = useState<string | undefined>(undefined);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [switcherOpen, setSwitcherOpen] = useState(false);
  const [paletteHistory, setPaletteHistory] = useState<readonly string[]>([]);

  useEffect(() => {
    const subscription = connection.subscribe((event) => {
      handleEvent(event, {
        setKnownTaskIds,
        setLastMessage,
        setLastError,
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

  const overlayOpen = paletteOpen || switcherOpen;

  // Top-level chord handler: toggles the overlays per the configured
  // keybindings. Disabled while an overlay is already open so the
  // overlay's own input handler owns the keystroke (Esc closes;
  // typing into the palette is forwarded as text).
  useInput(
    (input, key) => {
      if (!inputEnabled || overlayOpen) {
        return;
      }
      if (matchesKeybinding(keybindings.commandPalette, input, key)) {
        setPaletteOpen(true);
        return;
      }
      if (matchesKeybinding(keybindings.taskSwitcher, input, key)) {
        setSwitcherOpen(true);
      }
    },
    { isActive: inputEnabled && !overlayOpen },
  );

  const handlePaletteSubmit = useCallback(
    (payload: CommandPayload, raw: string) => {
      setPaletteHistory((previous) => trimHistory([raw, ...previous]));
      setPaletteOpen(false);
      // Fire-and-forget: the App reflects the dispatch in the status
      // bar; per-command lifecycle is owned by the relevant wave.
      void dispatchCommand((envelope) => connection.send(envelope), payload).then(
        (result) => {
          if (result.kind === "error") {
            setLastError(result.message);
            return;
          }
          setLastMessage(result.message);
        },
        // The send is wrapped in a success/error result, so the
        // promise itself only rejects on truly exceptional errors;
        // surface those to the status bar.
        (error) => {
          setLastError(
            error instanceof Error ? error.message : String(error),
          );
        },
      );
    },
    [connection.send],
  );

  const handlePaletteClose = useCallback(() => {
    setPaletteOpen(false);
  }, []);

  const handleSwitcherPick = useCallback(
    (taskId: TaskId) => {
      focus.focusTask(taskId);
      setSwitcherOpen(false);
    },
    [focus],
  );

  const handleSwitcherClose = useCallback(() => {
    setSwitcherOpen(false);
  }, []);

  const keybindingsHint =
    `${keybindings.commandPalette} palette · ${keybindings.taskSwitcher} switcher · Ctrl+C exit`;

  return (
    <Box flexDirection="column">
      <Header
        version={version}
        status={connection.status}
        focusedTaskId={focus.focusedTaskId}
      />
      <MainPane
        activeTaskCount={Math.max(knownTaskIds.size, focus.tasks.length)}
        hint={hint}
      />
      {paletteOpen
        ? (
          <CommandPalette
            history={paletteHistory}
            onSubmit={handlePaletteSubmit}
            onClose={handlePaletteClose}
            inputEnabled={inputEnabled}
          />
        )
        : null}
      {switcherOpen
        ? (
          <TaskSwitcher
            tasks={focus.tasks}
            focusedTaskId={focus.focusedTaskId}
            onPick={handleSwitcherPick}
            onClose={handleSwitcherClose}
            inputEnabled={inputEnabled}
          />
        )
        : null}
      <StatusBar
        message={lastMessage ?? "ready"}
        errorMessage={lastError ?? connection.lastError}
        keybindingsHint={keybindingsHint}
      />
    </Box>
  );
}

/**
 * Trim `entries` to {@link COMMAND_PALETTE_HISTORY_LIMIT}, keeping the
 * head (most-recent) entries. Exported for unit-test reuse.
 *
 * @param entries Candidate history entries.
 * @returns The trimmed array.
 */
export function trimHistory(entries: readonly string[]): readonly string[] {
  if (entries.length <= COMMAND_PALETTE_HISTORY_LIMIT) {
    return entries;
  }
  return entries.slice(0, COMMAND_PALETTE_HISTORY_LIMIT);
}

/**
 * Outcome reported back to {@link App}'s status bar after a palette
 * submission. Exported so the unit tests around
 * {@link dispatchCommand} can pin the discriminant.
 */
export interface DispatchOutcome {
  /** `"ok"` for an accepted command; `"error"` for a refusal. */
  readonly kind: "ok" | "error";
  /** Human-readable status-bar message. */
  readonly message: string;
}

/**
 * Send a parsed command envelope to the daemon and translate the
 * reply into a status-bar message. Exported for unit-test reuse.
 *
 * @param send The {@link useDaemonConnection} `send` callback.
 * @param payload The parsed command payload.
 * @returns The status-bar outcome.
 */
export async function dispatchCommand(
  send: (envelope: MessageEnvelope) => Promise<DaemonReply>,
  payload: CommandPayload,
): Promise<DispatchOutcome> {
  const envelope: MessageEnvelope = {
    id: makeCommandEnvelopeId(),
    type: "command",
    payload,
  };
  try {
    const reply = await send(envelope);
    if (reply.type !== "ack") {
      return {
        kind: "error",
        message: `unexpected reply for /${payload.name}: ${reply.type}`,
      };
    }
    if (reply.payload.ok === false) {
      return {
        kind: "error",
        message: reply.payload.error ?? `/${payload.name} refused`,
      };
    }
    return { kind: "ok", message: `/${payload.name} dispatched` };
  } catch (error) {
    return {
      kind: "error",
      message: error instanceof Error ? error.message : String(error),
    };
  }
}

/**
 * Mint a fresh envelope id for a command dispatch.
 *
 * Uses `crypto.randomUUID()` so collisions cannot happen in practice;
 * isolated to a helper so tests that stub the random source have a
 * single seam.
 *
 * @returns The envelope id.
 */
function makeCommandEnvelopeId(): string {
  return `cmd-${crypto.randomUUID()}`;
}

/**
 * Setters {@link handleEvent} reaches into when an event arrives. The
 * indirection keeps the event handler easy to unit-test: the production
 * call site passes React `useState` setters, while tests pass plain
 * closures that capture into local variables.
 *
 * Wave 3 lifted the focused-task slot into {@link useFocusedTask}, so
 * the handler no longer reaches in here. The remaining slots are owned
 * by the App component: known-task tally, last log/error.
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
 * Production launch entrypoint wired in by `main.ts`.
 *
 * Builds a {@link SocketDaemonClient} pointing at `socketPath` and
 * renders the {@link App} into Ink's default `process.stdout` stream.
 * Ink's `exitOnCtrlC` flag handles Ctrl+C natively — Deno-level SIGINT
 * listeners do not fire while Ink holds stdin in raw mode (the kernel's
 * `^C → SIGINT` translation is suppressed by the raw-mode termios), so
 * we let Ink intercept the byte and call `app.exit()` itself.
 *
 * @param overrides Socket path (must match the daemon's bind path) and
 *   an optional client (the latter exists so an integration test can
 *   drive the real render with a non-default client).
 * @returns A promise that settles when the app exits.
 *
 * @example
 * ```ts
 * import { launch } from "./src/tui/App.tsx";
 * await launch({ socketPath: "/tmp/makina.sock" });
 * ```
 */
export async function launch(overrides: {
  readonly socketPath: string;
  readonly client?: DaemonClient & ConnectionLifecycle;
}): Promise<void> {
  const { socketPath } = overrides;
  const client = overrides.client ?? new SocketDaemonClient(socketPath);
  const instance = render(<App client={client} />, {
    exitOnCtrlC: true,
    patchConsole: false,
  });
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
