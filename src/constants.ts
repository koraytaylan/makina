/**
 * constants.ts — every named numeric (and small string) constant in makina.
 *
 * Identifiers carry units so call sites read like English:
 * `await delay(SETTLING_WINDOW_MILLISECONDS)` rather than `await delay(60000)`.
 * Every consumer module imports from here; bare numeric literals are forbidden
 * in `src/` modules, with the narrow exception of obvious mathematical
 * identities (`0`, `1`, `-1`, length comparisons, percentages, etc.).
 *
 * When a value gains a config override (Wave 2's loader), this file is the
 * **default** — the loader resolves user values against the schema first and
 * falls back here when a field is omitted.
 *
 * @module
 */

/**
 * Single source of truth for the makina version.
 *
 * Wave 0 hard-coded the same string in `main.ts`. Wave 1 onwards reads it
 * from here so the release pipeline only has to bump one place.
 *
 * @example
 * ```ts
 * import { MAKINA_VERSION } from "./constants.ts";
 * console.log(MAKINA_VERSION); // "0.0.0-dev"
 * ```
 */
export const MAKINA_VERSION = "0.0.0-dev";

/**
 * Settling window after the stabilize loop becomes idle, in milliseconds.
 *
 * The supervisor only transitions a task from `STABILIZING` to
 * `READY_TO_MERGE` once **no** stabilize phase has produced work for at least
 * this many milliseconds. Prevents flapping when CI flips green-red-green
 * within a polling cycle. Configurable via `lifecycle.settlingWindowMilliseconds`.
 */
export const SETTLING_WINDOW_MILLISECONDS = 60_000;

/**
 * Per-task GitHub poll cadence, in milliseconds.
 *
 * The Poller queries the combined commit status, check runs, and review
 * activity at this cadence, backing off on `Retry-After` /
 * `X-RateLimit-Reset`. Configurable via `lifecycle.pollIntervalMilliseconds`.
 */
export const POLL_INTERVAL_MILLISECONDS = 30_000;

/**
 * Maximum number of agent iterations the supervisor will spend on a single
 * task before transitioning it to `NEEDS_HUMAN`.
 *
 * Bounds runaway agent loops on perpetual conflicts or perpetually-red CI.
 * Configurable via `agent.maxIterationsPerTask`.
 */
export const MAX_TASK_ITERATIONS = 8;

/**
 * Lead time before a GitHub installation token expires at which the auth
 * cache treats it as stale and refreshes, in milliseconds.
 *
 * Without lead time, an in-flight request can race a token expiry; one
 * minute is enough headroom for the slowest reasonable GitHub call to
 * complete after the cache hit.
 */
export const INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS = 60_000;

/**
 * Maximum decoded size of a single IPC frame, in bytes.
 *
 * Real IPC traffic is well under one megabyte (envelope + a few KB of event
 * payload). The cap protects the daemon from a malicious or buggy client
 * exhausting memory by claiming an enormous length-prefix.
 */
export const MAX_IPC_FRAME_BYTES = 1_048_576;

/**
 * Maximum length-prefix value that the codec will accept, in characters.
 *
 * `MAX_IPC_FRAME_BYTES` plus a small overhead, expressed as the largest
 * decimal-string length the framer will read before declaring the frame
 * malformed. Six digits caps the prefix at 999,999 bytes which is below
 * `MAX_IPC_FRAME_BYTES` — both bounds are checked at decode time.
 */
export const MAX_IPC_LENGTH_PREFIX_DIGITS = 8;

/**
 * Default capacity of the in-process event-bus per-subscriber queue.
 *
 * Subscribers consume events through a bounded `ReadableStream`; once the
 * buffer fills the publisher applies backpressure (Wave 2 picks the exact
 * policy). The cap keeps a slow TUI from holding the daemon hostage.
 */
export const EVENT_BUS_DEFAULT_BUFFER_SIZE = 256;

/**
 * Lower bound on `lifecycle.settlingWindowMilliseconds` accepted by the
 * config schema.
 *
 * Anything below one second turns the settling window into a no-op (a single
 * polling tick easily exceeds it) so the schema rejects tighter values with
 * a helpful message.
 */
export const MIN_SETTLING_WINDOW_MILLISECONDS = 1_000;

/**
 * Upper bound on `lifecycle.settlingWindowMilliseconds` accepted by the
 * config schema.
 *
 * One hour is a generous ceiling that catches obviously-wrong values (a
 * misplaced unit, e.g. seconds for milliseconds yielding 60_000_000).
 */
export const MAX_SETTLING_WINDOW_MILLISECONDS = 3_600_000;

/**
 * Lower bound on `lifecycle.pollIntervalMilliseconds`.
 *
 * Below five seconds we are likely to trip GitHub's secondary rate limits
 * even for small repos; the schema rejects tighter values.
 */
export const MIN_POLL_INTERVAL_MILLISECONDS = 5_000;

/**
 * Upper bound on `lifecycle.pollIntervalMilliseconds`.
 *
 * Above one hour the loop is effectively asleep; almost certainly a config
 * mistake.
 */
export const MAX_POLL_INTERVAL_MILLISECONDS = 3_600_000;

/**
 * Lower bound on `agent.maxIterationsPerTask`.
 *
 * One iteration is the minimum useful budget — enough for a single agent
 * fix attempt before the task escalates to `NEEDS_HUMAN`.
 */
export const MIN_MAX_TASK_ITERATIONS = 1;

/**
 * Upper bound on `agent.maxIterationsPerTask`.
 *
 * Above 64 iterations the supervisor is almost certainly thrashing; the
 * schema rejects values that high.
 */
