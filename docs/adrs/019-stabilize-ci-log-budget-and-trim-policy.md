# ADR-019: Stabilize-CI log byte budget and line-aware trim policy

## Status

Accepted (2026-04-27).

## Context

The Wave 4 stabilize loop's CI sub-phase (issue #16) reacts to a red `getCombinedStatus` by fetching
the failing-job logs through `getCheckRunLogs`, embedding the excerpt into an agent prompt, and
dispatching the agent for a fix iteration. The full check-run log a CI workflow emits — even on a
small repo — is regularly several megabytes; verbose Node/JS toolchains push it past ten. Forwarding
the full blob to Claude has three concrete failure modes:

1. **Prompt-budget pressure.** A single ten-megabyte log blows past every model's context window
   before any other prompt content lands. The agent hard-fails with a request-too-large error rather
   than reading the failing assertion.

2. **Bus-queue and persistence amplification.** The supervisor publishes every agent message on the
   in-process event bus (subscribers' bounded queues are sized to a few hundred entries per
   `EVENT_BUS_DEFAULT_BUFFER_SIZE`). Forwarding multi-megabyte tool outputs without a budget floods
   the queue and trips the per-subscriber drop-and-warn path documented in ADR-012.

3. **Signal-to-noise for the model.** Real CI logs are dominated by noise (linter warnings,
   environment dumps, retries) before the line that actually broke. The agent has to scroll past
   kilobytes of green progress messages to find the failing stack trace.

The trimming policy needs to satisfy two competing constraints:

- **Generous enough** that the trailing context around the failing assertion is preserved intact. A
  1 KB excerpt strands the agent in mid-sentence; 1 MB defeats the purpose.
- **Bounded enough** that pathological logs (stack-trace explosions, infinite-loop output) do not
  overwhelm the prompt. Hard cap by **bytes**, not lines, because a single pathological line can
  blow past a line-count budget while staying under a sensible byte budget.

The ADR-017 poller already separates cadence from byte-level concerns; this ADR fills the matching
gap on the CI-fetcher side.

## Decision

The supervisor exposes a `STABILIZE_CI_LOG_BUDGET_BYTES` constant (default **100 KB** =
`100 *
1_024`) and a per-supervisor override `TaskSupervisorOptions.ciLogBudgetBytes`. The CI phase
fetches each failing-job log through `getCheckRunLogs`, decodes the bytes as UTF-8 (lossy, so
malformed sequences become `U+FFFD` rather than aborting the iteration), and runs the result through
`trimLogToBudget(text, budgetBytes)`:

1. **Fast path.** If the encoded length already fits within `budgetBytes`, return the input
   unchanged (no marker, no allocation cost beyond the one-shot encode-and-measure).

2. **Line-boundary trim.** Otherwise, walk the input's lines from the **end** backwards,
   accumulating whole lines until the next line would push the running tail past `budgetBytes`. The
   CI failure context lives at the **end** of the log (the assertion that broke is the last line;
   the lines just before it are the immediate setup). Keeping the trailing window also matches what
   a human would scroll to first when reading a CI log.

3. **Hard byte slice fallback.** If even a single trailing line exceeds the budget (e.g. a
   megabyte-long stack-trace pretty-printed onto one line), fall back to a hard byte slice against
   the encoded form and a UTF-8 decode of the tail. The lossy decoder absorbs the surrogate pair an
   arbitrary slice may have split.

4. **Marker line.** Every trimmed result is prefixed with a sentinel
   `[…truncated; showing trailing <budget> bytes…]` so downstream readers (the agent prompt, log
   viewers, integration tests) can tell at a glance the excerpt was truncated. The marker is **not**
   counted against `budgetBytes` because it is metadata, not content; counting it in would shrink
   the actual log window every time the marker grew.

## Consequences

**Positive:**

- The agent prompt stays inside the model's context window even on pathological logs.
- The TUI and persistence sinks see bounded payloads end-to-end through the CI phase.
- Tests can drive the trim policy deterministically — `trimLogToBudget` is exported as a pure
  function and the unit suite covers fast-path, line-boundary, byte-slice, and zero-budget branches.

**Negative:**

- A failing job whose useful context lives in the **middle** of the log (rather than the trailing
  window) loses signal. We accept this trade because the trailing-window heuristic matches the
  dominant CI shape and the human-fallback path (`NEEDS_HUMAN`) preserves the worktree for
  inspection — the operator can always read the full log through the GitHub UI.

- The default 100 KB cap is generous but not infinite; a verbose-by-design test framework can still
  produce single failing logs that overflow. The override on
  `TaskSupervisorOptions.ciLogBudgetBytes` is the escape hatch for repos that genuinely need more
  context.

**Neutral:**

- The line-walk is `O(lines)` because each iteration re-encodes one line; for the 100 KB budget this
  is a few hundred lines at most. Encoding the input once up-front (to detect the fast path) and
  re-using it would optimize this further, but the current cost is well below the rest of the
  supervisor's per-iteration overhead.

## References

- `src/constants.ts` — `STABILIZE_CI_LOG_BUDGET_BYTES`.
- `src/daemon/supervisor.ts` — `trimLogToBudget`, `runStabilizeCi`.
- `tests/unit/stabilize_ci_test.ts` — coverage.
- ADR-012 — event-bus backpressure.
- ADR-017 — poller cadence and backoff.
