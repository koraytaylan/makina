# Makina Testing Strategy

This document defines the testing conventions for the Makina codebase.
All tasks MUST follow these conventions when writing new tests.

---

## 1. Unit Tests vs Integration Tests

### Unit Tests

- **Location**: in-module under `#[cfg(test)] mod tests { … }` in the source file.
- **Scope**: pure logic in isolation — FSM transitions, config merge/validate, serde round-trips,
  small async helpers, role-turn prompt/response handling, and backend contracts.
- **Dependencies**: only the module under test, standard library, and workspace crates.
  No real subprocesses, no real agent CLIs, no network, no disk I/O beyond `tempfile`.
- **Examples in this codebase**:
  - `state_machine.rs` — exhaustive Cartesian-product test of every `(state, event)` pair.
  - `task.rs` — JSON round-trip, `TaskGraph::validate`, `TaskId` display / serde.
  - `backend/noop.rs` — NoopBackend contract tests (TurnComplete, idempotent terminate, recording).
  - `actors/developer.rs` — Developer role-turn behavior, prompt construction, and metrics emission.
  - `actors/reviewer.rs` — Reviewer verdict parsing and error classification.

### Integration Tests

- **Location**: `crates/<crate>/tests/*.rs` (one file per feature surface being tested).
- **Scope**: cross-module / scheduler / end-to-end behavior exercised through the PUBLIC API
  of the crate. Each `tests/*.rs` file is compiled as its own crate, so it can only access
  `pub` symbols.
- **Shared helpers**: `crates/<crate>/tests/common/mod.rs` — included per file with `mod common;`.
- **Backend**: always `NoopBackend`. Never a real agent CLI, never a real model call,
  never a real gate command.
- **Examples in this codebase**:
  - `artifact_schema.rs` — loads a committed JSON fixture, checks serde and `validate()`.
  - `fsm_end_to_end.rs` — drives a task from `New` to `Done` through the FSM using `NoopBackend`
    (see Section 4 for the path).

---

## 2. Determinism Rules

Flaky tests erode confidence and slow delivery. Makina tests MUST be deterministic.

### Prefer Awaited Futures Over Sleeps

When exercising async behavior, await the concrete future or stream event that
represents completion. `run_graph`, role-turn helpers, `CoreApi::execute`, and
backend response streams all provide awaitable completion points, so tests should
use those directly.

```rust
// GOOD: await the production scheduler's result.
let result = run_graph(/* ... */).await?;
assert_eq!(result, expected);

// BAD: sleeping assumes work completed.
tokio::time::sleep(Duration::from_millis(50)).await;
```

### Poll with a bounded deadline (never fixed sleeps)

When you must observe a side effect that is not returned by the awaited future
(for example, an event on a broadcast stream), poll with an explicit deadline.

```rust
async fn poll_until(counter: &Arc<AtomicU32>, target: u32, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if counter.load(Ordering::SeqCst) >= target {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting: {what} (counter never reached {target})");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
```

The key properties: fast exit when the condition is met quickly, bounded wait for slow CI,
no arbitrary `sleep(200ms)` that makes every test suite run take forever.

### Seed / avoid randomness

Do not use `rand` or `uuid::Uuid::new_v4()` in test setup without a fixed seed. Use
deterministic IDs like `"task-001"`, `"test-graph"`.

### Forbidden in tests

| Forbidden | Reason |
|-----------|--------|
| Real agent subprocess (e.g. `claude` CLI) | Requires external binary, non-deterministic output |
| Real model calls (any LLM API) | Network, latency, non-determinism, cost |
| Gate commands (`cargo test`, `clippy`, etc.) | Env-dependent, slow, side-effectful |
| Wall-clock `sleep` without a deadline | Flaky under load |
| Fixed random seeds tied to global state | Can leak across parallel tests |
| Writing to `.tasks/` or project files | Side effects on the workspace |

---

## 3. The NoopBackend as the Standard Agent Stand-in

`makina_core::backend::noop::NoopBackend` is the canonical test double for any code that needs
an `AgentBackend`. It is production-compiled (not `#[cfg(test)]`), deterministic, and requires
no external processes.

### Construction

```rust
// Default: every prompt returns "noop response"
let backend = NoopBackend::new();

// Custom canned responses, cycled in order (wraps around when exhausted)
let backend = NoopBackend::with_responses(vec![
    "developer output".into(),
    "reviewer approved".into(),
]);
```

### Spawning a session and draining the stream

```rust
let config = SessionConfig {
    working_dir: PathBuf::from("/tmp/test"),
    system_prompt: "You are a test agent.".into(),
    extra: None,
};
let mut session = backend.spawn(config).await?;
let stream = session.prompt(Prompt::new("do the work")).await?;

let mut full_text = String::new();
let mut events = stream;
while let Some(item) = events.next().await {
    match item? {
        ResponseEvent::TextChunk { text } => full_text.push_str(&text),
        ResponseEvent::TurnComplete => break,
    }
}
session.terminate().await?;
```

### Asserting on `recorded_prompts()`

After one or more sessions complete, inspect what the scheduler or role turn actually sent:

```rust
let prompts = backend.recorded_prompts();
assert!(prompts[0].contains("implement task"), "developer prompt must mention the task");
assert!(prompts[1].contains("review"), "reviewer prompt must request review");
```

All sessions spawned from the same `NoopBackend` instance share the same recorder.
`recorded_prompts()` returns a `Vec<String>` in arrival order across all sessions.

---

## 4. Decision Guide: What Belongs Where

| Situation | Location |
|-----------|----------|
| Testing a single pure function (FSM, serde, config parse) | Unit test (`#[cfg(test)] mod tests`) |
| Testing a single role-turn helper or backend contract | Unit test in the source file |
| Testing the interaction of two or more modules / scheduler paths | Integration test (`tests/`) |
| Testing the full lifecycle of a task through the FSM with the noop backend | Integration test (`tests/fsm_end_to_end.rs`) |
| Reusable test helpers (builders, lifecycle drivers) | `tests/common/mod.rs` |
| Testing real gate execution | NEVER in automated tests |
| Testing real model / agent CLI behavior | NEVER in automated tests |
| Anything that requires network or subprocesses | NEVER in automated tests |

---

## 5. Integration Test File Layout

```
crates/makina-core/
  tests/
    common/
      mod.rs          ← shared builders + lifecycle-driver simulator
    artifact_schema.rs
    fsm_end_to_end.rs
```

Each `tests/*.rs` file that uses shared helpers must include:

```rust
mod common;
```

at the top. This compiles `tests/common/mod.rs` into the test crate.

---

## 6. Accepted Testing Libraries

| Crate | Purpose |
|-------|---------|
| `tokio` (`#[tokio::test]`) | Async test runner (already a workspace dep) |
| `futures` (`StreamExt`) | Drain `ResponseStream` in tests |
| `tempfile` | Temporary directories for file-system tests |
| `serde_json` | JSON fixture loading / serialization assertions |

No additional test framework dependencies should be added without team discussion.

---

## 7. Relationship to Future Tasks

- **Develop-review scheduler**: production lifecycle tests should drive `run_graph`
  or `CoreApi` with `NoopBackend`, using helpers from `tests/common/mod.rs` where
  possible.
- **Gate runner**: gate execution is always forbidden in automated tests. Test
  gate logic with unit tests that mock the gate outcome.
- **Prompt-answer stream**: multi-chunk `NoopBackend` responses (newlines in the
  canned response string produce one `TextChunk` per line) are available for streaming tests.
