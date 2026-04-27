# ADR-009: Conventional Commits + `git-cliff` for changelogs

## Status

Accepted (2026-04-26).

## Context

We want releases to be cheap to cut and auditable to read. That means:

- The release notes are mechanically derived from commit history (no human-curated `CHANGELOG.md`).
- Commits carry enough structured metadata to do that.
- The release pipeline can run end-to-end on a tag push.

Tooling considered:

1. **Commit convention only**, no automated changelog — fast to set up, leaves the changelog as
   ongoing manual work.
2. **`semantic-release`** — opinionated version-bumping + npm publish flow; pulls in a Node
   toolchain we do not want.
3. **`release-please`** — Google's PR-based flow; takes over branch hygiene in ways that conflict
   with our `develop` → `main` model.
4. **`git-cliff`** — single statically-linked Rust binary, configurable Tera template, first-class
   Conventional Commits parser.

## Decision

Adopt **Conventional Commits** for every commit and use **`git-cliff`** (via the official
`orhun/git-cliff-action` in the release workflow) to render the changelog at release time.

Format:

```
<type>[(<scope>)][!]: <subject>
```

`<type>` ∈ `feat | fix | docs | refactor | perf | test | build | ci | chore | revert`. `<scope>` is
optional but strongly preferred; for issue-related work, scope is `(#<issueNumber>)`. `!` or
`BREAKING CHANGE:` triggers a major-version section.

Validation:

- `.githooks/commit-msg` validates locally (regex).
- `validate-commits` job in `ci.yml` re-validates every commit on a PR.

The same convention extends to the bot's commits in target repos — the AgentRunner system prompt
instructs the agent to author commits in this format.

## Consequences

**Positive:**

- Releases are one tag push; the workflow does the rest.
- The changelog is grep-able, machine-parseable, and reflects what actually shipped.
- A consistent convention across the project's own commits and the bot's commits in target repos.

**Negative:**

- Contributors must learn the convention; we mitigate with the hook + PR-time CI gate + examples in
  `CONTRIBUTING.md`.
- A small Rust binary in CI, but the action handles installation; no toolchain on the runner.
