# ADR-004: Dependency policy — minimum, justified, JSR-preferred

## Status

Accepted (2026-04-26).

## Context

Pulling in npm packages incurs cost: install size, supply-chain surface, version churn, and (under
Deno's `npm:` resolution) occasional rough edges. We want a small, explicit, defensible dependency
footprint.

## Decision

Every npm dependency that lands in `deno.json` carries a one-line justification in this ADR. JSR
`@std/*` is preferred over npm wherever capability parity exists. New dependencies require an ADR
amendment in the same PR that introduces them.

### Current npm dependencies

| Package                          | Why this                                                                                                                                                                                                                                                                                                                                                 | Why not the alternative                                                                                                                                                                                                                                                           |
| -------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ink`                            | Only mature React-for-CLI library. Drives the entire TUI.                                                                                                                                                                                                                                                                                                | Hand-rolling ANSI/raw-mode TTY would be hundreds of LOC and worse output.                                                                                                                                                                                                         |
| `react`                          | Peer dependency of `ink`.                                                                                                                                                                                                                                                                                                                                | Not optional.                                                                                                                                                                                                                                                                     |
| `@anthropic-ai/claude-agent-sdk` | Official Anthropic SDK; handles subprocess management, MCP, hooks, sessions.                                                                                                                                                                                                                                                                             | Shelling out to `claude` and parsing output by hand sacrifices session resume, structured outputs, and hooks — exactly the features we rely on.                                                                                                                                   |
| `@octokit/auth-app`              | Battle-tested GitHub App auth (JWT signing, installation token exchange, expiry handling).                                                                                                                                                                                                                                                               | Hand-rolling JWT + token refresh is correct but error-prone; this is a security boundary where we want the boring, audited path.                                                                                                                                                  |
| `@octokit/core`                  | Tiny (~10 KB) request layer with retry + rate-limit awareness. Composes with the auth strategy above. Wave 4 (#17) settled the GraphQL question via ADR-019: the conversations phase routes the single `resolveReviewThread` mutation through `@octokit/core`'s built-in `graphql()` helper rather than pulling in `@octokit/graphql` as a separate dep. | The full `octokit` meta-package pulls in REST + GraphQL plugins we mostly do not need; using `core` alone is leaner. ADR-019 documents why we do not add `@octokit/graphql`: one mutation does not justify a new dependency when the existing client already exposes `graphql()`. |
| `zod`                            | One runtime-validation library used across config, IPC, and parsed GitHub responses. Single source of truth for types + validators.                                                                                                                                                                                                                      | Hand-rolled type guards: ~250 LOC of boilerplate that has to be kept in sync with TS types; productivity and safety loss outweighs the ~50 KB.                                                                                                                                    |

### Current JSR (Deno standard library) dependencies

`@std/assert`, `@std/path`, `@std/fs`, `@std/encoding`, `@std/cli` (`parseArgs`), `@std/log`,
`@std/async`, `@std/jsonc`, `@std/streams`, `@std/testing` (BDD, mock, snapshot). These are
first-party Deno utilities, not "dependencies" in the heavyweight sense.

## Consequences

**Positive:** explicit footprint; reviewers can challenge any new addition; the dependency graph is
auditable in a single ADR.

**Negative:** small ergonomic friction when an obvious-looking npm package would shave ten lines of
code. We accept the friction because it forces the question "is this worth the supply-chain
surface?"
