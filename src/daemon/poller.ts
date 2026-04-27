/**
 * daemon/poller.ts — single-flight per-task poll loop with backoff.
 *
 * The supervisor (Wave 4) drives the stabilize loop's CI and conversations
 * phases by handing this module a `fetcher` and an `onResult` callback;
 * the poller spaces the calls, surfaces results synchronously through the
 * callback, and stops cleanly on cancel. The W1 `Poller` interface in
 * `src/types.ts` is the contract every caller (the supervisor, the
 * stabilize loop's per-phase wirings, the unit tests' in-memory drivers)
 * binds to.
 *
 * The behavior split between this module and {@link GitHubClientImpl}
 * mirrors ADR-006 + ADR-011 + ADR-017:
 *
 * - The GitHub client honors header-driven rate-limit waits (`Retry-After`,
 *   `X-RateLimit-Reset`) **inside** the fetch wrapper, so a single
 *   logical call returns the resolved payload after at most one retry.
 * - The poller is responsible for the **cadence** and the **inter-tick**
 *   backoff. A fetcher rejection takes the exponential-backoff path; a
 *   `PollerError(kind: "rate-limited")` carrying `retryAfterMs` short-
 *   circuits the cadence to honor an upstream wait the client did not
 *   absorb.
 *
 * **Single-flight per task.** Calling {@link Poller.poll} a second time
 * with the same `taskId` cancels the prior loop before starting fresh.
 * The supervisor relies on this when a stabilize-phase transition tears
 * down and re-arms the poller atomically.
 *
 * **Synthetic clock seam.** `createPoller` accepts a
 * `clock: { now(): number; sleep(ms): Promise<void> }` so tests never
 * touch the real clock. The default implementation uses `Date.now`
 * and `setTimeout`; tests inject a deterministic clock that records
 * every sleep duration. The clock is the **only** way the module
 * measures time. Per ADR-017, no `Date.now()` call lives outside the
 * default-clock factory.
 *
 * **Cancellation.** Both the returned `cancel()` handle and a caller-
 * provided `AbortSignal` flip the same internal flag and abort the in-
 * flight `clock.sleep`. After cancellation the poller makes no further
 * calls to `fetcher` or `onResult`.
 *
 * The cadence semantics, the `PollerError` taxonomy, and the synthetic-
 * clock contract are captured in
 * `docs/adrs/017-poller-cadence-and-backoff.md`. Any change to those
 * shapes is a contract change and needs an ADR amendment.
 *
 * @module
 */

import { getLogger } from "@std/log";

import {
  POLLER_BACKOFF_BASE_MILLISECONDS,
  POLLER_BACKOFF_JITTER_RATIO,
  POLLER_BACKOFF_MAX_ATTEMPT_EXPONENT,
  POLLER_BACKOFF_MAX_MILLISECONDS,
} from "../constants.ts";
import type { Poller, TaskId } from "../types.ts";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/**
 * Discriminant kinds of {@link PollerError}.
 *
 * - `"rate-limited"` — the fetcher already exhausted its own retries and
 *   wants the poller to wait `retryAfterMs` before the next tick. The
 *   poller treats this as a successful tick for backoff bookkeeping
 *   (the consecutive-failure counter does **not** climb), but the next
 *   tick is delayed.
 * - `"fatal"` — the fetcher hit a non-retryable error (auth revocation,
 *   missing installation, malformed response). The poller stops the loop
 *   and surfaces the error through {@link PollerOptions.onError}; without
 *   `onError` the error is logged and the loop exits.
 */
export const POLLER_ERROR_KINDS = ["rate-limited", "fatal"] as const;

/** Discriminant union of valid {@link PollerError.kind} values. */
export type PollerErrorKind = typeof POLLER_ERROR_KINDS[number];

/**
 * Domain error a fetcher may throw to signal one of two narrow outcomes
 * the poller treats specially: a known rate-limit wait or a fatal
 * non-retryable failure. Plain `Error` rejections take the exponential-
 * backoff path instead — the poller assumes they are transient.
 *
 * @example
 * ```ts
 * // fetcher signals an upstream rate-limit wait
 * throw new PollerError("rate-limited", "GitHub primary rate limit", {
 *   retryAfterMs: 30_000,
 * });
 *
 * // fetcher signals a fatal failure
 * throw new PollerError("fatal", "installation token revoked");
 * ```
 */
