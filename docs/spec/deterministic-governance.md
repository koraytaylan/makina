# Deterministic Governance Contract

Task: `governance-docs` (plan 0026, task 0077)
Status: Normative — specification of implemented behaviour

---

## 1. The Contract

Every task must pass every configured quality gate, in its isolated worktree, **before a Reviewer ever sees the work.** A failing gate loops the work back to the Developer until it passes or an iteration cap fires.

The governance loop is implemented as:

```
develop_until_gates_pass(ctx, task_id, developer, worktree_path, feedback)
    → GateRunner::run_gates(&config.gates, worktree_path)
```

**Key property:** gates **run after the Developer's coding turn and before the Reviewer's approval turn**. This ordering is enforced in the `task_driver` loop in `crates/makina-core/src/actors/supervisor.rs` (step 3–6): `develop_until_gates_pass` is called, it returns `ReadyForReview`, and *then* the `reviewer.ask(Review { … })` block executes.

---

## 2. Where Gates Come From

Gates may be written by hand or discovered by an LLM.

**Hand-written gates:** edit the `[[gates]]` array directly in `.makina/config.toml` at your repo root:

```toml
[[gates]]
name    = "test"
command = "cargo test"

[[gates]]
name    = "clippy"
command = "cargo clippy -- -D warnings"
```

**Discovered gates:** the discovery pass (plan 0025) inspects your repo and proposes gate commands, which are merged into the same `[[gates]]` array with a `source = "discovered"` marker:

```toml
[[gates]]
name    = "test"
command = "cargo test"
# source is absent (manual gate)

[[gates]]
name    = "check-generated"
command = "cargo generate-code && git diff --exit-code"
source  = "discovered"  # added by discovery
```

**Key principle:** *discovery is model-driven; execution is deterministic.* The written `config.toml` is the single auditable, editable record of all gates — no gate exists outside the config, and no gate runs without an entry there.

---

## 3. Composition and Ordering

When discovery merges new gates into the config, the final `config.gates` list contains:

1. **Configured gates first** (hand-written `[[gates]]` entries, in order)
2. **Discovered gates second** (`source = "discovered"` entries, in order)

All gates are `GateConfig` structs (`crates/makina-core/src/config.rs`) with:

- `name`: human-readable label (e.g. `"test"`, `"lint"`)
- `command`: shell command (run via `sh -c`, must exit 0)
- `image`: optional Docker image to run the command inside
- `source`: `None` (manual) or `Some("discovered")` (discovered) — **provenance metadata only**

**Execution ignores the `source` field.** A discovered gate blocks identically to a hand-written gate; the `source` marker is for audit and editing, not for special logic in the loop.

`GateRunner` (`crates/makina-core/src/gate.rs`) iterates `&config.gates` in stored order and stops at the **first non-zero exit code**, reporting that gate's name and combined output. All gates re-run from the top on the next iteration.

---

## 4. Execution Position in the Lifecycle

The develop → gate → review loop, per `task_driver` in `supervisor.rs` (step 3–6):

1. **Developer** — `develop.ask(Develop { task, worktree, feedback, … })` implements the work
2. **Gates** — `develop_until_gates_pass(ctx, task_id, developer, worktree, feedback)` runs all gates; a failing gate loops back to step 1 (Developer turn again) with failure feedback
3. **Reviewer** — `reviewer.ask(Review { … })` (only reached if all gates pass)
4. **Merge** — approved work is squash-merged into the base branch

**Gates run before the Reviewer.** A discovered gate can truly block the path to review — not just decorate the config.

---

## 5. Loop-Back and the Shared Cap

When `GateRunner::run_gates` detects a failing gate (any gate exits non-zero):

1. The failing gate's name and combined output are captured.
2. A `TaskEvent::GateFailed` event fires, moving the task state to `InProgress` (self-loop).
3. The gate iterations counter increments: `gate_iterations += 1` (via `increment_gate_iterations_locked`).
4. The failing gate's name and output are formatted as the **Developer's next feedback**:
   ```
   "Gate `{gate}` failed (exit code {exit_code}):\n{output}\nFix the issue so the gate passes."
   ```
5. The Developer is asked again with this feedback (the loop continues).
6. **All gates re-run from the top** on the next iteration (the entire `config.gates` list, in order).

This is enforced in `develop_until_gates_pass` (`supervisor.rs`), which calls `increment_gate_iterations_locked` and then rebuilds the feedback string.

**The shared cap** is `config.caps.gate_iterations` (default often 5). When `gate_iterations >= caps.gate_iterations`:

