# ADR-017: Poller cadence, backoff, and rate-limit honoring

## Status

Accepted (2026-04-26).

## Context

ADR-006 settled the polling-vs-webhooks question for the stabilize loop: the daemon polls GitHub at
a fixed cadence rather than standing up a public webhook endpoint. Issue #13 (Wave 3) ships the
implementation — one timer per task in any `STABILIZING` sub-phase, driven by a `Poller` abstraction
the supervisor wires up with a per-phase `fetcher`.

The behavioral surface needs to satisfy four constraints simultaneously:

1. **Steady-state cadence.** A successful tick spaces the next call by `intervalMilliseconds`
   (default `POLL_INTERVAL_MILLISECONDS`, 30 s). Wall-clock-from-completion, not
   wall-clock-from-issue, so a slow GitHub call does not pile up the next tick on top of itself.
2. **Honor explicit rate-limit signals.** `GitHubClient` (issue #5, ADR-011) already retries
   header-driven rate-limit responses internally, so the fetcher the supervisor passes in normally
   returns a successful payload by the time it reaches the poller. But the request budget the client
   uses can still surface — the supervisor's per-phase fetcher may throw a typed `PollerError` that
   carries an explicit `retryAfterMs` the poller must honor before scheduling the next tick.
3. **Backoff on transient errors.** A fetcher that simply rejects (network blip, transient 5xx,
   parse error) must not spin: the next tick spaces exponentially with jitter, capped at a sane
   upper bound, until a successful tick clears the backoff state.
4. **Clean cancellation.** `AbortSignal.abort()` stops the loop within one pending sleep — no
   stranded timers, no spurious final fetch, no lingering reference to the supervisor.

The poller also has to be **testable without real time**. Wave 2 ran into trouble when tests reached
for `Date.now`/`setTimeout` mocks process-wide; ADR-014's persistence work codified the lesson
("inject the clock; never mutate global timers"). The poller's contract therefore exposes a
synthetic clock seam, the way `GitHubClient` does for its retry sleep.

## Decision

`createPoller(opts)` returns the W1 `Poller` interface. `poll(args)` schedules a single-flight loop
per `taskId` and returns a `{ cancel(): void }` handle. The supervisor calls `cancel()` when the
task leaves a `STABILIZING` sub-phase, and may also pass an `AbortSignal` whose abort event triggers
the same cancellation path.

### Cadence

After each fetcher resolution the poller sleeps for one of three durations before the next call:

- **Success without `retryAfterMs`** → `intervalMilliseconds`.
- **Success or `PollerError` carrying `retryAfterMs`** → that value, clamped to
  `[0, POLLER_BACKOFF_MAX_MILLISECONDS]`. Header-driven rate-limit waits dominate any other signal.
- **Failure (rejection or `PollerError` without `retryAfterMs`)** → exponential backoff with jitter:
  `min(POLLER_BACKOFF_BASE_MILLISECONDS * 2^(attempt - 1), POLLER_BACKOFF_MAX_MILLISECONDS)`
  multiplied by a uniform jitter factor in
  `[1 - POLLER_BACKOFF_JITTER_RATIO, 1 + POLLER_BACKOFF_JITTER_RATIO]`. The attempt counter is the
  per-task consecutive-failure count starting at 1 (so the first backoff is exactly the base) and
  resets to zero on the next successful tick. The exponent is internally capped at
  `POLLER_BACKOFF_MAX_ATTEMPT_EXPONENT` (30) to keep `Math.pow(2, …)` away from `Infinity`; with the
  default `MAX` of five minutes the series saturates well before that cap.

`intervalMilliseconds` is required on the `poll(...)` call; the supervisor passes
`POLL_INTERVAL_MILLISECONDS` from `src/constants.ts` (the per-phase default). Callers can pass `0`
when they want the poller to fire back-to-back (used by the in-memory tests for the conversations
phase).

### Single-flight per task

The poller keeps at most one _active_ poll loop per `taskId`. The supervisor is the only caller that
should reuse the same task id for overlapping `poll(...)` calls; if it does, the second call returns
a handle that cancels the prior loop before starting fresh. This matches the supervisor's
expectation that a stabilize-phase transition tears down and re-arms the poller atomically.

The current `fetcher` contract does **not** accept an `AbortSignal`, so a supersession that lands
while a previous `fetcher()` promise is still pending cannot abort the in-flight call — the new
loop's first `fetcher()` may run concurrently with the old loop's last one. This is acceptable
because (a) the cancelled loop drops its result on resolution (the `isCancelled()` guard short-
circuits before `onResult`/`onError` run), and (b) the supervisor's `fetcher` implementations are
read-only against GitHub. If a future `fetcher` needs to mutate state, the contract will need an
`AbortSignal` parameter and an ADR amendment; the synthetic clock seam is already abortable, so the
extension is mechanical.

### `PollerError` taxonomy

The poller exports `PollerError` extending `Error` with two narrow subclasses (encoded as a
discriminant `kind`):

- `kind: "rate-limited"` — fetcher exhausted retries inside `GitHubClient` and the supervisor wants
  the loop to wait `retryAfterMs`. Treated as a successful tick for backoff bookkeeping (the attempt
  counter does **not** climb), but the next tick is delayed.
- `kind: "fatal"` — non-retryable error (auth revocation, missing installation). The poller stops
  the loop and surfaces the error through the optional `onError` callback; without `onError` the
  error is logged and the loop exits. Either way the supervisor's higher-level state machine decides
  the task's fate.

Plain `Error` rejections (everything else a fetcher might throw) take the exponential-backoff path:
the poller assumes the rejection is transient and retries, capped by
`POLLER_BACKOFF_MAX_MILLISECONDS`.

### Synthetic clock seam

`createPoller` accepts an optional
`clock: { now(): number; sleep(ms, signal?: PollerSleepAbort): Promise<void> }`. Production wiring
leaves it undefined and the poller uses a `Date.now`/`setTimeout`-backed default; tests inject a
deterministic clock that records every sleep duration. The clock is the **only** way the poller
measures time — there is no `Date.now()` call elsewhere in the module, no fall-through to a global
timer.

The `signal` argument on `sleep` is the cancellation seam: `cancel()` (and external `AbortSignal`
abort events) flip an internal flag and call `controller.abort()`, which the default sleep wakes on.
Implementations must resolve immediately when `signal.aborted === true` at entry and on the first
abort event after entry, and must resolve immediately for `milliseconds <= 0` (or non-finite)
without scheduling a real timer.

### Cancellation

A `cancel()` call (or an `abort` event on the supplied signal) flips a local `cancelled` flag and
resolves the in-flight `clock.sleep` promise via `AbortController`-coordinated `clock.sleep`. The
default sleep implementation listens for the abort signal and resolves immediately on abort;
injected test clocks do the same. After cancellation the poller makes no further calls to `fetcher`
or `onResult`.

## Consequences

**Positive:**

- The supervisor sees a single, narrow surface (the W1 `Poller` interface) regardless of which
  stabilize phase drives the loop. No per-phase backoff math leaks into the supervisor.
- Header-driven rate-limit waits live with the GitHub client (ADR-011) while the poller respects the
  _resolved_ wait surfaced as `retryAfterMs`. The two layers compose without duplicating header
  parsing.
- The synthetic-clock seam keeps tests under a millisecond each — no real timers, no `Date.now`
  global mutation. Coverage is straightforward even for the jitter branch (the test injects a
  deterministic `random()` source through the same options bag).
- Cancellation is observable within one pending sleep. The TUI's "stop task" button does not have to
  wait the full poll interval before the stabilize loop quiesces.

**Negative:**

- The poller is intentionally minimalist about retry policy: it does not distinguish 5xx from
  network errors from parse errors. A fetcher that wraps non-HTTP failures as exceptions still takes
  the backoff path, which is correct for transient hazards but adds latency on truly fatal bugs (a
  parse-error bug retries until the supervisor times out the phase). Mitigated by
  `PollerError(kind: "fatal")` — fetchers that _know_ an error is non-retryable can short-circuit
  the loop.
- The exponential cap is global. Per-phase tuning (CI vs. conversations) would need additional
  knobs; deferred until a real use case shows up. The current cap
  (`POLLER_BACKOFF_MAX_MILLISECONDS`) is wide enough that both phases are comfortable inside it.

## Alternatives considered

- **Honor headers in the poller.** Rejected: the GitHub client already parses `Retry-After` /
  `X-RateLimit-Reset` (ADR-011). Re-doing it in the poller would mean every fetcher had to plumb
  headers through, and fetchers that aren't GitHub-backed (the conversations phase reads the
  in-memory event log of agent-runner output too) have no headers to forward. Surfacing the resolved
  wait as `retryAfterMs` keeps the decision colocated with the layer that knows the protocol.
- **Use `setInterval`.** Rejected: `setInterval` does not wait for the prior tick to settle, which
  can pile up calls when the fetcher occasionally takes longer than `intervalMilliseconds`. The
  await-then-sleep loop is one line longer and avoids the failure mode entirely.
- **Cancel by throwing inside the fetcher.** Rejected: makes cancellation observable only after the
  next fetcher resolution. An `AbortSignal`-backed sleep cancels mid-wait, which is what the TUI
  expects.