export class PollerError extends Error {
  /**
   * Build a poller domain error.
   *
   * @param kind The classification controlling poller behavior.
   * @param message Human-readable description.
   * @param options Optional payload fields.
   * @param options.retryAfterMs Sleep, in milliseconds, the poller honors
   *   before the next tick when `kind === "rate-limited"`. Ignored on
   *   `"fatal"`. Negative values clamp to 0.
   * @param options.cause Underlying error to chain via the standard
   *   `Error.cause` field.
   *
   * @throws Never; the constructor only assembles fields.
   */
  constructor(
    public readonly kind: PollerErrorKind,
    message: string,
    options: { readonly retryAfterMs?: number; readonly cause?: unknown } = {},
  ) {
    super(message, options.cause === undefined ? undefined : { cause: options.cause });
    this.name = "PollerError";
    if (options.retryAfterMs !== undefined) {
      this.retryAfterMs = options.retryAfterMs;
    }
  }

  /**
   * When `kind === "rate-limited"`, the inter-tick wait the poller honors,
   * in milliseconds. `undefined` (or unset) when the fetcher does not
   * have a precise wait to share — the poller falls back to its default
   * cadence on rate-limited tickets without `retryAfterMs`.
   */
  public readonly retryAfterMs?: number;
}

/**
 * Synthetic clock seam used by {@link createPoller}.
 *
 * The poller calls only `now()` (for jitter seeding diagnostics, never
 * for cadence math) and `sleep()` (between ticks). Tests inject a
 * deterministic implementation that records every sleep without touching
 * real time. The default clock uses `Date.now` and a `setTimeout`-backed
 * sleep that respects {@link PollerSleepAbort}.
 */
export interface PollerClock {
  /**
   * Current wall-clock value in milliseconds since the Unix epoch.
   * The poller does not subtract one `now()` from another — the value
   * is only used for diagnostics (e.g. log lines a future caller may
   * want), so a non-monotonic test clock is acceptable.
   */
  now(): number;
  /**
   * Sleep for `milliseconds`. Resolves early when `signal` aborts.
   *
   * @param milliseconds Duration to wait. Implementations must clamp
   *   negative values to zero and resolve immediately for `0`.
   * @param signal Optional abort signal. When it aborts (already-aborted
   *   counts), the sleep must resolve on the next microtask without
   *   throwing. The poller never observes the abort reason — the
   *   resolution itself is the cancellation signal.
   */
  sleep(milliseconds: number, signal?: PollerSleepAbort): Promise<void>;
}

/**
 * Subset of {@link AbortSignal} the poller passes to the clock's `sleep`.
 *
 * Mirrors the two methods the default implementation uses so tests can
 * inject lightweight doubles without constructing a real
 * `AbortController`. Production wiring uses real `AbortSignal`s.
 */
export interface PollerSleepAbort {
  /** Whether the underlying signal has already aborted. */
  readonly aborted: boolean;
  /**
   * Subscribe to the abort event. Implementations must invoke `listener`
   * exactly once on the first transition to aborted; subsequent abort
   * events are no-ops.
   */
  addEventListener(type: "abort", listener: () => void): void;
  /** Unsubscribe a previously-added abort listener. */
  removeEventListener(type: "abort", listener: () => void): void;
}

/**
 * Narrow logger surface used by the poller. The `@std/log` `Logger` class
 * satisfies this shape (its `warn` accepts a string), but the narrower
 * interface lets tests inject a recording double without matching the
 * SDK's full overload signature.
 */
export interface PollerLogger {
  /** Emit a warning. */
  warn(message: string): void;
}

/**
 * Optional callback taxonomy on {@link Poller.poll}'s extended options.
 *
 * The W1 `Poller.poll` signature only accepts `onResult`; production
 * wiring also wants to learn about errors and rate-limited backoff
 * episodes. {@link PollerImpl.poll} widens the surface with these
 * optional callbacks; existing callers binding to the W1 interface
 * keep working unchanged.
 */
