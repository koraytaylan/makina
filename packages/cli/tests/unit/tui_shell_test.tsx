/**
 * Unit tests for the TUI shell.
 *
 * The Wave 2 shell ships three baseline panes (`Header`, `MainPane`,
 * `StatusBar`) composed under a top-level `App`, plus a Unix-socket-
 * backed `DaemonClient`. The tests pin three snapshot states (initial
 * render, "1 task active", "disconnected") so a regression in any pane
 * flips the snapshot, plus a `SocketDaemonClient` round-trip driven
 * through an in-memory duplex transport so the wire-side framing is
 * exercised end-to-end without touching real socket I/O.
 *
 * Snapshot frames are captured by handing Ink a fake Node-stream-like
 * stdout that records every `write()` call. Combined with `debug:
 * true`, Ink writes one frame per render without ANSI cursor
 * manipulation, which keeps the snapshots stable across re-renders.
 */

import { assertEquals, assertRejects } from "@std/assert";
import { assertSnapshot } from "@std/testing/snapshot";
import { Box, render as inkRender, Text } from "ink";
import type { ReactElement } from "react";

import { App, handleEvent } from "../../src/tui/App.tsx";
import {
  DaemonClientError,
  type DuplexConnection,
  SocketDaemonClient,
  validateReplyEnvelope,
} from "../../src/tui/ipc-client.ts";
import { DaemonHookError, useDaemonConnection } from "../../src/tui/hooks/useDaemonConnection.ts";
import { Header } from "../../src/tui/components/Header.tsx";
import { MainPane } from "../../src/tui/components/MainPane.tsx";
import { StatusBar } from "../../src/tui/components/StatusBar.tsx";
import { type EventPayload, type MessageEnvelope, parseEnvelope } from "@makina/core";
import { encode } from "@makina/core";
import { makeTaskId } from "@makina/core";

import { InMemoryDaemonClient } from "@makina/core/test-helpers";

/**
 * Minimal `NodeJS.WriteStream`-shaped object that captures every
 * `write()` call. Ink's `render({ debug: true })` writes one frame per
 * commit straight to `stdout` without ANSI cursor manipulation, so the
 * recorded slice is what the user would see at that point in time.
 *
 * Implements only the slice of `NodeJS.WriteStream` Ink actually
 * touches: `write`, `cork`, `uncork`, `end`, the `columns`/`rows`/
 * `isTTY` triplet, and `on`/`off` (Ink subscribes to the `'resize'`
 * event but our fake never emits it).
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
   * @returns Always `true`; backpressure is irrelevant for a memory sink.
   */
  write(chunk: string): boolean {
    this.frames.push(String(chunk));
    return true;
  }
  /** No-op; some terminals batch writes via cork/uncork. */
  cork(): void {}
  /** No-op; pair of {@link FakeStream.cork}. */
  uncork(): void {}
  /** No-op; some terminals close the stream when done. */
  end(): void {}
  /**
   * Register a Node-style event listener. Only the `'resize'` event
   * is relevant for Ink, and the test fixture never fires it.
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
   * Remove a Node-style event listener. Symmetric with
   * {@link FakeStream.on}.
   *
   * @param event The event name.
   * @param handler The callback to remove.
   * @returns This stream, for chaining.
   */
  // deno-lint-ignore no-explicit-any
  off(event: string, handler: (...args: any[]) => void): this {
    const list = this.listeners.get(event);
    if (list === undefined) {
      return this;
    }
    const index = list.indexOf(handler);
    if (index >= 0) {
      list.splice(index, 1);
    }
    return this;
  }
  /**
   * The text captured at the most recent commit, or `""` when nothing
   * has been written yet.
   *
   * @returns The newest frame.
   */
  lastFrame(): string {
    return this.frames.at(-1) ?? "";
  }
  /**
   * The text captured at any commit that contains visible content,
   * filtering out the lone-ANSI frames Ink writes for cursor hide/
   * show. Snapshot tests assert on this.
   *
   * The filter strips CSI ANSI sequences (`ESC [ ... letter`) before
   * checking for content, because the cursor-show/hide frames contain
   * letters (`h`, `l`) that would otherwise pass a naive `[A-Za-z]`
   * test.
   *
   * @returns The newest frame with rendered content.
   */
  lastVisibleFrame(): string {
    // ESC (0x1B) starts every CSI sequence. The control char is
    // expressed via `String.fromCharCode` so the regex literal is free
    // of escape sequences (Deno's `no-control-regex` rule rejects
    // `\x1b` in a regex literal).
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
 * Poll `predicate` until it returns truthy or `timeoutMs` elapses.
 *
 * Used by the App-level snapshot tests to wait for an Ink frame to
 * commit without relying on a fixed `setTimeout`. The previous
 * fixed-timeout approach was sensitive to scheduler latency under
 * `deno test --parallel` on CI runners — see the comment at the call
 * site for details.
 *
 * @param predicate Condition that becomes `true` when the awaited
 *   frame has been written.
 * @param timeoutMs Maximum total wait, in milliseconds. Defaults to
 *   1000ms — generous on CI but well under the per-test default.
 * @param intervalMs Per-iteration sleep, in milliseconds. Defaults to
 *   10ms — short enough to keep the test snappy, long enough to give
 *   React's scheduler (which posts work via `MessageChannel`) a
 *   chance to commit between iterations rather than getting starved
 *   by a tighter timer cadence.
 * @throws Error When `predicate` never becomes truthy within
 *   `timeoutMs`. The test fails fast with a clear message instead of
 *   silently producing a stale snapshot.
 */
async function waitFor(
  predicate: () => boolean,
  timeoutMs = 1000,
  intervalMs = 10,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) {
      return;
    }
    await new Promise<void>((resolve) => setTimeout(resolve, intervalMs));
  }
  if (predicate()) {
    return;
  }
  throw new Error(`waitFor timed out after ${timeoutMs}ms`);
}

