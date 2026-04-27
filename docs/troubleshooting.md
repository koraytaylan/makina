# Troubleshooting

This page collects failure modes observed across Waves 2–5. New entries land here when a real
operator hits something that is not obvious from the error message alone.

## Setup

### `deno task ci` fails on the first run

- Verify Deno version: `deno --version` should match `.dvmrc` (currently `2.7.13`).
- Check that the hooks are registered: `git config --get core.hooksPath` should print `.githooks`.

### `setup` wizard cannot list installations

- Confirm the GitHub App is installed on at least one repository (App page → "Install App").
- Confirm the private-key path points at a valid PKCS#1 / PKCS#8 RSA PEM.

## Runtime

### Ink fails to render under Deno

This is the documented Ink-on-Deno feasibility risk (see ADR-001 and the Wave 2 TUI shell issue).
Yoga's WASM and `node:tty` raw-mode are the usual culprits. Workarounds, in order:

1. Verify Deno permissions: the daemon needs `--allow-run` and `--allow-net`; the TUI needs
   read/write to the socket.
2. Try `deno cache --reload` to refresh the npm graph.
3. If still broken, see ADR-010 (added if invoked) for the Node-side Ink subprocess fallback.

### Copilot review request returns 422

Two distinct causes share this status code; check the response body to tell them apart:

- **"Could not resolve to a node with the global id of 'Copilot'"** — Copilot is not enabled on the
  target repository. Settings → Code & automation → Copilot → enable. Retry the request.
- **"Reviewer is not a collaborator on the repository"** — the repository owner has not granted
  Copilot the "collaborator" role required to review PRs on that repo. The fix is owner-side: add
  Copilot via the same Settings page, or add a personal Copilot access entitlement to the
  installation. The supervisor surfaces this as a transient `github-call` event with the precise
  body so an operator can see which case fired.

### Copilot reviewer login form

GitHub's PR-reviewer endpoint expects the literal `Copilot` (no suffix) for the App-driven Copilot
review feature. Some installations historically required the `[bot]` suffix (`copilot[bot]`); this
has been observed to fail with 422 on more recent App versions. The supervisor uses the unsuffixed
form, hard-coded as `COPILOT_REVIEWER_LOGIN = "Copilot"` in `src/daemon/supervisor.ts`. The form is
not currently configurable at runtime: there is no daemon flag, App-setting toggle, or `config.json`
key for switching it. If your installation rejects the unsuffixed form, the `github-call` event
surfaces the response body so the operator can confirm the failure mode; making the constant
configurable is tracked as v0.2.0 work.

### `git rebase` / `git push` fails with the wrong base branch

The supervisor reads the PR's base branch from the GitHub API at task creation, not at PR open.
Repos that rename the default branch (e.g. `master` → `main`) mid-flight will surface as a "could
not read from remote" or "couldn't find remote ref" diagnostic during the stabilize-rebase phase.
Cancel the task and retry; the new task picks up the renamed base. (See ADR-018 for the rebase
loop's bounded-retry policy.)

## Daemon / TUI

### TUI says "daemon unavailable"

The daemon may have crashed. Check `~/Library/Logs/makina/daemon.log` for the stack (or the
foreground stderr if you launched via `deno task daemon`). Restart with `makina daemon` foregrounded
for live logs while the TUI runs.

### A task is stuck in `STABILIZING` forever

Run `/logs <task-id>` to see the current sub-phase. If the `stabilizePhase` is `CI`, GitHub may be
slow — verify on github.com. If it is `CONVERSATIONS`, the agent may have hit `maxIterationsPerTask`
addressing comments; check task state in the persistence file (`<workspace>/state.json`) or use
`/cancel` and retry.

### Daemon refuses to bind: socket address already in use

A previous daemon process held the socket and did not release it on exit. Per ADR-013, the listener
removes a stale socket file before binding, but a still-listening peer cannot be reclaimed
automatically. Find and stop the old process: `lsof -t /path/to/daemon.sock`, then `kill <pid>`.
Re-run.

### `deno task ci` fails on `deno task build:smoke`

The smoke test compiles `main.ts` and runs `--version`. If your local Deno cache is incomplete this
step can fail with "module not found"; `deno cache --reload main.ts` repopulates the npm graph.

## End-to-end suite

### `tests/e2e/` are skipping when I expected them to run

The suite is gated by `MAKINA_E2E=1`. Even with the gate on, every `MAKINA_E2E_APP_ID`,
`MAKINA_E2E_PRIVATE_KEY_PATH`, `MAKINA_E2E_REPO`, and `MAKINA_E2E_INSTALLATION_ID` must be set; a
missing variable surfaces as a one-line skip note naming the variable. Per-scenario issue numbers
(`MAKINA_E2E_HAPPY_ISSUE`, `MAKINA_E2E_CI_FAIL_ISSUE`, `MAKINA_E2E_REVIEW_COMMENT_ISSUE`) are
optional; an unset scenario issue skips that scenario only.
