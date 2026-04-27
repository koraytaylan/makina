# ADR-022: Conversations-phase watermark is monotonic

## Status

Accepted (2026-04-26).

## Context

Wave 4's stabilize loop CONVERSATIONS phase (issue #17) tracks a per-task `lastReviewAtIso`
high-water mark so a subsequent poll only sees comments strictly newer than the last tick. The
watermark is persisted via the supervisor's `transition()` after every successful poll — both when
the poll yielded fresh comments (after the agent dispatch + thread resolution path) and when the
poll yielded an empty timeline.

The original implementation derived the next watermark with a coalesce:

```ts
const nextWatermark = pollResult.latestCreatedAtIso ?? current.lastReviewAtIso;
```

Round-2 review (PR #40) flagged a regression hazard: `pollResult.latestCreatedAtIso` is _whatever
was the maximum `createdAtIso` in the freshly-fetched batch_. Two real GitHub behaviours can lower
it relative to the persisted watermark:

1. **Truncated/partial timelines.** GitHub may return a window of comments that does not span the
   full history the previous tick saw (e.g., pagination edge or transient rate-limit shedding).
2. **Comment deletions.** A deleted comment vanishes from the timeline; if it was the comment that
   set the previous watermark, the next poll's max would be older.

If we coalesced into a lower value, the next tick's `filterNewComments(_, nextWatermark)` would
treat already-processed comments as new and dispatch the agent on stale work — a correctness bug,
not just a wasted iteration. The brief calls for the watermark to advance after every successful
tick; nothing in the brief authorises it to _retreat_.

## Decision

The conversations-phase watermark is **monotonically non-decreasing** in ISO-8601 lexicographic
order. The supervisor combines the persisted `current.lastReviewAtIso` with the freshly-polled
`latestCreatedAtIso` via a dedicated helper:

```ts
export function monotonicWatermark(
  current: string | undefined,
  incoming: string | undefined,
): string | undefined {
  if (current === undefined) return incoming;
  if (incoming === undefined) return current;
  return incoming > current ? incoming : current;
}
```

The helper is a pure function exported from `src/daemon/supervisor.ts` so it can be unit-tested in
isolation; both the no-new-comments path and the after-dispatch path read the same value.

Behavioural contract:

- `(undefined, x)` → `x` (first observation wins).
- `(x, undefined)` → `x` (no new evidence; pin the existing mark).
- `(x, y)` with `y > x` → `y` (advance).
- `(x, y)` with `y <= x` → `x` (deletion/truncation cannot regress the mark).

The matching unit tests in `tests/unit/stabilize_conversations_test.ts` cover all four cases plus
the both-undefined branch.

## Consequences

**Positive:**

- The watermark cannot regress; comments processed in a prior tick stay below the high-water mark
  even if the upstream timeline is truncated or rewritten.
- The helper is a pure function — testable in isolation, no new dependencies, no clock seam.
- The two call sites in `runConversationsPhase` (no-new-comments early return; after-dispatch
  persist) share one implementation.

**Negative:**

- A pinned watermark hides a _real_ recovery scenario where the operator deletes the comment that
  drove the previous mark and _intentionally_ wants the timeline to look earlier. We treat that as a
  manual intervention case; the operator can edit the persisted `lastReviewAtIso` directly. The
  loop's correctness for the common case dominates this rare edge.
- Slightly more code than the one-line coalesce. The cost is justified by the test coverage and the
  documented contract.

## Alternatives considered

- **Coalesce-only (`pollResult.latestCreatedAtIso ?? current.lastReviewAtIso`).** Rejected — the
  Copilot reviewer correctly identified that this regresses on a partial timeline.
- **Trust GitHub to never truncate.** Rejected — observed in production for other Octokit users; we
  do not control upstream pagination behaviour.
- **Persist the maximum across the whole task lifetime as a separate field.** Rejected — adds a
  schema change for a problem solvable with a one-function fix on the existing field.

## References

- ADR-014 (persistence durability protocol) — the watermark lives in `Task.lastReviewAtIso`.
- ADR-016 (supervisor persist-then-emit-then-act) — the watermark is persisted via `transition()`
  before any side-effect that depends on it.
- ADR-017 (poller cadence and backoff) — the poller drives the per-tick `latestCreatedAtIso`.
- Issue #17 (stabilize loop conversations phase).
- PR #40 round-2 Copilot review (this ADR's triggering feedback).
