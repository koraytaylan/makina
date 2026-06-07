# Contributing to Makina

Thanks for your interest in Makina. This document covers the licensing terms for
contributions and the development workflow.

## Licensing & the CLA (read first)

Makina is **source-available** under the [Elastic License 2.0](LICENSE) (ELv2),
not OSI "open source." You can self-host and modify it freely; you may not offer
it to others as a hosted/managed service. A commercial license is available from
the maintainer for uses ELv2 does not permit.

Because the project is offered under **both** ELv2 and separate commercial terms,
all contributions require a **Contributor License Agreement**:

- **Individuals** sign the [Individual CLA](CLA.md#individual-contributor-license-agreement-icla).
- **Companies** execute the [Corporate CLA](CLA.md#corporate-contributor-license-agreement-ccla).

The CLA is a **license grant, not a copyright assignment** — you keep ownership of
your work. It grants the maintainer the right to license your contribution under
any terms (ELv2 and commercial), which is what makes the dual-license / hosted-SaaS
model possible. On your first pull request the **CLA-assistant bot** will ask you
to sign by commenting the sign-off phrase; it records your acceptance
automatically. PRs cannot be merged until the CLA check passes.

If you don't agree to the CLA, please don't submit a pull request — but bug
reports, ideas, and discussion are always welcome.

## Development workflow

1. **Fork / branch.** Create a topic branch off the integration branch
   (`develop`). Don't commit directly to `develop` or `main`.
2. **Build & test.**
   ```bash
   cargo build
   cargo test            # full suite; real-agent tests are #[ignore]d
   ```
3. **Match the existing code.** Follow the patterns, naming, and comment density of
   the surrounding code. New code should read like the code already there.
4. **Tests are required.** Add or update tests for any behavior change; follow the
   TDD style already present in the affected crate. Don't weaken or delete an
   existing test to make a change pass.

## Quality gates (must pass before review)

Every PR must be green on all three gates the project enforces:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Run them locally before pushing — CI and the maintainer will run the same.

## Pull requests

- Keep PRs focused; one logical change per PR.
- Write a clear description of what changed and why; link any related issue.
- Use clear commit messages (the repo uses `area(scope): summary` style, e.g.
  `feat(0006): …`, `fix(plan-0007): …`).
- Expect review feedback; address it in follow-up commits on the same branch.

## Crate layout & licensing notes

| Crate | Role |
|-------|------|
| `makina-core` | Orchestration engine (actors, state machine, worktrees, gates, config, backend trait, `api`). |
| `makina-acp`  | ACP agent backend (spawns the agent CLI, JSON-RPC over stdio). |
| `makina`      | The ratatui TUI binary / entry point. |

All crates inherit the workspace `license = "Elastic-2.0"`. If a crate is ever
intended as a freely-embeddable SDK / client library, it can be carved out to a
permissive license by overriding its own `Cargo.toml`:

```toml
# in that crate's [package]
license = "Apache-2.0"   # overrides the workspace ELv2 for this crate only
```

and adding the appropriate per-file headers. Decide such carve-outs deliberately —
only for code that genuinely cannot constitute a competing hosted service.

> Questions about licensing, the CLA, or a commercial license: contact the
> maintainer.
