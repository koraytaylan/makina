# Vision

A multi-agent software factory in Rust, with a `ratatui` TUI as the
primary surface and entry point. The user opens a **task list** in
structured text; a **Planner** interprets it into a JSON task graph
(flagging cross-task dependencies so conflicting work serializes); a
**Supervisor** — the hub every other actor reports to — drives each
task through its lifecycle. The Supervisor creates a per-task git worktree and branch,
hands the task to a **Developer** (which works against an **ACP**-
compatible agent CLI and iterates over configured gates — test, lint,
build — until they pass), then hands the result to a **Reviewer** (the
same backend, a reviewer prompt). On approval the Supervisor squash-
merges into `develop`; on rejection it relays the feedback back to the
Developer. The TUI streams every prompt and answer live: what each
agent saw, what it said back, in real time.

Fault tolerance is structural, not aspirational. Every long-lived
component is a supervised **kameo** actor on Tokio; failures are
isolated and recovered at the smallest scope. Orchestrating open-
ended integrations (model APIs, agent CLIs, git, the filesystem) is
only credible if open-ended failure is structurally contained.

This is the **MVP** — what one person can test-drive and judge from.
Governance, multiple task sources, cost-tiered routing, PR/GitHub
integration, hosted SaaS — all real directions, but they're deferred
to [`FUTURE.md`](FUTURE.md) until the MVP earns them.

## Principles

Operational values — invokable in code review.

- **Maximalist core, thin shell.** Orchestration, the state machine,
  plugin contracts — all in `makina-core`. The TUI is presentation
  only: configuration editing, run launch, live monitoring.
- **Failures are scoped.** No single agent or integration takes the
  system down. Long-lived components are supervised kameo actors;
  failures are caught, logged, and recovered at the smallest scope
  possible.
- **Work state is a tracked artifact.** Task graph, progress, reviewer
  outcomes — all materialize as files in the repo. Reviewable via
  diff, recoverable from history.
- **Agents are external processes.** We coordinate; we don't
  impersonate or embed model clients. Anything that talks to a model
  is reached through the swappable agent backend (`makina-acp`). The
  Planner is the one component that *might* call a model directly
  instead — a narrow, bounded exception, decided in 0004.
- **Open-core trajectory.** `makina-core` and the TUI are designed
  to be open-source; the license itself is deferred until shipping
  clarifies the trade-offs. Fresh (the eventual hosted product)
  will be closed and live in a private repo.
