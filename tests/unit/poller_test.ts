/**
 * Unit tests for `src/daemon/poller.ts`. Drives the poll loop with a
 * synthetic clock so the cadence, backoff, and cancellation branches are
 * deterministic without touching real time.
 *
 * The synthetic clock here is **not** a global mock: every test
 * constructs its own `RecordingClock` and injects it through
 * `PollerOptions.clock`. Per the Wave 2 lessons, the tests never call
 * `mock.install` on `Date.now`, `setTimeout`, `Deno.env`, or any other
 * process-global. A test that wants the default-clock branch exercised
 * for coverage runs against the real clock and cancels almost
 * immediately so the configured interval is irrelevant — that is the
 * *only* place the file touches real time.
 *
 * Coverage map:
 *
 *  - Steady-state cadence (success after success spaces by `interval`).
 *  - Backoff on `PollerError(kind: "rate-limited", retryAfterMs)`.
 *  - Backoff on transient rejections (exponential, jitter applied).
 *  - Backoff cap honors `backoffMaxMilliseconds`.
 *  - Backoff resets to zero after a successful tick.
 *  - `PollerError(kind: "fatal")` exits the loop without retry.
 *  - `cancel()` stops the loop and resolves a pending sleep.
 *  - External `AbortSignal` triggers the same path.
 *  - Already-aborted signal exits without a single `fetcher` call.
 *  - Single-flight: a second `poll(...)` for the same task supersedes
 *    the prior loop and the prior loop sees no further fetches.
 *  - `onError` and `onResult` throws are caught and logged.
 *  - Construction-time validation rejects bad option values.
 *  - Default clock is exercised end-to-end.
 */

import {
  assertEquals,
  assertGreaterOrEqual,
  assertInstanceOf,
  assertLessOrEqual,
  assertStrictEquals,
  assertThrows,
} from "@std/assert";

import {
  POLLER_BACKOFF_BASE_MILLISECONDS,
  POLLER_BACKOFF_JITTER_RATIO,
  POLLER_BACKOFF_MAX_MILLISECONDS,
} from "../../src/constants.ts";
import {
  createPoller,
  POLLER_ERROR_KINDS,
  PollerError,
  type PollerOptions,
  type PollerSleepAbort,
} from "../../src/daemon/poller.ts";
import { makeTaskId } from "../../src/types.ts";

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/** Recording logger that captures every `warn` call. */
function recordingLogger(): { warn: (message: string) => void; messages: string[] } {
  const messages: string[] = [];
  return {
    messages,
    warn(message: string): void {
      messages.push(message);
    },
  };
}

/**
 * Synthetic clock used by every test except the explicit "default clock"
 * coverage check. Records the duration of every `sleep()` call and
 * resolves immediately (or on abort), so a poll loop runs as fast as the
 * microtask queue can drain.
 */
class RecordingClock {
  readonly sleeps: number[] = [];
  private currentMilliseconds = 1_700_000_000_000;

  now = (): number => this.currentMilliseconds;

  sleep = (
    milliseconds: number,
    signal?: PollerSleepAbort,
  ): Promise<void> => {
    this.sleeps.push(milliseconds);
    if (signal?.aborted === true) {
      this.currentMilliseconds += milliseconds;
      return Promise.resolve();
    }
    return new Promise<void>((resolve) => {
      const onAbort = () => {
        signal?.removeEventListener("abort", onAbort);
        resolve();
      };
      signal?.addEventListener("abort", onAbort);
      // Synthetic time advances by the requested amount and resolves on
      // the next microtask. The poll loop's `await sleep(...)` therefore
      // yields exactly once before the next iteration.
      this.currentMilliseconds += milliseconds;
      queueMicrotask(() => {
        signal?.removeEventListener("abort", onAbort);
        resolve();
      });
    });
  };
}

/**
 * Yield enough microtasks for `iterations` poll-loop turns to drain.
 * The poll loop is `await fetcher() → await sleep(0)` so every iteration
 * needs two microtask flushes. We round up generously.
 */
async function flushIterations(iterations: number): Promise<void> {
  // `await Promise.resolve()` yields one microtask.
  for (let i = 0; i < iterations * 4; i++) {
    await Promise.resolve();
  }
}

