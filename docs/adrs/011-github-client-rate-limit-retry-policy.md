# ADR-011: Header-driven retry policy for the GitHub client

## Status

Accepted (2026-04-26).

## Context

`GitHubClientImpl` (Wave 2) wraps `@octokit/core` and is the only HTTP path the supervisor uses to
talk to GitHub. We need a retry policy for transient failures that:

1. Honors GitHub's two flavors of rate limit (primary quota and secondary abuse limits) so we wait
   the right amount instead of pounding the API.
2. Does **not** retry on non-rate-limit errors, because every retry adds latency the supervisor
   hides badly — the stabilize loop already has a higher-level backoff for unknown failures.

GitHub signals rate limits two ways:

- `Retry-After: <seconds>` on `429` — the secondary (abuse) rate limit.
- `X-RateLimit-Remaining: 0` paired with `X-RateLimit-Reset: <epoch-seconds>` on `403` (and
  occasionally `429`) — the primary, hourly-bucket quota.

Plain `403` without those headers is a permission/scope error: the installation token is missing a
permission, the SSO authorization expired, the resource is private to a different org. Retrying
makes the same call return the same `403` after a needless sleep.

The first cut of the client retried _any_ `403`/`429`: when no header guidance was present,
`computeRetrySleepMilliseconds` always fell back to a 1 s constant sleep. The Copilot review on PR
#29 (issue #5) flagged that this both contradicts the JSDoc on the function ("returns null when no
header guidance applies") and inflates latency on the permission-error path.

A second pass of that same review flagged a leftover contradiction: the prose said the retry should
fire on a `Retry-After` header for any 4xx, but `requestWithRetry` still gated on
`status ∈ {403, 429}` before consulting the headers. We resolved it the way the prose read:
`requestWithRetry` no longer gates on status, and the header check itself is the gate.

## Decision

The retry fires **only** on a clear rate-limit signal. The decision lives entirely in
`computeRetrySleepMilliseconds(status, headers)`; `requestWithRetry` simply propagates when that
function returns `null`. The signals it honors:

1. `Retry-After` present (any HTTP status) → sleep that many seconds, retry once.
2. `X-RateLimit-Remaining: 0` paired with `X-RateLimit-Reset` present (any HTTP status) → sleep
   until the reset, retry once.
3. `429` with neither header → fall back to `DEFAULT_FALLBACK_RETRY_SLEEP_MILLISECONDS` (1 s) and
   retry once. The `429` status alone is a rate-limit signal.
4. `403` with neither header → propagate immediately. No sleep, no retry.
5. Anything else (`5xx`, `404`, etc.) with no rate-limit headers → propagate immediately.

`computeRetrySleepMilliseconds` returns `number | null` for real now: `null` means "do not retry,"
which `requestWithRetry` translates into a propagated error. The 1 s constant is cap-clamped by
`maxRetrySleepMilliseconds` so a future caller cannot exceed the configured retry ceiling.

Decoupling the retry decision from the status code costs us nothing in practice — GitHub only sets
`Retry-After` / `X-RateLimit-Reset` on rate-limited responses — but it keeps the impl and the ADR
saying the same thing in one place: "did GitHub tell us to wait?"

Sleep durations are clamped from above by `maxRetrySleepMilliseconds` (default 5 minutes) so a
runaway `Retry-After` cannot stall the supervisor for an hour. Negative deltas (a reset already in
the past) clamp to 0 — we still issue the retry immediately because the bucket has already refilled.

## Consequences

**Positive:**

- Permission errors (`403` without rate-limit headers) surface in roughly one round-trip instead of
  one round-trip plus 1 s of dead-air. Useful when the supervisor needs to flag `NEEDS_HUMAN`
  promptly.
- The retry semantics now match the JSDoc and read straight off the headers — there is one sentence
  ("did GitHub tell us to wait?") that decides whether to retry, and that sentence is inspectable
  from the response.
- The narrower retry surface keeps the request log shorter, which makes the `github-call` TaskEvents
  easier to read in the TUI.

**Negative:**

- A future GitHub change that started returning `403` for transient rate-limit conditions without
  any of the three headers above would not retry. We accept that: GitHub's contract for primary rate
  limits is documented to include the `X-RateLimit-Reset` header, and if it ever stops we want a
  loud failure rather than silent retries.
- The retry policy is per-request and stateless. Multiple concurrent rate-limited calls each pay
  their own retry sleep instead of coordinating — the supervisor caps in-flight requests elsewhere;
  no shared limiter is needed at this layer.

## Alternatives considered

- **Always retry on 4xx with a constant fallback.** Original behavior. Rejected: confuses rate-limit
  and permission failures, and the constant sleep adds latency on every permission error.
- **Retry 5xx as well.** Rejected: the supervisor's stabilize loop has its own backoff that already
  handles transient GitHub trouble; retrying both layers compounds latency on a real outage.
- **Use `@octokit/plugin-retry`.** Rejected for now: the plugin's policy is opinionated and adds
  another dependency for behavior we can express in ~30 lines. Revisit if our retry needs grow (e.g.
  exponential backoff, circuit-breaker semantics).
