# Architecture — Plan 0007 (deltas)

> Deltas to plan 0004 (and transitively to 0001/0002/0003). File:line references are grounded against `develop @ a7e6b76` (immediately after the plan-0004 ingest work); symbol names are the stable anchors. The review that motivated this plan is preserved at the review artifact from the session that produced these docs.

Four small, tightly-coupled workstreams that close the gaps identified by the post-0004 review:
- **A. Lint wiring + offline fidelity** (0021) — make `lint_source` actually participate.
- **B. Rich failure reports** (0022) — turn generic `interpreter-failed` into the specific `Validator`/`Interpreter` codes + suggestions the user would have seen from `validate`/`lint`.
- **C. Race hardening for reinterpret** (0023) — the two-phase lock that 0004 review found was missing its post-await guard.
- **D. TUI fidelity** (0024) — surface the `suggestion` text that core already emits; make tests cover the missing cases.

Workstreams A+B change the shape of "interpret problem" data that flows into the report; C is a pure guard fix inside one function; D is a pure render + test addition. They can be implemented in parallel after the shared type change in B lands.

---

## The updated fresh-interpret + report flow (delta from 0004)

```
OpenRun / ReinterpretRun (fresh path)
  read source_text
  let lint_issues = ingestion::lint_source(&source_text);   // NEW — always computed on the bytes
  match interpreter.interpret(...) {
    Ok(graph) => {
      seed-persist(graph);
      (graph, vec![])
    }
    Err(e) => {
      let interpret_issues = match &e {
        InterpretError::ParseError { .. } if !lint_issues.is_empty() =>
          lint_issues,                    // the four detailed Interpreter/Blocking codes
        InterpretError::ValidationFailed(ge) =>
          vec![ validator_issue_from_graph_error(ge) ],  // "duplicate-task-id", "dangling-dependency" etc. as Validator
        _ =>
          vec![ generic_interpreter_failed(e) ]
      };
      (empty_graph(slug), interpret_issues)
    }
  }

let mut issues = ingestion::validate(&graph);
issues.extend(ingestion::qualify(&graph));
issues.extend(interpret_issues);   // now Vec, was Option<single>
let report = IngestionReport { issues };

register Pending run with graph + report
```

`StartRun` and the TUI panel are unchanged in contract (they only look at `report.is_blocked()` and the list of issues). The only observable change is that many more issues now have the right `code`, `source=Validator|Interpreter`, `task_id`, `suggestion`, and that offline convention errors finally produce the multi-error list promised in 0004 SCOPE/ARCH/TASKS.

The `interpret_and_seed` helper (the single place that knows "we just read the .md and called interpret") grows a `Vec<IngestionIssue>` return for the interpret-sourced problems and stops being the place that decides the generic message for everything.

---

## A. Lint wiring (orchestrator.rs + ingestion.rs)

### Today (post-0004)
`lint_source` exists at `ingestion.rs:226` with the exact four codes, `task_id=None`, `suggestion=None`, `Blocking`+`Interpreter`, and the "reports all at once" behaviour required by 0004 TASKS. It is only exercised by its own unit tests. `interpret_and_seed` (`orchestrator.rs:676`) does `read → interpret → on Err { empty + one generic "interpreter-failed" }`. The three report sites (`open_run:616`, the artifact-fallback paths, `reinterpret_run:949`) only ever do `validate + qualify + (optional single carried)`.

### Change
- In `interpret_and_seed` (after the `read_to_string` succeeds) compute `let lint_issues = crate::ingestion::lint_source(&text);`.
- In the `Err(e)` arm: if the error is `ParseError` *and* `!lint_issues.is_empty()`, use the lint vec as the carried interpret issues (they are already correctly shaped `Interpreter` items). Otherwise fall back to the single generic item.
- Change the return type of `interpret_and_seed` from `Result<(TaskGraph, Option<IngestionIssue>), ApiError>` to `Result<(TaskGraph, Vec<IngestionIssue>), ApiError>`.
- Update the three call sites and the report-construction blocks to `issues.extend(carried);` (works for both empty and populated vecs).
- The function's rustdoc is updated to say "on structured convention errors the detailed lint issues are returned so the report contains the multi-error diagnostics; other errors still produce a single explanatory item."