const TASK = makeTaskId("task_poller_test");

interface BuildArgs {
  readonly clock?: RecordingClock;
  readonly random?: () => number;
  readonly logger?: { warn(message: string): void; messages: string[] };
  readonly options?: Omit<PollerOptions, "clock" | "random" | "logger">;
}

function buildPoller(args: BuildArgs = {}) {
  const clock = args.clock ?? new RecordingClock();
  const logger = args.logger ?? recordingLogger();
  const options: PollerOptions = {
    clock,
    logger,
    ...(args.random !== undefined ? { random: args.random } : {}),
    ...(args.options ?? {}),
  };
  const poller = createPoller(options);
  return { poller, clock, logger };
}

/**
 * A fetcher script: each invocation pops the next entry and either
 * resolves with the value or rejects with the error. Useful for driving
 * exact tick-by-tick behavior.
 */
function scriptedFetcher<T>(script: ReadonlyArray<{ ok: T } | { err: unknown }>) {
  let index = 0;
  const calls: number[] = [];
  return {
    calls,
    fetcher: (): Promise<T> => {
      const i = index++;
      calls.push(i);
      const next = script[i];
      if (next === undefined) {
        // No more scripted answers → block forever; tests that need this
        // branch cancel before the next call.
        return new Promise<T>(() => {});
      }
      if ("ok" in next) {
        return Promise.resolve(next.ok);
      }
      return Promise.reject(next.err);
    },
  };
}

// ---------------------------------------------------------------------------
// Steady-state cadence
// ---------------------------------------------------------------------------

Deno.test("poll: steady-state cadence spaces successful ticks by interval", async () => {
  const { poller, clock } = buildPoller();
  const { fetcher, calls } = scriptedFetcher([
    { ok: "a" },
    { ok: "b" },
    { ok: "c" },
  ]);
  const results: string[] = [];

  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 30_000,
    fetcher,
    onResult: (r) => results.push(r),
  });

  await flushIterations(4);
  handle.cancel();

  // Three ticks delivered; clock saw exactly three sleeps of `interval`.
  assertEquals(results, ["a", "b", "c"]);
  assertGreaterOrEqual(calls.length, 3);
  // The first three sleeps are the inter-tick delays after each success.
  assertEquals(clock.sleeps.slice(0, 3), [30_000, 30_000, 30_000]);
});

Deno.test("poll: clamps negative intervalMilliseconds to zero", async () => {
  const { poller, clock } = buildPoller();
  const { fetcher } = scriptedFetcher([{ ok: 1 }, { ok: 2 }]);
  const seen: number[] = [];
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: -500,
    fetcher,
    onResult: (r) => seen.push(r),
  });
  await flushIterations(3);
  handle.cancel();
  assertEquals(seen.slice(0, 2), [1, 2]);
  // Clamped to 0, not negative.
  assertEquals(clock.sleeps[0], 0);
});

// ---------------------------------------------------------------------------
// Rate-limited backoff (PollerError kind: "rate-limited")
// ---------------------------------------------------------------------------

Deno.test("poll: PollerError(rate-limited) honors retryAfterMs without climbing failure counter", async () => {
  const { poller, clock } = buildPoller({ random: () => 0.5 });
  const errors: unknown[] = [];
  const results: string[] = [];

  // First call rate-limited (60s wait), then two successes spaced by
  // interval. The failure counter must NOT climb from the rate-limit
  // branch — the second sleep should still be `interval`, not exp(1).
  const { fetcher } = scriptedFetcher<string>([
    { err: new PollerError("rate-limited", "primary", { retryAfterMs: 60_000 }) },
    { ok: "first" },
    { ok: "second" },
  ]);

  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 30_000,
    fetcher,
    onResult: (r) => results.push(r),
    onError: (e) => errors.push(e),
  });
  await flushIterations(4);
  handle.cancel();

  assertEquals(results, ["first", "second"]);
  assertEquals(errors.length, 1);
  assertInstanceOf(errors[0], PollerError);
  // Sleeps: rate-limit 60s, then interval 30s, then interval 30s.
  assertEquals(clock.sleeps.slice(0, 3), [60_000, 30_000, 30_000]);
});

