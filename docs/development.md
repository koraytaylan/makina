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
| `deno task dev`           | `deno run -A --watch packages/cli/main.ts` — re-runs on edit.                           |
| `deno task daemon`        | Starts the daemon foregrounded (mostly for debugging; the TUI auto-spawns it normally). |
| `deno task setup`         | First-run GitHub-App configuration wizard.                                              |
| `deno task test`          | Runs every `*_test.ts` file in parallel.                                                |
| `deno task test:coverage` | Same as `test` plus an LCOV report and the ≥ 80% gate.                                  |
| `deno task doc:lint`      | Fails on missing/malformed JSDoc on exported symbols.                                   |
| `deno task doc:html`      | Generates a static API site under `docs/api/`.                                          |
| `deno task build:smoke`   | Compiles `packages/cli/main.ts` for the host target and runs `--version`. Used by CI.   |
| `deno task ci`            | Full quality gate (the same one CI runs).                                               |

## Branching

- Branch off the latest `develop`, name it `feature/<wave>-<topic>`.
- Open a PR into `develop`. Fill the PR template; link the issue with `Closes #N`.
- See `CONTRIBUTING.md` for the full convention.

## Debugging the daemon

Stop the auto-spawned instance, run `deno task daemon` in a separate terminal, then launch the TUI
in another terminal — the TUI connects to the foregrounded daemon over the same socket and you get
its logs in line.

## Running the e2e suite

`tests/e2e/` exercises the full daemon → supervisor → GitHub → merge flow against a real sandbox
repo with the GitHub App installed. The suite is **opt-in**: every test calls `registerE2eTest`,
which registers a `Deno.test` whose body skips with a one-line note when the gate is off.
`deno task ci` therefore runs the suite on every push without making any GitHub API calls.

Set the gate plus the four required variables to opt in:

| Variable                      | Meaning                                                             |
| ----------------------------- | ------------------------------------------------------------------- |
| `MAKINA_E2E=1`                | Gate. Without `=1` every test skips immediately.                    |
| `MAKINA_E2E_APP_ID`           | Numeric GitHub App id of the sandbox App.                           |
| `MAKINA_E2E_PRIVATE_KEY_PATH` | Filesystem path to the App's PEM private key.                       |
| `MAKINA_E2E_REPO`             | `<owner>/<name>` of the sandbox repo (App must be installed there). |
| `MAKINA_E2E_INSTALLATION_ID`  | Numeric installation id for the sandbox repo.                       |

Each scenario also takes an optional issue-number variable; an unset variable skips that scenario
only:

| Scenario                | Variable                          | Sandbox precondition                                                                  |
| ----------------------- | --------------------------------- | ------------------------------------------------------------------------------------- |
| Happy path              | `MAKINA_E2E_HAPPY_ISSUE`          | A small, well-scoped open issue.                                                      |
| CI-fail recovery        | `MAKINA_E2E_CI_FAIL_ISSUE`        | An issue whose first agent commit is expected to fail CI; subsequent commit recovers. |
| Review-comment recovery | `MAKINA_E2E_REVIEW_COMMENT_ISSUE` | A reviewer (human or scripted) leaves a comment on the open PR mid-flight.            |

Optional: `MAKINA_E2E_TIMEOUT_MS` overrides the per-scenario wait (default 30 minutes).

Run a single scenario:

```bash
deno test -A --no-check tests/e2e/happy_path_test.ts
```

Or the whole suite:

```bash
deno test -A --no-check tests/e2e/
```

The harness builds a synthetic `HOME`, writes `config.json` pointed at the sandbox repo, spawns
`packages/cli/main.ts daemon`, and drives `/issue <n>` over the daemon's Unix socket. It tears the
daemon down (SIGTERM → SIGKILL fallback) and removes the temp directory after each scenario.

## Common pitfalls

See `docs/troubleshooting.md`.
