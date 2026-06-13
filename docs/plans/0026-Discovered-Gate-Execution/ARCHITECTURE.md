# Architecture — Plan 0026

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). This plan touches `crates/makina-core` (the gate loop +
> integration tests) and the docs (`docs/spec/`, `README.md`).

## Current shape (what exists)

- **The gate loop** lives in `crates/makina-core/src/actors/supervisor.rs`,
  `async fn develop_until_gates_pass(ctx, task_id, developer, worktree_path,
  initial_feedback)`. Each iteration:
  1. asks the `Developer` (`Develop { task, worktree, feedback, run, sink,
     idle_secs }`),
  2. runs **all** gates: `ctx.gate_runner.run_gates(&ctx.config.gates,
     worktree_path)` (`GateRunner::run_gates`),
  3. on `Ok(GateOutcome::Passed)` → `TaskEvent::GatesPassed` (InProgress →
     InReview) → returns `Ok(DevelopGateOutcome::ReadyForReview)`,
  4. on `Ok(GateOutcome::Failed { gate, output, exit_code })` →
     `TaskEvent::GateFailed` (InProgress self-loop), `increment_gate_iterations_
     locked`, then if `iterations >= ctx.config.caps.gate_iterations` →
     `TaskEvent::GateCapReached` + `set_failure_reason_locked(…, FailureKind::
     GateCap, …)` → returns `Ok(DevelopGateOutcome::GateCapReached)`; otherwise
     rebuilds `feedback = Some(format!("Gate `{gate}` failed (exit code
     {exit_code}):\n{output}\nFix the issue so the gate passes."))` and loops.
- **The per-task driver** (`task_driver`, the `Step 3–6` loop in `supervisor.rs`)
  calls `develop_until_gates_pass(...)` **before** the reviewer turn (the
  `reviewer.ask(Review { … })` block), matching the contract; `GateCapReached`
  breaks to `terminal_state = TaskState::Failed`. (The `scheduler` fn launches one
  `task_driver` per ready task.)
- **`GateRunner`** (`crates/makina-core/src/gate.rs`): stateless; `run_gates(&[
  GateConfig], working_dir) -> Result<GateOutcome, GateRunnerError>` runs each
  gate's `command` via `sh -c` (or `docker run … sh -c` when `GateConfig.image`
  is `Some`) in `working_dir`, stopping at the **first** non-zero gate and
  returning `GateOutcome::Failed { gate, output, exit_code }` (combined +
  truncated output); else `GateOutcome::Passed`.
- **`GateConfig`** (`crates/makina-core/src/config.rs:395`): `{ name: String,
  command: String, image: Option<String> }` today. **Plan 0025 adds** a provenance
  field — `#[serde(default)] pub source: Option<String>` (`Some("discovered")` for
  discovered gates, `Some("manual")` / `None` ⇒ manual) — and the merge that appends
  discovered gates after the configured ones into the single `Config::gates`
  (`pub gates: Vec<GateConfig>`).
- **`FailureKind`** (`crates/makina-core/src/api.rs`): the `GateCap` variant
  already exists (`set_failure_reason_locked` writes it on cap exhaustion). Plan
  0017 lands first and consumes `FailureKind` for the retry surface; this plan
  does not change the enum.
- **Integration tests** (`crates/makina-core/tests/gate_runner.rs`): a temp-git-
  repo harness with `config_with_gates(gates, gate_iterations)`, `gate(name,
  command)`, `task(id)`, and `build_actor_tree(...)`, plus the three existing
  acceptance tests `passing_gates_advance_to_review_and_done`,
  `gate_failure_loops_then_passes_and_relays_feedback`, and
  `always_failing_gate_hits_cap_and_fails_task`. These prove the *mechanism* for
  hand-written gates; they do not yet exercise a `source="discovered"` gate.

## 0076 — Run merged gates with loop-back

Edits in `crates/makina-core/tests/gate_runner.rs` (and, only if a gap is found,
`crates/makina-core/src/actors/supervisor.rs` / `gate.rs`).

The execution machinery is already correct for *any* `GateConfig` — `run_gates`
iterates `&ctx.config.gates` opaquely and never inspects provenance, so a
discovered gate already runs identically to a configured one. The work here is to
**prove** the contract holds for discovered gates and **lock it with tests**, and
to add the one small thing the merge can't guarantee on its own: that execution
preserves the *merged order* (configured first, then discovered) so the first
failure reported is deterministic.

- **Verify the consumer takes the merged list.** Confirm the loop reads
  `ctx.config.gates` (it does — `run_gates(&ctx.config.gates, worktree_path)`).
  Because plan 0025 merges discovered gates into that exact `Vec`, no change to
  the call site is required; assert this with a test rather than a code edit.

- **Test helper for discovered gates.** Extend the `gate.rs`-style test harness
  in `tests/gate_runner.rs` with a `discovered_gate(name, command)` constructor
  that builds a `GateConfig` whose `source` marks it discovered (the
  `#[serde(default)] pub source: Option<String>` field plan 0025 adds, set to
  `Some("discovered")`). Keep the existing `gate(...)` for configured gates (which
  leaves `source: None` ⇒ manual):

  ```rust
  /// A `GateConfig` flagged as discovered (provenance only — execution treats it
  /// identically to a configured gate). Uses the `source` field plan 0025 adds.
  fn discovered_gate(name: &str, command: &str) -> GateConfig {
      GateConfig {
          name: name.to_string(),
          command: command.to_string(),
          image: None,
          source: Some("discovered".to_string()), // field introduced by plan 0025
      }
  }
  ```

