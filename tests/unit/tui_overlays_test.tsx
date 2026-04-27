/**
 * Snapshot + unit tests for the Wave 3 overlays.
 *
 * Two overlays land here:
 *   - {@link CommandPalette} — autocomplete + history + parse error.
 *   - {@link TaskSwitcher} — 0 / 1 / N tasks with state badges + ages.
 *
 * The test fixtures are pure props-driven renders (Ink's `useInput`
 * hooks are wired but disabled, so the snapshot output is stable
 * across the Deno sanitizer's strict op accounting). Each overlay's
 * `inputEnabled={false}` path is exercised by the snapshot tests, and
 * a small set of in-memory direct-keystroke tests drives the
 * stateful behaviour of `TaskSwitcher` without touching Ink's stdin.
 */

import { assertEquals } from "@std/assert";
import { assertSnapshot } from "@std/testing/snapshot";
import { Box, render as inkRender } from "ink";

import { dispatchCommand, trimHistory } from "../../src/tui/App.tsx";
import { CommandPalette, filterSuggestions } from "../../src/tui/components/CommandPalette.tsx";
import { TaskSwitcher } from "../../src/tui/components/TaskSwitcher.tsx";
import type { MessageEnvelope } from "../../src/ipc/protocol.ts";
import type { DaemonReply } from "../../src/tui/ipc-client.ts";
import { COMMAND_PALETTE_HISTORY_LIMIT } from "../../src/constants.ts";
import {
  formatAgeBetween,
  formatTaskAge,
  handleFocusedTaskEvent,
  type TaskSummary,
} from "../../src/tui/hooks/useFocusedTask.ts";
import {
  KeybindingParseError,
  matchesKeybinding,
  parseKeybinding,
} from "../../src/tui/keybindings.ts";
import { makeTaskId, type TaskId } from "../../src/types.ts";
import { SLASH_COMMAND_NAMES } from "../../src/tui/slash-command-parser.ts";

// ---------------------------------------------------------------------------
// Fake stdout — mirrors the harness used in `tui_shell_test.tsx`.
// ---------------------------------------------------------------------------

/**
 * Minimal `NodeJS.WriteStream`-shaped object that captures every
 * `write()` call. Identical to the shell-test harness; copied here
 * rather than exported so the two test files can evolve independently.
 */
class FakeStream {
  /** Width of the simulated terminal. */
  columns = 80;
  /** Height of the simulated terminal. */
  rows = 24;
  /** Tells Ink to render with terminal escapes. */
  isTTY = true;
  /** Recorded frame buffer. */
  readonly frames: string[] = [];
  // deno-lint-ignore no-explicit-any
  private readonly listeners = new Map<string, Array<(...args: any[]) => void>>();

  /**
   * Capture a frame. Mimics Node's `WriteStream.write` contract.
   *
   * @param chunk The frame text.
   * @returns Always `true`.
   */
  write(chunk: string): boolean {
    this.frames.push(String(chunk));
    return true;
  }
  /** No-op; Ink occasionally batches writes via cork/uncork. */
  cork(): void {}
  /** No-op; pair of {@link FakeStream.cork}. */
  uncork(): void {}
  /** No-op; some terminals close the stream when done. */
  end(): void {}
  /**
   * Register a Node-style event listener.
   *
   * @param event The event name.
   * @param handler The callback to register.
   * @returns This stream, for chaining.
   */
  // deno-lint-ignore no-explicit-any
  on(event: string, handler: (...args: any[]) => void): this {
    const list = this.listeners.get(event) ?? [];
    list.push(handler);
    this.listeners.set(event, list);
    return this;
  }
  /**
   * Remove a Node-style event listener.
   *
   * @param event The event name.
   * @param handler The callback to remove.
   * @returns This stream, for chaining.
   */
  // deno-lint-ignore no-explicit-any
  off(event: string, handler: (...args: any[]) => void): this {
    const list = this.listeners.get(event);
    if (list === undefined) return this;
    const index = list.indexOf(handler);
    if (index >= 0) list.splice(index, 1);
    return this;
  }
  /**
   * The most recent frame containing visible content.
   *
   * @returns The newest frame.
   */
  lastVisibleFrame(): string {
    const escape = String.fromCharCode(0x1B);
    const csiRegex = new RegExp(`${escape}\\[[0-9?;]*[a-zA-Z]`, "g");
    for (let index = this.frames.length - 1; index >= 0; index -= 1) {
      const frame = this.frames[index];
      if (frame === undefined) continue;
      const stripped = frame.replace(csiRegex, "");
      if (stripped.trim().length > 0) {
        return frame;
      }
    }
    return "";
  }
}

