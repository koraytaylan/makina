# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-07-05

### Added

- **Multi-agent orchestration engine** — A state-machine orchestrator driving Planner, Developer, and Reviewer roles through a configurable task graph with automatic parallelization and error recovery.
- **ACP agent backend** — Full Agent Communication Protocol (ACP) support with mode discovery, config-option (model/effort) selection, and session lifecycle management.
- **Provider and role configuration** — Named providers, per-role assignment of backend agents, and dynamic discovery of agent capabilities (modes, models, reasoning effort).
- **Ratatui TUI** — A production-grade terminal UI with sidebar plan/task/run tree navigation, tabbed plan-detail pane, expandable task accordion sections, and a live exchange log pane for real-time agent interaction.
- **Task persistence** — Hermetic `.tasks/{slug}.json` serialization on every orchestrator state transition, with per-run audit log (`audit.jsonl`) and automatic crash-resume recovery.
- **Governance and permissions** — Declarative permission audit framework with worktree-scoped auto-allow policy, intercepting ACP `session/request_permission` requests and logging all decisions.
- **Worktree isolation** — Git worktree-per-run isolation with squash-merge consolidation and automatic cleanup, preventing developer/reviewer conflicts and enabling parallel runs.
- **Plan auto-discovery** — Automatic scanning and surfacing of `docs/plans/NNNN-*/` convention, with sidebar integration and fallback to planner-driven task-list generation for in-flight authoring.
- **Planner-generated task graphs** — Automatically generate structured task lists from plan SCOPE and ARCHITECTURE markdown via planner-driven LLM summarization.
- **Ayu theme system** — Built-in Ayu Dark, Ayu Mirage, and Ayu Light themes with semantic theming abstraction, live switching via command palette, and persistent selection.
- **First-run configuration UX** — Auto-detection of common ACP backends (Claude, Gemini, Ollama) with guided configuration walkthrough, removing friction from initial setup.
- **Mouse and keyboard navigation** — Arrow key navigation, mouse scroll support with intelligent routing, selection highlighting, and command palette for action discovery.
- **Markdown rendering** — Hardened Markdown rendering for task descriptions, plan scopes, and agent output with syntax-aware formatting and edge-case resilience.
- **Exchange pane fidelity** — Real-time streaming of agent thoughts and tool use with structured rendering, JSON object expansion, and diff visualization for code changes.
- **Settings and configuration UI** — In-app settings editor for provider/role assignment, theme selection, and role-specific defaults with live application and persistence.
- **Run lifecycle management** — Full control over run state: pause/resume, stop, reset, and historical run replay with task-list recovery from old runs.
- **Structured logging and observability** — Per-task execution logs, role-tagged output streams, and comprehensive audit trails for compliance and debugging.
- **Agent coverage improvements** — Enhanced compatibility with Claude, Gemini, and Ollama, including handling of reasoning-effort modes, streaming optimizations, and protocol edge cases.

### Changed

- TUI colors migrated from terminal palette to fixed Ayu `Color::Rgb` palette for consistency and theme control.

[Unreleased]: https://github.com/koraytaylan/makina
[0.1.0]: https://github.com/koraytaylan/makina