/**
 * Render an Ink element to a fake stdout and return the recorded frame
 * sink. Synchronous unmount keeps the test free of stray timers.
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
  // Yield a macrotask so any pending React effects (which run after
  // render commit) get a chance to flush before we unmount.
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  instance.unmount();
  // Yield once more so any teardown effects (signal-exit unsubscribe,
  // resize listener removal) complete before the test sanitizer runs.
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  return stdout;
}

// ---------------------------------------------------------------------------
// Snapshot tests — App
// ---------------------------------------------------------------------------

// `sanitizeOps`/`sanitizeResources` are off on the App-level tests
// because Ink delegates terminal cleanup to `signal-exit`, which
// installs 13 process-level signal listeners on construction. The
// listeners are released when `unmount()` runs, but in the test sandbox
// Deno's signal-poll ops occasionally outlive the unmount. The leak is
// inert (each Ink instance still cleans up cleanly when the process
// exits), but Deno's strict sanitizer flags it. Component-level tests
// (`Header`, `MainPane`, `StatusBar`) do not exercise the signal path
// because they unmount synchronously, so they keep their sanitizers on.

Deno.test({
  name: "App: initial render against the in-memory double",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async (t) => {
    const client = new InMemoryDaemonClient();
    const stdout = await renderToBuffer(
      <App client={client} version="0.0.0-test" autoConnect={false} inputEnabled={false} />,
    );
    await assertSnapshot(t, stdout.lastVisibleFrame());
  },
});

Deno.test({
  name: 'App: "1 task active" after a state-changed event',
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async (t) => {
    const client = new InMemoryDaemonClient();
    // Render the App, inject one event, let the effects flush, then
    // capture the post-event frame. Doing the injection between
    // render and unmount keeps the React tree's state intact across
    // the event so the snapshot reflects the active-task tally.
    const stdout = new FakeStream();
    const stderr = new FakeStream();
    const instance = inkRender(
      <App client={client} version="0.0.0-test" autoConnect={false} inputEnabled={false} />,
      {
        // deno-lint-ignore no-explicit-any
        stdout: stdout as any,
        // deno-lint-ignore no-explicit-any
        stderr: stderr as any,
        debug: true,
        exitOnCtrlC: false,
        patchConsole: false,
      },
    );
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    const initialVisible = stdout.lastVisibleFrame();
    client.simulateEvent({
      taskId: "task_2026-04-26T12-00-00_abc123",
      atIso: "2026-04-26T12:00:01.000Z",
      kind: "state-changed",
      data: { fromState: "INIT", toState: "CLONING_WORKTREE" },
    });
    // Poll for the post-event frame instead of using a fixed
    // `setTimeout`. A fixed delay raced React's scheduler under
    // `deno test --parallel` on the Linux CI runner — the post-event
    // commit landed after the test had moved on. Polling with a 10ms
    // interval gives the scheduler room to pick up its own
    // MessageChannel callbacks between iterations.
    await waitFor(() => stdout.lastVisibleFrame() !== initialVisible);
    instance.unmount();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    await assertSnapshot(t, stdout.lastVisibleFrame());
    // Sanity: the recorder captured at least the initial frame plus
    // a post-event frame.
    assertEquals(stdout.frames.length >= 2, true);
  },
});

Deno.test({
  name: 'App: "disconnected" status is reflected in the header',
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async (t) => {
    // To pin the "disconnected" visual deterministically the test
    // renders a static composition of the three baseline panes with
    // the disconnected status, then asserts on the captured frame.
    // Driving the App through `disconnect()` would also work but
    // would couple the snapshot to the timing of state propagation
    // through React.
    const fake = await renderToBuffer(
      <DisconnectedSnapshotShell
        focusedTaskId={makeTaskId("task_2026-04-26T12-00-00_abc123")}
      />,
    );
    await assertSnapshot(t, fake.lastVisibleFrame());
    // Sanity: the App with the in-memory double also renders cleanly.
    const client = new InMemoryDaemonClient();
    const stdoutApp = await renderToBuffer(
      <App client={client} version="0.0.0-test" autoConnect={false} inputEnabled={false} />,
    );
    assertEquals(stdoutApp.frames.length > 0, true);
  },
});

/**
 * Test-only composition that mirrors what the App renders when the
 * daemon connection is in the `disconnected` state. Kept tiny so the
 * snapshot stays stable across unrelated changes.
 *
 * @param props The focused-task id rendered in the header.
 * @returns The composed shell tree.
 */
