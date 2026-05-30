# Makina

Makina is a multi-agent **software-factory orchestrator** written in Rust, with a
terminal UI (ratatui) as its entry point. You point it at a **task list** written
in structured Markdown; Makina plans it into a dependency-aware task graph, then
drives each task through a **develop → gate → review → merge** loop using an
external AI coding agent — isolating every task in its own git worktree and
squash-merging approved, gate-passing work back into your integration branch.

Makina *coordinates* agents; it does not embed a model. Every agent runs as an
external **ACP** (Agent Client Protocol) CLI subprocess that is already signed in,
so Makina never handles model credentials.

> **Status: MVP.** The full loop works end-to-end against a real agent. See
> [`docs/trial/trial-findings.md`](docs/trial/trial-findings.md) for what's proven
> and the known gaps, and [`docs/plans/0001-Initial/`](docs/plans/0001-Initial/) for
> the vision, architecture, and roadmap.

## How it works

A central **Supervisor** drives each task through its lifecycle:

1. **Plan** — the Planner interprets your Markdown task list into a task graph and
   infers cross-cutting dependencies so conflicting work serializes.
2. **Develop** — the Supervisor creates a `task/{plan_slug}--{task_id}` branch +
   worktree off your base branch and hands the task to a Developer agent, which
   implements it.
3. **Gate** — configured shell gates (e.g. `cargo test`/`clippy`/`fmt`) run in the
   worktree; failures are fed back to the agent until they pass or a cap is hit.
4. **Review** — a Reviewer agent approves or rejects (with feedback that loops back
   to the Developer).
5. **Merge** — approved work is squash-merged into the base branch and the worktree
   is torn down.

Tasks run concurrently up to a configured limit; gate / review / wall-clock caps
bound runaway work. The TUI streams per-task status and the live agent
prompt/answer exchange as it happens.

## Requirements

- **Rust** (edition 2024; built with 1.94) and **git**.
- An **ACP-compatible, already-authenticated agent CLI** — e.g.
  [gemini-cli](https://github.com/google-gemini/gemini-cli) (`gemini --acp`).

## Build

```bash
cargo build
cargo test            # full suite; real-agent tests are #[ignore]d (no agent needed for CI)
```

## Configure

Makina merges two TOML layers, with the project layer winning on conflict.

**Global** — `~/.makina/config.toml` (machine-specific, not committed): the agent
backend command and the Planner mechanism.

```toml
[backend]
command = "gemini"
args    = ["--acp", "--yolo"]   # see "Limitations" re: --yolo

[planner]
mechanism = "one-shot-agent"
```

**Project** — `.makina/config.toml` at the repo root (committed): the base branch and the
quality gates. (This repo already ships one.)

```toml
base_branch = "develop"
concurrency = 2

[caps]
gate_iterations     = 5
reviewer_iterations = 3
wall_clock_secs     = 1200

[[gates]]
name    = "test"
command = "cargo test"
[[gates]]
name    = "clippy"
command = "cargo clippy -- -D warnings"
[[gates]]
name    = "fmt"
command = "cargo fmt --check"
```

Gates are exit-code-zero shell commands run (via `sh -c`) in each task's worktree;
a task must pass all of them before it is reviewed and merged. Sign the agent in
once (e.g. run `gemini` interactively) — Makina inherits its session.

## Write a task list

A task list is structured Markdown: numbered sections, and per task a description,
a `Depends on` line, and a gate-verifiable `Done when`. Full grammar:
[`docs/spec/structured-text-convention.md`](docs/spec/structured-text-convention.md);
worked example: [`docs/trial/dogfood-tasks.md`](docs/trial/dogfood-tasks.md).

```markdown
## 0001 — My Slice

### add-helper — Add a small helper
Add a documented helper function with unit tests to `makina-core`.
- **Depends on:** —
- **Done when:** the function exists with tests; `cargo test` and `cargo clippy -- -D warnings` pass.
```

## Run

```bash
cargo run -p makina    # requires the two config files above
```

Keys:

- **`o`** — open the file browser and pick a task list (starts a Run)
- **`↑/↓`** (or `j/k`) — navigate · **`Tab`** — switch panel (Runs ↔ Detail)
- **`s` / `p` / `c`** — start / pause / cancel the selected Run
- **`q`** / `Esc` / `Ctrl-C` — quit

Open a list, press `s`, and watch per-task state, iteration counts, and the live
prompt/answer stream as the loop runs; approved tasks land on your base branch.

## Caution — running a list mutates the repository

A Run creates `task/{plan_slug}--{task_id}` branches and
`.makina/worktrees/{plan_slug}--{task_id}/` checkouts and
**squash-merges approved work into `base_branch` (default `develop`)**. Point it at
a repository you're comfortable having it write to; for experimentation, use a
throwaway clone:

```bash
git clone /path/to/repo /tmp/repo-trial && cd /tmp/repo-trial
```

## Project layout

| Crate | Role |
|-------|------|
| [`makina-core`](crates/makina-core) | Orchestration engine: kameo actors, the task state machine, worktrees, gate runner, squash-merge, config, the agent-backend trait, and the `api` the TUI consumes. |
| [`makina-acp`](crates/makina-acp) | The ACP agent-backend: spawns the agent CLI and speaks JSON-RPC over stdio. |
| [`makina`](crates/makina) | The ratatui TUI — the binary and entry point. |

## Limitations (MVP)

- Some agents require an auto-approve flag (gemini's `--yolo`); without it the
  agent's permission prompt stalls the turn — Makina's ACP client does not yet
  answer `session/request_permission`.
- Task-graph state lives in memory; `.makina/tasks/{slug}.json` persistence and crash
  recovery are not implemented yet.

These and prioritized next steps are tracked in
[`docs/trial/trial-findings.md`](docs/trial/trial-findings.md).

## License

Makina is **source-available** under the [Elastic License 2.0](LICENSE) (ELv2) —
**not** OSI "open source." You may use, modify, and self-host it freely, but you
may **not** offer it to third parties as a hosted or managed service. A separate
**commercial license** (which lifts that restriction) is available from the
maintainer.

Contributions are accepted under a Contributor License Agreement
([CLA.md](CLA.md)) — a license grant, not a copyright assignment — which keeps the
dual-license model possible. See [CONTRIBUTING.md](CONTRIBUTING.md).
