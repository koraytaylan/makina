# ADR-003: GitHub App for authentication, not a PAT

## Status

Accepted (2026-04-26).

## Context

The CLI needs to read issues, push commits, open PRs, request Copilot review, watch CI, address
comments, and merge — across potentially many repos, on behalf of a single user. Three auth options:

1. **`gh` CLI session** — reuse the user's `gh auth login` token.
2. **Personal access token** — a long-lived PAT in env or config.
3. **GitHub App** — an installed App with fine-grained permissions and per-installation tokens.

## Decision

Authenticate as a GitHub App. The App is created and installed by the user; the CLI mints
installation tokens on demand using the App's private key.

## Consequences

**Positive:**

- **Higher rate limits** than user PATs (essential when polling many tasks in parallel).
- **Fine-grained, per-repo permissions** — the App requests only what it needs (Contents, Issues,
  Pull requests, Metadata, Statuses, Checks).
- **Bot identity** — commits and PRs surface as the App, distinguishable from the human user's
  manual edits.
- Multi-repo support is natural: one App, many installations.

**Negative:**

- Heavier setup: the user creates the App, installs it on each target repo, downloads a private key,
  and points the CLI at it. The `setup` wizard guides this; `docs/setup-github-app.md` documents the
  steps.
- We carry the responsibility of correct JWT minting and token refresh — delegated to
  `@octokit/auth-app` (see ADR-005).