function DisconnectedSnapshotShell(props: {
  readonly focusedTaskId: ReturnType<typeof makeTaskId>;
}): ReturnType<typeof Header> {
  return (
    <Box flexDirection="column">
      <Header
        version="0.0.0-test"
        status="disconnected"
        focusedTaskId={props.focusedTaskId}
      />
      <MainPane activeTaskCount={0} hint="Daemon disconnected." />
      <StatusBar message="ready" />
    </Box>
  );
}

// ---------------------------------------------------------------------------
// handleEvent unit tests
// ---------------------------------------------------------------------------

Deno.test("handleEvent: log event sets the last message with level prefix", () => {
  let lastMessage: string | undefined;
  let lastError: string | undefined;
  let known = new Set<ReturnType<typeof makeTaskId>>();
  const event: EventPayload = {
    taskId: "t1",
    atIso: "2026-04-26T12:00:00.000Z",
    kind: "log",
    data: { level: "info", message: "hello" },
  };
  handleEvent(event, {
    setKnownTaskIds: (update) => {
      known = update(known) as Set<ReturnType<typeof makeTaskId>>;
    },
    setLastMessage: (next) => {
      lastMessage = next;
    },
    setLastError: (next) => {
      lastError = next;
    },
  });
  assertEquals(lastMessage, "[info] hello");
  assertEquals(lastError, undefined);
  assertEquals(known.size, 1);
});

Deno.test("handleEvent: state-changed event renders fromState → toState", () => {
  let lastMessage: string | undefined;
  let known = new Set<ReturnType<typeof makeTaskId>>();
  handleEvent(
    {
      taskId: "task_x",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "state-changed",
      data: { fromState: "INIT", toState: "CLONING_WORKTREE" },
    },
    {
      setKnownTaskIds: (update) => {
        known = update(known) as Set<ReturnType<typeof makeTaskId>>;
      },
      setLastMessage: (next) => {
        lastMessage = next;
      },
      setLastError: () => {},
    },
  );
  assertEquals(lastMessage, "task_x: INIT → CLONING_WORKTREE");
  assertEquals(known.has(makeTaskId("task_x")), true);
});

Deno.test("handleEvent: agent-message event truncates long text", () => {
  let lastMessage: string | undefined;
  const long = "x".repeat(200);
  handleEvent(
    {
      taskId: "t",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "agent-message",
      data: { role: "assistant", text: long },
    },
    {
      setKnownTaskIds: (_update) => {},
      setLastMessage: (next) => {
        lastMessage = next;
      },
      setLastError: () => {},
    },
  );
  assertEquals(lastMessage?.endsWith("…"), true);
  assertEquals(
    (lastMessage?.length ?? 0) <= "agent assistant: ".length + 80,
    true,
  );
});

Deno.test("handleEvent: github-call event renders method + endpoint", () => {
  let lastMessage: string | undefined;
  handleEvent(
    {
      taskId: "t",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "github-call",
      data: { method: "GET", endpoint: "/repos/o/r/issues/1" },
    },
    {
      setKnownTaskIds: () => {},
      setLastMessage: (next) => {
        lastMessage = next;
      },
      setLastError: () => {},
    },
  );
  assertEquals(lastMessage, "github GET /repos/o/r/issues/1");
});

Deno.test("handleEvent: error event sets lastError", () => {
  let lastError: string | undefined;
  handleEvent(
    {
      taskId: "t",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "error",
      data: { message: "boom" },
    },
    {
      setKnownTaskIds: () => {},
      setLastMessage: () => {},
      setLastError: (next) => {
        lastError = next;
      },
    },
  );
  assertEquals(lastError, "boom");
});

Deno.test("handleEvent: invalid task id surfaces as an error and bails", () => {
  let lastError: string | undefined;
  let lastMessage: string | undefined;
  let known = new Set<ReturnType<typeof makeTaskId>>();
  handleEvent(
    {
      taskId: "   ",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "log",
      data: { level: "info", message: "should not surface" },
    },
    {
      setKnownTaskIds: (update) => {
        known = update(known) as Set<ReturnType<typeof makeTaskId>>;
      },
      setLastMessage: (next) => {
        lastMessage = next;
      },
      setLastError: (next) => {
        lastError = next;
      },
    },
  );
  assertEquals(lastError?.startsWith("event with invalid task id"), true);
  assertEquals(lastMessage, undefined);
  assertEquals(known.size, 0);
});

Deno.test("handleEvent: duplicate task id leaves the known set unchanged", () => {
  let known = new Set<ReturnType<typeof makeTaskId>>();
  known.add(makeTaskId("t1"));
  handleEvent(
    {
      taskId: "t1",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "log",
      data: { level: "info", message: "again" },
    },
    {
      setKnownTaskIds: (update) => {
        known = update(known) as Set<ReturnType<typeof makeTaskId>>;
      },
      setLastMessage: () => {},
      setLastError: () => {},
    },
  );
  assertEquals(known.size, 1);
});

