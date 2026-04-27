/**
 * daemon/event-bus.ts — typed in-process pub/sub for {@link TaskEvent}s.
 *
 * The bus is the seam between the supervisor (which produces task events)
 * and every consumer that wants to observe them — the daemon server (which
 * fans them out across IPC), the TUI in dev/embedded mode, and the unit
 * tests. Wave 2 implements only the **in-process** layer; cross-process
 * fan-out lives in `[W2-daemon-server]`.
 *
 * **Design.** Each subscriber owns a bounded queue plus a background pump
 * that drains the queue into the user's callback. Publish is synchronous
 * and never blocks the caller: events are appended to every matching
 * subscriber's queue and the pumps deliver them on the microtask queue.
 * The queue is bounded so a slow handler cannot grow the daemon's heap
 * without limit — once full, **subsequent** publishes for that subscriber
 * are dropped and a single warning is logged per overflow episode (a fresh
 * warning fires the next time the queue fills after draining).
 *
 * The contract surface ({@link EventBus} from `src/types.ts`) uses a
 * synchronous handler callback rather than a `ReadableStream` — that is
 * deliberate so consumers can write linear test code without `for await`
 * boilerplate. Internally each subscriber is backed by a
 * `ReadableStream`-style queue so the publisher/consumer decoupling is
 * preserved and a misbehaving handler cannot poison sibling subscribers.
 *
 * **Wildcard fan-out.** A publish carrying {@link TaskEvent.taskId} `id`
 * is delivered to every subscriber whose `target` is either the same
 * {@link TaskId} `id` (exact match) or the literal wildcard `"*"` (every
 * event regardless of task). `target` is always a {@link TaskId} value or
 * `"*"`, never a `task:<id>` string — the `task:` prefix only appears in
 * log messages emitted by the bus itself.
 *
 * **Handler safety.** A handler that throws is caught at the pump
 * boundary; the throw is logged at `warn` and the pump moves on to the
 * next event. The bus survives an arbitrarily-buggy subscriber. An
 * `async` handler whose returned promise *rejects* is also caught — the
 * pump detects the `PromiseLike` return and attaches a `.catch` so the
 * rejection takes the same warn-and-continue path.
 *
 * The drop-on-overflow policy and the synchronous-callback contract are
 * both load-bearing design choices captured in
 * `docs/adrs/012-event-bus-backpressure-and-sync-callbacks.md`. Any change
 * to either is a contract change and needs an ADR amendment.
 *
 * @module
 */

import { getLogger } from "@std/log";

import { EVENT_BUS_DEFAULT_BUFFER_SIZE } from "../constants.ts";
import type { EventBus, EventSubscription, TaskEvent, TaskId } from "../types.ts";

/**
 * Subscription handle returned by {@link EventBus.subscribe}.
 *
 * Re-exported as a named alias for the design-document term used by the
 * Wave 2 issue brief; callers should prefer the contract type
 * {@link EventSubscription} from `src/types.ts`.
 */
export type SubscriptionHandle = EventSubscription;

/**
 * Narrow logger surface used by the event bus. The `@std/log` `Logger`
 * class satisfies this shape (its `warn` accepts a string), but the
 * narrower interface lets tests inject a recording double without
 * matching the SDK's full overload signature.
 */
export interface EventBusLogger {
  /** Emit a warning. */
  warn(message: string): void;
}

/**
 * Optional construction-time knobs for {@link createEventBus}.
 */
export interface EventBusOptions {
  /**
   * Maximum number of events buffered per subscriber before publishes start
   * dropping with a warning. Defaults to {@link EVENT_BUS_DEFAULT_BUFFER_SIZE}.
   *
   * Must be a positive integer.
   */
  readonly bufferSize?: number;
  /**
   * Logger used to emit slow-consumer warnings and handler-throw warnings.
   * Defaults to `getLogger()` from `@std/log` (the default-namespace logger),
   * adapted to the {@link EventBusLogger} surface.
   *
   * Tests inject a recording logger to assert warning behavior without
   * touching the global logger registry.
   */
  readonly logger?: EventBusLogger;
}

/**
 * Construct an in-process {@link EventBus}.
 *
 * The returned bus supports unbounded publishes from many producers and a
 * bounded queue per subscriber. See the module doc for the full contract.
 *
 * @param options Optional configuration overrides.
 * @returns A fresh `EventBus` ready to wire into the supervisor and the
 *   daemon server.
 *
 * @example
 * ```ts
 * const bus = createEventBus();
 * const sub = bus.subscribe("*", (event) => {
 *   console.log(`[${event.taskId}] ${event.kind}`);
 * });
 * bus.publish({
 *   taskId: makeTaskId("task_demo"),
 *   atIso: new Date().toISOString(),
 *   kind: "log",
 *   data: { level: "info", message: "hello" },
 * });
 * sub.unsubscribe();
 * ```
 */