- **`discovered_gate_failure_loops_back_to_developer`.** Build a config whose
  merged list is `[configured-pass, discovered-fail-then-pass]` (a passing
  configured gate first, then a discovered counter-file gate that fails once then
  passes — mirroring `gate_failure_loops_then_passes_and_relays_feedback`). Drive
  one task; assert it reaches `Done`, `gate_iterations == 1`, and the **retry
  Developer prompt contains the discovered gate's name + output** (via
  `recorded_prompts()`), proving the discovered gate's failure fed back to the
  Developer through the same self-loop.

- **`passing_gates_proceed_to_reviewer`.** Build a config whose merged list is
  `[configured-pass, discovered-pass]` (both exit 0). Drive one task; assert the
  Reviewer **is** reached (its approve prompt appears in `recorded_prompts()` —
  the existing tests already key off the reviewer's JSON-verdict prompt text) and
  the task reaches `Done` with `gate_iterations == 0`. This pins "all gates pass →
  proceed to the reviewer" with a discovered gate in the list.

- **`gate_cap_classifies_gatecap`.** Build a config with a single always-failing
  **discovered** gate and a small `caps.gate_iterations` (e.g. 3). Drive one
  task; assert it ends `Failed`, `gate_iterations == cap`, the Reviewer is
  **never** reached, the worktree is torn down, and the task's `failure_reason`
  is `Some(FailureReason { kind: FailureKind::GateCap, .. })` (read it off the
  `TaskGraphSnapshot`'s task, which carries `failure_reason`). This proves a
  discovered gate hits the shared cap and classifies `GateCap` identically.

- **Ordering guard (only if a gap surfaces).** If the merged-order assertion
  fails — i.e. discovered gates are observed running *before* configured ones —
  fix it at the **merge** (plan 0025) boundary, not by re-sorting in the loop;
  the loop must run `config.gates` in its stored order. Add a focused assertion:
  with `[configured-fail, discovered-fail]` the **first** failure reported is the
  configured gate's name (first-failure-stops semantics), confirming order.

No production code change is expected if 0025 merged the list in order and the
loop already reads `ctx.config.gates`; the deliverable is the three named tests
(plus the optional ordering guard) turning the contract into a regression fence.

## 0077 — Governance documentation

New file `docs/spec/deterministic-governance.md`, plus a one-line link from
`README.md`.

- **Follow the `docs/spec/*.md` convention** (see `planner-model-mechanism.md`):
  a `#` title, a `Task:`/`Status:` line, a `---` rule, then numbered sections.
  Concretely document, against the *implemented* symbols:

  1. **The contract.** "Every task must pass every gate, in the worktree, before
     review; a failing gate loops the work back to the Developer until it passes
     or the gate cap fires." Name the loop: `develop_until_gates_pass`
     (`supervisor.rs`) → `GateRunner::run_gates(&config.gates, worktree)`
     (`gate.rs`).
  2. **Where gates come from.** Hand-written `[[gates]]` in `.makina/config.toml`
     **and** LLM-discovered gates that plan 0025 merges into the same list with
     `source="discovered"`. Stress: *discovery is model-driven; execution is
     deterministic* — the written `config.toml` is the auditable, editable record.
  3. **Composition & ordering.** Configured gates run first, then discovered
     gates; both are `GateConfig`s in the one `config.gates` `Vec`; execution
     ignores `source` (it is provenance only). `GateRunner` stops at the first
     failing gate each pass and re-runs all gates next pass.
  4. **Execution position.** Gates run **after** the Developer turn and **before**
     the Reviewer turn (cite the `task_driver` `Step 3–6` loop ordering:
     `develop_until_gates_pass` returns `ReadyForReview` *then* the
     `reviewer.ask(Review …)` block runs).
  5. **Loop-back & the cap.** A failing gate → `TaskEvent::GateFailed` (InProgress
     self-loop) → the gate's name + combined output become the Developer's next
     `feedback` → `gate_iterations += 1`. The **shared** `caps.gate_iterations`
     bounds it; exhaustion → `GateCapReached` → `FailureKind::GateCap`
     (`set_failure_reason_locked`). Note plan 0017 owns the user-initiated retry
     of a `GateCap` failure.
  6. **Editing / overriding gates.** Edit `[[gates]]` in `.makina/config.toml`
     directly (it is the source of truth), or via the settings screen; re-running
     discovery is idempotent (the `[discovery]` stamp) and re-merges. To disable a
     discovered gate, delete or edit its `[[gates]]` entry.

- **README link.** In `README.md`'s "## How it works" section (which already
  describes the **develop → gate → review → merge** loop and the "Gate" step),
  add a sentence linking to `docs/spec/deterministic-governance.md` for the full
  contract (configured + discovered gates, loop-back, the shared cap). No other
  README change.

## Testing notes

- 0076's three tests reuse the `tests/gate_runner.rs` harness verbatim
  (`setup_temp_repo`, `config_with_gates`, `build_actor_tree`, `NoopBackend::
  with_responses`, `recorded_prompts`, `TaskGraphSnapshot`); they only add a
  `discovered_gate(...)` constructor and the new gate lists. They are
  deterministic (`ask`/await, shell builtins / counter files — no sleeps, no real
  agent), matching `docs/spec/testing-strategy.md`.
- 0077 is docs-only; its "Done when" is that the doc exists, matches the
  implemented flow, and the README links it — no code change, so all gates pass
  trivially.
- `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt
  --check` stay green.