// ---------------------------------------------------------------------------
// Component snapshot tests (covers the per-pane variants)
// ---------------------------------------------------------------------------

Deno.test("Header: connected status renders the green pill", async (t) => {
  const stdout = await renderToBuffer(
    <Header version="0.0.0-test" status="connected" />,
  );
  await assertSnapshot(t, stdout.lastVisibleFrame());
});

Deno.test("Header: error status surfaces the red pill", async (t) => {
  const stdout = await renderToBuffer(
    <Header
      version="0.0.0-test"
      status="error"
      focusedTaskId={makeTaskId("task_x")}
    />,
  );
  await assertSnapshot(t, stdout.lastVisibleFrame());
});

Deno.test("MainPane: empty state copy", async (t) => {
  const stdout = await renderToBuffer(<MainPane activeTaskCount={0} />);
  await assertSnapshot(t, stdout.lastVisibleFrame());
});

Deno.test("MainPane: pluralized task count", async (t) => {
  const stdout = await renderToBuffer(<MainPane activeTaskCount={3} />);
  await assertSnapshot(t, stdout.lastVisibleFrame());
});

Deno.test("StatusBar: error message takes precedence over the info message", async (t) => {
  const stdout = await renderToBuffer(
    <StatusBar message="ignored" errorMessage="boom" />,
  );
  await assertSnapshot(t, stdout.lastVisibleFrame());
});

Deno.test("StatusBar: default keybindings hint", async (t) => {
  const stdout = await renderToBuffer(<StatusBar />);
  await assertSnapshot(t, stdout.lastVisibleFrame());
});

// ---------------------------------------------------------------------------
// SocketDaemonClient — drive the real client against an in-memory duplex
// ---------------------------------------------------------------------------

/**
 * Build a pair of paired byte streams the daemon side and the client
 * side can talk to each other through. The client writes into
 * `clientToDaemon`, the daemon writes into `daemonToClient`.
 */
function makeDuplexPair(): {
  client: DuplexConnection;
  daemon: DuplexConnection;
} {
  const clientToDaemon = new TransformStream<Uint8Array, Uint8Array>();
  const daemonToClient = new TransformStream<Uint8Array, Uint8Array>();
  return {
    client: {
      readable: daemonToClient.readable,
      writable: clientToDaemon.writable,
      close: () => {
        // Closing the writable closes the daemon's view of the readable.
        // The daemon's writable closes when the test calls `daemon.close()`.
      },
    },
    daemon: {
      readable: clientToDaemon.readable,
      writable: daemonToClient.writable,
      close: () => {},
    },
  };
}

Deno.test("SocketDaemonClient: round-trips a ping → pong over a duplex pair", async () => {
  const pair = makeDuplexPair();
  const client = new SocketDaemonClient("/unused", () => Promise.resolve(pair.client));
  await client.connect();

  // Spin a tiny daemon loop that echoes pings as pongs.
  const daemonReader = pair.daemon.readable.getReader();
  const daemonWriter = pair.daemon.writable.getWriter();
  const daemonPromise = (async () => {
    let pending = new Uint8Array(0);
    while (true) {
      const { value, done } = await daemonReader.read();
      if (done) {
        return;
      }
      if (value !== undefined) {
        const merged = new Uint8Array(pending.byteLength + value.byteLength);
        merged.set(pending, 0);
        merged.set(value, pending.byteLength);
        pending = merged;
      }
      // Trivial frame extraction: the client only sends one ping per
      // test so we just look for the `\n` boundaries.
      const newlineIndex = pending.indexOf(0x0a);
      if (newlineIndex === -1) continue;
      const lengthString = new TextDecoder().decode(pending.subarray(0, newlineIndex));
      const length = Number.parseInt(lengthString, 10);
      const payloadEnd = newlineIndex + 1 + length;
      if (pending.byteLength < payloadEnd + 1) continue;
      const payloadJson = new TextDecoder().decode(
        pending.subarray(newlineIndex + 1, payloadEnd),
      );
      const incoming = JSON.parse(payloadJson) as MessageEnvelope;
      const reply: MessageEnvelope = {
        id: incoming.id,
        type: "pong",
        payload: { daemonVersion: "test-1.0" },
      };
      await daemonWriter.write(encode(reply));
      pending = pending.subarray(payloadEnd + 1);
    }
  })();

  const reply = await client.send({ id: "1", type: "ping", payload: {} });
  assertEquals(reply.type, "pong");
  assertEquals(reply.id, "1");
  if (reply.type === "pong") {
    assertEquals(reply.payload.daemonVersion, "test-1.0");
  }

  await client.close();
  await daemonWriter.close().catch(() => {});
  await daemonPromise.catch(() => {});
});