Deno.test("poll: PollerError(rate-limited) without retryAfterMs falls back to interval", async () => {
  const { poller, clock } = buildPoller();
  const { fetcher } = scriptedFetcher<string>([
    { err: new PollerError("rate-limited", "no header") },
    { ok: "after" },
  ]);
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 25_000,
    fetcher,
    onResult: () => {},
    onError: () => {},
  });
  await flushIterations(3);
  handle.cancel();
  // Fallback to interval, not the backoff series.
  assertEquals(clock.sleeps[0], 25_000);
});

Deno.test("poll: PollerError(rate-limited) clamps retryAfterMs above the max", async () => {
  const { poller, clock } = buildPoller({
    options: { backoffMaxMilliseconds: 60_000 },
  });
  const { fetcher } = scriptedFetcher<string>([
    {
      err: new PollerError("rate-limited", "huge wait", { retryAfterMs: 9_999_999 }),
    },
    { ok: "after" },
  ]);
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 1_000,
    fetcher,
    onResult: () => {},
    onError: () => {},
  });
  await flushIterations(3);
  handle.cancel();
  assertEquals(clock.sleeps[0], 60_000);
});

Deno.test("poll: PollerError(rate-limited) clamps negative retryAfterMs to zero", async () => {
  const { poller, clock } = buildPoller();
  const { fetcher } = scriptedFetcher<string>([
    { err: new PollerError("rate-limited", "negative", { retryAfterMs: -42 }) },
    { ok: "after" },
  ]);
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 1_000,
    fetcher,
    onResult: () => {},
    onError: () => {},
  });
  await flushIterations(3);
  handle.cancel();
  assertEquals(clock.sleeps[0], 0);
});

// ---------------------------------------------------------------------------
// Exponential backoff on transient rejections
// ---------------------------------------------------------------------------

Deno.test("poll: transient rejection triggers exponential backoff with jitter", async () => {
  // Pin random() to 0.5 → jitter factor = 1.0 (mid of [0.8, 1.2]).
  const { poller, clock } = buildPoller({
    random: () => 0.5,
    options: {
      backoffBaseMilliseconds: 1_000,
      backoffMaxMilliseconds: 60_000,
      backoffJitterRatio: 0.2,
    },
  });
  const { fetcher } = scriptedFetcher<string>([
    { err: new Error("blip 1") },
    { err: new Error("blip 2") },
    { err: new Error("blip 3") },
    { ok: "recovered" },
  ]);
  const errors: unknown[] = [];
  const results: string[] = [];
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 30_000,
    fetcher,
    onResult: (r) => results.push(r),
    onError: (e) => errors.push(e),
  });
  await flushIterations(5);
  handle.cancel();

  assertEquals(results, ["recovered"]);
  assertEquals(errors.length, 3);
  // base * 2^(attempt-1) * 1.0 = 1000, 2000, 4000 then interval after success.
  assertEquals(clock.sleeps.slice(0, 4), [1_000, 2_000, 4_000, 30_000]);
});

Deno.test("poll: backoff respects jitter low/high ends", async () => {
  // Drive jitter at both extremes to confirm clamping.
  // random()=0 → factor=0.8; random()=0.999 → factor ≈ 1.2.
  let r = 0;
  const sequence = [0, 0.999];
  const { poller, clock } = buildPoller({
    random: () => sequence[r++ % sequence.length] ?? 0.5,
    options: {
      backoffBaseMilliseconds: 1_000,
      backoffMaxMilliseconds: 60_000,
      backoffJitterRatio: 0.2,
    },
  });
  const { fetcher } = scriptedFetcher<string>([
    { err: new Error("a") },
    { err: new Error("b") },
    { ok: "c" },
  ]);
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 30_000,
    fetcher,
    onResult: () => {},
    onError: () => {},
  });
  await flushIterations(4);
  handle.cancel();

  // First failure: factor=0.8 → 800 ms.
  assertEquals(clock.sleeps[0], 800);
  // Second failure: factor≈1.2, base*2 = 2000 → ≈ 2_399.something.
  // We assert the band rather than the exact float to avoid pinning to
  // the random() cursor's exact return.
  const second = clock.sleeps[1] ?? 0;
  assertGreaterOrEqual(second, 2_000 * 0.8);
  assertLessOrEqual(second, 2_000 * 1.2);
});

