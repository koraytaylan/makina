# makina

> Agentic CLI that drives a GitHub issue end-to-end: implement, open the PR, request a Copilot
> review, drive the PR to a stable mergeable state, then merge.

**Status:** Wave 5 complete. The daemon, TUI, supervisor, and stabilize loop are wired end-to-end;
the v0.1.0 release is cut from `main`. Track future work via the
[open issues](https://github.com/koraytaylan/makina/issues).

## Concept

Run the TUI, type `/issue 42`, and walk away. The agent implements, commits, pushes, opens a PR,
requests a Copilot review, watches CI, addresses review comments, re-requests review, and finally
merges the PR per the configured policy. Run multiple `/issue` commands in parallel — each task gets
its own git worktree on its own branch.

The TUI ships a command palette (default `Ctrl+P`) and a task switcher (default `Ctrl+G`) for
mid-session control. See the [slash command catalog](docs/architecture.md#slash-commands-w3) for the
full vocabulary.

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

```bash
deno install --allow-all --name makina --force https://raw.githubusercontent.com/koraytaylan/makina/main/main.ts
makina setup     # one-time GitHub App + default repo configuration
makina           # launch the TUI
```

`makina setup` walks you through the GitHub App configuration, discovers the installations the App
can see, and writes `config.json` to the platform-appropriate path (see
[`docs/configuration.md`](docs/configuration.md)). After that, launching `makina` auto-spawns the
daemon if it is not already running and connects the TUI; type `/issue <number>` and walk away.

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
