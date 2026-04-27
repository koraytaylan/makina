# Troubleshooting

> Wave 0 stub. Entries are added as real failure modes are observed in later waves.

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

Copilot is not enabled on the target repo. Settings → Code & automation → Copilot → enable. Retry
the request.

## Daemon / TUI

### TUI says "daemon unavailable"

The daemon may have crashed. Check `~/Library/Logs/makina/daemon.log` for the stack. Restart with
`makina daemon` foregrounded for live logs while the TUI runs.

### A task is stuck in `STABILIZING` forever

Run `/logs <task-id>` to see the current sub-phase. If `AWAITING_CI`, GitHub may be slow — verify on
github.com. If `ADDRESSING_COMMENTS`, the agent may have hit `maxIterationsPerTask`; check task
state in the persistence file or use `/cancel` and retry.
