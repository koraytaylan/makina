# Development

## Prerequisites

- **Deno ≥ 2.7.13** (pinned in `.dvmrc`). Install via `curl -fsSL https://deno.land/install.sh | sh`
  or `brew install deno`.
- **`gh` CLI** for repo and issue work.
- **`claude` CLI** (the local Claude Code binary) on `PATH` — the agent-runner discovers it via
  `which claude`.

No Node, no `npm install`. Deno resolves `npm:` and `jsr:` imports automatically and caches them
under `~/.cache/deno`.

## First clone

```bash
git clone git@github.com:koraytaylan/makina.git
cd makina
git config core.hooksPath .githooks    # enable pre-commit + commit-msg hooks
deno task ci                           # confirm the gate is green locally
```

## Tasks

| Task                      | What it does                                                                            |
| ------------------------- | --------------------------------------------------------------------------------------- |
| `deno task dev`           | `deno run -A --watch main.ts` — re-runs on edit.                                        |
| `deno task daemon`        | Starts the daemon foregrounded (mostly for debugging; the TUI auto-spawns it normally). |
| `deno task setup`         | First-run GitHub-App configuration wizard.                                              |
| `deno task test`          | Runs every `*_test.ts` file in parallel.                                                |
| `deno task test:coverage` | Same as `test` plus an LCOV report and the ≥ 80% gate.                                  |
| `deno task doc:lint`      | Fails on missing/malformed JSDoc on exported symbols.                                   |
| `deno task doc:html`      | Generates a static API site under `docs/api/`.                                          |
| `deno task build:smoke`   | Compiles `main.ts` for the host target and runs `--version`. Used by CI.                |
| `deno task ci`            | Full quality gate (the same one CI runs).                                               |

## Branching

- Branch off the latest `develop`, name it `feature/<wave>-<topic>`.
- Open a PR into `develop`. Fill the PR template; link the issue with `Closes #N`.
- See `CONTRIBUTING.md` for the full convention.

## Debugging the daemon

Stop the auto-spawned instance, run `deno task daemon` in a separate terminal, then launch the TUI
in another terminal — the TUI connects to the foregrounded daemon over the same socket and you get
its logs in line.

## Common pitfalls

See `docs/troubleshooting.md`.
