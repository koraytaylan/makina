# Scope — Plan 0007

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Plan 0004 (Planner & Ingestion Robustness) delivered the core machinery: `IngestionIssue`/`IngestionReport`, `validate` + `qualify`, model-default wiring via `build_ingestion_interpreter`, report threading, `StartRun` guard, `ReinterpretRun`, and the TUI panel + key. All per-task "Done when" unit tests, integration tests, clippy, and fmt passed.

A post-implementation thorough review (code + spec + tests against SCOPE/ARCHITECTURE/TASKS) found several gaps between the *delivered* surface and the *promised* behaviour:

- `lint_source` (the entire "multi-error convention lint for the deterministic offline parse path") was implemented with every required unit test but **never called**. Offline malformed task lists still surface only a single opaque `"interpreter-failed"` instead of the four detailed `Interpreter` codes (`task-before-section`, `heading-missing-em-dash`, `task-missing-depends-on`, `task-missing-done-when`).
- Interpret failures (both `ParseError` and `ValidationFailed` from the structured path, and model errors) always produce a zero-task graph + one generic `interpreter-failed` issue. The later `validate`/`qualify` passes therefore never emit the specific `Validator` codes (`dangling-dependency`, `duplicate-task-id`, …) or their actionable suggestions for the most common structural problems on fresh bad input.
- `reinterpret_run` performs the required "only while Pending" check before the await, then does an unconditional swap under a second lock with only an `UnknownRun` guard. This allows a concurrent `StartRun` (once a re-interpret has cleared blockers) or `CancelRun` to violate the state machine and the spec.
- `suggestion` text (present on nearly every `Validator` and `Qualifier` issue) is never rendered in the TUI panel — users see only `code — message` and lose the remediation guidance the core already produces.
- The acceptance tests that existed were insufficient to catch the missing wiring or the race (they only asserted generic codes or sequential happy paths).

This plan closes exactly those gaps so that the review gate is *faithful* to the 0004 promises, the offline path is *rich*, failure reports are *actionable*, and the re-interpret boundary is *race-safe*. It adds the cross-cutting tests and code checks that would have made the 0004 gaps impossible.

## In scope

Exactly the tasks in [TASKS.md](TASKS.md) (sections 0021–0024):

- **0021 — Lint wiring and offline fidelity.** Invoke `lint_source` on the raw text in the fresh-interpret path; on `ParseError` (the offline convention failures) fold the detailed `Interpreter`/`Blocking` lint issues into the report instead of (or in addition to) a generic failure. Add an orchestrator-level test that feeds a dashless + missing-field source under the deterministic path and asserts the specific lint codes appear (multiple) and the run is blocked.
- **0022 — Rich interpret-failure reports.** Change the carried "interpret issue" from `Option<IngestionIssue>` to `Vec<IngestionIssue>`. On `ValidationFailed` (dup / dangling from either model or structured parser) synthesize the matching `Validator` issues (with `task_id`, `code`, `message`, `suggestion`) so the panel shows the same actionable items `validate` would have emitted had a graph been produced. Map `ParseError` cases to the lint results (already done in 0021). Update folding at the three report-computation sites and the two acceptance tests that asserted the old generic message.
- **0023 — Reinterpret and gate race hardening.** In `reinterpret_run`, after the second registry lock re-validate `status == Pending` before mutating (the check that the pre-await guard intended). Add a code-level "Done when" grep that the re-check exists. Add a comment acknowledging last-writer-wins for truly concurrent re-interprets (consistent with existing OpenRun tolerance) or a lightweight in-flight flag if simple. Ensure `StartRun` guard + re-interpret still compose cleanly.
- **0024 — TUI report fidelity.** Update `render_ingestion_panel` to append ` — suggestion: …` (when present) for every issue line. Extend the render tests to cover a suggestion-bearing issue and assert the text appears; add/expand a test for a `Warning` (yellow) issue. Minor doc and status-bar polish if needed to keep "press r" discoverable.