/**
 * Render an Ink element to a fake stdout.
 *
 * @param node The element to render.
 * @returns The frame recorder.
 */
async function renderToBuffer(
  node: Parameters<typeof inkRender>[0],
): Promise<FakeStream> {
  const stdout = new FakeStream();
  const stderr = new FakeStream();
  const instance = inkRender(node, {
    // deno-lint-ignore no-explicit-any
    stdout: stdout as any,
    // deno-lint-ignore no-explicit-any
    stderr: stderr as any,
    debug: true,
    exitOnCtrlC: false,
    patchConsole: false,
  });
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  instance.unmount();
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  return stdout;
}

// ---------------------------------------------------------------------------
// CommandPalette snapshots
// ---------------------------------------------------------------------------

Deno.test({
  name: "CommandPalette: empty input shows every spec",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async (t) => {
    const stdout = await renderToBuffer(
      <CommandPalette
        onSubmit={() => {}}
        onClose={() => {}}
        inputEnabled={false}
      />,
    );
    await assertSnapshot(t, stdout.lastVisibleFrame());
  },
});

Deno.test({
  name: "CommandPalette: filtered to a single command by typed prefix",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async (t) => {
    const stdout = await renderToBuffer(
      <CommandPalette
        onSubmit={() => {}}
        onClose={() => {}}
        inputEnabled={false}
        initialInput="/iss"
      />,
    );
    await assertSnapshot(t, stdout.lastVisibleFrame());
  },
});

Deno.test({
  name: "CommandPalette: closed = unmounted (no overlay markup rendered)",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async (t) => {
    // Rendering no palette results in only a wrapper, but the snapshot
    // pins the absence of any palette markup so a regression that
    // accidentally renders the palette unconditionally trips this.
    const stdout = await renderToBuffer(<Box flexDirection="column" />);
    await assertSnapshot(t, stdout.lastVisibleFrame());
  },
});

Deno.test({
  name: "CommandPalette: history seed reports the count",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async (t) => {
    const stdout = await renderToBuffer(
      <CommandPalette
        onSubmit={() => {}}
        onClose={() => {}}
        inputEnabled={false}
        history={["/issue 42", "/status", "/help"]}
      />,
    );
    await assertSnapshot(t, stdout.lastVisibleFrame());
  },
});

Deno.test("CommandPalette: filterSuggestions returns every spec for an empty input", () => {
  assertEquals(filterSuggestions("").length, SLASH_COMMAND_NAMES.length);
  assertEquals(filterSuggestions("  ").length, SLASH_COMMAND_NAMES.length);
  assertEquals(filterSuggestions("/").length, SLASH_COMMAND_NAMES.length);
});

Deno.test("CommandPalette: filterSuggestions narrows by prefix", () => {
  const suggestions = filterSuggestions("/iss");
  assertEquals(suggestions.length, 1);
  assertEquals(suggestions[0]?.name, "issue");
});

Deno.test("CommandPalette: filterSuggestions keeps every match for a shared prefix", () => {
  // "re" is shared by /repo and /retry, so both should be returned.
  const suggestions = filterSuggestions("/re");
  const names = suggestions.map((spec) => spec.name);
  assertEquals(names.includes("repo"), true);
  assertEquals(names.includes("retry"), true);
});

Deno.test("CommandPalette: filterSuggestions returns empty for an unknown stem", () => {
  assertEquals(filterSuggestions("/sneeze").length, 0);
});