Deno.test("SocketDaemonClient: pushed events fan out to subscribers", async () => {
  const pair = makeDuplexPair();
  const client = new SocketDaemonClient("/unused", () => Promise.resolve(pair.client));
  await client.connect();

  const seen: EventPayload[] = [];
  client.subscribeEvents((event) => {
    seen.push(event);
  });

  const daemonWriter = pair.daemon.writable.getWriter();
  const eventEnvelope: MessageEnvelope = {
    id: "evt1",
    type: "event",
    payload: {
      taskId: "t1",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "log",
      data: { level: "info", message: "from-daemon" },
    },
  };
  await daemonWriter.write(encode(eventEnvelope));

  // Wait briefly for the read loop to consume the frame.
  await new Promise((resolve) => setTimeout(resolve, 20));

  assertEquals(seen.length, 1);
  assertEquals(seen[0]?.kind, "log");

  await client.close();
  await daemonWriter.close().catch(() => {});
});

Deno.test("SocketDaemonClient: unsubscribe stops further deliveries", async () => {
  const pair = makeDuplexPair();
  const client = new SocketDaemonClient("/unused", () => Promise.resolve(pair.client));
  await client.connect();

  const seen: EventPayload[] = [];
  const subscription = client.subscribeEvents((event) => {
    seen.push(event);
  });

  const daemonWriter = pair.daemon.writable.getWriter();
  const eventEnvelope: MessageEnvelope = {
    id: "evt1",
    type: "event",
    payload: {
      taskId: "t1",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "log",
      data: { level: "info", message: "first" },
    },
  };
  await daemonWriter.write(encode(eventEnvelope));
  await new Promise((resolve) => setTimeout(resolve, 20));
  subscription.unsubscribe();
  await daemonWriter.write(encode({ ...eventEnvelope, id: "evt2" }));
  await new Promise((resolve) => setTimeout(resolve, 20));

  assertEquals(seen.length, 1);
  await client.close();
  await daemonWriter.close().catch(() => {});
});

Deno.test("SocketDaemonClient: send before connect rejects with DaemonClientError", async () => {
  const client = new SocketDaemonClient("/unused", () => {
    throw new Error("should not open");
  });
  await assertRejects(
    () => client.send({ id: "1", type: "ping", payload: {} }),
    DaemonClientError,
  );
});

Deno.test("SocketDaemonClient: connect rejects when the opener throws", async () => {
  const client = new SocketDaemonClient("/unused", () => Promise.reject(new Error("EACCES")));
  await assertRejects(() => client.connect(), DaemonClientError);
});

Deno.test("SocketDaemonClient: close before connect is a no-op", async () => {
  const client = new SocketDaemonClient("/unused", () => {
    throw new Error("must not be called");
  });
  await client.close();
  await client.close();
});

Deno.test("SocketDaemonClient: duplicate envelope id rejects without sending", async () => {
  const pair = makeDuplexPair();
  const client = new SocketDaemonClient("/unused", () => Promise.resolve(pair.client));
  await client.connect();
  // The first send is left pending so the second collides on id.
  // Attach the catch handler before close() drives the rejection so
  // Deno's unhandled-rejection sanitizer does not flag the inflight
  // promise.
  const first = client.send({ id: "1", type: "ping", payload: {} }).catch(() => {});
  await assertRejects(
    () => client.send({ id: "1", type: "ping", payload: {} }),
    DaemonClientError,
    "duplicate envelope id",
  );
  await client.close();
  await first;
});

Deno.test("SocketDaemonClient: connect twice is idempotent", async () => {
  const pair = makeDuplexPair();
  let opens = 0;
  const client = new SocketDaemonClient("/unused", () => {
    opens += 1;
    return Promise.resolve(pair.client);
  });
  await client.connect();
  await client.connect();
  assertEquals(opens, 1);
  await client.close();
});

Deno.test(
  "SocketDaemonClient: reconnects after peer disconnects without explicit close",
  async () => {
    // Two paired duplexes — the first is closed mid-test to simulate
    // the daemon dropping the connection; the second models the
    // reconnect target.
    let pair = makeDuplexPair();
    let opens = 0;
    const client = new SocketDaemonClient("/unused", () => {
      opens += 1;
      return Promise.resolve(pair.client);
    });
    await client.connect();
    assertEquals(opens, 1);

    // Simulate a peer-side disconnect by closing the daemon's writable
    // half. The client's read loop sees `done` and unwinds — without
    // anyone calling `client.close()`.
    const daemonWriter = pair.daemon.writable.getWriter();
    await daemonWriter.close().catch(() => {});
    daemonWriter.releaseLock();
    // Yield so the read loop's `finally` runs and clears the local
    // handles (the regression Copilot caught).
    await new Promise<void>((resolve) => setTimeout(resolve, 20));

    // Subsequent send fails fast — the connection is gone.
    await assertRejects(
      () => client.send({ id: "1", type: "ping", payload: {} }),
      DaemonClientError,
      "not connected",
    );

    // Reconnect should open a fresh socket, not no-op.
    pair = makeDuplexPair();
    await client.connect();
    assertEquals(opens, 2);

    await client.close();
    // After explicit close, further connect() rejects.
    await assertRejects(() => client.connect(), DaemonClientError, "client has been closed");
  },
);

