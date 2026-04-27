# ADR-005: Use `@octokit/auth-app` for JWT, not `jose`

## Status

Accepted (2026-04-26). Supersedes an earlier draft that proposed `jose` for raw JWT signing.

## Context

GitHub App authentication requires:

1. Mint a short-lived (10-minute) JWT signed RS256 with the App's private key.
2. Exchange the JWT for a per-installation token via `POST /app/installations/{id}/access_tokens`.
3. Cache the installation token until ~60 seconds before it expires.

Two reasonable approaches:

- **`jose`** — generic JWT/JOSE library. We write the install-token exchange and cache ourselves.
- **`@octokit/auth-app`** — purpose-built for this exact flow.

## Decision

Use `@octokit/auth-app`. Compose with `@octokit/core` so all GitHub requests share the same auth
path.

## Consequences

**Positive:**

- One dependency replaces both a JWT lib and ~80 lines of bespoke token-exchange + cache code.
- Battle-tested against GitHub's real behavior (expiry edge cases, clock skew, retry on 401).
- Aligns with ADR-004 (prefer the audited path on security boundaries).

**Negative:**

- A small amount of indirection through Octokit's auth-strategy plumbing. Acceptable — the Octokit
  ecosystem is the canonical client for the GitHub API.
