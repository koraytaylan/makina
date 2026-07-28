# Makina

Makina is a multi-agent **software-factory orchestrator** written in Rust, with a
terminal UI (ratatui) as its entry point. You point it at a validated **plan directory**
containing per-task Markdown documents; Makina projects it into a dependency-aware task graph, then
drives each task through a **develop → gate → review → merge** loop using an
external AI coding agent — isolating every task in its own git worktree and
squash-merging approved, gate-passing work back into your integration branch.

Makina *coordinates* agents; it does not embed a model. Every agent runs as an
external **ACP** (Agent Client Protocol) CLI subprocess that is already signed in,
so Makina never handles model credentials.

> **Status: MVP.** The full loop works end-to-end against a real agent. See
> [`docs/current-status.md`](docs/current-status.md) for current capabilities and
> safety boundaries, [`docs/trial/trial-findings.md`](docs/trial/trial-findings.md)
> for the historical first-run evidence, [`docs/demo/makina.tape`](docs/demo/makina.tape) for a
> scriptable demo ([render recipe](docs/demo/README.md)), and
> [`docs/plans/0001-Initial/`](docs/plans/0001-Initial/) for the vision,
> architecture, and roadmap.

## How it works

A central **Supervisor** drives each task through its lifecycle:

1. **Plan** — Makina validates typed plan/task documents, projects their DAG, and
   adds deterministic footprint-collision edges so conflicting work serializes.
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
prompt/answer exchange as it happens. For the full contract — configured and
discovered gates, loop-back to the Developer, and the shared cap — see
[`docs/spec/deterministic-governance.md`](docs/spec/deterministic-governance.md).

## Requirements

- **Rust** (edition 2024; MSRV 1.85, pinned to 1.96.1 via `rust-toolchain.toml`) and **git**.
- An **ACP-compatible, already-authenticated agent CLI** from the supported-agent compatibility matrix below.

### Supported agents

Makina auto-detects the first installed agent in detection priority order, so no configuration is needed when one is present. To use a specific agent, configure it in `~/.makina/config.toml` (see Configure section).