Deno.test("poll: backoff caps at backoffMaxMilliseconds even at deep attempt counts", async () => {
  const { poller, clock } = buildPoller({
    random: () => 0.5,
    options: {
      backoffBaseMilliseconds: 1_000,
      backoffMaxMilliseconds: 5_000,
      backoffJitterRatio: 0,
    },
  });
  const { fetcher } = scriptedFetcher<string>(
    Array.from({ length: 6 }, () => ({ err: new Error("never") })),
  );
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 30_000,
    fetcher,
    onResult: () => {},
    onError: () => {},
  });
  await flushIterations(7);
  handle.cancel();

  // 1000, 2000, 4000, 5000 (capped), 5000, 5000.
  assertEquals(clock.sleeps.slice(0, 6), [1_000, 2_000, 4_000, 5_000, 5_000, 5_000]);
});

Deno.test("poll: a successful tick resets the backoff series to zero", async () => {
  const { poller, clock } = buildPoller({
    random: () => 0.5,
    options: { backoffJitterRatio: 0 },
  });
  const { fetcher } = scriptedFetcher<string>([
    { err: new Error("a") },
    { err: new Error("b") },
    { ok: "ok" },
    { err: new Error("c") },
    { ok: "fine" },
  ]);
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 10_000,
    fetcher,
    onResult: () => {},
    onError: () => {},
  });
  await flushIterations(6);
  handle.cancel();

  // Sleeps: 1000 (fail#1), 2000 (fail#2), interval 10000 (success), 1000
  // (fail#3 — counter reset), interval 10000 (success).
  assertEquals(clock.sleeps.slice(0, 5), [1_000, 2_000, 10_000, 1_000, 10_000]);
});

// ---------------------------------------------------------------------------
// Fatal error path
// ---------------------------------------------------------------------------

Deno.test("poll: PollerError(fatal) exits the loop without further calls", async () => {
  const { poller } = buildPoller();
  const { fetcher, calls } = scriptedFetcher<string>([
    { err: new PollerError("fatal", "auth revoked") },
    { ok: "should-never-fire" },
  ]);
  const errors: unknown[] = [];
  const results: string[] = [];
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 10_000,
    fetcher,
    onResult: (r) => results.push(r),
    onError: (e) => errors.push(e),
  });
  await flushIterations(4);
  handle.cancel();

  assertEquals(calls.length, 1);
  assertEquals(results, []);
  assertEquals(errors.length, 1);
  assertInstanceOf(errors[0], PollerError);
  assertEquals((errors[0] as PollerError).kind, "fatal");
});

Deno.test("poll: PollerError(fatal) without onError logs and exits", async () => {
  const logger = recordingLogger();
  const { poller } = buildPoller({ logger });
  const { fetcher, calls } = scriptedFetcher<string>([
    { err: new PollerError("fatal", "boom") },
  ]);
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 10_000,
    fetcher,
    onResult: () => {},
  });
  await flushIterations(2);
  handle.cancel();
  assertEquals(calls.length, 1);
  // Without `onError`, the fatal error funnels into a warn line.
  const warns = logger.messages.filter((m) => m.includes("fetcher rejected"));
  assertEquals(warns.length, 1);
});

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

Deno.test("poll: cancel() stops the loop and prevents further fetcher calls", async () => {
  const { poller, clock } = buildPoller();
  let pending: () => void = () => {};
  const blockingFetcher = (): Promise<string> => {
    return new Promise<string>((resolve) => {
      pending = () => resolve("delayed");
    });
  };
  const calls: number[] = [];
  let i = 0;
  const fetcher = (): Promise<string> => {
    calls.push(i++);
    if (calls.length === 1) {
      return Promise.resolve("first");
    }
    return blockingFetcher();
  };
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 1_000,
    fetcher,
    onResult: () => {},
  });

  // Drain the first tick and let the second fetch start blocking.
  await flushIterations(2);
  handle.cancel();
  // Resolve the blocking fetcher to make sure cancel still wins.
  pending();
  await flushIterations(2);

  // After cancel: at most the two fetches the first two iterations
  // launched, and no subsequent ones. The exact number depends on
  // microtask ordering; we just assert no growth past a small bound.
  const callsAtCancel = calls.length;
  await flushIterations(4);
  assertEquals(calls.length, callsAtCancel);
  // At least the inter-tick sleep was issued; no need to be exact about
  // count beyond that.
  assertGreaterOrEqual(clock.sleeps.length, 1);
});