// ---------------------------------------------------------------------------
// TaskSwitcher snapshots
// ---------------------------------------------------------------------------

const FIXED_NOW = new Date("2026-04-26T12:05:00.000Z");

Deno.test({
  name: "TaskSwitcher: zero tasks renders the empty-state hint",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async (t) => {
    const stdout = await renderToBuffer(
      <TaskSwitcher
        tasks={[]}
        onPick={() => {}}
        onClose={() => {}}
        inputEnabled={false}
        now={FIXED_NOW}
      />,
    );
    await assertSnapshot(t, stdout.lastVisibleFrame());
  },
});

Deno.test({
  name: "TaskSwitcher: single task with focus pill",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async (t) => {
    const taskId = makeTaskId("task_2026-04-26T12-00-00_abc123");
    const tasks: readonly TaskSummary[] = [
      {
        id: taskId,
        state: "DRAFTING",
        firstSeenAtIso: "2026-04-26T12:00:00.000Z",
        lastSeenAtIso: "2026-04-26T12:04:30.000Z",
      },
    ];
    const stdout = await renderToBuffer(
      <TaskSwitcher
        tasks={tasks}
        focusedTaskId={taskId}
        onPick={() => {}}
        onClose={() => {}}
        inputEnabled={false}
        now={FIXED_NOW}
      />,
    );
    await assertSnapshot(t, stdout.lastVisibleFrame());
  },
});

Deno.test({
  name: "TaskSwitcher: N tasks with assorted state badges",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async (t) => {
    const tasks: readonly TaskSummary[] = [
      {
        id: makeTaskId("task_alpha"),
        state: "STABILIZING",
        firstSeenAtIso: "2026-04-26T12:00:00.000Z",
        lastSeenAtIso: "2026-04-26T12:04:00.000Z",
      },
      {
        id: makeTaskId("task_beta"),
        state: "READY_TO_MERGE",
        firstSeenAtIso: "2026-04-26T11:50:00.000Z",
        lastSeenAtIso: "2026-04-26T12:01:00.000Z",
      },
      {
        id: makeTaskId("task_gamma"),
        state: "FAILED",
        firstSeenAtIso: "2026-04-26T11:00:00.000Z",
        lastSeenAtIso: "2026-04-26T11:30:00.000Z",
      },
    ];
    const stdout = await renderToBuffer(
      <TaskSwitcher
        tasks={tasks}
        focusedTaskId={makeTaskId("task_beta")}
        onPick={() => {}}
        onClose={() => {}}
        inputEnabled={false}
        now={FIXED_NOW}
      />,
    );
    await assertSnapshot(t, stdout.lastVisibleFrame());
  },
});

// ---------------------------------------------------------------------------
// useFocusedTask — direct unit tests against handleFocusedTaskEvent
// ---------------------------------------------------------------------------

Deno.test("handleFocusedTaskEvent: state-changed updates the per-task summary", () => {
  let map: ReadonlyMap<TaskId, TaskSummary> = new Map();
  let focused: TaskId | undefined;
  handleFocusedTaskEvent(
    {
      taskId: "t1",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "state-changed",
      data: { fromState: "INIT", toState: "DRAFTING" },
    },
    {
      setTaskMap: (update) => {
        map = update(map);
      },
      setFocusedTaskId: (update) => {
        focused = update(focused);
      },
    },
  );
  const summary = map.get(makeTaskId("t1"));
  assertEquals(summary?.state, "DRAFTING");
  assertEquals(summary?.firstSeenAtIso, "2026-04-26T12:00:00.000Z");
  assertEquals(summary?.lastSeenAtIso, "2026-04-26T12:00:00.000Z");
  assertEquals(focused, makeTaskId("t1"));
});