export interface PollerCallbacks<TResult> {
  /**
   * Invoked synchronously with each successful fetcher resolution.
   *
   * A throw inside `onResult` is caught at the loop boundary, logged at
   * `warn`, and the loop continues. The poller does not retry the tick
   * — `onResult` is the supervisor's responsibility and any side-effect
   * failure there is independent of the fetcher's success.
   */
  readonly onResult: (result: TResult) => void;
  /**
   * Invoked once per fetcher rejection (including
   * {@link PollerError}-typed rejections). The poller still applies the
   * appropriate backoff/cadence on top of the callback.
   *
   * A throw inside `onError` is caught and logged the same way as
   * `onResult`.
   */
  readonly onError?: (error: unknown) => void;
}

/**
 * Construction options for {@link createPoller}.
 *
 * Every field is optional; the default values reproduce real-world
 * behavior. The `clock`, `random`, and `logger` hooks exist so the unit
 * tests can drive the cadence and jitter branches deterministically.
 */
export interface PollerOptions {
  /**
   * Synthetic clock seam. Defaults to a `Date.now`/`setTimeout`-backed
   * implementation. Tests inject a recording clock so cadence assertions
   * are deterministic.
   */
  readonly clock?: PollerClock;
  /**
   * Source of uniform `[0, 1)` randomness for jitter. Defaults to
   * `Math.random`. Tests pin this to a constant so the jittered backoff
   * is exact.
   */
  readonly random?: () => number;
  /**
   * Lower bound on the exponential backoff base, in milliseconds.
   * Defaults to {@link POLLER_BACKOFF_BASE_MILLISECONDS}.
   */
  readonly backoffBaseMilliseconds?: number;
  /**
   * Upper bound on a single sleep (both `retryAfterMs` and the
   * exponential series). Defaults to
   * {@link POLLER_BACKOFF_MAX_MILLISECONDS}.
   */
  readonly backoffMaxMilliseconds?: number;
  /**
   * Symmetric jitter ratio applied to every backoff sleep. Must lie in
   * `[0, 1)`; values outside that range raise {@link RangeError}. A
   * value of `0` disables jitter (useful in tests). Defaults to
   * {@link POLLER_BACKOFF_JITTER_RATIO}.
   */
  readonly backoffJitterRatio?: number;
  /**
   * Logger used for handler-throws and per-task supersession warnings.
   * Defaults to `getLogger()` from `@std/log` (the default-namespace
   * logger), adapted to the {@link PollerLogger} surface.
   */
  readonly logger?: PollerLogger;
}

/**
 * Concrete return type of {@link createPoller}. Widens the W1
 * {@link Poller} contract with:
 *
 *  - an `onError` callback alongside `onResult`;
 *  - an externally-supplied `AbortSignal` that triggers the same
 *    cancellation path as the returned `cancel()` handle;
 *  - an extension hook for per-task single-flight bookkeeping the
 *    supervisor relies on.
 *
 * The wider surface stays out of the cross-wave contract file
 * (`src/types.ts`) so consumer waves not in the supervisor (TUI,
 * persistence) are not coupled to it.
 */
