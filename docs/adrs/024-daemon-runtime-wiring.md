# ADR-024: Production daemon runtime wiring

## Status

Accepted (2026-04-26).

## Context

Wave 5 (#43) is the integration that turns `main.ts daemon` from "binds a Unix socket and answers
ping" into "starts the supervisor, persistence, worktree manager, GitHub-app auth, agent runner,
poller, and an IPC command dispatcher". Earlier waves built the modules; this wave wires them.

Three architectural choices needed to be made up front:

1. **Where the persistence file lives.** The supervisor's `Persistence` writes a JSON file
   atomically (write-tmp + rename + dir-fsync, per ADR-014). The schema makes `config.workspace`
   mandatory but does not name a `state.json` location.
2. **How multi-repo support routes through a single-client supervisor.** `TaskSupervisorOptions`
   takes one `GitHubClient`, but the daemon's `config.github.installations` is a map of
   `<owner/repo> → installationId`. The wiring needs a way to keep the `(repo, installationId)`
   pairs explicit so future code can pick the right client per task without rewriting the FSM.
3. **How `persistence.loadAll()` should rehydrate non-terminal tasks.** The brief calls for replay,
   but the supervisor does not yet expose a `hydrate(tasks)` API: `start()` is the only entry point
   that mints in-memory entries and it walks an FSM forwards from `INIT`. A naïve "loop and call
   `start()`" would mint duplicate task ids and try to re-clone existing worktrees.

## Decision

### 1. Persistence path

The persistence layer reads and writes `<config.workspace>/state.json`. Co-locating with the
workspace keeps every piece of per-task state (worktrees, bare clones, the JSON store) under a
single path the operator can `rm -rf` to reset. The daemon ensures the workspace directory exists
before constructing `createPersistence` so the first save never races a `mkdir`.

Operators who want a different location (a faster disk, a network mount, etc.) can override by
adding a `persistencePath` field to the schema in a follow-up; the wiring is a single `path`
argument away.

### 2. Multi-repo client registry

`main.ts` builds a `Map<RepoFullName, { auth, client }>` keyed by every entry in
`config.github.installations`. The map is passed to `createDaemonHandlers` via a
`hasGitHubClientFor(repo)` predicate so an `/issue --repo owner/unconfigured` fails fast with a
precise error.

The supervisor itself receives the **default repo's** client today
(`createTaskSupervisor({ githubClient: defaultRegistryEntry.client, ... })`). This is the documented
gap: a task targeting a non-default repo will issue API calls under the default installation. The
supervisor's surface is widening to accept a `githubClientFor(repo)` lookup as a follow-up; once
that lands, the wiring passes the same registry through. The handler-level guard (plus the
supervisor's per-repo state machine being indistinguishable across repos other than the client)
means that until then, the only operator-visible difference is _which installation token_ a
multi-repo task uses — not whether it gets stuck.

Tracked as a follow-up; the registry already exists, only the supervisor option name and the
threading change.

### 3. Persistence replay

`persistence.loadAll()` runs at boot and the daemon logs the rehydrated count. The supervisor's
in-memory table is **not** seeded today because the FSM has no `hydrate` entry point: re-injecting a
`STABILIZING` task would skip every transition the supervisor needs to make to drive it forward
(re-establishing the worktree, re-resolving the head SHA, restarting the poller).

The conservative wiring loads the file, surfaces a startup log line, and stops there. Operators see
how many tasks survived a restart; the task records remain on disk; nothing re-issues a side effect
the supervisor cannot replay safely. A proper resume protocol — rebind worktrees, re-arm pollers,
catch `STABILIZING` tasks back up to live — needs supervisor-side support and is tracked as a
follow-up.

This is the conservative interpretation of "the supervisor's existing API may already do this —
verify": the supervisor does not, so the daemon does not pretend it does.

## Alternatives considered

### Persistence-side resume protocol (rejected for this PR)

We considered building a `supervisor.hydrate(tasks)` shim in `main.ts` that injects loaded tasks
into a private setter on the supervisor. This would require either changing the supervisor's public
surface (a future-incompatible change to the cross-wave contract in `src/types.ts`) or reaching into
private state (a layering violation). The follow-up issue can do this cleanly with a real API.

### Per-repo supervisor instances (rejected)

We considered constructing one supervisor per repo so each has the right client. This duplicates the
persistence layer (each supervisor would need its own slice of `state.json` or its own file), the
worktree manager (which is already per-repo internally), and the event bus subscribers. The
single-supervisor-with-registry path keeps the architecture's existing seams intact and pushes the
multi-repo concern onto the supervisor, which is where the FSM can resolve the right client per
task.

### Embedding the JSON status payload in `ack.error` (accepted, with a note)

The `/status` command needs to ship a structured payload back to the IPC client. The current
`AckPayload` schema is `{ ok, error? }` — no structured payload variant. We considered widening the
schema to add a `data?` field but that is a wire-format change every consumer (TUI included) would
have to re-validate. For Wave 5 we embed the JSON in `error` (a string) and document it; a future
schema bump can introduce a dedicated `status-response` envelope without breaking the existing flow.

## Consequences

- The daemon now boots a fully-wired runtime: every supervisor collaborator is constructed, the IPC
  `command` and `prompt` envelopes route to the supervisor surface, and the event bus delivers the
  supervisor's transitions to subscribed clients.
- The fallback "no config" path stays narrow: the binary still boots without `config.json` (so
  `--version` and the smoke tests work) but the handler map is empty in that mode and the daemon
  answers `unimplemented` to every command. The fallback is documented in `main.ts` and is the
  reason the existing `tests/integration/main_daemon_test.ts` continues to pass.
- Two follow-up issues are tracked in the JSDoc:
  - Widen the supervisor surface to accept a per-task GitHub client lookup so the multi-repo
    registry can route correctly.
  - Add a supervisor `hydrate(tasks)` (or equivalent) entry point so `persistence.loadAll()` can
    actually resume non-terminal tasks.