Deno.test("handleFocusedTaskEvent: log event preserves the state and bumps lastSeen", () => {
  let map: ReadonlyMap<TaskId, TaskSummary> = new Map([[
    makeTaskId("t1"),
    {
      id: makeTaskId("t1"),
      state: "DRAFTING",
      firstSeenAtIso: "2026-04-26T12:00:00.000Z",
      lastSeenAtIso: "2026-04-26T12:00:00.000Z",
    },
  ]]);
  handleFocusedTaskEvent(
    {
      taskId: "t1",
      atIso: "2026-04-26T12:01:00.000Z",
      kind: "log",
      data: { level: "info", message: "tick" },
    },
    {
      setTaskMap: (update) => {
        map = update(map);
      },
      setFocusedTaskId: () => {},
    },
  );
  const summary = map.get(makeTaskId("t1"));
  assertEquals(summary?.state, "DRAFTING");
  assertEquals(summary?.firstSeenAtIso, "2026-04-26T12:00:00.000Z");
  assertEquals(summary?.lastSeenAtIso, "2026-04-26T12:01:00.000Z");
});

Deno.test("handleFocusedTaskEvent: focused task does not jump on subsequent events", () => {
  let map: ReadonlyMap<TaskId, TaskSummary> = new Map();
  let focused: TaskId | undefined;
  handleFocusedTaskEvent(
    {
      taskId: "t1",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "log",
      data: { level: "info", message: "first" },
    },
    {
      setTaskMap: (update) => {
        map = update(map);
      },
      setFocusedTaskId: (update) => {
        focused = update(focused);
      },
    },
  );
  handleFocusedTaskEvent(
    {
      taskId: "t2",
      atIso: "2026-04-26T12:00:30.000Z",
      kind: "log",
      data: { level: "info", message: "second" },
    },
    {
      setTaskMap: (update) => {
        map = update(map);
      },
      setFocusedTaskId: (update) => {
        focused = update(focused);
      },
    },
  );
  assertEquals(focused, makeTaskId("t1"));
  assertEquals(map.size, 2);
});

Deno.test("handleFocusedTaskEvent: malformed task id is ignored without crashing", () => {
  let map: ReadonlyMap<TaskId, TaskSummary> = new Map();
  let focused: TaskId | undefined;
  handleFocusedTaskEvent(
    {
      taskId: "  ",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "log",
      data: { level: "info", message: "skip" },
    },
    {
      setTaskMap: (update) => {
        map = update(map);
      },
      setFocusedTaskId: (update) => {
        focused = update(focused);
      },
    },
  );
  assertEquals(map.size, 0);
  assertEquals(focused, undefined);
});

// ---------------------------------------------------------------------------
// formatTaskAge / formatAgeBetween
// ---------------------------------------------------------------------------

Deno.test("formatTaskAge: seconds-granular near the present", () => {
  const summary: TaskSummary = {
    id: makeTaskId("t1"),
    state: undefined,
    firstSeenAtIso: "2026-04-26T12:00:00.000Z",
    lastSeenAtIso: "2026-04-26T12:00:00.000Z",
  };
  assertEquals(formatTaskAge(summary, new Date("2026-04-26T12:00:05.000Z")), "5s");
});

Deno.test("formatAgeBetween: minutes / hours / days", () => {
  assertEquals(
    formatAgeBetween("2026-04-26T12:00:00.000Z", new Date("2026-04-26T12:30:00.000Z")),
    "30m",
  );
  assertEquals(
    formatAgeBetween("2026-04-26T08:00:00.000Z", new Date("2026-04-26T12:00:00.000Z")),
    "4h",
  );
  assertEquals(
    formatAgeBetween("2026-04-22T12:00:00.000Z", new Date("2026-04-26T12:00:00.000Z")),
    "4d",
  );
});

Deno.test("formatAgeBetween: malformed iso returns ?", () => {
  assertEquals(formatAgeBetween("not-a-date", new Date()), "?");
});

Deno.test("formatAgeBetween: clamps negative deltas to 0s", () => {
  // The reference clock is before the event — should not go negative.
  assertEquals(
    formatAgeBetween("2026-04-26T12:30:00.000Z", new Date("2026-04-26T12:00:00.000Z")),
    "0s",
  );
});