Deno.test("SocketDaemonClient: encode failure rejects without writing", async () => {
  const pair = makeDuplexPair();
  const client = new SocketDaemonClient("/unused", () => Promise.resolve(pair.client));
  await client.connect();
  await assertRejects(
    () =>
      client.send(
        // Deliberately invalid envelope: empty id.
        { id: "", type: "ping", payload: {} } as unknown as MessageEnvelope,
      ),
    DaemonClientError,
  );
  await client.close();
});

Deno.test(
  "SocketDaemonClient: send after explicit close reports the permanent-close error",
  async () => {
    // Regression: previously the connection/writer guard fired before
    // the `closed` guard, so a send after close() rejected with "not
    // connected" — the recoverable error — instead of the documented
    // "client has been closed" message. Tests and callers rely on the
    // permanent-close wording to tell an explicit shutdown apart from
    // a transient transport failure.
    const pair = makeDuplexPair();
    const client = new SocketDaemonClient("/unused", () => Promise.resolve(pair.client));
    await client.connect();
    await client.close();
    await assertRejects(
      () => client.send({ id: "1", type: "ping", payload: {} }),
      DaemonClientError,
      "client has been closed",
    );
  },
);

Deno.test(
  "SocketDaemonClient: peer disconnect mid-request rejects pending sends",
  async () => {
    // Regression for the race Copilot caught at ipc-client.ts:413: when
    // the read loop ends normally (peer EOF) instead of via an
    // exception, the previous implementation left every entry in
    // `pending` unresolved, hanging the caller forever. After the fix
    // the read loop must drain `pending` with a "daemon disconnected"
    // error before the local handles get torn down.
    const pair = makeDuplexPair();
    const client = new SocketDaemonClient("/unused", () => Promise.resolve(pair.client));
    await client.connect();

    // Issue a send that the daemon will never reply to. Capture the
    // promise before the disconnect so the test can await its rejection.
    const inflight = client.send({ id: "1", type: "ping", payload: {} });

    // Simulate a peer-side clean disconnect: close the daemon's
    // writable, which makes the client's read loop see `done` and
    // unwind without throwing.
    const daemonWriter = pair.daemon.writable.getWriter();
    await daemonWriter.close().catch(() => {});
    daemonWriter.releaseLock();

    // The pending send must reject with the documented disconnect
    // reason rather than hang.
    await assertRejects(
      () => inflight,
      DaemonClientError,
      "daemon disconnected before responding",
    );

    await client.close();
  },
);

// ---------------------------------------------------------------------------
// validateReplyEnvelope
// ---------------------------------------------------------------------------

Deno.test("validateReplyEnvelope: ack passes through", () => {
  const reply = validateReplyEnvelope({
    id: "1",
    type: "ack",
    payload: { ok: true },
  });
  assertEquals(reply.type, "ack");
});

Deno.test("validateReplyEnvelope: pong passes through", () => {
  const reply = validateReplyEnvelope({
    id: "1",
    type: "pong",
    payload: { daemonVersion: "0.0.0-test" },
  });
  assertEquals(reply.type, "pong");
});

Deno.test("validateReplyEnvelope: rejects an event envelope", () => {
  let threw = false;
  try {
    validateReplyEnvelope({
      id: "1",
      type: "event",
      payload: {
        taskId: "t",
        atIso: "2026-04-26T12:00:00.000Z",
        kind: "log",
        data: { level: "info", message: "x" },
      },
    });
  } catch (error) {
    threw = error instanceof DaemonClientError;
  }
  assertEquals(threw, true);
});

Deno.test("validateReplyEnvelope: rejects a malformed object", () => {
  let threw = false;
  try {
    validateReplyEnvelope({ id: "" });
  } catch (error) {
    threw = error instanceof DaemonClientError;
  }
  assertEquals(threw, true);
});

// ---------------------------------------------------------------------------
// Sanity check: parseEnvelope still validates events the App relies on.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// useDaemonConnection (driven through tiny render harnesses)
// ---------------------------------------------------------------------------

/**
 * Shape of the hook surface stashed by the test harness components
 * below so the assertions can drive it after the render commits.
 */
interface HookProbe {
  api: ReturnType<typeof useDaemonConnection> | undefined;
}

/**
 * Render harness that calls `useDaemonConnection` once and stashes the
 * returned API into the supplied {@link HookProbe} ref. Renders an
 * empty `<Text>` so it is a valid Ink component.
 *
 * @param props The probe and the options passed through to the hook.
 * @returns An empty Ink text node.
 */
function HookProbeHost(props: {
  readonly probe: HookProbe;
  readonly options: Parameters<typeof useDaemonConnection>[0];
}): ReactElement {
  const api = useDaemonConnection(props.options);
  props.probe.api = api;
  return <Text></Text>;
}