No change to `parse_structured_text` or the `TaskListInterpreter` trait — they remain fail-fast.

---

## B. Rich failure reports (orchestrator.rs + ingestion.rs + api surface)

### Today
A `ValidationFailed` (dup, unresolved/dangling) from either parser path or from `ModelInterpreter` produces exactly one `IngestionIssue { code: "interpreter-failed", source: Interpreter, task_id: None, suggestion: Some("fix the task list and re-interpret") }`. The `validate` that runs later sees an empty graph and emits nothing. Users in the TUI see only the opaque message even though `ingestion::validate` already knows how to emit the precise `Validator` codes with good suggestions and the correct `task_id`.

### Change
- Add a small pure helper in `ingestion.rs` (or inline in the error arm):
  ```rust
  fn validator_issues_from_graph_error(e: &TaskGraphError) -> Vec<IngestionIssue> { ... }
  ```
  that produces exactly the same `IngestionIssue` shapes that `validate` would have emitted for that defect (`duplicate-task-id`, `dangling-dependency`, `task_id = Some(...)`, `source=Validator`, `severity=Blocking`, and the matching suggestion text).
- In the `Err` arm of `interpret_and_seed`:
  - `ParseError` path already handled by lint (A).
  - `ValidationFailed(ge)` path → `validator_issues_from_graph_error(ge)` (works for both model and structured, gives the user the codes they expect).
  - Everything else (model transport errors, `ModelResponseInvalid`, `MechanismNotSupported`) → the single generic `interpreter-failed` item.
- Because the carried type is now `Vec`, a future `ValidationFailed` that somehow had multiple problems could return multiple; today we return one (matching the shape of `TaskGraphError`).
- Update the two tests that asserted the literal string `"interpreter-failed"` for a dangling case (`open_run_with_invalid_content_is_reviewable` and any similar) to instead assert the presence of a `Validator` issue with code `"dangling-dependency"` (or the exact code for the fixture). The test name may be left as-is or clarified.
- The report that reaches `RunView` now contains the good codes even when `interpret` itself failed; `is_blocked()` and the TUI panel just work.

This satisfies the review suggestion "so that `validate` findings can be folded for a richer report" without violating the "interpreters still need a hard pass/fail to build a graph at all" rule from 0004.

---

## C. Reinterpret race hardening (orchestrator.rs)

### Today
```rust
// first lock
let (path, slug) = { let entry = ...; if entry.status != Pending { err } ; (clone, clone) };
// await (I/O + model)
let (new_graph, carried) = interpret_and_seed(...).await?;
// second lock
let entry = runs.get_mut(...).ok_or(Unknown)?;   // no status check
entry.graph = ...;
entry.report = ...;
send(RunOpened);
```
The comment says "second lookup is defensive for races with Cancel". A `StartRun` that becomes legal because the re-interpret cleared the last blocker can interleave after the first check but before the swap.

### Change
After the second `get_mut` (still under the lock):

```rust
let entry = ... .ok_or(UnknownRun { run })?;
if entry.status != RunStatus::Pending {
    return Err(ApiError::InvalidCommand {
        reason: "reinterpret only valid for Pending runs".into(),
    });
}
entry.graph = Arc::new(AsyncMutex::new(new_graph));
entry.report = report;
```

This is the minimal delta that restores the invariant the 0004 TASKS spec required ("Only legal while the run is `Pending`").

Add a one-line comment:

```rust
// Re-check under the lock (the await above released the first guard).
// Mirrors the defensive style already used for Cancel races.
```

We do **not** hold the registry lock across the await (that would violate the discipline documented in `open_run` and used by the whole crate). Last-writer-wins for two racing `ReinterpretRun` calls on the same Pending run is accepted and documented in a `///` comment (exactly as multi-`OpenRun` of the same slug is tolerated today).