export const MAX_MAX_TASK_ITERATIONS = 64;

/**
 * Lower bound on `github.appId`.
 *
 * GitHub App IDs are positive integers; one is the smallest legal value.
 */
export const MIN_GITHUB_APP_ID = 1;

/**
 * Lower bound on a GitHub App installation id.
 *
 * Installation ids share the same positive-integer space as App ids.
 */
export const MIN_GITHUB_INSTALLATION_ID = 1;

/**
 * Lower bound on a GitHub issue number.
 *
 * Issue numbers start at one within each repository.
 */
export const MIN_GITHUB_ISSUE_NUMBER = 1;

/**
 * Maximum width, in UTF-16 code units, of an `agent-message` payload
 * rendered to the TUI status bar.
 *
 * The status bar is one Ink `<Text>` row; longer text wraps and shoves
 * the rest of the layout down. The cut is by code units (matching
 * `String.prototype.slice`), which is good enough for ASCII-heavy agent
 * chatter and tolerated by Ink's renderer when a surrogate pair lands
 * on the boundary. Bumped only if the status bar grows a multi-line
 * mode in a later wave.
 */
export const STATUS_BAR_TRUNCATION_WIDTH_CODE_UNITS = 80;

/**
 * Chunk size used by `createWizardIo` when pulling bytes from the byte
 * reader, in bytes.
 *
 * One kilobyte is large enough that a single read drains a full prompt
 * answer typed at the terminal, but small enough that the unit tests'
 * "concatenate across reads" path still gets exercised when the test
 * reader hands back fewer bytes per call.
 */
export const WIZARD_READ_CHUNK_BYTES = 1_024;

/**
 * Radix used when parsing decimal user input (App ID, picker indexes).
 *
 * `Number.parseInt(text, RADIX_DECIMAL)` reads slightly clearer than the
 * bare `10`, and the named constant keeps `src/` modules free of magic
 * numbers per the rule at the top of this file.
 */
export const RADIX_DECIMAL = 10;

/**
 * The literal `~/` prefix the loader and wizard expand to `$HOME`.
 *
 * Centralised so the slice operations that strip it
 * (`path.slice(HOME_PREFIX.length)`) carry their intent in the name
 * rather than a bare numeric `2`.
 */
export const HOME_PREFIX = "~/";

/**
 * Base sleep used by the {@link Poller} when retrying after a transient
 * fetcher rejection, in milliseconds.
 *
 * The actual wait is `min(BASE * 2^(attempt - 1), MAX) * jitter`, where
 * `attempt` is the run of consecutive failures since the last successful
 * tick (1 on the first failure, so the first backoff is exactly `BASE`).
 * One second is short enough that a single transient blip recovers inside
 * one poll interval, and long enough that we do not pound a struggling
 * GitHub.
 *
 * See {@link https://github.com/koraytaylan/makina/blob/develop/docs/adrs/017-poller-cadence-and-backoff.md ADR-017}.
 */
export const POLLER_BACKOFF_BASE_MILLISECONDS = 1_000;

/**
 * Upper bound on a single {@link Poller} sleep, in milliseconds.
 *
 * Caps both the exponential backoff series and a `retryAfterMs` value
 * surfaced by a fetcher (e.g. from an upstream rate-limit response) so a
 * runaway value cannot stall the supervisor for an hour. Five minutes is
 * comfortably above any realistic GitHub `Retry-After` and well below
 * the supervisor's settling-window upper bound.
 *
 * See {@link https://github.com/koraytaylan/makina/blob/develop/docs/adrs/017-poller-cadence-and-backoff.md ADR-017}.
 */
export const POLLER_BACKOFF_MAX_MILLISECONDS = 5 * 60 * 1_000;

/**
 * Jitter ratio applied to each {@link Poller} backoff sleep.
 *
 * The exponential delay is multiplied by a uniform random factor in
 * `[1 - ratio, 1 + ratio]` so a fleet of pollers that all hit a transient
 * outage at the same wall-clock instant do not retry in lockstep. A
 * 20% spread is the AWS architecture-blog default for the same problem
 * shape and is small enough that the steady-state cadence stays
 * recognisable.
 *
 * See {@link https://github.com/koraytaylan/makina/blob/develop/docs/adrs/017-poller-cadence-and-backoff.md ADR-017}.
 */
export const POLLER_BACKOFF_JITTER_RATIO = 0.2;

/**
 * Internal ceiling on the consecutive-failure count fed into the
 * {@link Poller}'s `2^(attempt - 1)` exponent.
 *
 * `Math.pow(2, 1023)` is the largest power-of-two finite double; beyond
 * that the multiplication overflows to `Infinity`. The poller's outer
 * `clamp` would still saturate the result to `POLLER_BACKOFF_MAX_MILLISECONDS`,
 * but capping the exponent keeps the math debuggable and avoids a flicker
 * of `Infinity` in trace logs. Thirty is comfortably above the saturation
 * point for every realistic `(base, max)` pair: with the default
 * `BASE = 1 s` and `MAX = 5 min`, the series saturates at attempt nine
 * (`1 s * 2^8 = 256 s`); even a `MAX` of one hour saturates at attempt
 * twelve.
 *
 * Callers configuring an unusually large `backoffMaxMilliseconds` should
 * note that increasing `MAX` past `BASE * 2^29` will not lengthen the
 * series further — the cap dominates first.
 */
export const POLLER_BACKOFF_MAX_ATTEMPT_EXPONENT = 30;
