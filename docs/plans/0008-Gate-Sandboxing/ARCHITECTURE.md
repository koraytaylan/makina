# Architecture — Plan 0008

> The structural changes introduced in Plan 0008 to sandbox gate execution and persist full session logs.

## 1. Gate Sandboxing 

Currently, gates are defined in `.makina/config.toml` like this:

```toml
[[gates]]
name    = "test"
command = "cargo test"
```

We will extend `GateConfig` (in `crates/makina-core/src/config.rs`) to accept an optional string field: `image`. By making it an `Option<String>`, we ensure existing config files that do not specify an image remain valid.

In `crates/makina-core/src/gate.rs`, the `GateRunner::run_gates` function will branch based on the presence of `gate.image`. If `Some`, it executes via Docker:

```rust
tokio::process::Command::new("docker")
    .arg("run")
    .arg("--rm")
    .arg("-v")
    .arg(format!("{0}:{0}", working_dir.display())) // Mount identically
    .arg("-w")
    .arg(working_dir)
    .arg(&image_tag)
    .arg("sh")
    .arg("-c")
    .arg(&gate.command)
```

## 2. Full Session Logging

The TUI receives its live updates via an `api::Event` broadcast channel (`EventSink`). Specifically, `Event::AgentExchange` carries the exact raw string prompts sent to the LLM and the responses (chunks and completions) it replies with.

To persist this, we will tap into the orchestrator's event sink implementation inside `crates/makina-core/src/orchestrator.rs`. 

When `make_sink` is called to build the `EventSink`, we will intercept `AgentExchange` events and synchronously (or asynchronously via `tokio::fs`) append them as JSON to a transcript file. 

The transcript files will be scoped per-task and stored next to standard tracing logs:
`.makina/runs/{run_uid}/logs/{task_slug}_transcript.jsonl`

**Implementation detail**:
```rust
if let Event::AgentExchange { run, task_id, exchange } = &event {
    // Determine the run_uid and log path
    // Serialize `exchange` to JSON string using serde_json
    // Append to `{task_id}_transcript.jsonl`
}
```

This guarantees that every single text interaction is committed to disk exactly as it happened, surviving UI crashes and enabling perfect post-mortem debugging.
