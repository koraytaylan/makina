# ADR-010: In-process event bus — drop-on-overflow + synchronous callbacks

## Status

Accepted (2026-04-26).

## Context

`src/daemon/event-bus.ts` (Wave 2) is the in-process seam between the supervisor (which produces
`TaskEvent`s) and every consumer that wants to observe them — the daemon server (which fans events
out across IPC), the TUI in dev/embedded mode, and the unit tests. Two design questions are
load-bearing enough that they need to be pinned down before the daemon server, the TUI client, and
the persistence layer all start depending on the bus's exact contract:

1. **What happens when a subscriber is slower than the publisher?** A bounded queue is mandatory —
   the bus runs inside a long-lived daemon and a slow handler must not grow the heap without limit —
   but the choice between `drop`, `block`, and `coalesce` is a real decision with externally visible
   consequences for the TUI's event stream and any future audit log.
2. **What is the handler signature: synchronous callback, async callback, or `ReadableStream`?**
   This shapes every consumer.

We did not write this down before merging the implementation. Copilot flagged the omission against
the issue's Definition of Done in PR #23. This ADR captures the rationale before later waves layer
on top of the contract.

## Decision

### 1. Backpressure: bounded per-subscriber queue, drop-on-overflow with a single warning per episode

Each subscriber owns a bounded queue (default capacity `EVENT_BUS_DEFAULT_BUFFER_SIZE = 256`). When
a publish would push the queue past capacity:

- The event is **dropped** for that subscriber. Other subscribers are unaffected.
- A **single** warning is logged via `@std/log` per overflow episode. The flag re-arms the next time
  the pump successfully drains an event, so a sustained overflow yields one log line and
  intermittent overflows each get their own. This is a deliberate compromise between "log every
  drop" (noisy) and "log once per process" (silently swallows new failures).

Alternatives considered and rejected:

- **Block the publisher.** The publisher is the supervisor's hot path; backpressure on the
  supervisor would couple TaskEvent throughput to the slowest subscriber and could deadlock if a
  subscriber's handler ever publishes back through the bus.
- **Unbounded queue.** Trivially exposes a heap-growth DoS — a buggy or hung TUI client could pin
  the daemon's RSS.
- **Coalesce / ring-buffer (overwrite oldest).** Reasonable for "latest known state" feeds, wrong
  for `TaskEvent`s where the consumer cares about the _sequence_ (`started → log → log → finished`).
  Dropping the _newest_ events keeps the consumer's view internally consistent — what it has seen is
  a true prefix of what was published — at the cost of missing the tail.
- **Per-subscriber on-disk overflow.** Defers the problem rather than solving it; adds a write path
  inside the daemon for events that are explicitly best-effort.

Drop-on-overflow is the right policy because `TaskEvent`s are observability signals, not operational
state. The supervisor's persisted state (ADR forthcoming for the persistence layer) is the source of
truth; events are a derived stream consumers can resync from.

### 2. Handler signature: synchronous callback `(event: TaskEvent) => void`

The contract surface (`EventBus.subscribe` in `src/types.ts`) accepts a synchronous callback. The
implementation guards against three failure modes inside the pump:

- A **synchronous throw** is caught and logged at `warn`; the pump moves on.
- An **async handler** (TypeScript permits an `async () => Promise<void>` where `() => void` is
  expected) returning a rejected promise: the pump detects the `PromiseLike` return and attaches a
  `.catch` so the rejection is funneled through the same warn-and-continue path. This was a real
  hole flagged by Copilot review on PR #23 and is now covered by tests.
- A **handler that subscribes or unsubscribes during delivery** does not perturb the in-flight
  publish: `publish` snapshots the subscriber set before iterating.

Alternatives considered and rejected:

- **`ReadableStream<TaskEvent>` per subscriber.** Forces every consumer (and every test) to write
  `for await` boilerplate or call `getReader()` and manage cancellation. The bus already uses a
  `ReadableStream`-backed queue _internally_ for the publisher/consumer decoupling — exposing it
  externally would leak that implementation choice into the contract.
- **Async-only handler.** Most consumers (TUI render, IPC fan-out) are happy synchronous; forcing
  them to be `async` adds turn boundaries with no benefit. A consumer that genuinely needs async
  work can pass an `async` function and rely on the rejection-handling path above.
- **Event emitter with array-of-handlers.** No isolation guarantee — a synchronous throw from one
  handler stops downstream handlers for the same event. The per-subscriber pump model is
  isolation-by-construction.

## Consequences

**Positive:**

- Publisher is never blocked by a slow subscriber; the supervisor's hot path stays bounded.
- Heap growth is bounded by `bufferSize × subscriberCount`.
- Handler API is the simplest thing that works (sync callback) without losing the async escape
  hatch.
- A misbehaving subscriber (sync throw, async rejection, or self-unsubscribe mid-publish) cannot
  poison the bus or other subscribers.

**Negative:**

- Drop-on-overflow means consumers cannot assume they have seen every event. Any consumer that needs
  strong ordering or completeness guarantees (e.g. an audit log) must read from the persisted state
  store, not from the bus. This is a deliberate split: the bus is the _fast, best-effort_
  observability path; the state store is the _durable, complete_ source of truth.
- A consumer that wants to do meaningful async work per event has to accept that successive events
  arrive on the same logical pump and that a slow async handler will drop newer events past the
  buffer. Consumers that need async fan-out should attach a sync handler that hands the event to
  their own work queue.
- Re-evaluating the policy later (e.g. switching to coalesce for a particular subscriber, or
  exposing a `mode: "block"` option) is a contract change that ripples to every consumer; this ADR
  is the gate for any such change.

## References

- Issue #8 (Definition of Done requires ADR for new architectural choices).
- PR #23 Copilot review: `https://github.com/koraytaylan/makina/pull/23`.
- ADR-004 (dependency policy — `@std/log` and `@std/async` are first-party JSR).