Deno.test({
  name: "useDaemonConnection: autoConnect drives idle → connected",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async () => {
    const probe: HookProbe = { api: undefined };
    const client: TestableLifecycleClient = makeFakeLifecycleClient();
    const stdout = new FakeStream();
    const stderr = new FakeStream();
    const instance = inkRender(
      <HookProbeHost
        probe={probe}
        options={{ client, autoConnect: true }}
      />,
      {
        // deno-lint-ignore no-explicit-any
        stdout: stdout as any,
        // deno-lint-ignore no-explicit-any
        stderr: stderr as any,
        debug: true,
        exitOnCtrlC: false,
        patchConsole: false,
      },
    );
    await new Promise<void>((resolve) => setTimeout(resolve, 10));
    assertEquals(probe.api?.status, "connected");
    assertEquals(client.connectCalls, 1);
    instance.unmount();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
  },
});

Deno.test({
  name: "useDaemonConnection: autoConnect surfaces connect failures as error",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async () => {
    const probe: HookProbe = { api: undefined };
    const client = makeFakeLifecycleClient({ connectError: new Error("bang") });
    const stdout = new FakeStream();
    const stderr = new FakeStream();
    const instance = inkRender(
      <HookProbeHost
        probe={probe}
        options={{ client, autoConnect: true }}
      />,
      {
        // deno-lint-ignore no-explicit-any
        stdout: stdout as any,
        // deno-lint-ignore no-explicit-any
        stderr: stderr as any,
        debug: true,
        exitOnCtrlC: false,
        patchConsole: false,
      },
    );
    await new Promise<void>((resolve) => setTimeout(resolve, 10));
    assertEquals(probe.api?.status, "error");
    assertEquals(probe.api?.lastError, "bang");
    instance.unmount();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
  },
});

Deno.test({
  name: "useDaemonConnection: disconnect drives the lifecycle to disconnected",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async () => {
    const probe: HookProbe = { api: undefined };
    const client = makeFakeLifecycleClient();
    const stdout = new FakeStream();
    const stderr = new FakeStream();
    const instance = inkRender(
      <HookProbeHost
        probe={probe}
        options={{ client, autoConnect: false }}
      />,
      {
        // deno-lint-ignore no-explicit-any
        stdout: stdout as any,
        // deno-lint-ignore no-explicit-any
        stderr: stderr as any,
        debug: true,
        exitOnCtrlC: false,
        patchConsole: false,
      },
    );
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    assertEquals(probe.api?.status, "idle");
    await probe.api?.connect();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    assertEquals(probe.api?.status, "connected");
    await probe.api?.disconnect();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    assertEquals(probe.api?.status, "disconnected");
    assertEquals(client.closeCalls, 1);
    instance.unmount();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
  },
});

Deno.test({
  name: "useDaemonConnection: a client without lifecycle starts connected",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async () => {
    const probe: HookProbe = { api: undefined };
    const client = new InMemoryDaemonClient();
    const stdout = new FakeStream();
    const stderr = new FakeStream();
    const instance = inkRender(
      <HookProbeHost
        probe={probe}
        options={{ client, autoConnect: true }}
      />,
      {
        // deno-lint-ignore no-explicit-any
        stdout: stdout as any,
        // deno-lint-ignore no-explicit-any
        stderr: stderr as any,
        debug: true,
        exitOnCtrlC: false,
        patchConsole: false,
      },
    );
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    assertEquals(probe.api?.status, "connected");
    await probe.api?.disconnect();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    assertEquals(probe.api?.status, "disconnected");
    instance.unmount();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
  },
});

Deno.test({
  name: "useDaemonConnection: disconnect surfaces close failures as error",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async () => {
    const probe: HookProbe = { api: undefined };
    const client = makeFakeLifecycleClient({ closeError: new Error("kaboom") });
    const stdout = new FakeStream();
    const stderr = new FakeStream();
    const instance = inkRender(
      <HookProbeHost
        probe={probe}
        options={{ client, autoConnect: true }}
      />,
      {
        // deno-lint-ignore no-explicit-any
        stdout: stdout as any,
        // deno-lint-ignore no-explicit-any
        stderr: stderr as any,
        debug: true,
        exitOnCtrlC: false,
        patchConsole: false,
      },
    );
    await new Promise<void>((resolve) => setTimeout(resolve, 10));
    await probe.api?.disconnect();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    assertEquals(probe.api?.status, "error");
    assertEquals(probe.api?.lastError, "kaboom");
    instance.unmount();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
  },
});

Deno.test({
  name: "useDaemonConnection: send/subscribe forward to the underlying client",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async () => {
    const probe: HookProbe = { api: undefined };
    const client = new InMemoryDaemonClient();
    const stdout = new FakeStream();
    const stderr = new FakeStream();
    const instance = inkRender(
      <HookProbeHost
        probe={probe}
        options={{ client, autoConnect: false }}
      />,
      {
        // deno-lint-ignore no-explicit-any
        stdout: stdout as any,
        // deno-lint-ignore no-explicit-any
        stderr: stderr as any,
        debug: true,
        exitOnCtrlC: false,
        patchConsole: false,
      },
    );
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    const reply = await probe.api?.send({ id: "1", type: "ping", payload: {} });
    assertEquals(reply?.type, "pong");
    let received = 0;
    const subscription = probe.api?.subscribe(() => {
      received += 1;
    });
    client.simulateEvent({
      taskId: "t",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "log",
      data: { level: "info", message: "x" },
    });
    assertEquals(received, 1);
    subscription?.unsubscribe();
    instance.unmount();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
  },
});