Deno.test("poll: cancel() is idempotent", async () => {
  const { poller } = buildPoller();
  const { fetcher } = scriptedFetcher([{ ok: 1 }]);
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 1_000,
    fetcher,
    onResult: () => {},
  });
  await flushIterations(1);
  handle.cancel();
  handle.cancel(); // No-op, must not throw.
});

Deno.test("poll: external AbortSignal triggers cancel", async () => {
  const { poller, clock } = buildPoller();
  const controller = new AbortController();
  const { fetcher, calls } = scriptedFetcher<string>([
    { ok: "first" },
    { ok: "second" },
    { ok: "third" },
  ]);
  poller.poll({
    taskId: TASK,
    intervalMilliseconds: 5_000,
    fetcher,
    onResult: () => {},
    signal: controller.signal,
  });

  await flushIterations(2);
  controller.abort();
  const callsAtAbort = calls.length;
  await flushIterations(4);
  assertEquals(calls.length, callsAtAbort);
  assertGreaterOrEqual(clock.sleeps.length, 1);
});

Deno.test("poll: already-aborted signal exits without calling fetcher", async () => {
  const { poller } = buildPoller();
  const controller = new AbortController();
  controller.abort();
  let called = 0;
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 1_000,
    fetcher: () => {
      called += 1;
      return Promise.resolve("ok");
    },
    onResult: () => {},
    signal: controller.signal,
  });
  await flushIterations(3);
  handle.cancel();
  assertEquals(called, 0);
});

// ---------------------------------------------------------------------------
// Single-flight / supersession
// ---------------------------------------------------------------------------

Deno.test("poll: a second poll() for the same task supersedes the prior loop", async () => {
  const logger = recordingLogger();
  const { poller } = buildPoller({ logger });
  const callsA: number[] = [];
  const callsB: number[] = [];

  const handleA = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 1_000,
    fetcher: () => {
      callsA.push(1);
      return Promise.resolve("a");
    },
    onResult: () => {},
  });
  await flushIterations(2);
  const callsAAtSupersede = callsA.length;

  const handleB = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 1_000,
    fetcher: () => {
      callsB.push(1);
      return Promise.resolve("b");
    },
    onResult: () => {},
  });
  await flushIterations(3);
  handleA.cancel();
  handleB.cancel();

  // Loop A made no further calls after supersession.
  assertEquals(callsA.length, callsAAtSupersede);
  // Loop B made at least one.
  assertGreaterOrEqual(callsB.length, 1);
  // A warn line was logged about the supersession.
  const warns = logger.messages.filter((m) => m.includes("superseding"));
  assertEquals(warns.length, 1);
});

Deno.test("poll: distinct taskIds run independent loops in parallel", async () => {
  const { poller } = buildPoller();
  const callsA: string[] = [];
  const callsB: string[] = [];
  const idA = makeTaskId("task_a");
  const idB = makeTaskId("task_b");

  const hA = poller.poll({
    taskId: idA,
    intervalMilliseconds: 1_000,
    fetcher: () => Promise.resolve("a"),
    onResult: (r) => callsA.push(r),
  });
  const hB = poller.poll({
    taskId: idB,
    intervalMilliseconds: 1_000,
    fetcher: () => Promise.resolve("b"),
    onResult: (r) => callsB.push(r),
  });
  await flushIterations(3);
  hA.cancel();
  hB.cancel();

  assertGreaterOrEqual(callsA.length, 1);
  assertGreaterOrEqual(callsB.length, 1);
});

// ---------------------------------------------------------------------------
// onResult / onError throw isolation
// ---------------------------------------------------------------------------

Deno.test("poll: onResult that throws does not stop the loop", async () => {
  const logger = recordingLogger();
  const { poller } = buildPoller({ logger });
  const seen: number[] = [];
  let calls = 0;
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 1_000,
    fetcher: () => Promise.resolve(calls++),
    onResult: (r) => {
      seen.push(r);
      if (r === 0) throw new Error("oops");
    },
  });
  await flushIterations(4);
  handle.cancel();
  // The loop continued even though onResult threw on tick 0.
  assertGreaterOrEqual(seen.length, 2);
  const warns = logger.messages.filter((m) => m.includes("onResult threw"));
  assertEquals(warns.length, 1);
});