The existing test `reinterpret_rejected_when_not_pending` continues to pass; a "Done when" grep in TASKS will assert that the exact `if entry.status != RunStatus::Pending` text appears after the second lock acquisition.

---

## D. TUI report fidelity (ui.rs)

### Today
```rust
format!("  [{}] {} — {}", source, issue.code, issue.message)
```
`suggestion` is ignored. The render test only ever supplies an issue with `suggestion: None`.

### Change
```rust
let suffix = issue.suggestion.as_ref()
    .map(|s| format!(" — suggestion: {}", s))
    .unwrap_or_default();
Line::from(vec![Span::styled(
    format!("  [{}] {} — {}{}", source, issue.code, issue.message, suffix),
    Style::default().fg(color),
)])
```
Update the doc comment on `render_ingestion_panel` to mention that suggestions are appended when present.

Update `render_ingestion_panel_shows_blocking_issue` (or add a sibling) so that the fixture issue carries a non-None suggestion; assert that `screen_of` contains the suggestion text.

Add or extend a test that a `Warning` (yellow) issue also renders (the existing blocked-notice test can be augmented).

No layout, key, or event changes — the data was already on `RunView.report`.

---

## Testing strategy (all workstreams)

Everything stays deterministic (NoopBackend or pure unit). No real agent required.

- **0021** — new `#[tokio::test]` in `orchestrator.rs` (modelled on `open_run_attaches_ingestion_report`): `OpenRun` with a `None`-backend interpreter (or the build helper) + a source that triggers two lint codes (dashless heading + missing Done when) → the resulting `RunView.report` contains *both* lint codes as `Interpreter`/`Blocking`, `is_blocked()==true`, and `StartRun` refused. A clean source still yields empty report.
- **0022** — the existing "invalid content" test now asserts the mapped `Validator` code (e.g. `dangling-dependency`) instead of the generic string. Add a second fixture that hits `duplicate-task-id` via the parser path. For model path, a `NoopBackend` that returns bad JSON with dup still produces the `Validator` code in the report.
- **0023** — the `reinterpret_rejected_when_not_pending` test stays. Add a `grep` (or rustfmt-checked source assertion) in the "Done when" that the re-check `if entry.status != RunStatus::Pending` text appears inside the second locked block of `reinterpret_run`. (True concurrent test is out of scope; the guard is the deliverable.)
- **0024** — two `ratatui::TestBackend` tests (or extensions of the existing two): one issue with `suggestion = Some("...")` must cause the suggestion text to appear in the panel buffer; a `Warning` issue must produce a yellow cell (scan `buf.content()`).
- Full `cargo test -p makina-core -p makina`, clippy, fmt remain green. The old generic `"interpreter-failed"` string may still appear for transport-level model errors; the structured bad-input cases must now show the good codes.

All new tests are added to the modules that already contain the 0004 acceptance tests so they are easy to find.

---

## Decisions & open questions

- **Lint on success path?** No. If `interpret` returns `Ok(graph)` we ignore `lint_issues` even if the source was sloppy. The model (or a perfect structured parse) "won". This matches the 0004 locked decision that lint is for the deterministic offline parse path.
- **Do we ever emit `Interpreter` + lint codes for a model failure?** No — lint only participates on `ParseError` (which model never produces). A model failure that happens to have a sloppy source still gets the generic (or a mapped Validation if it was a late graph validate).
- **Validator codes for model-produced bad graphs?** Yes — `ValidationFailed` from `parse_model_response` now yields the nice `Validator` items. Bonus: the user sees "duplicate task id" instead of "model gave us a bad graph".
- **Changing the carried type to Vec is a 0004-internal refactor.** Call sites inside `orchestrator.rs` and the two tests are the only updates; the public `RunView.report` shape is untouched.
- **No new events, no new `RunStatus`.** Everything re-uses `RunOpened`, `Pending`, `InvalidCommand`, etc.
- **Message wording for lint issues.** We keep the messages that `lint_source` already emits (they were written to the 0004 spec). If alignment with `ParseError` Display is desired it can be a tiny follow-up inside the same files; not required for the gate to be functional.