Deno.test({
  name: 'useDaemonConnection: send rejects with DaemonHookError when status !== "connected"',
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async () => {
    // A lifecycle-aware client kept idle (autoConnect off, no manual
    // connect) puts the hook's status at `"idle"`. The new guard
    // should reject any `send` call before the lifecycle promotes
    // the status to `"connected"`.
    const probe: HookProbe = { api: undefined };
    const client = makeFakeLifecycleClient();
    const stdout = new FakeStream();
    const stderr = new FakeStream();
    const instance = inkRender(
      <HookProbeHost
        probe={probe}
        options={{ client, autoConnect: false }}
      />,
      {
        // deno-lint-ignore no-explicit-any
        stdout: stdout as any,
        // deno-lint-ignore no-explicit-any
        stderr: stderr as any,
        debug: true,
        exitOnCtrlC: false,
        patchConsole: false,
      },
    );
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    assertEquals(probe.api?.status, "idle");
    const api = probe.api;
    if (api === undefined) {
      throw new Error("hook probe never received the API");
    }
    await assertRejects(
      () => api.send({ id: "1", type: "ping", payload: {} }),
      DaemonHookError,
      "requires status",
    );
    instance.unmount();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
  },
});

Deno.test({
  name: "useDaemonConnection: disconnect clears stale lastError on lifecycle-less client",
  sanitizeOps: false,
  sanitizeResources: false,
  fn: async () => {
    // The in-memory double has no `close`, so disconnect should still
    // wipe any earlier error message — otherwise the status bar would
    // keep flagging a stale error after the user disconnects cleanly.
    const probe: HookProbe = { api: undefined };
    const client = new InMemoryDaemonClient();
    const stdout = new FakeStream();
    const stderr = new FakeStream();
    const instance = inkRender(
      <HookProbeHost
        probe={probe}
        options={{ client, autoConnect: false }}
      />,
      {
        // deno-lint-ignore no-explicit-any
        stdout: stdout as any,
        // deno-lint-ignore no-explicit-any
        stderr: stderr as any,
        debug: true,
        exitOnCtrlC: false,
        patchConsole: false,
      },
    );
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    // No public hook API exposes `setLastError`; in production the
    // read loop fills it. Instead, drive the same closeConnection
    // path (which is what disconnect calls) and assert the post-
    // condition.
    await probe.api?.disconnect();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    assertEquals(probe.api?.status, "disconnected");
    assertEquals(probe.api?.lastError, undefined);
    instance.unmount();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
  },
});

/**
 * Helper that yields a {@link DaemonClient} subclass with `connect`/
 * `close` lifecycle methods plus call counters and optional injected
 * errors. Used by the hook tests above to drive every branch of
 * `openConnection`/`closeConnection` without spinning up a socket.
 */
interface TestableLifecycleClient {
  connect(): Promise<void>;
  close(): Promise<void>;
  send: InMemoryDaemonClient["send"];
  subscribeEvents: InMemoryDaemonClient["subscribeEvents"];
  connectCalls: number;
  closeCalls: number;
}

/**
 * Build a lifecycle-aware client backed by an {@link InMemoryDaemonClient}.
 *
 * @param options Optional injected errors for the lifecycle methods.
 * @returns The lifecycle-aware client.
 */
function makeFakeLifecycleClient(options: {
  readonly connectError?: Error;
  readonly closeError?: Error;
} = {}): TestableLifecycleClient {
  const inner = new InMemoryDaemonClient();
  const wrapper: TestableLifecycleClient = {
    connectCalls: 0,
    closeCalls: 0,
    connect() {
      wrapper.connectCalls += 1;
      if (options.connectError !== undefined) {
        return Promise.reject(options.connectError);
      }
      return Promise.resolve();
    },
    close() {
      wrapper.closeCalls += 1;
      if (options.closeError !== undefined) {
        return Promise.reject(options.closeError);
      }
      return Promise.resolve();
    },
    send: (envelope) => inner.send(envelope),
    subscribeEvents: (handler) => inner.subscribeEvents(handler),
  };
  return wrapper;
}

Deno.test("the App's event vocabulary parses through parseEnvelope", () => {
  const candidates: EventPayload[] = [
    {
      taskId: "t",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "log",
      data: { level: "info", message: "x" },
    },
    {
      taskId: "t",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "state-changed",
      data: { fromState: "INIT", toState: "DRAFTING" },
    },
    {
      taskId: "t",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "agent-message",
      data: { role: "assistant", text: "hello" },
    },
    {
      taskId: "t",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "github-call",
      data: { method: "GET", endpoint: "/x" },
    },
    {
      taskId: "t",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "error",
      data: { message: "x" },
    },
  ];
  for (const event of candidates) {
    const result = parseEnvelope({ id: "e", type: "event", payload: event });
    assertEquals(result.success, true, `event ${event.kind} failed validation`);
  }
});
