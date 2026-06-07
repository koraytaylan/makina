# Scope — Plan 0008

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

This plan addresses two critical operational and security findings from recent architectural reviews:

1. **Gate Security Vulnerability**: The `GateRunner` evaluates an agent's work natively on the host machine by executing `sh -c` inside the task's generated worktree. If an agent maliciously or inadvertently alters build configurations (like `.cargo/config.toml`), executing gates like `cargo test` leads to an arbitrary code execution risk on the host. 
2. **Missing Session Observability**: While standard tracing logs capture system events (info, warn, etc.), the full raw payload of the agent interactions (the `AgentExchange` prompts and responses) is only streamed transiently to the TUI. When a session fails, there is no persistent transcript on disk to troubleshoot what the LLM was prompted with and exactly how it responded.

This plan hardens the gating pipeline by introducing containerized sandboxing, and introduces persistent Full Session Logging for post-run troubleshooting.

## In scope

- **Gate Configuration Update**: Extend `GateConfig` to support an optional `image` field for Docker sandboxing.
- **Sandboxed Execution**: Update `GateRunner` in `crates/makina-core/src/gate.rs` to spawn commands inside a transient Docker container when an image is specified.
- **Worktree Mounting**: The Docker execution must properly mount the isolated `.makina/worktrees/{plan_slug}--{task_id}` directory.
- **Full Session Logging**: Intercept `api::Event::AgentExchange` events within the `CoreApi` event sink and append them as JSONL to `.makina/runs/{run_uid}/logs/{task_slug}_transcript.jsonl`.
- **Testing**: Add tests for both Docker parsing and ensuring `AgentExchange` events are persistently written to disk.

## Origin → workstream mapping

| Review finding | Addressed by |
|---|---|
| `GateRunner` executes arbitrary code on the host, introducing a security risk if the agent alters test configurations. | `0025`, `0026` |
| No persistent log of the raw LLM prompts and responses (AgentExchange) makes post-mortem troubleshooting impossible. | `0027` |

## Locked decisions

- **Docker as the Sandbox**: We will use Docker (`docker run`) as the sandboxing mechanism for gates.
- **Opt-in per Gate**: Sandboxing will be opt-in per gate via the `image` attribute in `makina.toml`. 
- **JSONL for Transcripts**: Session logs will be written as JSON Lines (`.jsonl`) files so they can be easily parsed by jq or replayed by future tooling. They will live alongside standard logs in the `logs/` directory.

## Out of scope

- Complex container orchestration like Kubernetes.
- Sandboxing the Agent CLI (`makina-acp` backend) since the CLI binary is user-provided and trusted.
- TUI playback feature for transcripts (we are only building the persistence layer right now).