// ---------------------------------------------------------------------------
// Keybindings
// ---------------------------------------------------------------------------

Deno.test("parseKeybinding: accepts ctrl+p", () => {
  const parsed = parseKeybinding("ctrl+p");
  assertEquals(parsed, { ctrl: true, shift: false, meta: false, key: "p" });
});

Deno.test("parseKeybinding: accepts ctrl+shift+tab", () => {
  const parsed = parseKeybinding("ctrl+shift+tab");
  assertEquals(parsed, { ctrl: true, shift: true, meta: false, key: "tab" });
});

Deno.test("parseKeybinding: alt+ folds into the meta flag (no separate alt field)", () => {
  const parsed = parseKeybinding("alt+x");
  assertEquals(parsed, { ctrl: false, shift: false, meta: true, key: "x" });
});

Deno.test("parseKeybinding: trims surrounding whitespace and lowercases components", () => {
  const parsed = parseKeybinding("  Ctrl+P  ");
  assertEquals(parsed.ctrl, true);
  assertEquals(parsed.key, "p");
});

Deno.test("parseKeybinding: rejects empty input", () => {
  let threw = false;
  try {
    parseKeybinding("");
  } catch (error) {
    threw = error instanceof KeybindingParseError;
  }
  assertEquals(threw, true);
});

Deno.test("parseKeybinding: rejects unknown modifier", () => {
  let threw = false;
  try {
    parseKeybinding("super+p");
  } catch (error) {
    threw = error instanceof KeybindingParseError;
  }
  assertEquals(threw, true);
});

Deno.test("parseKeybinding: rejects modifier-only chord", () => {
  let threw = false;
  try {
    parseKeybinding("ctrl");
  } catch (error) {
    threw = error instanceof KeybindingParseError;
  }
  assertEquals(threw, true);
});

Deno.test("matchesKeybinding: ctrl+p fires on Ctrl+P keystroke", () => {
  const fired = matchesKeybinding("ctrl+p", "p", {
    ctrl: true,
    shift: false,
    meta: false,
    tab: false,
    return: false,
    escape: false,
    backspace: false,
    delete: false,
    upArrow: false,
    downArrow: false,
    leftArrow: false,
    rightArrow: false,
    pageUp: false,
    pageDown: false,
  });
  assertEquals(fired, true);
});

Deno.test("matchesKeybinding: ctrl+p does not fire without ctrl", () => {
  const fired = matchesKeybinding("ctrl+p", "p", {
    ctrl: false,
    shift: false,
    meta: false,
    tab: false,
    return: false,
    escape: false,
    backspace: false,
    delete: false,
    upArrow: false,
    downArrow: false,
    leftArrow: false,
    rightArrow: false,
    pageUp: false,
    pageDown: false,
  });
  assertEquals(fired, false);
});

Deno.test("matchesKeybinding: named keys (escape, tab) match by flag", () => {
  const flags = {
    ctrl: false,
    shift: false,
    meta: false,
    tab: true,
    return: false,
    escape: false,
    backspace: false,
    delete: false,
    upArrow: false,
    downArrow: false,
    leftArrow: false,
    rightArrow: false,
    pageUp: false,
    pageDown: false,
  };
  assertEquals(matchesKeybinding("tab", "", flags), true);
  assertEquals(matchesKeybinding("escape", "", flags), false);
});

Deno.test("matchesKeybinding: unparseable chord never fires", () => {
  const fired = matchesKeybinding("this is not a chord", "p", {
    ctrl: true,
    shift: false,
    meta: false,
    tab: false,
    return: false,
    escape: false,
    backspace: false,
    delete: false,
    upArrow: false,
    downArrow: false,
    leftArrow: false,
    rightArrow: false,
    pageUp: false,
    pageDown: false,
  });
  assertEquals(fired, false);
});

Deno.test("parseKeybinding: rejects empty component (`ctrl++p`)", () => {
  let threw = false;
  try {
    parseKeybinding("ctrl++p");
  } catch (error) {
    threw = error instanceof KeybindingParseError;
  }
  assertEquals(threw, true);
});

