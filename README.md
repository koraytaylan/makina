# makina

> Agentic CLI that drives a GitHub issue end-to-end: implement, open the PR, request a Copilot
> review, drive the PR to a stable mergeable state, then merge.

**Status:** under construction. The skeleton, CI, release pipeline, and issue catalog are in place;
feature work tracks the [open issues](https://github.com/koraytaylan/makina/issues).

## Concept

Run the TUI, type `/issue 42`, and walk away. The agent implements, commits, pushes, opens a PR,
requests a Copilot review, watches CI, addresses review comments, re-requests review, and finally
merges the PR per the configured policy. Run multiple `/issue` commands in parallel — each task gets
its own git worktree on its own branch.

A long-running daemon owns the agents and the polling. Quitting the TUI does not stop the daemon;
reattach later and the scrollback is intact.

## Architecture

Two cooperating processes:

- **`makina daemon`** — long-running supervisor. Owns agents, worktrees, GitHub App auth, polling,
  persisted state.
- **`makina`** (TUI) — Ink app. Connects to the daemon over a Unix domain socket; auto-spawns the
  daemon when it is not running.

See [`docs/architecture.md`](docs/architecture.md) for the full picture,
[`docs/lifecycle.md`](docs/lifecycle.md) for the per-PR stabilize loop, and
[`docs/configuration.md`](docs/configuration.md) for every config knob.

## Quick start

> Not yet — the daemon and TUI are implemented in waves 1–4. Track progress on the
> [milestones](https://github.com/koraytaylan/makina/milestones).

When ready:

```bash
deno install --allow-all --name makina --force https://raw.githubusercontent.com/koraytaylan/makina/main/main.ts
makina setup     # one-time GitHub App + default repo configuration
makina           # launch the TUI
```

## Development

This project lives entirely on Deno (≥ 2.7) — no Node toolchain. The full quality gate is one
command:

```bash
deno task ci
```

CI runs the same gate on every push and pull request. See
[`docs/development.md`](docs/development.md) and [`CONTRIBUTING.md`](CONTRIBUTING.md) for setup,
branching, and the parallel-agent workflow.

## License

[MIT](LICENSE)