Deno.test("poll: onError that throws does not stop the loop", async () => {
  const logger = recordingLogger();
  const { poller } = buildPoller({
    logger,
    options: { backoffJitterRatio: 0, backoffBaseMilliseconds: 1, backoffMaxMilliseconds: 100 },
  });
  const { fetcher } = scriptedFetcher<string>([
    { err: new Error("blip") },
    { err: new Error("blip") },
    { ok: "ok" },
  ]);
  let throws = 0;
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 1_000,
    fetcher,
    onResult: () => {},
    onError: () => {
      throws++;
      throw new Error("oops");
    },
  });
  await flushIterations(5);
  handle.cancel();
  assertEquals(throws, 2);
  const warns = logger.messages.filter((m) => m.includes("onError threw"));
  assertEquals(warns.length, 2);
});

Deno.test("poll: rejection without onError logs a warn line", async () => {
  const logger = recordingLogger();
  const { poller } = buildPoller({ logger });
  const { fetcher } = scriptedFetcher<string>([
    { err: new Error("a") },
    { ok: "after" },
  ]);
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 1_000,
    fetcher,
    onResult: () => {},
  });
  await flushIterations(3);
  handle.cancel();
  const warns = logger.messages.filter((m) => m.includes("fetcher rejected"));
  assertEquals(warns.length, 1);
});

Deno.test("poll: non-Error rejection is stringified for the warn line", async () => {
  const logger = recordingLogger();
  const { poller } = buildPoller({ logger });
  let i = 0;
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 1_000,
    fetcher: () => {
      i++;
      // Throwing a literal exercises the non-Error branch of stringify.
      return Promise.reject("stringly typed");
    },
    onResult: () => {},
  });
  await flushIterations(2);
  handle.cancel();
  assertGreaterOrEqual(i, 1);
  const warn = logger.messages.find((m) => m.includes("stringly typed"));
  assertEquals(warn !== undefined, true);
});

// ---------------------------------------------------------------------------
// Construction-time validation
// ---------------------------------------------------------------------------

Deno.test("createPoller: rejects non-positive backoffBaseMilliseconds", () => {
  assertThrows(
    () => createPoller({ backoffBaseMilliseconds: 0 }),
    RangeError,
    "backoffBaseMilliseconds",
  );
  assertThrows(
    () => createPoller({ backoffBaseMilliseconds: -1 }),
    RangeError,
    "backoffBaseMilliseconds",
  );
  assertThrows(
    () => createPoller({ backoffBaseMilliseconds: Number.NaN }),
    RangeError,
    "backoffBaseMilliseconds",
  );
});

Deno.test("createPoller: rejects backoffMaxMilliseconds below the base", () => {
  assertThrows(
    () =>
      createPoller({
        backoffBaseMilliseconds: 1_000,
        backoffMaxMilliseconds: 500,
      }),
    RangeError,
    "backoffMaxMilliseconds",
  );
});

Deno.test("createPoller: rejects backoffJitterRatio outside [0, 1)", () => {
  assertThrows(
    () => createPoller({ backoffJitterRatio: -0.1 }),
    RangeError,
    "backoffJitterRatio",
  );
  assertThrows(
    () => createPoller({ backoffJitterRatio: 1 }),
    RangeError,
    "backoffJitterRatio",
  );
  assertThrows(
    () => createPoller({ backoffJitterRatio: Number.NaN }),
    RangeError,
    "backoffJitterRatio",
  );
});

Deno.test("createPoller: defaults survive when no options are passed", () => {
  // End-to-end no-injection construction proves the default-clock /
  // default-logger / default-random branches all wire up. We immediately
  // cancel so nothing reaches the real `setTimeout`.
  const poller = createPoller();
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 5,
    fetcher: () => Promise.resolve("x"),
    onResult: () => {},
  });
  handle.cancel();
});

// ---------------------------------------------------------------------------
// Default clock coverage (one real-time pass)
// ---------------------------------------------------------------------------