1. A `TaskEvent::GateCapReached` event fires, moving the task to `Failed` (terminal).
2. The task's failure reason is set to `FailureKind::GateCap` via `set_failure_reason_locked`.
3. The failure message documents the iteration count: `"gate cap reached for {task_id}: {iterations}/{cap} iterations"`.
4. The Reviewer is **never reached** (the loop exits before the Reviewer turn).
5. The worktree is torn down.

**One cap, all gates.** A discovered gate and a hand-written gate count toward the same `caps.gate_iterations` budget. There is no per-gate cap or per-source cap; all iterations are pooled.

> Note: plan 0017 owns the user-initiated retry of a `GateCap`-failed task.

---

## 6. Editing and Overriding Gates

Gates are configured in `.makina/config.toml` at your repo root. The `[[gates]]` array is the source of truth.

**To edit a hand-written gate:**

- Edit the `name` or `command` in the appropriate `[[gates]]` entry.
- Commit the change to version control.
- On the next run, the updated gate will execute.

**To disable a discovered gate:**

- Delete its `[[gates]]` entry, or
- Edit it to remove the problematic parts (e.g. change `command` to a no-op like `true`), or
- Set `source` to something other than `"discovered"` (marks it manual).

**To re-run discovery** (plan 0025):

- The discovery pass checks the `[discovery]` stamp in `.makina/config.toml` for idempotency (`DiscoveryStamp` in `config.rs`); you can delete this section to force a fresh discovery run.
- Discovery merges its new gates into the `[[gates]]` array **below the existing hand-written gates**, preserving your manual gates' position.
- Re-running discovery is safe and idempotent within the stamp window.

**The settings screen:** if your Makina installation has a settings UI, you can also add, edit, or delete gates through it — changes are written to `.makina/config.toml` exactly as if you had edited the file by hand.

---

## 7. Implementation Symbols

The contract is implemented across these symbols (verified via grep):

| Symbol | File | Role |
|--------|------|------|
| `develop_until_gates_pass` | `crates/makina-core/src/actors/supervisor.rs` | The gate loop: asks the Developer, runs gates, on failure feeds output back and loops; on cap exhaustion emits `GateCapReached` |
| `GateRunner::run_gates` | `crates/makina-core/src/gate.rs` | Runs each gate's command in order, stops at first non-zero, returns `GateOutcome::Passed` or `GateOutcome::Failed { gate, output, exit_code }` |
| `task_driver` (step 3–6) | `crates/makina-core/src/actors/supervisor.rs` | The per-task state machine: calls `develop_until_gates_pass`, then (if gates passed) `reviewer.ask(Review …)` |
| `GateConfig` | `crates/makina-core/src/config.rs` | `{ name, command, image, source }`; `source` is `None` (manual) or `Some("discovered")` (discovered) |
| `TaskEvent::GateFailed` | `crates/makina-core/src/state_machine.rs` | Fired when a gate exits non-zero; triggers InProgress self-loop |
| `TaskEvent::GateCapReached` | `crates/makina-core/src/state_machine.rs` | Fired when `gate_iterations >= caps.gate_iterations`; moves task to Failed (terminal) |
| `FailureKind::GateCap` | `crates/makina-core/src/api.rs` | The failure reason stored when the gate cap fires; plan 0017 owns retry of this failure |
| `increment_gate_iterations_locked` | `crates/makina-core/src/actors/supervisor.rs` | Bumps the task's `gate_iterations` counter |
| `set_failure_reason_locked` | `crates/makina-core/src/actors/supervisor.rs` | Writes the `FailureKind::GateCap` reason and message to the task when cap is exhausted |
| `DiscoveryStamp` | `crates/makina-core/src/config.rs` | Metadata (`last_run`, `scanned_files`) used for idempotent re-discovery |

---

## 8. Deferred Work

- **Retrying a `GateCap`-failed task:** plan 0017 owns the user-initiated retry surface that allows re-dispatching a task that hit the gate cap.
- **Per-gate caps, parallel execution, or changing first-failure-stops semantics:** `GateRunner` stops at the first failing gate per iteration and re-runs all gates next iteration; this is by design and not changed here.
- **Sandboxing / Docker execution:** `GateConfig.image` already drives container execution; unchanged.

---

## 9. Acceptance Criteria

This document is normative: it describes the **implemented** behaviour. The acceptance test is:

1. The three new tests in `crates/makina-core/tests/gate_runner.rs` (task 0076) prove that discovered gates run in the merged order with loop-back semantics and hit the shared cap identically.
2. `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check` remain green.
3. Every symbol this document cites exists and behaves as described (verified via grep and code inspection).
4. The README's "How it works" section links this document.