export function createEventBus(options: EventBusOptions = {}): EventBus {
  const bufferSize = options.bufferSize ?? EVENT_BUS_DEFAULT_BUFFER_SIZE;
  if (!Number.isInteger(bufferSize) || bufferSize <= 0) {
    throw new RangeError(
      `EventBus bufferSize must be a positive integer; got ${bufferSize}`,
    );
  }
  const logger = options.logger ?? defaultLogger();
  const subscribers = new Set<Subscriber>();

  return {
    publish(event: TaskEvent): void {
      // Snapshot the subscriber set so a handler that subscribes (or
      // unsubscribes) during delivery does not mutate the iteration order.
      const snapshot = [...subscribers];
      for (const subscriber of snapshot) {
        if (matches(subscriber.target, event.taskId)) {
          subscriber.enqueue(event);
        }
      }
    },
    subscribe(
      target: TaskId | "*",
      handler: (event: TaskEvent) => void,
    ): EventSubscription {
      const subscriber = createSubscriber(target, handler, bufferSize, logger);
      subscribers.add(subscriber);
      let unsubscribed = false;
      return {
        unsubscribe(): void {
          if (unsubscribed) return;
          unsubscribed = true;
          subscribers.delete(subscriber);
          subscriber.close();
        },
      };
    },
  };
}

/**
 * Internal per-subscriber state. Holds the bounded queue, the pump task,
 * and the overflow flag used to suppress duplicate warnings.
 */
interface Subscriber {
  /** The subscriber's filter: a specific {@link TaskId} or the wildcard `"*"`. */
  readonly target: TaskId | "*";
  /** Append `event` to the queue or drop it if the queue is full. */
  enqueue(event: TaskEvent): void;
  /** Tear down the queue and stop the pump. Idempotent. */
  close(): void;
}

function matches(target: TaskId | "*", taskId: TaskId): boolean {
  return target === "*" || target === taskId;
}

function createSubscriber(
  target: TaskId | "*",
  handler: (event: TaskEvent) => void,
  bufferSize: number,
  logger: EventBusLogger,
): Subscriber {
  // Per-subscriber bounded queue, exposed externally via a ReadableStream so
  // the pump can `await reader.read()` and yield to the event loop between
  // deliveries. This keeps the publisher synchronous while letting handlers
  // run on the microtask queue.
  let controller!: ReadableStreamDefaultController<TaskEvent>;
  const stream = new ReadableStream<TaskEvent>({
    start(c) {
      controller = c;
    },
  });
  const reader = stream.getReader();

  let queueDepth = 0;
  let overflowing = false;
  let closed = false;

  const pump = async () => {
    while (true) {
      let next: ReadableStreamReadResult<TaskEvent>;
      try {
        next = await reader.read();
      } catch {
        // Stream errored or was cancelled; nothing more to do.
        return;
      }
      if (next.done) return;
      queueDepth--;
      // Re-arm the warning so the *next* time the queue fills we get a
      // fresh log line. We re-arm on the first successful drain, not at
      // queue-empty, so the warning rate scales with overflow episodes
      // and not with publish bursts.
      if (overflowing) {
        overflowing = false;
      }
      try {
        // The contract type is `(event) => void`, but TypeScript still
        // permits passing an `async` function (an `async () => void`
        // returns `Promise<void>` which is assignable to `void`). A
        // synchronous throw lands in this catch; a *rejected* Promise
        // would otherwise become an unhandled rejection. We detect a
        // PromiseLike return and attach a `.catch` so async handlers are
        // isolated to the same warn-and-continue policy as sync ones.
        const result = handler(next.value) as unknown;
        if (isPromiseLike(result)) {
          result.then(undefined, (caught: unknown) => {
            logger.warn(
              `event-bus subscriber handler rejected; bus continues. error=${
                stringifyError(caught)
              }`,
            );
          });
        }
      } catch (caught) {
        logger.warn(
          `event-bus subscriber handler threw; bus continues. error=${stringifyError(caught)}`,
        );
      }
    }
  };
  // Detached pump promise: failures are surfaced through the per-handler
  // try/catch above so this `.catch` is defensive only.
  pump().catch((caught) => {
    logger.warn(`event-bus pump exited unexpectedly: ${stringifyError(caught)}`);
  });

  return {
    target,
    enqueue(event: TaskEvent): void {
      if (closed) return;
      if (queueDepth >= bufferSize) {
        // Drop the event. Log exactly once per overflow episode: the flag
        // is cleared the next time the pump successfully drains an event
        // (see above), so a sustained overflow yields one warning while
        // intermittent overflows each get their own.
        if (!overflowing) {
          overflowing = true;
          logger.warn(
            `event-bus subscriber dropping events: queue full (capacity=${bufferSize}, target=${
              stringifyTarget(target)
            })`,
          );
        }
        return;
      }
      queueDepth++;
      controller.enqueue(event);
    },
    close(): void {
      if (closed) return;
      closed = true;
      try {
        controller.close();
      } catch {
        // Already closed (e.g. via cancel); nothing to do.
      }
    },
  };
}

function defaultLogger(): EventBusLogger {
  // `getLogger()` returns the default-namespace logger from `@std/log`. Its
  // `warn` overload accepts a plain string, but TypeScript needs a thin
  // adapter to project the SDK's shape onto our narrow surface.
  const inner = getLogger();
  return {
    warn(message: string): void {
      inner.warn(message);
    },
  };
}

function stringifyTarget(target: TaskId | "*"): string {
  return target === "*" ? "*" : `task:${target}`;
}

function stringifyError(caught: unknown): string {
  if (caught instanceof Error) {
    return `${caught.name}: ${caught.message}`;
  }
  return String(caught);
}

function isPromiseLike(value: unknown): value is PromiseLike<unknown> {
  return (
    value !== null &&
    (typeof value === "object" || typeof value === "function") &&
    typeof (value as { then?: unknown }).then === "function"
  );
}
