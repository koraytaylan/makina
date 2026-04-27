# ADR-014: Persistence durability protocol — atomic JSON store with parent-chain fsync

## Status

Accepted (2026-04-26).

## Context

Issue #7 (Wave 2) ships the supervisor's persistence layer. The daemon is a long-lived process that
must be able to crash at any byte boundary — `kill -9`, OOM, kernel panic, hardware power loss — and
come back up with a coherent view of every in-flight task. The supervisor reloads state on boot
through `Persistence.loadAll`, then drives every state transition through `Persistence.save` /
`Persistence.remove`. The on-disk store is the single source of truth between daemon lifetimes.

A concrete failure mode the protocol must rule out: an operator yanks the power cord one byte into
the next `save`. On reboot the daemon must observe **either** the previous full snapshot **or** the
new full snapshot — never a torn mix, never silent data loss, never a half-overwritten file that the
JSON parser rejects on next startup.

Two requirements push beyond a naive `Deno.writeTextFile`:

1. **Atomicity across observers.** A concurrent `loadAll` (or an operator with `cat`) must always
   see a complete snapshot. POSIX `rename(2)` within a single filesystem is the standard primitive
   for this — readers see either the old inode or the new inode, never a partially written file.
2. **Durability across power loss.** Linux ext4, XFS, macOS APFS, and FreeBSD UFS all let renames
   live in the parent directory's dirty page cache after the syscall returns. A power loss between
   `rename` and the next directory flush can drop the new directory entry **even though** the file
   data is on disk. The same hazard applies to `mkdir(2)` for any intermediate directory we create
   on first boot.

The acceptance criteria for #7 require ADRs for any new architectural choice. Copilot's review on PR
#27 (round 4, comment 3145359805) flagged the missing ADR for the durability protocol; this document
is that ADR.

## Decision

### 1. Atomic write protocol

Every mutation rewrites the **entire** store using the standard write-tmp + fsync + rename +
parent-chain-fsync dance, in this exact order:

1. **Serialize** the in-memory snapshot to JSON (2-space indent, matching `deno fmt`).
2. **Ensure** every directory from the filesystem root down to the live file's parent exists,
   tracking which directories we actually created so we can fsync their parents in step 7.
3. **Open** `<path>.tmp` with `{ write: true, create: true, truncate: true }`. Truncating any
   leftover from a previous crash is correct: the rename in step 5 never happened, so the live
   `<path>` is still intact and the orphan tmp file carries no information.
