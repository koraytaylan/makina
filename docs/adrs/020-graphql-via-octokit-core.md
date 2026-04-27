# ADR-020: GraphQL via `@octokit/core`, not a separate `@octokit/graphql` dependency

## Status

Accepted (2026-04-26).

## Context

Wave 4's stabilize loop CONVERSATIONS phase (issue #17) needs to mark a review thread as resolved
once the agent has pushed a fix for it. GitHub does **not** expose a REST endpoint for resolving a
review thread — the `resolveReviewThread` mutation lives in the GraphQL API, full stop. The
conversations phase therefore needs a single GraphQL mutation, called once per resolved thread per
agent push.

The dependency-policy bar set by ADR-004 says: every npm dependency that lands in `deno.json`
carries a one-line justification in that ADR, and `@octokit/core` is preferred over the full
`octokit` meta-package because we do not need most of its surface. ADR-004 also calls out that we
would only add `@octokit/graphql` "later **only if** review-thread resolution requires it." Wave 4's
arrival makes this the moment that question lands on the table.

We have three live options:

1. Add `@octokit/graphql` to `deno.json` for a single mutation. The library is ~3 kB compressed, has
   the same maintainer surface as `@octokit/core`, and exposes a tiny `graphql()` helper.
2. Use `@octokit/core`'s built-in `graphql()` method. Octokit's core class already exposes a
   `graphql()` callable that issues the same `POST /graphql` request the standalone library does; it
   shares the same retry/rate-limit pipeline as the REST methods.
3. Hand-roll a `fetch()` against `https://api.github.com/graphql` with a manually-built body.

## Decision

Use the existing `@octokit/core` instance's built-in `graphql()` callable to issue the
`resolveReviewThread` mutation. Do **not** add `@octokit/graphql` to `deno.json`.

The mutation lives as a module-level constant (`RESOLVE_REVIEW_THREAD_MUTATION` in
`src/github/client.ts`), and `GitHubClientImpl.resolveReviewThread` is a one-line call:

```ts
async resolveReviewThread(threadId: string): Promise<void> {
  const token = await this.auth.getInstallationToken(this.installationId);
  await this.octokit.graphql(RESOLVE_REVIEW_THREAD_MUTATION, {
    threadId,
    headers: { authorization: `token ${token}` },
  });
}
```

The token strategy mirrors every other call on `GitHubClientImpl`: mint via the injected
`GitHubAuth`, attach to the GraphQL request as the `authorization` header. Token rotation behaviour
is identical to the REST path, which keeps the security and retry surfaces consistent.

## Consequences

**Positive:**

- Zero new npm dependencies. ADR-004's table is unchanged.
- One Octokit instance per `GitHubClientImpl`; the GraphQL call shares its retry, rate-limit, and
  test-seam wiring (`request: { fetch }`) with the REST methods. Tests inject a scripted fetch and
  drive the mutation through the same harness.
- The `RESOLVE_REVIEW_THREAD_MUTATION` constant is a stable, snapshot-friendly string; the unit test
  asserts on the exact body the production code sends, catching silent drift.

**Negative:**

- `@octokit/core`'s `graphql()` method is less ergonomic than the standalone library's typed helpers
  — there is no built-in fragment-composition or schema-driven typing. We use it for one trivial
  mutation, so the ergonomic loss is limited; if a future wave adds dozens of mutations or needs
  typed fragments, revisiting the decision (and adding `@octokit/graphql` then) is a one-line
  amendment.
- The token attachment lives in this module instead of being centralised in a
  `graphqlWithAuth`-style helper. Re-introducing centralisation would require a small wrapper —
  trivial when a second mutation arrives.

## Alternatives considered

- **Add `@octokit/graphql` to `deno.json`.** Rejected because the library does nothing the existing
  `@octokit/core` instance does not already do. Adding a dependency for one mutation contradicts the
  "minimum, justified" half of ADR-004.
- **Hand-roll a `fetch()`.** Rejected because we would duplicate the rate-limit/retry pipeline
  ADR-011 already routes through the Octokit request layer. The savings (one fewer level of
  indirection) are not worth the policy duplication.
- **Defer the mutation.** Rejected because the conversations phase brief explicitly calls for thread
  resolution after every agent push; without it, Copilot review re-requests would surface stale
  threads as still-open and the loop would not converge.

## References

- ADR-004 (dependency policy).
- ADR-011 (GitHub client rate-limit retry policy) — same retry pipeline.
- Issue #17 (stabilize loop conversations phase).
