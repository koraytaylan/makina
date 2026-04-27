# ADR-022: Stabilize-CI ZIP extraction for check-run logs

## Status

Accepted (2026-04-27).

## Context

The Wave 4 stabilize-CI loop (issue #16, ADR-019) fetches per-job CI logs through
`GitHubClient.getCheckRunLogs(repo, checkRunId)` and embeds the trimmed excerpt into the agent
prompt that drives the next fix iteration. The W1 GitHub client documents and implements that method
as returning the raw `application/zip` bytes the GitHub REST API hands back
(`GET /repos/{owner}/{repo}/check-runs/{check_run_id}/logs`):

```ts
// src/github/client.ts:467-491
async getCheckRunLogs(repo, checkRunId): Promise<Uint8Array> {
  // …
  return await coerceToBytes(response.data); // raw application/zip
}
```

Round-1 of PR #41 wired the supervisor to decode those bytes directly through `TextDecoder("utf-8")`
and feed the result into `trimLogToBudget`. For real GitHub responses that yields gibberish: the ZIP
container's local-file headers, deflate-compressed payloads, and central-directory records are not
UTF-8, and the agent prompt ends up with a few hundred bytes of `PK\x03\x04` magic plus binary noise
instead of the failing assertion. The in-memory test double sidestepped this by queueing
pre-extracted text bytes — round-2 review (Copilot) flagged the asymmetry.

Two options were on the table:

- **Change the client contract.** Have `getCheckRunLogs` extract the ZIP and return text. Cleaner
  call sites, but every other consumer of the bytes (snapshots, replays, future analysis tools)
  loses the ability to round-trip the original archive. Forces an immediate test rewrite for the
  in-memory double.
- **Extract on the supervisor side.** Keep the client honest about what GitHub returns; have the CI
  loop do the unzip. Constrains the change to the only consumer that actually needs decoded text
  right now, and lets the test double keep queuing ready-to-use bytes that bypass the ZIP step (a
  `looksLikeZip(...)` magic-byte check guards the unzip path).

We picked the second option. It keeps W1's `GitHubClient` interface stable, avoids a churn cycle for
`tests/helpers/in_memory_github_client.ts`, and confines the extraction logic to the single consumer
that needs it.

## Decision

The supervisor decodes `getCheckRunLogs` bytes through a new `decodeCheckRunLogs(bytes)` helper in
`src/daemon/supervisor.ts` rather than calling `decodeUtf8Lossy(bytes)` directly. The helper:

1. Inspects the magic prefix (`PK\x03\x04` / `PK\x05\x06`). Non-ZIP byte streams take the legacy
   raw-UTF-8 path so the in-memory test double's ready-to-use text is forwarded verbatim.
2. For real ZIP responses, parses the End-of-Central-Directory record from the tail, walks every
   Central-Directory File Header, and reads each entry's local header to find the data offset.
3. Decompresses each entry: method 0 (`STORED`) copies through; method 8 (`DEFLATE`) feeds the
   payload through `DecompressionStream("deflate-raw")` (WHATWG Compression Streams; supported in
   Deno 2). Other methods raise.
4. Concatenates entries with a `--- <path> ---` separator so the agent prompt sees readable per-step
   output, then hands the full text to `trimLogToBudget` (ADR-019) which still enforces the 100 KB
   byte budget.
5. Falls back to the raw-byte UTF-8 decode if the parse fails mid-way — a partial-garbage prompt is
   still more useful than no log context, and the trim step still bounds the size.

No new npm dependency is added (ADR-004 dependency-policy):

- The ZIP parser is ~120 LOC of inline central-directory reader. We only need to support the two
  compression methods GitHub actually uses (`STORED`, `DEFLATE`); a streaming `unzip` library would
  carry far more code than that.
- `DecompressionStream("deflate-raw")` is a platform built-in in Deno 2 — no additional install, no
  transitive dependencies.

## Consequences

**Positive:**

- The agent prompt now contains the actual failing-job log content, restoring the value of the CI
  sub-phase.
- W1's `GitHubClient` contract stays intact; no other consumer pays a churn tax.
- Test doubles can keep queuing pre-extracted text bytes; the magic-byte short-circuit means the
  unzip path never runs in unit tests unless the test explicitly builds a ZIP fixture.

**Negative:**

- We now own a small ZIP parser. The blast radius is contained to one helper, but if GitHub ever
  switches to a different compression method (none on the roadmap; method 8 has been the default
  since 1989) the helper would need extending.
- The supervisor pays a small CPU cost for `DecompressionStream` per failing check on red CI; this
  is dominated by the GitHub round-trip latency, not a bottleneck.
- Two layers of bytes-to-text conversion (ZIP entries → UTF-8 → trim-to-budget) — acceptable because
  the trim step's input contract is text and the helper composes cleanly.

## Round-3 amendment (2026-04-27): bounded streaming decompression

Round-3 review (Copilot) flagged the original `inflateRaw` helper as an OOM risk: it drained the
full `DecompressionStream` into memory before the supervisor's later `trimLogToBudget` step capped
the output. For a single multi-MB compressed entry that unpacks to tens of MB (which the GitHub docs
explicitly warn about — large CI jobs routinely produce log archives in the 5–50 MB range), the
daemon could see a multiplicatively larger transient allocation than the configured
`STABILIZE_CI_LOG_BUDGET_BYTES` would suggest.

The amendment threads the byte budget through the extractor:

1. `decodeCheckRunLogs(bytes, maxBytes?)` — the call site at `dispatchCiAgent` now passes
   `ciLogBudgetBytes` as `maxBytes`.
2. `extractZipEntriesAsText(bytes, maxBytes?)` — tracks a `remainingBudget` counter across entries,
   slices `STORED` payloads with `subarray(0, remainingBudget)`, and forwards the per-entry
   remaining budget into the DEFLATE inflater. Entries past the cumulative budget are skipped
   entirely (not just trimmed) so we never fault their data into memory.
3. `inflateRawBounded(compressed, maxBytes)` (replacing the unbounded `inflateRaw`) reads the
   decompression stream chunk-by-chunk and `cancel()`s the reader as soon as the accumulated chunks
   reach `maxBytes`. A trailing chunk that would spill past the cap is sliced; subsequent chunks are
   never decompressed.

Returned tuple: `{ entries, truncated }` so `decodeCheckRunLogs` can append a
`[…truncated; extraction stopped at <N> bytes…]` marker — operators reading the agent prompt see
_both_ the in-extraction cap (this ADR) and the in-prompt cap (ADR-019) when the budget bites.

Peak memory after the amendment is bounded by `O(maxBytes)` regardless of the original archive's
uncompressed size. The `trimLogToBudget` step (ADR-019) still runs on the rendered output to
preserve line-boundary trimming and the leading marker contract.

The unbounded path (`maxBytes` omitted or non-finite) is preserved so the in-memory test double and
any future caller that legitimately needs the full archive can continue to drain the stream.

## Related

- ADR-004: Dependency policy (justification for the no-new-dep choice).
- ADR-019: Stabilize-CI log byte budget and line-aware trim policy.
- Issue #16: stabilize loop — CI phase.