VISION principles served: **"no guessing on ambiguity"** (now the offline lint actually participates), **"deterministic governance is the wedge"** (rich, code-specific, suggestion-bearing reports for every failure mode), and **"maximalist core, thin shell"** (all issue synthesis stays in `makina-core`; TUI only renders what it is given).

## Origin → workstream mapping

| Review finding (post-0004) | Addressed by |
|---|---|
| `lint_source` implemented but completely unwired; offline multi-error never reaches report or gate | `0021` |
| Structural `ValidationFailed` / `ParseError` always collapse to generic `interpreter-failed`; specific `Validator` codes + suggestions invisible | `0022` |
| `reinterpret_run` two-phase lock without post-await status re-check (violates "only Pending" contract under concurrency) | `0023` |
| `suggestion` fields produced by core are never shown in panel | `0024` |
| Acceptance tests insufficient to detect missing wiring or races (only sequential/generic paths exercised) | `0021` + `0022` + `0023` (new cross tests + grep checks) |

## Locked decisions

Settled from the 0004 review + this plan's brainstorming; detail in [ARCHITECTURE.md](ARCHITECTURE.md).

- **Lint only for the deterministic offline (structured) path.** `lint_source` is invoked on the raw `.md` text for every fresh `OpenRun`/`ReinterpretRun`, but its issues are only turned into `Interpreter` report entries on the `ParseError` arm (or when we know we used `StructuredTextInterpreter`). A model that successfully interprets a sloppy source is not blocked by lint findings — the model "normalised" it.
- **Rich failures still produce an empty graph + issues.** We do not change the hard `Result<TaskGraph, InterpretError>` contract of the interpreters, nor do we make the parser produce partial graphs on `ValidationFailed`. Instead `interpret_and_seed` (and its callers) map the error payload into the corresponding `IngestionIssue`(s) using the same codes/messages/suggestions that `validate` would have used. The `RunView` still carries a usable (empty) graph so the rest of the UI and re-interpret continue to work.
- **Carried interpret issues become a `Vec`.** The previous `Option<single>` was sufficient for one generic message; lint and mapped validation errors are multi-valued. The return type of `interpret_and_seed` and the folding sites become `Vec<IngestionIssue>`.
- **Re-check after await (no lock held across I/O).** We keep the existing "no registry lock across await" discipline used by `open_run`. The fix is the defensive re-validation of `status == Pending` under the second lock, plus the pre-await early return. Last-writer-wins for truly concurrent re-interprets on the same run is accepted (documented) and consistent with multi-`OpenRun` tolerance elsewhere.
- **Suggestions are first-class render data.** The TUI now renders them; no new types or events required.

## Out of scope — deferred to [FUTURE.md](../0001-Initial/FUTURE.md) or a later plan

- Full action-gateway + policy engine + audit for `session/request_permission` (the original trial #1 finding; the `WorktreePolicy` seam exists but broader governance is larger).
- Writing live task-state updates into `.makina/tasks/{slug}.json` (plan 0002/0003/0004 only seed at open; the "Supervisor is the only writer" + crash-recovery promise is still unmet).
- Hang detection (idle-output timeout) below the wall-clock cap.
- Cost accounting / `UsageReport` / budget caps.
- Agent-driven squash-merge conflict reconciliation (the dead-code path in supervisor).
- FSM terminal-state hygiene (`ReviewCapReached` overload, dedicated `HardError` / `MergeConflict` events).
- Multiple task sources, in-TUI editing, new FSM states for non-actionable, etc.
- Any change to the `TaskListInterpreter` trait or the fail-fast `parse_structured_text` / `graph.validate()` inside interpreters (those stay hard contracts; richness is only for the report gate).

This plan is deliberately narrow: it makes the 0004 ingestion gate *do what the 0004 documents said it would do*, with tests that would have caught the gaps on the first implementation pass. Larger governance and persistence workstreams remain separate plans.