4. **Write** the bytes through a `writeAll` loop (Deno's `FsFile.write` may short-write), then
   `fdatasync` the file descriptor via `FsFile.syncData()`.
5. **Rename** `<path>.tmp` over `<path>` with `Deno.rename` — the atomic commit point.
6. **Fsync the leaf parent directory.** Without this the rename's directory-entry update can live in
   dirty page cache and be lost on power failure even after the file's data is durable.
7. **Fsync every parent of every directory we just created** in step 2, walking outward from leaf to
   root. Deduplicated against step 6 so we never fsync the same directory twice.

POSIX `rename(2)` within one filesystem is atomic, so any observer — a concurrent `loadAll`, the
next daemon restart, an operator with `cat` or `jq` — sees either the previous full snapshot or the
new full snapshot. There is no third option.

### 2. Concurrency

A single internal mutex (a chained promise on the `AtomicJsonPersistence` instance) serializes
`save` and `remove` so two callers cannot race the rename. `loadAll` waits behind any in-flight
write so it never observes a partially serialized in-memory snapshot, even though the on-disk file
is itself atomic.

The mutex chain catches and discards rejection so a single failed save does not poison every
subsequent caller. The cost — a few microseconds of promise-chain overhead per mutation — is
negligible against disk IO.

### 3. Crash recovery

If `<path>.tmp` exists at startup it is the residue of a crash mid-write: the rename in step 5 never
happened, so the live `<path>` is still intact and the orphan is information-free. `loadAll` ignores
it; the next successful `save` overwrites it via `truncate: true`. If the live `<path>` is missing
entirely, the store is empty — `loadAll` returns `[]` rather than failing. This is the correct
semantics for a fresh daemon install and matches what the supervisor would reconstruct from GitHub's
task list anyway.

### 4. `@std/fs` posture (Option A)

Issue #7 specifies "All file IO through `@std/fs` where possible". Copilot's round-4 review
(comment 3145359822) asked us to reconsider. We evaluated three approaches and chose **Option A: use
`Deno.*` natives directly with documented justification**.

The persistence layer's IO calls split cleanly into three categories:

| Operation                                 | Used in                    | `@std/fs` equivalent | Decision                                                                             |
| ----------------------------------------- | -------------------------- | -------------------- | ------------------------------------------------------------------------------------ |
| `Deno.readTextFile`                       | `loadAll`                  | none                 | Direct — `@std/fs` does not wrap the read path.                                      |
| `Deno.rename`                             | `writeAtomic`              | none                 | Direct — `@std/fs` does not expose rename. This is the atomic commit point.          |
| `Deno.open` for tmp file + `syncData()`   | `writeAtomic`              | none                 | Direct — `@std/fs` returns no `FsFile`, so `syncData()` is not reachable through it. |
| `Deno.open` for parent dir + `syncData()` | `syncDirectory`            | none                 | Direct — same reason; durability requires the file descriptor.                       |
| `Deno.mkdir` per directory in the chain   | `ensureDurableParentChain` | `ensureDir` (close)  | Direct — see below.                                                                  |

`@std/fs.ensureDir` is the closest fit. We **rejected** it for the durability path because it does
not report which intermediate directories it created. We need that list to fsync each new
directory's parent in step 7 — without it, a power loss between `mkdir` and the next external sync
can drop the new directory's entry and orphan the state file even though we fsync'd the file itself.
Replacing the explicit `Deno.mkdir`-with-tracking loop with `ensureDir` would silently weaken the
protocol to "best-effort" durability across power loss on a fresh install.

`@std/fs.exists` is unsuitable for the same reason — it adds a TOCTOU window between the check and
the dependent operation. We use `Deno.errors.NotFound` on the operation itself instead.

`@std/fs.emptyDir` is irrelevant — we never want to empty the persistence directory.

Wave-2 `persistence.ts` therefore imports only `@std/path` (for `dirname`). Future modules that need
broad-strokes file management without a durability requirement (configuration scaffolding, worktree
teardown, cache directories) will use `@std/fs` per the issue's policy. The persistence layer is the
pinned exception, justified by the durability contract that `@std/fs` cannot preserve.

**Option B considered and rejected.** Wrapping every syscall in a thin internal `IO` interface and
routing the few operations `@std/fs` exposes through it would add an indirection layer for pure
policy compliance. The signal-to-noise ratio is wrong: at most one of the five distinct operations
can plausibly be routed through `@std/fs` (the mkdir loop), and that one is the one that must NOT be
routed because the API contract is too coarse to express the durability requirement.

## Consequences

**Positive:**

- The store survives `kill -9`, OOM, kernel panic, and power loss with the strongest durability
  guarantee POSIX permits: either the previous snapshot or the new snapshot, never a torn mix and
  never a "lost rename" where the file data is on disk but the directory entry is gone.
- A concurrent `loadAll` always observes a complete snapshot — readers and writers cannot tear.
- The on-disk format is human-readable JSON, inspectable with `cat` / `jq` for postmortems.
- The serialization mutex closes the in-process race for `save` / `remove` callers without exposing
  the mutex on the public `Persistence` interface — the daemon never needs to think about it.
- The protocol is the same on macOS (APFS) and Linux (ext4/XFS) — both filesystems honor `fdatasync`
  on a directory file descriptor and rename(2) atomicity within a filesystem.

**Negative:**

- Every mutation rewrites the entire file. At Wave-2 scale (≤ a few dozen tasks) this is invisible.
  If the task count grows past a few hundred, Wave 5+ may switch to a sharded layout or a WAL — the
  `Persistence` interface in `src/types.ts` is stable across both.
- Two extra fsyncs per mutation (the file descriptor and the leaf parent directory; more if the
  directory chain was just created on first boot). Real-world cost: low single-digit milliseconds on
  SSD, dominated by the rename itself. Acceptable against the durability win.
- The mutex serializes mutations even when the underlying filesystem could handle concurrent renames
  safely. We accept the conservative bound; daemon write rates are dominated by polling cadence
  (seconds), not by mutex contention.
- The deviation from the issue's "all file IO through `@std/fs`" policy is documented but real. We
  mitigate by keeping `@std/path` (the only import we still need) and by limiting the exception to
  the persistence module — every non-durability file IO callsite in the codebase remains free to use
  `@std/fs`.

**Alternatives considered and rejected:**

- **Write-ahead log (WAL).** Lower per-mutation cost (append-only fsync of a small record) at the
  cost of a more complex recovery path: replay the log on startup, periodically compact, handle
  partial-record tail truncation. Wrong shape for Wave-2 scale where the entire store is small
  enough to rewrite atomically and the operator value of a single human-readable JSON file is high.
  Reconsider in Wave 5+ if the active task set crosses the rewrite-cost threshold.
- **SQLite (via an FFI binding).** Buys real ACID semantics for free, but introduces a native
  dependency, an embedded query language, and an opaque on-disk format that `cat`/`jq` cannot
  inspect. The Wave-2 supervisor's data model is "list of small denormalized records" — well below
  the threshold where a relational store is the right tool.
- **`Deno.writeTextFile` with no `.tmp` staging.** Loses atomicity: a crash mid-write leaves the
  live file partially overwritten and the JSON parser fails on next startup. Trivially wrong for the
  durability contract.
- **Direct `write()` syscalls (skipping `FsFile`).** `FsFile.syncData` is the durable-write
  primitive Deno exposes; bypassing it would require FFI and gain nothing. Rejected.

## References

- Issue #7 (Wave 2 — atomic JSON state store).
- Copilot review on PR #27, round 4 (comments 3145359805 ADR-required, 3145359822 `@std/fs`
  posture).
- `src/daemon/persistence.ts` — production implementation.
- `src/types.ts::Persistence` — the contract this module satisfies.
- `tests/integration/persistence_test.ts` — durability and atomicity coverage including parent-chain
  fsync, mid-write crash, concurrent save/load, and corrupt-file rejection.
- ADR-012 (event bus) — references this ADR as the source of truth for supervisor state.
