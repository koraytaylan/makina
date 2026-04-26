# Contributing to makina

This project is built by parallel agents (and humans) working from a catalog of GitHub issues. The
conventions below are what keep that workflow tractable.

## Branching strategy

A two-tier model:

- **`main`** is always releaseable. Tagged commits trigger releases. Protected: PRs only, all CI
  checks required, linear history (squash or rebase merge).
- **`develop`** is the integration branch. All feature branches PR into here. Protected: PRs only,
  all CI checks required.
- **`feature/<wave>-<topic>`** is branched off `develop`. One PR per branch, narrowly scoped.
  Squash-merged into `develop`.

**Release flow**: `develop` → PR into `main` → merge → `git tag vX.Y.Z` → push tag → `release.yml`
ships.

## Conventional Commits

Every commit subject follows:

```
<type>[(<scope>)][!]: <subject>
```

- `<type>` ∈ `feat | fix | docs | refactor | perf | test | build | ci | chore | revert`
- `<scope>` is **optional but strongly preferred**. When the change relates to a GitHub issue, the
  scope is the issue reference: `feat(#42): add task switcher overlay`. For cross-cutting work
  without an issue, use a domain word: `refactor(daemon): extract supervisor`.
- `!` after the type or `BREAKING CHANGE:` in the footer triggers a major-version section in the
  changelog.
- Subject is imperative, lower-case, no trailing period, ≤ 72 chars.

The `commit-msg` git hook validates this regex locally; CI re-validates on every PR commit. Bypass
with `git commit --no-verify` only with a written justification in the PR.

Enable the hooks once, in the repo root:

```bash
git config core.hooksPath .githooks
```

## Definition of Done (every PR)

- [ ] New exported symbols carry JSDoc (`@param`, `@returns`, `@example` where useful);
      `deno doc --lint` passes.
- [ ] Tests cover the new code; ≥ 80% lines AND branches; in-memory doubles for any external
      collaborator land in the same PR as the contract they implement.
- [ ] Any new architectural choice ships with its ADR under `docs/adrs/`.
- [ ] User-facing behavior changes update `README.md` and the relevant `docs/*.md`.
- [ ] Commits follow Conventional Commits.
- [ ] `deno task ci` is green on the branch.

The PR template reproduces this list. Don't merge with anything unchecked.

## Parallel-agent guidelines

The project is structured so that many agents can ship work concurrently without conflict:

1. **Contract-first.** Each wave produces or consumes typed interfaces (`src/types.ts`,
   `src/ipc/protocol.ts`, `src/config/schema.ts`). New work imports the contract; it does not
   redefine it. Contract changes need their own focused PR before any consumer changes.
2. **In-memory doubles co-located with the contract** so a parallel agent building a _consumer_ can
   run tests immediately, without waiting for the _provider_ to land. Example:
   `tests/helpers/in_memory_github_client.ts` ships with the `GitHubClient` interface in the same
   wave.
3. **One feature branch per task.** Branch name: `feature/<wave>-<topic>` (e.g.,
   `feature/w2-worktree-manager`). One PR per branch. Squash-merge into `develop`.
4. **Each PR is independently mergeable.** It includes its own tests, JSDoc, ADR (if any), and docs
   updates. The PR template enforces the Definition of Done.
5. **Cross-task coordination is via the contracts**, not via shared mutable types. If an agent needs
   to extend a contract, they raise a contract-change PR first; consumers rebase after it lands.

## Picking up an issue

1. Find an issue labeled `agent:claimable` whose dependencies (in the body) are all closed.
2. Comment on the issue claiming it; flip the label to `agent:in-progress`.
3. Branch off the latest `develop`: `git checkout -b feature/<wave>-<topic>`.
4. Implement against the issue's Acceptance criteria.
5. Open a PR against `develop`. Link the issue with `Closes #N`. Fill the PR template.
6. Address review feedback (Copilot or human). Re-request review on each push.
7. Merge.

## Reporting bugs

Use the **Bug** issue template (`gh issue create --template bug.yml`). Do not file blank issues.
