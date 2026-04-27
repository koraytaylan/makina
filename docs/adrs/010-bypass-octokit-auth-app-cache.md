# ADR-010: Bypass `@octokit/auth-app`'s built-in token cache

## Status

Accepted (2026-04-26). Refines [ADR-005](005-no-jose-using-octokit-auth-app.md).

## Context

`@octokit/auth-app@7` ships an internal LRU token cache (`dist-src/cache.js`):

- Capacity 15 000 entries.
- TTL = 1 minute less than the GitHub-issued installation-token lifetime (~59 of 60 minutes).
- Cache key derived from `installationId` plus optional repository/permission scoping.

By default, `auth({ type: "installation", installationId })` returns the cached token until it falls
inside the library's own ~1-minute lead window.

The Wave 2 GitHub App auth implementation in `src/github/app-auth.ts` maintains its own cache, keyed
by the branded `InstallationId`, with refresh lead time
`INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS` (currently 60 s). The point of _our_ cache is:

1. The branded type means the daemon, not a string-typed Octokit-internal key, owns identity.
2. The lead time is configurable in one place and unit-testable with an injected clock.
3. The same cache layer naturally handles in-flight request deduplication for concurrent callers.

If the library's cache is left enabled, those guarantees do not hold:

- The library may serve a token that is still inside its 1-minute window even though our cache
  considers it expired (or the other way around — clock-granularity drift between the two layers).
- A future tightening of `INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS` (e.g. to react to a slow
  network or to widen safety margin) silently does nothing because the library still hands back the
  pre-existing cached token.
- Two layers of cache means two places where a stale token can hide — bad for the audit trail on a
  security boundary that is the actual GitHub auth credential.

## Decision

Pass `refresh: true` on every `@octokit/auth-app` `auth({ type: "installation" })` call from
`src/github/app-auth.ts`. This bypasses the library's LRU cache and forces a fresh
`POST /app/installations/{id}/access_tokens` exchange every time _our_ cache decides a refresh is
needed. Caching is then governed end-to-end by the daemon's logic, never by the library's.

The `refresh: true` flag is encoded as a `readonly refresh: true` field on the
`InstallationAuthHook` interface so a missed forward (e.g. a future strategy implementation) is a
TypeScript compile error rather than a silent regression.

A regression-guard test
(`createGitHubAppAuth: forwards refresh:true to bypass the library's LRU cache`) asserts the flag is
set on every hook invocation.

## Consequences

**Positive:**

- One cache, one source of truth, one audit point. The daemon's `InstallationId`-keyed cache is the
  only place a token lives.
- `INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS` actually controls the refresh boundary. Tuning this
  constant has the documented effect.
- The TypeScript signature makes the bypass mandatory; future contributors cannot quietly drop it.

**Negative:**

- We give up the library cache's "free" deduplication and 15 000-entry headroom. Mitigated:
  - The daemon already deduplicates concurrent requests through its own `inFlight` map.
  - Realistic deployment has a small constant number of installations, well under any LRU cap.
- Each refresh always pays the network cost of the install-token exchange. That is the correct
  trade-off for a security credential — caching past our chosen lead time is exactly what we want to
  forbid.

## Alternatives considered

- **Match our lead time to the library's** (~60 s) and hope the two caches stay in lockstep.
  Rejected: relies on a private library implementation detail that the library is free to change in
  a patch release, and does not solve the dual-source-of-truth problem.
- **Supply a no-op cache adapter via `createAppAuth`'s undocumented `cache` slot.** Rejected:
  undocumented surface, no compile-time guarantee, and the per-call `refresh: true` flag is the
  library's documented escape hatch for exactly this case.