Deno.test("matchesKeybinding: meta-required chord does not fire without meta", () => {
  const fired = matchesKeybinding("meta+x", "x", {
    ctrl: false,
    shift: false,
    meta: false,
    tab: false,
    return: false,
    escape: false,
    backspace: false,
    delete: false,
    upArrow: false,
    downArrow: false,
    leftArrow: false,
    rightArrow: false,
    pageUp: false,
    pageDown: false,
  });
  assertEquals(fired, false);
});

Deno.test("matchesKeybinding: alt+x requires meta flag", () => {
  const without = matchesKeybinding("alt+x", "x", {
    ctrl: false,
    shift: false,
    meta: false,
    tab: false,
    return: false,
    escape: false,
    backspace: false,
    delete: false,
    upArrow: false,
    downArrow: false,
    leftArrow: false,
    rightArrow: false,
    pageUp: false,
    pageDown: false,
  });
  assertEquals(without, false);
  const withMeta = matchesKeybinding("alt+x", "x", {
    ctrl: false,
    shift: false,
    meta: true,
    tab: false,
    return: false,
    escape: false,
    backspace: false,
    delete: false,
    upArrow: false,
    downArrow: false,
    leftArrow: false,
    rightArrow: false,
    pageUp: false,
    pageDown: false,
  });
  assertEquals(withMeta, true);
});

Deno.test("matchesKeybinding: shift-required chord does not fire without shift", () => {
  const fired = matchesKeybinding("ctrl+shift+p", "p", {
    ctrl: true,
    shift: false,
    meta: false,
    tab: false,
    return: false,
    escape: false,
    backspace: false,
    delete: false,
    upArrow: false,
    downArrow: false,
    leftArrow: false,
    rightArrow: false,
    pageUp: false,
    pageDown: false,
  });
  assertEquals(fired, false);
});

// ---------------------------------------------------------------------------
// App helpers — dispatchCommand and trimHistory
// ---------------------------------------------------------------------------

Deno.test("trimHistory: passes through arrays at or under the limit", () => {
  const tiny: readonly string[] = ["/issue 1", "/status"];
  assertEquals(trimHistory(tiny), tiny);
});

Deno.test("trimHistory: truncates oversized arrays from the tail", () => {
  const oversized = Array.from(
    { length: COMMAND_PALETTE_HISTORY_LIMIT + 5 },
    (_value, index) => `/cmd-${index}`,
  );
  const trimmed = trimHistory(oversized);
  assertEquals(trimmed.length, COMMAND_PALETTE_HISTORY_LIMIT);
  // Newest entries are preserved (head); the oldest 5 are dropped.
  assertEquals(trimmed[0], "/cmd-0");
});

Deno.test("dispatchCommand: ack with ok=true reports success", async () => {
  const recorded: MessageEnvelope[] = [];
  const send = (envelope: MessageEnvelope): Promise<DaemonReply> => {
    recorded.push(envelope);
    return Promise.resolve({
      id: envelope.id,
      type: "ack",
      payload: { ok: true },
    });
  };
  const outcome = await dispatchCommand(send, {
    name: "status",
    args: [],
  });
  assertEquals(outcome.kind, "ok");
  assertEquals(outcome.message, "/status dispatched");
  assertEquals(recorded.length, 1);
  assertEquals(recorded[0]?.type, "command");
});

Deno.test("dispatchCommand: ack with ok=false reports an error message", async () => {
  const send = (envelope: MessageEnvelope): Promise<DaemonReply> => {
    return Promise.resolve({
      id: envelope.id,
      type: "ack",
      payload: { ok: false, error: "no such task" },
    });
  };
  const outcome = await dispatchCommand(send, {
    name: "cancel",
    args: ["task_x"],
  });
  assertEquals(outcome.kind, "error");
  assertEquals(outcome.message, "no such task");
});

