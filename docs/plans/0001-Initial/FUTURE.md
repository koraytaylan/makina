# Future Directions

The MVP — plan 0001 — deliberately ships only the orchestration
backbone: the star actor topology (Supervisor / Planner / Developer /
Reviewer), the state machine, the `makina-acp` agent backend,
worktrees, and the TUI.
Everything below is a *direction worth exploring once the MVP lands*,
**not a roadmap commitment**. The MVP test-drive will inform which
directions to pursue, in what order, and what shape they should take.

Each entry names the value it adds, not the design. Designs land
when these become real plans.

## Deterministic governance

An **action gateway** that intercepts agent-initiated actions, a
**policy engine** evaluating declared role-scoped rules, and an
**audit log** of every decision. Turns "smart orchestrator" into
"compliant orchestrator" — the wedge for enterprise adoption,
distinct from prompt-and-hope orchestrators. Requires resolving the
sandbox approach (OS-level isolation vs. ACP-protocol cooperation)
— the hard engineering problem the MVP sidesteps.

## Qualifier — entry-gate quality checks

Deterministic checks against task descriptions *before* delegation.
Ambiguous or non-actionable tasks bounce back to the source unmodified
rather than wasting an agent run. Adds "no guessing on ambiguity" as
an enforced property.

## Multiple task sources

GitHub Issues, JIRA, Excel/CSV imports — each as a plugin that
surfaces tasks and writes status back. The plugin contract shape is
informed by lessons from the MVP's single file-driven source.

## Cost-tiered routing across multiple backends

Complexity tags (T-shirt sizes), multiple agent backends each
declaring its capability surface, routing tasks to the cheapest
backend that can do the job. Backends report a standardized
`UsageReport` so spend is tracked per task. Requires the orchestrator
to know about more than one backend at a time.

## PR + external review integration

For GitHub-hosted projects, the Developer agent opens the PR and the
Reviewer's iteration shows up as a PR review object — bringing
external human reviewers into the loop naturally. Generalizes to
GitLab / Bitbucket later.

## Hard enforcement via sandboxing

The teeth behind the governance pitch: per-task sandboxes (filesystem
isolation, network namespaces, process restrictions, package-manager-
vs-agent network differentiation). Turns the gateway from
declarative-audit into pre-execution-enforcement. Platform-specific
engineering — likely Linux first.

## Fresh hosted SaaS

Closed-source hosted product built on `makina-core`. Multi-tenant;
tasks via API or UI; no local setup. Begins after the MVP and the
governance work have proven the architecture.

## Smaller threads

- **License decision** (BSL / Apache / AGPL / dual). Defer until the
  open-source release date is in sight.
- **Plugin distribution mechanism** beyond compile-in (subprocess +
  JSON-RPC, WASM, dynamic libraries).
- **Multi-user concurrency** in self-hosted mode (file locks,
  coordinator process, shared `.tasks/`).
- **LLM-driven Supervisor escalation paths** for ambiguous reviewer
  outcomes or unrecoverable failures.
- **Crash recovery** beyond best-effort (WAL, atomic-write strategy,
  replay on restart).
- **Cost accounting and budget caps** as first-class concerns
  (per-task / per-list spend tracking with terminal exhaustion).
- **Hang detection** as an explicit subsystem (cooperative heartbeats,
  idle-output timeouts, wall-clock deadlines).
- **State machine extensions** (`unclear` / `non-actionable` /
  `blocked-on-dep` / `requires-human`) that become relevant once the
  qualifier and multi-source workflows land.
- **Direct API agent backends** (alongside ACP) as a second-class
  integration path for providers without ACP-compatible CLIs.
- **Richer reviewer-side rule kinds.** MVP already ships Developer-
  side gates (exit-code-zero shell commands run before Reviewer
  dispatch). The Reviewer itself is LLM-based in MVP. Future:
  declared Reviewer rule kinds (interpreted lint output, AST
  predicates, custom DSL) make Reviewer verdicts deterministic too.
- **Per-task gate overrides.** Gates are configured globally in MVP.
  Future: per-task overrides in the task list schema.