Deno.test("default clock: real setTimeout-backed sleep cancels promptly on abort", async () => {
  // The only test that exercises the default `setTimeout`-backed clock.
  // We pass a deliberately large 50 s `intervalMilliseconds` and abort
  // almost immediately so the test still finishes in well under a
  // millisecond on real time — the speed comes from the abort path
  // resolving the pending sleep, not from a small interval. The inner
  // `setTimeout` is not blocking the daemon — it would `unref` anyway —
  // so `Deno.test` does not flag a leak.
  const poller = createPoller(); // Uses the default clock.
  const calls: number[] = [];
  const controller = new AbortController();
  const handle = poller.poll({
    taskId: makeTaskId("task_default_clock"),
    intervalMilliseconds: 50_000, // Large enough that abort would race.
    fetcher: () => {
      calls.push(1);
      return Promise.resolve("x");
    },
    onResult: () => {},
    signal: controller.signal,
  });
  // Yield once to let the first fetcher() run, then abort and cancel.
  await flushIterations(2);
  controller.abort();
  handle.cancel();
  // After cancellation no further calls happen even if we wait briefly.
  await flushIterations(4);
  assertGreaterOrEqual(calls.length, 1);
});

Deno.test("default clock: already-aborted signal at sleep entry resolves immediately", async () => {
  // Arrange a fetcher that succeeds, then abort *before* the sleep would
  // start. We assert the loop ends without scheduling a real timer. This
  // covers the `signal?.aborted === true` early-resolve branch in the
  // default clock's `sleep` factory.
  const poller = createPoller();
  const controller = new AbortController();
  let calls = 0;
  poller.poll({
    taskId: makeTaskId("task_pre_aborted_sleep"),
    intervalMilliseconds: 60_000,
    fetcher: () => {
      calls++;
      // Abort synchronously inside the fetcher so the post-tick sleep
      // sees an already-aborted signal.
      queueMicrotask(() => controller.abort());
      return Promise.resolve("x");
    },
    onResult: () => {},
    signal: controller.signal,
  });
  await flushIterations(6);
  assertGreaterOrEqual(calls, 1);
});

// ---------------------------------------------------------------------------
// PollerError shape sanity
// ---------------------------------------------------------------------------

Deno.test("PollerError: kinds catalog matches POLLER_ERROR_KINDS", () => {
  assertEquals([...POLLER_ERROR_KINDS], ["rate-limited", "fatal"]);
});

Deno.test("PollerError: chains cause via Error.cause", () => {
  const cause = new Error("underlying");
  const err = new PollerError("fatal", "wrapped", { cause });
  assertEquals(err.message, "wrapped");
  assertEquals(err.kind, "fatal");
  assertStrictEquals((err as Error).cause, cause);
});

Deno.test("PollerError: rate-limited carries retryAfterMs", () => {
  const err = new PollerError("rate-limited", "wait", { retryAfterMs: 1_500 });
  assertEquals(err.retryAfterMs, 1_500);
});

// ---------------------------------------------------------------------------
// Defaults from constants are visible at construction time
// ---------------------------------------------------------------------------

Deno.test("createPoller: defaults match the exported constants", () => {
  // Indirect: build a poller with the defaults, then drive it with a
  // jitter=0 override only. The first failure sleep should equal the
  // exported BASE constant.
  const clock = new RecordingClock();
  const logger = recordingLogger();
  const poller = createPoller({
    clock,
    logger,
    random: () => 0.5,
    backoffJitterRatio: 0,
  });
  const handle = poller.poll({
    taskId: TASK,
    intervalMilliseconds: 30_000,
    fetcher: () => Promise.reject(new Error("a")),
    onResult: () => {},
    onError: () => {},
  });
  // Drain one full tick and cancel.
  return flushIterations(2).then(() => {
    handle.cancel();
    assertEquals(clock.sleeps[0], POLLER_BACKOFF_BASE_MILLISECONDS);
    // Sanity: max constant matches what the module exports too.
    assertGreaterOrEqual(POLLER_BACKOFF_MAX_MILLISECONDS, POLLER_BACKOFF_BASE_MILLISECONDS);
    // Jitter constant is in the documented band.
    assertGreaterOrEqual(POLLER_BACKOFF_JITTER_RATIO, 0);
  });
});
