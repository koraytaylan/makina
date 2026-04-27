# ADR-013: Stale-socket cleanup policy for the daemon listener

## Status

Accepted (2026-04-26).

## Context

The daemon binds a Unix-domain socket at a configured path
(`~/Library/Application Support/makina/makina.sock` by default). A clean shutdown unlinks the file;
an unclean shutdown (SIGKILL, OOM, hardware crash) leaves the socket entry on disk. On restart,
`Deno.listen({ transport: "unix", path })` will fail with `AddrInUse` until the leftover entry is
removed.

The first cut of `cleanupStaleSocket` (PR #26, original revision) probed the path with
`Deno.connect` and unlinked it on **any** non-`AddrInUse` failure — and it accepted both `isSocket`
and `isFile` as candidates for unlinking. Two problems followed:

1. **Data loss risk.** If the operator typo'd `socketPath` to point at a regular file (e.g. an
   unrelated `.sock` they keep elsewhere, or the wrong path inside their data directory), the daemon
   would silently delete it on startup. That is a much worse failure mode than refusing to start.
2. **Diagnostic loss.** A `PermissionDenied` from `Deno.connect` (e.g. the socket lives under a
   directory the daemon cannot enter) would also be treated as "stale" and lead to an unlink attempt
   that itself fails opaquely. The operator gets no signal that the original problem was a
   permissions misconfiguration.

Copilot review on PR #26 flagged both issues independently (comments 3 and 4). We agree.

## Decision

`cleanupStaleSocket` is now strictly conservative:

1. **`stat.isSocket` is required.** Any other entry type (regular file, directory, symlink to
   either, character device, …) is left untouched. The subsequent `Deno.listen` call surfaces its
   native error (`AddrInUse`, `IsADirectory`, etc.) and the operator corrects the configuration. We
   **never** delete a non-socket filesystem entry.
2. **The probe failure must be `ConnectionRefused` or `NotFound`** for the path to be classified as
   stale. `ECONNREFUSED` is the canonical kernel signal that a Unix-domain socket file is present
   but unbound — that is exactly the "crashed daemon left a dead socket" case we want to recover
   from. `NotFound` covers the narrow race where the file vanishes between the `lstat` and the
   `connect`.
3. **Every other probe error is rethrown.** `PermissionDenied`, address family mismatches, resource
   limits — all surface to the caller so the daemon fails fast with the original diagnostic instead
   of silently deleting filesystem entries on an unrelated error.

Tests cover the three failure modes:

- A real stale socket (created by `Deno.listen` then `close()` without unlinking) is recovered from
  on the next `startDaemon` call.
- A regular file at `socketPath` survives a failed `startDaemon` with its contents intact.
- A directory at `socketPath` survives a failed `startDaemon`.

## Consequences

**Positive:**

- A misconfigured `socketPath` cannot cause data loss. The worst outcome of a typo is a refusal to
  start.
- Operators see the underlying cause (`PermissionDenied`, `IsADirectory`, …) instead of a generic
  "stale socket cleanup failed".
- The "what does this helper unlink?" question has a one-line answer: "only Unix-domain sockets that
  have no listener bound."

**Negative:**

- The (rare) case where a previous run wrote a regular file at the configured path — for instance, a
  debug script that `touch`ed it — now requires manual cleanup. We accept that trade-off; the
  operator's intent is unknowable from the daemon's perspective and the safe default is to refuse.
- One additional discriminator function (`isStaleSocketProbeError`) and an ADR-mandated set of
  recognised "no listener" error classes that future contributors must keep in sync if Deno adds new
  error types. Acceptable; the surface is two classes today.

## References

- Copilot review on PR #26, comments 3 and 4 (stale-socket cleanup).
- `src/daemon/server.ts::cleanupStaleSocket`.
- `tests/integration/daemon_server_test.ts` — three stale-socket cases.
