# ADR-024: App-level GitHub client for the setup wizard

## Status

Accepted (2026-04-26).

## Context

When the first-run `makina setup` wizard (#3) was implemented in Wave 2, it depended on a
`WizardGitHubClient` interface to discover installations. The production wiring was a stub that
threw "GitHub App auth not yet implemented" because `[W2-github-app-auth]` (#4) had not yet landed.
With #4 merged, `createGitHubAppAuth` (in `src/github/app-auth.ts`) now provides installation-token
minting against real GitHub.

The wizard, however, runs **before** any installation has been chosen. It needs two endpoints
neither of which is reachable from the existing installation-scoped `GitHubClientImpl`:

1. `GET /app/installations` — App-scoped, authenticated with a 10-minute App JWT (RS256).
2. `GET /installation/repositories` — installation-scoped, but minted under a token that is
   discarded immediately after the wizard finishes.

`GitHubClientImpl` is constructed with a fixed `installationId` and a `GitHubAuth` strategy that
mints a token for _that_ installation. It cannot answer App-level questions, and bolting a JWT mode
onto its already non-trivial rate-limit-retry surface would couple two unrelated concerns
(installation API ergonomics + setup-time discovery) into one class.

We considered:

- **Option A: Separate `AppClient` companion to `GitHubClient`.** The wizard depends on `AppClient`;
  the supervisor keeps depending on `GitHubClient`. The two surfaces share Octokit and
  `@octokit/auth-app` underneath but expose distinct method sets to callers.
- **Option B: Inline JWT + fetch helpers inside the wizard's production `WizardGitHubClient`
  factory.** No new module, just a small block of code in `src/config/wizard-github-client.ts` that
  mints a JWT, calls the two endpoints with raw `fetch`, and projects the result.

## Decision

We took **Option A**. `src/github/app-client.ts` exposes:

- `AppClient` interface — `listAppInstallations()` and `listInstallationRepositories(id)`.
- `createAppClient(opts)` factory — wires the App credentials to `@octokit/auth-app`, uses
  `@octokit/core` for the REST calls, and projects responses to typed surfaces.
- `GitHubAppClientError` — discriminated error matching the convention set by `app-auth.ts`'s
  `GitHubAppAuthError`. Each error names the failing operation (`createAppAuth` / `mintAppJwt` /
  `mintInstallationToken` / `listAppInstallations` / `listInstallationRepositories` /
  `projectInstallations` / `projectRepositories`).

The wizard's production `WizardGitHubClient` lives in `src/config/wizard-github-client.ts`. It:

1. Reads the PEM key from disk via an injectable `ReadKeyFile` callback (production default expands
   `~/` and uses `Deno.readTextFile`).
2. Constructs an `AppClient` via an injectable factory (production default uses `createAppClient`).
3. Calls `listAppInstallations`, then `listInstallationRepositories` per installation, and projects
   the join into the `WizardInstallation[]` shape the wizard renders to the user.

`main.ts`'s `setup` branch now wires `createWizardGitHubClient()` (no arguments — production
defaults) instead of the stub.

## Consequences

**Positive:**

- The wizard's logic stays clean — it sees one narrow `WizardGitHubClient` interface, not raw
  Octokit handles or JWT blobs.
- The supervisor's installation-scoped client is unchanged; we did not have to widen its surface.
- Both layers reuse the same `@octokit/auth-app` strategy seam so test patterns (factory + hook
  stub) carry over directly from `app_auth_test.ts` to `app_client_test.ts`.
- `GitHubAppClientError` matches the convention of `GitHubAppAuthError`, so call-sites classify
  auth/network failures the same way regardless of which client raised them.

**Negative:**

- Two GitHub-client modules (`client.ts` for installation-scoped use, `app-client.ts` for App-scoped
  use). The duplication is intentional — the auth model differs by call site — but reviewers should
  look in both files when chasing a "GitHub HTTP" question.
- `createAppClient` does not currently inherit the rate-limit retry policy from `GitHubClientImpl`
  (ADR-011). The wizard runs at most a handful of requests per `makina setup` invocation against the
  App's own quota, so a transient 429 is acceptable fallout — the user can simply rerun. If a future
  caller exercises this client at higher cadence we will lift the retry policy into a shared helper
  rather than duplicate it.
