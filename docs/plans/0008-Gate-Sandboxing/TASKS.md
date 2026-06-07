# Makina Plan 0008 — Gate Sandboxing & Session Logging

This plan implements sandboxed execution for gates using Docker, and introduces full JSONL session transcripts to allow for post-mortem troubleshooting of agent workflows. 

This file is written to be **junior-friendly**. Step-by-step instructions are provided so you can focus on building and learning the architecture cleanly!

**Conventions**
- Each task has a stable kebab-case **id**.
- **Depends on** lists structural prerequisites.
- **Done when** is the verifiable acceptance check.
- When writing tests, use `#[tokio::test]` or `#[test]` and use the exact names specified.

---

## 0025 — Configuration Updates

### add-image-to-gate-config — Extend `GateConfig` to support Docker images

Right now, Makina reads `.makina/config.toml` to understand what gates to run. We need to teach the configuration system to understand an optional `image` field.

**Steps:**
1. Open `crates/makina-core/src/config.rs`.
2. Locate the `GateConfig` struct and add a new field:
   ```rust
   #[serde(default)]
   pub image: Option<String>,
   ```
   *Note: `#[serde(default)]` ensures that if a user doesn't put `image = "..."` in their config file, it safely defaults to `None` instead of crashing!*
3. Update any test fixtures in the codebase (e.g. `crates/makina-core/src/gate.rs`) that construct `GateConfig` manually to include `image: None`.

- **Depends on:** —
- **Done when:** `cargo test -p makina-core` compiles and passes.

---

## 0026 — Docker Execution Implementation

### implement-sandboxed-gate-execution — Branch `GateRunner` to use Docker

Now we update the engine that executes the gates. If a gate has an `image`, we run it in Docker.

**Steps:**
1. Open `crates/makina-core/src/gate.rs`.
2. Locate the `run_gates` function inside the `impl GateRunner` block. 
3. Replace the `tokio::process::Command::new("sh")` logic with an `if-else` branch based on `gate.image`:

   ```rust
   let mut cmd = if let Some(ref image) = gate.image {
       let mut docker_cmd = tokio::process::Command::new("docker");
       let wd = working_dir.to_string_lossy();
       docker_cmd
           .arg("run")
           .arg("--rm")
           .arg("-v")
           .arg(format!("{wd}:{wd}"))
           .arg("-w")
           .arg(working_dir)
           .arg(image)
           .arg("sh")
           .arg("-c")
           .arg(&gate.command);
       docker_cmd
   } else {
       let mut sh_cmd = tokio::process::Command::new("sh");
       sh_cmd.arg("-c").arg(&gate.command).current_dir(working_dir);
       sh_cmd
   };
   ```
4. Add a basic unit test in `mod tests` called `sandboxed_gate_uses_docker` that constructs a `GateConfig` with `image: Some(...)` and calls `run_gates`. Match the `GateOutcome` or `GateRunnerError::Launch` robustly.

- **Depends on:** add-image-to-gate-config
- **Done when:** `cargo test -p makina-core gate` compiles and all tests pass. A literal `grep -n 'docker' crates/makina-core/src/gate.rs` shows the logic.

---

## 0027 — Full Session Logging

### persist-agent-exchange-transcripts — Write Agent exchanges to JSONL

We need to persist every LLM interaction to a transcript file so we can troubleshoot what happened during past sessions.

**Steps:**
1. Open `crates/makina-core/src/orchestrator.rs`.
2. Locate the `make_sink` function inside the `impl CoreState` block. This function creates the `EventSink` that broadcasts all events.
3. Modify the closure returned by `make_sink` to intercept `Event::AgentExchange`.
4. When `AgentExchange` is intercepted, extract the `task_id` and the `run_uid` (you might need to fetch `run_uid` from `self.runs.lock()`).
5. Open an append-mode file at `paths::run_logs_dir(&self.worktree_manager.repo_root, &run_uid)` appending `{task_id}_transcript.jsonl`.
6. Serialize the `exchange` object using `serde_json::to_string` and write it to the file followed by a newline.
   *Hint: Wrap the file-writing in a best-effort `if let Err(e) = ...` block so that logging failures do not crash the orchestrator!*

- **Depends on:** —
- **Done when:** `cargo test -p makina-core orchestrator` passes. Running a task list generates a `{task_id}_transcript.jsonl` file in the `.makina/runs/{run_uid}/logs/` directory. `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` are green.

---

**End of plan 0008 TASKS.** When every "Done when" bullet is green, Makina will have a secure sandboxing mechanism and full post-mortem observability!
