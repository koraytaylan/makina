# Makina Plan 0026 — Discovered-Gate Execution in the Governance Loop

Close the loop on **deterministic governance**: run the merged `[[gates]]` list
(configured first, then `source="discovered"`) **after the Developer and before
the Reviewer**, looping a failing gate back to the Developer under the shared
gate-iteration cap, and **document** the contract end to end.

The execution machinery already exists (`develop_until_gates_pass` →
`GateRunner::run_gates(&config.gates, …)`) and treats every `GateConfig`
identically; this plan proves discovered gates participate exactly like
configured ones, fills any missing wiring, and writes the governance spec.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas. (Plan 0025 merges discovered gates into the single
`config.gates` with a `source` marker; plan 0017 owns the retry of a
`GateCap`-failed task. Both land before this plan.)

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0076 — Run merged gates with loop-back

### run-discovered-gates — Gates block + loop to developer before review

Prove (and fill any missing wiring so) the full merged `config.gates` list —
configured gates first, then `source="discovered"` — runs after the Developer and
before the Reviewer in `develop_until_gates_pass`; a failing discovered gate
feeds its output back to the Developer and increments the **shared**
`gate_iterations`; exhaustion of `caps.gate_iterations` classifies
`FailureKind::GateCap`. The execution path is already opaque to provenance
(`GateRunner::run_gates` iterates `&ctx.config.gates` without inspecting
`source`), so the deliverable is regression-locking tests plus the small ordering
guard.

**Steps:**

1. In `crates/makina-core/tests/gate_runner.rs`, add a `discovered_gate(name,
   command)` helper alongside the existing `gate(name, command)`: build a
   `GateConfig` whose `source` field (the `#[serde(default)] pub source:
   Option<String>` introduced by plan 0025) marks it discovered —
   `source: Some("discovered".to_string())` — leaving `image: None`. The existing
   `gate(...)` helper leaves `source: None` (⇒ manual).

2. Confirm the loop consumes the merged list unchanged: in
   `crates/makina-core/src/actors/supervisor.rs`, `develop_until_gates_pass` must
   call `ctx.gate_runner.run_gates(&ctx.config.gates, worktree_path)` (it does).
   Because plan 0025 merges discovered gates into that exact `Vec`, **no call-site
   change is required**; the tests below pin the behaviour. Do **not** special-
   case `source` anywhere in the loop or in `GateRunner`.

3. Add `discovered_gate_failure_loops_back_to_developer`: merged list
   `[gate("configured", "true"), discovered_gate("discovered-check", <counter
   gate that fails once then passes>)]`, `gate_iterations = 5`. Drive one task;
   assert it reaches `Done`, `gate_iterations == 1`, and the **retry Developer
   prompt** (second recorded prompt) contains the discovered gate's name and its
   failure output — proving the discovered gate's failure looped back to the
   Developer via the existing self-loop + `feedback` path.

4. Add `passing_gates_proceed_to_reviewer`: merged list `[gate("configured",
   "true"), discovered_gate("discovered-check", "true")]`. Drive one task; assert
   the task reaches `Done`, `gate_iterations == 0`, and the **Reviewer was
   reached** (a recorded prompt contains the reviewer's JSON-verdict instruction
   text, mirroring how the existing tests detect the reviewer) — i.e. all gates
   passing proceeds past the gate stage to review.

5. Add `gate_cap_classifies_gatecap`: merged list `[discovered_gate("always-
   fail", "false")]`, `gate_iterations = 3`. Drive one task; assert it ends
   `Failed`, `gate_iterations == cap`, the Reviewer was **never** reached, the
   worktree is torn down, and the task's `failure_reason` (off the
   `TaskGraphSnapshot` task) is `Some` with `kind == FailureKind::GateCap` —
   proving a discovered gate hits the shared cap and classifies identically.

6. **Ordering guard (only if step 3–5 surface a gap):** if a discovered gate is
   observed running before a configured one, fix the **merge** (plan 0025), not
   the loop. Optionally pin order with a list `[gate("configured", "false"),
   discovered_gate("disc", "false")]` and assert the first failure relayed to the
   Developer names the **configured** gate (first-failure-stops proves order).

7. Test stubs:

   ```rust
   /* fn discovered_gate(name, command) -> GateConfig { GateConfig { name, command, image: None, source: Some("discovered".to_string()) /* field added by plan 0025 */ } } */
   #[tokio::test]
   async fn discovered_gate_failure_loops_back_to_developer() { /* gates [configured-pass, discovered counter-fail-then-pass]; one task => Done, gate_iterations==1, retry developer prompt contains the DISCOVERED gate name + output */ }
   #[tokio::test]
   async fn passing_gates_proceed_to_reviewer() { /* gates [configured-pass, discovered-pass]; one task => Done, gate_iterations==0, a recorded prompt is the reviewer JSON-verdict prompt (reviewer reached) */ }
   #[tokio::test]
   async fn gate_cap_classifies_gatecap() { /* gates [discovered always-fail], cap=3; one task => Failed, gate_iterations==cap, reviewer never reached, worktree gone, snapshot task.failure_reason.kind == FailureKind::GateCap */ }
   ```