Deno.test("dispatchCommand: ack with ok=false but no error uses a fallback message", async () => {
  const send = (envelope: MessageEnvelope): Promise<DaemonReply> => {
    return Promise.resolve({
      id: envelope.id,
      type: "ack",
      payload: { ok: false },
    });
  };
  const outcome = await dispatchCommand(send, {
    name: "cancel",
    args: ["task_x"],
  });
  assertEquals(outcome.kind, "error");
  assertEquals(outcome.message, "/cancel refused");
});

Deno.test("dispatchCommand: pong reply is treated as a protocol violation", async () => {
  const send = (envelope: MessageEnvelope): Promise<DaemonReply> => {
    return Promise.resolve({
      id: envelope.id,
      type: "pong",
      payload: { daemonVersion: "x" },
    });
  };
  const outcome = await dispatchCommand(send, {
    name: "status",
    args: [],
  });
  assertEquals(outcome.kind, "error");
  assertEquals(outcome.message.startsWith("unexpected reply"), true);
});

Deno.test("dispatchCommand: a rejected send surfaces as an error outcome", async () => {
  const send = (_envelope: MessageEnvelope): Promise<DaemonReply> => {
    return Promise.reject(new Error("transport down"));
  };
  const outcome = await dispatchCommand(send, {
    name: "status",
    args: [],
  });
  assertEquals(outcome.kind, "error");
  assertEquals(outcome.message, "transport down");
});

Deno.test("dispatchCommand: a non-Error rejection is stringified", async () => {
  const send = (_envelope: MessageEnvelope): Promise<DaemonReply> => {
    return Promise.reject("oh no");
  };
  const outcome = await dispatchCommand(send, {
    name: "status",
    args: [],
  });
  assertEquals(outcome.kind, "error");
  assertEquals(outcome.message, "oh no");
});

Deno.test("matchesKeybinding: ctrl+p with empty input still matches via key length", () => {
  // Some terminals deliver Ctrl+P as key.ctrl=true with input="".
  const fired = matchesKeybinding("ctrl+p", "", {
    ctrl: true,
    shift: false,
    meta: false,
    tab: false,
    return: false,
    escape: false,
    backspace: false,
    delete: false,
    upArrow: false,
    downArrow: false,
    leftArrow: false,
    rightArrow: false,
    pageUp: false,
    pageDown: false,
  });
  assertEquals(fired, true);
});

Deno.test("matchesKeybinding: ctrl+p matches the ASCII control byte \\x10", () => {
  // xterm-style terminals deliver Ctrl+P as key.ctrl=true with input="\x10".
  // The chord must match the right control byte; a chord asking for ctrl+p
  // must NOT fire on a ctrl+, keystroke that delivers a different byte.
  const flags = {
    ctrl: true,
    shift: false,
    meta: false,
    tab: false,
    return: false,
    escape: false,
    backspace: false,
    delete: false,
    upArrow: false,
    downArrow: false,
    leftArrow: false,
    rightArrow: false,
    pageUp: false,
    pageDown: false,
  };
  assertEquals(matchesKeybinding("ctrl+p", "\x10", flags), true);
  assertEquals(matchesKeybinding("ctrl+a", "\x01", flags), true);
  assertEquals(matchesKeybinding("ctrl+z", "\x1a", flags), true);
  // ctrl+p (\x10) keystroke must not fire a ctrl+a (\x01) chord.
  assertEquals(matchesKeybinding("ctrl+a", "\x10", flags), false);
});

Deno.test("matchesKeybinding: shift-tolerant — chord without shift does not fire while shift is held", () => {
  // Doc/impl drift fix: the JSDoc claims modifiers must match exactly,
  // and the implementation now enforces that for shift too.
  const fired = matchesKeybinding("ctrl+p", "p", {
    ctrl: true,
    shift: true,
    meta: false,
    tab: false,
    return: false,
    escape: false,
    backspace: false,
    delete: false,
    upArrow: false,
    downArrow: false,
    leftArrow: false,
    rightArrow: false,
    pageUp: false,
    pageDown: false,
  });
  assertEquals(fired, false);
});