| Agent | Install | Launch (`command` + `args`) | Sign-in |
|-------|---------|------------------------------|---------|
| Gemini CLI | [google-gemini/gemini-cli](https://github.com/google-gemini/gemini-cli) | `gemini --acp` | run `gemini` once |
| Claude Code (ACP) | [@zed-industries/claude-code-acp](https://github.com/zed-industries/zed) | `claude-code-acp` | Claude sign-in |
| Grok | [grok CLI](https://grok.com) | `grok --acp` | grok sign-in |
| GitHub Copilot CLI | [github/cli](https://github.com/cli/cli) | `copilot --acp` | `gh`/Copilot auth |
| opencode | [opencode CLI](https://github.com/abi/opencode) | `opencode acp` | opencode auth |
| Codex (codex-acp) | [zed codex-acp adapter](https://github.com/zed-industries/zed) | `codex-acp` | Codex/OpenAI auth |
| Qwen Code | [Qwen qwen-cli](https://github.com/QwenLM/qwen-cli) | `qwen --experimental-acp` | qwen auth |
| Goose | [block/goose](https://github.com/block/goose) | `goose acp` | goose config |
| Kilo | [@kilocode/cli](https://www.kilocode.ai) | `kilo acp` | kilo auth |
| Cursor CLI | [cursor](https://www.cursor.com) | `agent acp` | cursor auth |

## Build

```bash
cargo build
cargo test            # full suite; real-agent tests are #[ignore]d (no agent needed for CI)
```

## Configure

Makina merges two TOML layers, with the project layer winning on conflict. When a supported
agent is installed on `$PATH`, Makina auto-detects it in registry order — **no global
configuration is required** when one is present.

**Global** — `~/.makina/config.toml` (machine-specific, not committed): optional agent backend
override (only needed if you want to use a non-default agent or a custom command path) and
the Planner mechanism.

Choose one `[backend]` from the supported agents above. If absent, auto-detection applies.

```toml
# Example: Gemini CLI
[backend]
command = "gemini"
args    = ["--acp"]

# Example: Claude Code (ACP)
# [backend]
# command = "claude-code-acp"
# args    = []

# Example: Qwen
# [backend]
# command = "qwen"
# args    = ["--experimental-acp"]

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

### Safe defaults

The shipped `.makina/config.toml` uses `concurrency = 2` (a first-timer-safe ceiling:
each gate pass compiles the full workspace and agent turns drive real model calls, so
concurrency ≥ 10 raises machine load and merge-lock contention). Raise it once you trust
the run on your machine. Prebuilt release binaries are published on
[GitHub Releases](https://github.com/koraytaylan/makina/releases) (built by the
tag-triggered Release workflow); release notes live in [`CHANGELOG.md`](CHANGELOG.md).

## Write a plan

A plan is a directory containing scope, architecture, status, and one typed
Markdown document per task. The directory is its stable identity; task
frontmatter declares dependencies, gates, footprints, and authored status.
Read the [authoritative format](docs/plans/README.md) and the complete
[Plan 0048 example](docs/plans/0048-Per-Task-Plan-Documents-And-Transactional-Status/).

Working-tree-only plans are `AwaitingCommit`; committed plans are `Unregistered`
until exact registration binds their immutable validation base. Only registered,
valid, dependency-ready, ungated tasks are `Ready`. Runtime checkpoints live
outside the repository and cannot override plan documents or Git landing evidence.

## Caution — running a plan mutates the repository

A Run creates `task/{plan_slug}--{task_id}` branches and
`.worktrees/{plan_slug}--{task_id}/` checkouts and
**squash-merges approved work into `base_branch` (default `develop`)**. For safe experimentation,
use `makina create` to scaffold a brand-new project outside this repo (see [Try it safely](#try-it-safely) below).

Alternatively, if you want to experiment on an existing repository, use a throwaway clone:

```bash
git clone /path/to/repo /tmp/repo-trial && cd /tmp/repo-trial
```

## Try it safely

The safest first run creates a brand-new project OUTSIDE this repo, so nothing
Makina does can touch your working tree:

```bash
makina create ~/tmp/todo --template todo   # scaffold a runnable project
cd ~/tmp/todo && makina                     # open it in the TUI
```

`makina create <path>` bootstraps an empty Makina git repo with no sample code
or plans. Pass `--template todo` explicitly to add the runnable todo project
and its committed, registered starter plans. Both forms leave `main` checked
out, with `develop` at the same scaffold commit and available for Makina.

## Run

```bash
cargo run -p makina    # auto-detects an installed agent — no global config required
```

**CLI flags:**
- **`makina --help`** — print usage and config file locations
- **`makina --version`** / **`-V`** — print the version and git sha (when available)
- **`makina --doctor`** — run a headless preflight check; exits 0 if a backend is configured or detected, non-zero otherwise

**Keys:**
- **`!`** — open the Doctor overlay for a headless health check
- **`w`** (in Doctor overlay) — auto-detect an agent and write a starter `~/.makina/config.toml`
- **`o`** — open the file browser and pick a plan directory
- **`Ctrl-P`** — open the command palette for start / pause / stop / reset
- **`↑/↓`** (or `j/k`) — navigate · **`Tab`** or **`→/←`** — switch panel (Runs ↔ Detail) · **wheel** — scroll content · hold **Shift** (or **Option** in iTerm2) and drag to select & copy text
- **`q`** / `Esc` / `Ctrl-C` — quit

Open a plan, choose **Start run** from the command palette, and watch per-task
state, iteration counts, and the live prompt/answer stream as the loop runs;
approved tasks land on your base branch.

## Project layout

| Crate | Role |
|-------|------|
| [`makina-core`](crates/makina-core) | Orchestration engine: Tokio scheduler, role turns, the task state machine, worktrees, gate runner, squash-merge, config, the agent-backend trait, and the `api` the TUI consumes. |
| [`makina-acp`](crates/makina-acp) | The ACP agent-backend: spawns the agent CLI and speaks JSON-RPC over stdio. |
| [`makina`](crates/makina) | The ratatui TUI — the binary and entry point. |

## Limitations & Governance (MVP)

- Volatile task checkpoints are persisted under Makina's external per-project
  state root; durable task/status truth remains in plan documents and Git evidence.
- Permission requests are answered automatically by `WorktreePolicy`, which
  auto-allows operations inside the assigned worktree and audits every decision.

Current capabilities, safety boundaries, and remaining operational limitations
are tracked in [`docs/current-status.md`](docs/current-status.md). The original
end-to-end trial remains available as
[`docs/trial/trial-findings.md`](docs/trial/trial-findings.md), but it is a
historical record rather than the current roadmap.

## License

Makina is **source-available** under the [Elastic License 2.0](LICENSE) (ELv2) —
**not** OSI "open source." You may use, modify, and self-host it freely, but you
may **not** offer it to third parties as a hosted or managed service. A separate
**commercial license** (which lifts that restriction) is available from the
maintainer.

Contributions are accepted under a Contributor License Agreement
([CLA.md](CLA.md)) — a license grant, not a copyright assignment — which keeps the
dual-license model possible. See [CONTRIBUTING.md](CONTRIBUTING.md).
