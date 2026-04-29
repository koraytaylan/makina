# ADR-026: Hybrid open-core monorepo restructure

## Status

Accepted (2026-04-28).

## Context

Through Waves 0–5 the project lived as a single Deno tree (`src/`, `tests/`, `main.ts` at repo
root). The product was a single deliverable — a CLI binary distributed via GitHub releases.

The product direction is shifting. We are pivoting from CLI-only to a SaaS model:

- The existing **CLI stays as a freemium open-source client** that runs the orchestration engine
  locally on the user's laptop.
- A future **Fresh-based web app** (separate, private repo) will provide a paid tier that runs
  agents in cloud per-tenant containers.
- A future **static marketing site** will be the brand face.

Both deliverables — the local CLI and the cloud web app — share the same orchestration engine
(`Supervisor`, `GitHubClient`, `Persistence`, `WorktreeManager`, `AgentRunner`, IPC). Only the
presentation layer differs: terminal vs. web. The single-tree layout could not express that split
cleanly: the engine and the TUI lived side by side under `src/`, the test helpers lived alongside
production helpers in `tests/`, and there was no boundary between "what's reusable in the cloud" and
"what is CLI-only."

We needed a layout that:

1. Lets the engine be consumed by **both** the local CLI (workspace link, today) and the cloud web
   app (JSR, when the private repo lands).
2. Keeps the CLI **and the engine** open source, but lets the SaaS code (auth, billing, tenant
   management) live in a **private** repo.
3. Doesn't over-engineer for Day 1 — JSR publishing is set up structurally but not actually
   triggered until the web repo exists.
4. Stays cheap to evolve: the engine and the CLI evolve in lockstep most of the time, so they should
   be easy to refactor across in one PR.

## Decision

Restructure into a Deno workspace with two packages on this (public) repo, leaving the future web
app for a separate (private) repo:

```
packages/core/   # @makina/core — orchestration engine (publishable to JSR)
packages/cli/    # @makina/cli  — TUI shell + daemon entry point (binary distribution)
tests/e2e/       # End-to-end suite (spans both packages)
```

Three sub-decisions inside this:

- **Hybrid repo strategy.** The CLI + core stay in this open-source repo because they evolve
  together — splitting them into separate repos would impose a multi-day cross-repo coordination
  cost on every contract refactor for no real benefit. The web app, when it lands, gets its own
  private repo and consumes `@makina/core` via JSR. Marketing site location is deferred — it can go
  in this repo as `packages/site` or in its own repo when we build it.

- **Maximalist core.** Everything that is environment-agnostic moves to `@makina/core`, including
  defaults that today happen to be "local" implementations (file-backed `Persistence`, real-git
  `WorktreeManager`, Claude SDK `AgentRunner`, Unix-socket `daemon/server.ts`). The cloud web app
  consumes core in a per-tenant container and either uses these defaults as-is (file persistence on
  a mounted volume; real-git in container fs) or swaps in alternative adapters (DB-backed
  persistence at scale) — both options stay open. Only `tui/`, the interactive `setup-wizard`, and
  `main.ts` are CLI-coupled enough to belong in `@makina/cli`.

- **JSR structure now, publish later.** `packages/core/deno.json` declares the package name,
  version, and `exports` map (`mod.ts` + `tests/helpers/mod.ts` as `@makina/core/test-helpers`).
  This forces explicit thinking about the public surface from day one. Actual `deno publish` to JSR
  is deferred until the external (web) consumer needs it. Today the CLI consumes `@makina/core` via
  Deno workspace local linking — no publishing involved.

We deliberately **did not split** the Unix-socket transport in `packages/core/src/daemon/server.ts`
out of the daemon handler logic. When the web app eventually needs HTTP/WebSocket transport, that
refactor will happen then; pre-emptively splitting today would add complexity for no current
benefit.

## Consequences

**Wins.**

- The cloud web app, when it lands, can be a thin Fresh frontend + container orchestration layer
  that wires the engine with its own (or the default) adapters. The mental model is "the engine is
  hosted somewhere — locally on your laptop, or on our infra" rather than two divergent code paths.
- The public API surface of core is now explicit (`packages/core/mod.ts`). Adding a new export is a
  deliberate step instead of an accidental side effect of `import` in another file.
- `deno doc --lint` enforces JSDoc on the explicit core surface. The barrel is one file to review
  when shaping what consumers can rely on.
- Test discipline holds: `@makina/core/test-helpers` ships the in-memory doubles alongside the
  contracts they implement, so external consumers (the future web repo) inherit them.

**Trade-offs.**

- Some imports inside `packages/cli/` reach into `packages/core/src/...` deep paths via the
  workspace link rather than going through the published `mod.ts` barrel. That is fine for now — the
  CLI is in the same workspace, can read whatever — but the day we publish to JSR we'll need to
  confirm everything the CLI uses is exposed via `mod.ts` (or accept that the CLI consumes deep
  paths and skip JSR for the CLI itself, which is already the plan).
- The `daemon/server.ts` transport coupling will need to be split when web arrives. We're paying
  that cost later instead of now.
- Two `InstallationAuthResult` interfaces in `packages/core/src/github/{app-auth,app-client}.ts`
  collide in the barrel; `mod.ts` resolves it by listing app-client's exports explicitly and
  omitting the colliding name. Source-level rename is a future cleanup, out of scope for this
  restructure.

**Migration cost (paid in the same PR).**

- 5 staged green commits on `restructure/monorepo`: workspace skeleton, core carve-out, cli
  carve-out, CI/release plumbing, this ADR. Every commit leaves `deno task ci` green.
- ~100 files moved with `git mv` so history follows. Imports updated via a perl pass for the
  `@makina/core` rewrite plus targeted edits for the corner cases (the wizard's sibling imports
  inside `src/config/`, the spawn-the-daemon paths in three test files and the e2e harness, the
  pre-commit gate's `deno check` target).

## Out of scope

The following are explicitly future work, each gets its own ADR / spec:

- The future cloud web app architecture (Fresh, deployment topology, per-tenant container model vs.
  shared worker pool).
- Cloud-side adapters (DB-backed `Persistence` at scale, alternative `WorktreeManager`,
  containerised `AgentRunner`).
- HTTP/WebSocket transport split for `daemon/server.ts`.
- Tenant authentication, billing, multi-tenancy.
- Marketing site location (this repo as `packages/site` or its own repo).
- First JSR publish of `@makina/core`.