export interface PollerImpl extends Poller {
  /**
   * Begin polling. Same shape as {@link Poller.poll} from `src/types.ts`,
   * widened with optional callbacks and a caller-provided abort signal.
   *
   * @param args Polling configuration.
   * @param args.taskId Task this loop drives. The poller keeps at most
   *   one in-flight invocation per `taskId`; a second call with the same
   *   id cancels the prior loop before starting fresh.
   * @param args.intervalMilliseconds Steady-state spacing between
   *   successful ticks, in milliseconds. Negative values clamp to zero;
   *   `0` is allowed and is used by some tests to fire back-to-back.
   * @param args.fetcher Async callable invoked once per tick. May throw
   *   {@link PollerError} for rate-limit waits or fatal exits, or any
   *   other rejection for the exponential-backoff path.
   * @param args.onResult Synchronous callback invoked with each
   *   successful fetcher resolution. Throws inside `onResult` are caught
   *   and logged.
   * @param args.onError Optional synchronous callback invoked with each
   *   fetcher rejection (including {@link PollerError}). Throws inside
   *   `onError` are caught and logged.
   * @param args.signal Optional {@link AbortSignal} whose abort event
   *   triggers the same cancellation path as the returned `cancel()`
   *   handle. An already-aborted signal is honored: the poller exits
   *   without a single call to `fetcher`.
   * @returns Cancellation handle; calling `cancel()` is idempotent.
   */
  poll<TResult>(args: {
    readonly taskId: TaskId;
    readonly intervalMilliseconds: number;
    readonly fetcher: () => Promise<TResult>;
    readonly onResult: (result: TResult) => void;
    readonly onError?: (error: unknown) => void;
    readonly signal?: AbortSignal;
  }): { cancel(): void };
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/**
 * Construct a {@link PollerImpl}.
 *
 * The returned poller drives one loop per task with the cadence and
 * backoff policy spelled out in
 * `docs/adrs/017-poller-cadence-and-backoff.md`. See the module doc
 * for the full contract.
 *
 * @param options Optional configuration overrides.
 * @returns A fresh poller ready to wire into the supervisor.
 *
 * @throws RangeError if `backoffJitterRatio` is outside `[0, 1)` or any
 *   numeric option is `NaN`/non-finite.
 *
 * @example
 * ```ts
 * import { createPoller, PollerError } from "./poller.ts";
 *
 * const poller = createPoller();
 * const handle = poller.poll({
 *   taskId,
 *   intervalMilliseconds: POLL_INTERVAL_MILLISECONDS,
 *   fetcher: () => client.getCombinedStatus(repo, sha),
 *   onResult: (status) => supervisor.observeCi(taskId, status),
 *   onError: (err) => supervisor.observeCiFailure(taskId, err),
 * });
 * // …later
 * handle.cancel();
 * ```
 */
export function createPoller(options: PollerOptions = {}): PollerImpl {
  const clock = options.clock ?? defaultClock();
  const random = options.random ?? Math.random;
  const backoffBase = validatePositiveDuration(
    options.backoffBaseMilliseconds ?? POLLER_BACKOFF_BASE_MILLISECONDS,
    "backoffBaseMilliseconds",
  );
  const backoffMax = validatePositiveDuration(
    options.backoffMaxMilliseconds ?? POLLER_BACKOFF_MAX_MILLISECONDS,
    "backoffMaxMilliseconds",
  );
  if (backoffMax < backoffBase) {
    throw new RangeError(
      `backoffMaxMilliseconds (${backoffMax}) must be ≥ backoffBaseMilliseconds (${backoffBase})`,
    );
  }
  const jitterRatio = validateJitterRatio(
    options.backoffJitterRatio ?? POLLER_BACKOFF_JITTER_RATIO,
  );
  const logger = options.logger ?? defaultLogger();

  /**
   * Per-task supersession bookkeeping. A second `poll(...)` call with an
   * id already present in this map cancels the prior loop before starting
   * fresh. Entries are removed when their loop exits (cancelled, fatal,
   * or signal abort).
   */
  const inFlight = new Map<TaskId, () => void>();

  return {
    poll<TResult>(args: {
      readonly taskId: TaskId;
      readonly intervalMilliseconds: number;
      readonly fetcher: () => Promise<TResult>;
      readonly onResult: (result: TResult) => void;
      readonly onError?: (error: unknown) => void;
      readonly signal?: AbortSignal;
    }): { cancel(): void } {
      const interval = Math.max(0, args.intervalMilliseconds);

      // Single-flight: cancel any prior loop for the same task before
      // installing the new one. The cancel call below resolves the prior
      // sleep on its next microtask — the prior loop exits without an
      // additional fetch.
      const previous = inFlight.get(args.taskId);
      if (previous !== undefined) {
        logger.warn(
          `poller: superseding in-flight poll for ${stringifyTaskId(args.taskId)}`,
        );
        previous();
      }

      const abortController = new AbortController();
      let cancelled = false;
      const cancel = (): void => {
        if (cancelled) return;
        cancelled = true;
        abortController.abort();
        // Only remove the bookkeeping entry if the slot still belongs to
        // *this* loop. A later `poll(...)` for the same task would have
        // overwritten the entry with its own canceller; we must not
        // delete that one.
        if (inFlight.get(args.taskId) === cancel) {
          inFlight.delete(args.taskId);
        }
      };
      inFlight.set(args.taskId, cancel);

      // Honor an already-aborted external signal without calling fetcher.
      if (args.signal?.aborted === true) {
        cancel();
        return { cancel };
      }
      // Wire external signal abort into our internal cancel path.
      const externalAbortListener = (): void => {
        cancel();
      };
      args.signal?.addEventListener("abort", externalAbortListener);

      // Detached loop: failures inside the loop are surfaced through
      // `onError`/`logger.warn` per the contract; this `.catch` is
      // defensive only.
      runLoop({
        clock,
        random,
        backoffBase,
        backoffMax,
        jitterRatio,
        logger,
        interval,
        fetcher: args.fetcher,
        onResult: args.onResult,
        onError: args.onError,
        abortSignal: abortController.signal,
        isCancelled: () => cancelled,
        cleanup: () => {
          args.signal?.removeEventListener("abort", externalAbortListener);
          if (inFlight.get(args.taskId) === cancel) {
            inFlight.delete(args.taskId);
          }
        },
      }).catch((caught: unknown) => {
        logger.warn(
          `poller: loop exited unexpectedly for ${stringifyTaskId(args.taskId)}: ${
            stringifyError(caught)
          }`,
        );
      });

      return { cancel };
    },
  };
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

interface LoopArgs<TResult> {
  readonly clock: PollerClock;
  readonly random: () => number;
  readonly backoffBase: number;
  readonly backoffMax: number;
  readonly jitterRatio: number;
  readonly logger: PollerLogger;
  readonly interval: number;
  readonly fetcher: () => Promise<TResult>;
  readonly onResult: (result: TResult) => void;
  readonly onError: ((error: unknown) => void) | undefined;
  readonly abortSignal: AbortSignal;
  readonly isCancelled: () => boolean;
  readonly cleanup: () => void;
}

/**
 * The actual await-then-sleep loop. Pulled out as a free function so
 * `createPoller`'s closure stays small and the loop's local state
 * (consecutive failures, attempt counter) does not leak into the
 * factory's lexical scope across `poll(...)` calls.
 */
async function runLoop<TResult>(args: LoopArgs<TResult>): Promise<void> {
  let consecutiveFailures = 0;
  try {
    while (!args.isCancelled()) {
      let nextWait: number;
      try {
        const result = await args.fetcher();
        if (args.isCancelled()) return;
        // Success resets the backoff series.
        consecutiveFailures = 0;
        safeInvoke(() => args.onResult(result), "onResult", args.logger);
        nextWait = args.interval;
      } catch (caught) {
        if (args.isCancelled()) return;
        if (caught instanceof PollerError) {
          if (caught.kind === "fatal") {
            // Surface and exit; the supervisor's higher-level state
            // machine decides what to do next (typically NEEDS_HUMAN).
            invokeOnError(args, caught);
            return;
          }
          // kind: "rate-limited" — honor the explicit wait when present,
          // otherwise fall back to the steady-state interval. Failure
          // counter does not climb because the upstream told us a
          // precise wait; the next tick is *delayed*, not an *error*.
          invokeOnError(args, caught);
          nextWait = clamp(
            caught.retryAfterMs ?? args.interval,
            0,
            args.backoffMax,
          );
        } else {
          // Generic rejection: exponential backoff with jitter.
          invokeOnError(args, caught);
          consecutiveFailures += 1;
          nextWait = computeBackoff(
            consecutiveFailures,
            args.backoffBase,
            args.backoffMax,
            args.jitterRatio,
            args.random,
          );
        }
      }

      if (args.isCancelled()) return;
      // The clock's sleep resolves on abort; we re-check `isCancelled`
      // after to break the loop without scheduling another fetch.
      await args.clock.sleep(nextWait, args.abortSignal);
    }
  } finally {
    args.cleanup();
  }
}

/**
 * Compute the next backoff sleep, in milliseconds, given the consecutive-
 * failure count.
 *
 * `attempt` is the *consecutive failure* count (1 for the first failure
 * after a success, 2 for the next, …). The returned value is
 * `min(base * 2^(attempt-1), max) * jitter`, where `jitter` is uniform
 * in `[1 - ratio, 1 + ratio]`. Clamped from above by `max`.
 *
 * The exponent is also capped at {@link POLLER_BACKOFF_MAX_ATTEMPT_EXPONENT}
 * to keep `Math.pow(2, …)` clear of `Infinity`; with realistic
 * `(base, max)` pairs the outer `min(…, max)` saturates the series long
 * before the cap takes effect (see the constant's JSDoc).
 */
function computeBackoff(
  attempt: number,
  base: number,
  max: number,
  jitterRatio: number,
  random: () => number,
): number {
  const safeAttempt = Math.min(attempt, POLLER_BACKOFF_MAX_ATTEMPT_EXPONENT);
  const exponential = base * Math.pow(2, safeAttempt - 1);
  const capped = Math.min(exponential, max);
  if (jitterRatio === 0) {
    return capped;
  }
  // Uniform jitter in [1 - ratio, 1 + ratio].
  const factor = 1 - jitterRatio + random() * (2 * jitterRatio);
  return clamp(capped * factor, 0, max);
}

function invokeOnError<TResult>(
  args: LoopArgs<TResult>,
  caught: unknown,
): void {
  if (args.onError === undefined) {
    args.logger.warn(`poller: fetcher rejected: ${stringifyError(caught)}`);
    return;
  }
  safeInvoke(() => args.onError?.(caught), "onError", args.logger);
}

function safeInvoke(
  call: () => void,
  label: string,
  logger: PollerLogger,
): void {
  try {
    call();
  } catch (caught) {
    logger.warn(`poller: ${label} threw; loop continues. error=${stringifyError(caught)}`);
  }
}

function clamp(value: number, lower: number, upper: number): number {
  if (Number.isNaN(value)) return lower;
  if (value < lower) return lower;
  if (value > upper) return upper;
  return value;
}

function validatePositiveDuration(value: number, field: string): number {
  if (!Number.isFinite(value) || value <= 0) {
    throw new RangeError(
      `${field} must be a positive finite number; got ${value}`,
    );
  }
  return value;
}

function validateJitterRatio(value: number): number {
  if (!Number.isFinite(value) || value < 0 || value >= 1) {
    throw new RangeError(
      `backoffJitterRatio must be in [0, 1); got ${value}`,
    );
  }
  return value;
}

function stringifyTaskId(taskId: TaskId): string {
  return `task:${taskId}`;
}

function stringifyError(caught: unknown): string {
  if (caught instanceof Error) {
    return `${caught.name}: ${caught.message}`;
  }
  return String(caught);
}

// ---------------------------------------------------------------------------
// Default clock and logger factories
// ---------------------------------------------------------------------------

/**
 * The default {@link PollerClock} backed by `Date.now` and a
 * `setTimeout`-based sleep that resolves early on `signal.abort()`.
 *
 * Pulled out as a separate factory so the poller's main body holds no
 * `Date.now()` or `setTimeout` reference — that lives entirely in this
 * function and is bypassed in every test that injects a clock.
 */
function defaultClock(): PollerClock {
  return {
    now(): number {
      return Date.now();
    },
    sleep(milliseconds: number, signal?: PollerSleepAbort): Promise<void> {
      const wait = milliseconds <= 0 ? 0 : milliseconds;
      return new Promise<void>((resolve) => {
        if (signal?.aborted === true) {
          resolve();
          return;
        }
        let resolved = false;
        // `abortListener` is declared up-front so the `setTimeout`
        // callback (and the listener itself) can both reference it
        // without tripping the temporal-dead-zone — and so the cleanup
        // path is plain to read without forward references.
        const abortListener = (): void => {
          if (resolved) return;
          resolved = true;
          clearTimeout(timeout);
          signal?.removeEventListener("abort", abortListener);
          resolve();
        };
        const timeout = setTimeout(() => {
          if (resolved) return;
          resolved = true;
          signal?.removeEventListener("abort", abortListener);
          resolve();
        }, wait);
        signal?.addEventListener("abort", abortListener);
        // `setTimeout` in Deno returns a numeric handle; `unref` is not
        // available on every runtime. We do not block daemon shutdown on
        // a pending poll sleep — the supervisor's cancel paths abort the
        // signal first.
        const maybeUnref = timeout as unknown as { unref?: () => void };
        if (typeof maybeUnref.unref === "function") {
          maybeUnref.unref();
        }
      });
    },
  };
}

function defaultLogger(): PollerLogger {
  // `getLogger()` returns the default-namespace logger from `@std/log`.
  // Its `warn` overload accepts a plain string, but TypeScript needs a
  // thin adapter to project the SDK's shape onto our narrow surface.
  const inner = getLogger();
  return {
    warn(message: string): void {
      inner.warn(message);
    },
  };
}