- **Depends on:** — (requires plan 0025 merged for the `source` field + merged
  discovered gates, and plan 0017 merged for the `FailureKind` retry surface;
  both land before this plan)
- **Done when:** the three tests pass; the merged gate list (configured then
  discovered) runs after the Developer and before the Reviewer; a failing
  discovered gate loops its output back to the Developer and bumps the shared
  `gate_iterations`; cap exhaustion ends the task `Failed` with
  `FailureKind::GateCap` and the Reviewer is never reached; no provenance special-
  casing exists in the loop or `GateRunner`; cargo test/clippy/fmt green.

---

## 0077 — Governance documentation

### governance-docs — Document the deterministic-governance contract

Write the normative spec for the deterministic-governance contract and link it
from the README, so a user (or future contributor) can read exactly how discovery
feeds gate commands into `config.toml`, how those gates run after the Developer
and before the Reviewer, how a failure loops back under the shared cap, and how
to edit or override gates.

**Steps:**

1. Create `docs/spec/deterministic-governance.md` following the existing
   `docs/spec/*.md` convention (see `docs/spec/planner-model-mechanism.md`): a
   `#` title, a `Task:`/`Status: Normative` line, a `---` rule, then numbered
   sections. Document, against the **implemented** symbols:
   - **The contract** — every task passes every gate, in its worktree, before
     review; a failing gate loops back to the Developer until it passes or the
     gate cap fires. Name `develop_until_gates_pass` (`supervisor.rs`) and
     `GateRunner::run_gates(&config.gates, worktree)` (`gate.rs`).
   - **Where gates come from** — hand-written `[[gates]]` in `.makina/config.toml`
     **plus** LLM-discovered gates that plan 0025 merges into the same list with
     `source="discovered"`; *discovery is model-driven, execution is
     deterministic*; the written `config.toml` is the auditable, editable record.
   - **Composition & ordering** — configured gates first, then discovered; both
     are `GateConfig`s in the one `config.gates` `Vec`; execution ignores
     `source`; `GateRunner` stops at the first failing gate each pass and re-runs
     all gates next pass.
   - **Execution position** — gates run after the Developer turn and before the
     Reviewer turn (cite the `task_driver` `Step 3–6` loop: gates return
     `ReadyForReview`, *then* the `reviewer.ask(Review …)` block runs).
   - **Loop-back & the cap** — `TaskEvent::GateFailed` (InProgress self-loop) →
     gate name + combined output become the Developer's next `feedback` →
     `gate_iterations += 1`; the shared `caps.gate_iterations` bounds it;
     exhaustion → `GateCapReached` → `FailureKind::GateCap`
     (`set_failure_reason_locked`); note plan 0017 owns user-initiated retry.
   - **Editing / overriding gates** — edit `[[gates]]` in `.makina/config.toml`
     (the source of truth) directly or via the settings screen; re-running
     discovery is idempotent (the `[discovery]` stamp) and re-merges; deleting or
     editing a `[[gates]]` entry disables a discovered gate.

2. In `README.md`'s "## How it works" section (which already describes the
   **develop → gate → review → merge** loop and the "Gate" step), add a single
   sentence linking to `docs/spec/deterministic-governance.md` for the full
   contract (configured + discovered gates, loop-back, the shared cap). Make no
   other README change.

3. No code change. The doc must describe the behaviour implemented by
   `run-discovered-gates`; verify each cited symbol exists (`grep`) before
   committing prose.

   ```rust
   /* docs-only task: no test code. "Done when" = the named doc exists and accurately describes the implemented gate-execution flow; README links it. */
   ```

- **Depends on:** run-discovered-gates
- **Done when:** `docs/spec/deterministic-governance.md` exists and accurately
  describes the implemented flow (discovery → merged gates in `config.toml` → run
  after Developer / before Reviewer → loop-back to Developer under the shared cap
  → `FailureKind::GateCap` on exhaustion → how configured vs discovered compose
  and how to edit/override); every symbol it cites is real; the README's "How it
  works" section links the doc; no code changed, so cargo test/clippy/fmt are
  trivially green.

---

**End of plan 0026 TASKS.** When every "Done when" bullet is green, the
LLM-discovered gates that plan 0025 wrote into `config.toml` truly **block** —
they run after the Developer and before the Reviewer in merged order, loop their
output back to the Developer under the one shared cap, and fail the task as
`GateCap` on exhaustion — and the deterministic-governance contract that makes
this Makina's wedge is written down where users and contributors can read it.
