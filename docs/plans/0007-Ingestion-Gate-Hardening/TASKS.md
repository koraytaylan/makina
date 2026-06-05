# Makina Plan 0007 — Ingestion Gate Hardening

Structured-text task list that closes the gaps found by the post-implementation review of plan 0004. Wires the offline multi-error lint, makes every interpret failure surface the specific actionable `Validator`/`Interpreter` codes + suggestions instead of a generic string, hardens the `ReinterpretRun` boundary against races, and renders the `suggestion` text that the core already produces. See [SCOPE.md](SCOPE.md) for what is in and out, and [ARCHITECTURE.md](ARCHITECTURE.md) for the updated flow, type changes, and mapping helpers.

**Conventions**
- Each task has a stable kebab-case **id** (also used for its branch `task/{id}` and worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only; the Planner adds further dependency edges automatically for tasks that touch the same files or areas.
- **Done when** is the verifiable acceptance check used by gates and the Reviewer. Every task must also keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check` green.
- Line numbers below are grounded against `develop @ a7e6b76` and are hints only; locate every site by the named symbol (grep), since earlier tasks shift lines.
- When a task says "add a test that asserts X", the test must be a `#[tokio::test]` (or `#[test]`) whose name appears literally in the "Done when" list, and the assertions must be written exactly as described (use the same helper style already present in the file).
- When a task says "a grep check must pass", include the exact `grep -n '...' path` line in the "Done when" and make sure it matches after your edit.

---

## 0021 — Lint Wiring and Offline Fidelity

### lint-wire-interpret-and-seed — Compute lint_issues on the source bytes and use them for ParseError cases

In `crates/makina-core/src/orchestrator.rs`, modify `interpret_and_seed` (the async fn whose signature and body are around orchestrator.rs:676 after the 0004 work).

1. Right after the successful `let text = tokio::fs::read_to_string(...)` (before the `match self.state.interpreter.interpret`), insert:
   ```rust
   let lint_issues: Vec<crate::ingestion::IngestionIssue> =
       crate::ingestion::lint_source(&text);
   ```
   (Use the full path or a `use` at the top of the impl block if the file already brings `crate::ingestion` in scope elsewhere; keep it local and obvious.)

2. Change the return type of the function from
   `Result<(TaskGraph, Option<crate::ingestion::IngestionIssue>), ApiError>`
   to
   `Result<(TaskGraph, Vec<crate::ingestion::IngestionIssue>), ApiError>`.

3. Update the `Ok(graph)` arm (the one that does seed-persist) to return `(graph, vec![])`.

4. Update the `Err(e)` arm:
   - If the error matches `InterpretError::ParseError { .. }` **and** `!lint_issues.is_empty()`, return `(empty_graph, lint_issues)`.
   - Otherwise keep the existing construction of a single generic `"interpreter-failed"` item but wrap it in a `vec![ ... ]`.
   - The empty graph construction stays identical.

5. Update the function's `///` rustdoc (the block that currently talks about "Returns `ApiError::InvalidCommand` only on read failure... Interpret failures surface as a carried issue"):
   - Change every mention of `Option<...IngestionIssue>` to `Vec<IngestionIssue>`.
   - Add one sentence: "When the underlying error is a `ParseError` from the deterministic structured-text path, the detailed issues produced by `lint_source` (the four convention codes) are returned instead of a generic item so that the report contains the multi-error diagnostics promised by plan 0004."

Do **not** call `lint_source` on the success path. Do **not** change `parse_structured_text` or the interpreter trait.

- **Depends on:** —
- **Done when:**
  - `cargo test -p makina-core ingestion` still passes (the lint unit tests are untouched).
  - `cargo test -p makina-core interpret_and_seed` (or the broader `open_run` filter) compiles and the new logic is exercised by later tasks.
  - A literal `grep -n 'lint_source' crates/makina-core/src/orchestrator.rs` shows the call inside `interpret_and_seed`.
  - `cargo test -p makina-core`, `cargo clippy -p makina-core --all-targets -- -D warnings`, and `cargo fmt --check` are all green.

### lint-offline-report-test — Add the orchestrator test that proves lint issues reach the report for the deterministic path

In the same file `crates/makina-core/src/orchestrator.rs`, inside the `mod tests` block that already contains `open_run_attaches_ingestion_report` and `open_run_with_invalid_content_is_reviewable` (around 2625 and 1661), add a new test:

```rust
#[tokio::test]
async fn open_run_with_bad_convention_source_produces_lint_issues_in_report() {
    // Use a helper that gives a deterministic (None-backend) interpreter path.
    // The exact construction must be the one already used by other 0004-era tests
    // in this file (build_ingestion_interpreter with OneShotAgent + None, or the
    // execution_core_api() variant that forces the offline path).  If a new
    // helper is required, add it as a small private fn in the test module only.

    let bad_source = r#"# Bad Convention

Preamble.

---

## 0001 Dashless Section   // missing em-dash

### first — First task
Desc that is long enough.
- **Depends on:** —
// missing Done when entirely
"#;

    let (_dir, path) = write_task_list(bad_source);

    let (api, _repo) = /* the offline / deterministic core api helper */;
    let outcome = api
        .execute(Command::OpenRun { task_list_path: path })
        .await
        .expect("OpenRun must succeed even for lint-only problems");
    let run = match outcome { CommandOutcome::RunOpened { run } => run, _ => panic!() };

    let view = api.run(run).await.expect("run must be queryable");
    let codes: Vec<_> = view.report.issues.iter().map(|i| i.code.as_str()).collect();

    assert!(
        codes.iter().any(|c| *c == "heading-missing-em-dash"),
        "must contain heading-missing-em-dash from lint; got: {:?}",
        codes
    );
    assert!(
        codes.iter().any(|c| *c == "task-missing-done-when"),
        "must contain task-missing-done-when from lint; got: {:?}",
        codes
    );
    assert!(
        view.report.is_blocked(),
        "lint issues must be Blocking so the gate refuses StartRun"
    );
    // Also assert that a later clean ReinterpretRun (using the 0004 test pattern)
    // clears the report — reuse the cycling NoopBackend trick if needed for the
    // deterministic path, or simply OpenRun a second clean list.
}
```

The test must be named exactly `open_run_with_bad_convention_source_produces_lint_issues_in_report`.

It must demonstrate **multiple** lint codes at once (the "reports_all_problems_at_once" property) and that `is_blocked()` is true.

- **Depends on:** lint-wire-interpret-and-seed
- **Done when:**
  - `cargo test -p makina-core open_run_with_bad_convention_source_produces_lint_issues_in_report -- --nocapture` passes and prints the report for inspection.
  - The test uses the deterministic path (a `None` backend or `build_ingestion_interpreter(OneShotAgent, None)`).
  - `cargo test -p makina-core open_run`, clippy, and fmt pass.
  - `grep -n 'heading-missing-em-dash' crates/makina-core/src/orchestrator.rs` (inside the new test) succeeds.

---

## 0022 — Rich Interpret-Failure Reports

### rich-failure-change-sig-and-map — Turn the carried interpret problems into a Vec and map ValidationFailed + ParseError to the good codes

(This task can start once the signature change from 0021 is visible.)

In `crates/makina-core/src/orchestrator.rs`:

1. The return type of `interpret_and_seed` is already `Vec<IngestionIssue>` from the previous task. If any call site still treats it as `Option`, fix them here.

2. In the `Err(e)` arm (after you already read `text` and have `lint_issues`), implement the mapping:
   - `ParseError` + non-empty lint → the lint vec (already done in 0021).
   - `ValidationFailed(ref ge)` → call (or inline) a mapping that turns `TaskGraphError::DuplicateId { id }` into an `IngestionIssue` whose fields are **identical** to the one `ingestion::validate` would emit for the same defect (`code = "duplicate-task-id"`, `source = Validator`, `severity = Blocking`, `task_id = Some(id)`, the exact message and suggestion strings that validate uses).
     Same for `UnresolvedDependency { task, missing }` → `code = "dangling-dependency"`.
   - All other errors → `vec![ generic "interpreter-failed" item ]`.

3. Add the mapping either as a private free fn in `ingestion.rs` (preferred, pure, testable) or a local fn inside the orchestrator error arm. If you add it to `ingestion.rs` export it as `pub(crate)` and document it with "used by interpret_and_seed to give users the same codes they would have seen from validate had a graph been built".

4. Update the three places that build the final report (the main one after the artifact/fresh match, plus the two fallback arms) to do `issues.extend(interpret_issues);` (the name may be `carried` or whatever you chose).

Do not change any public API surface (`RunView`, `Command`, etc.).

- **Depends on:** lint-wire-interpret-and-seed
- **Done when:**
  - A `grep -n 'ValidationFailed' crates/makina-core/src/orchestrator.rs` inside `interpret_and_seed` shows the mapping arm.
  - `cargo test -p makina-core` (the open_run family) passes.
  - The test `open_run_with_invalid_content_is_reviewable` (or its successor name) now asserts a `Validator` issue with code `"dangling-dependency"` (or the exact code for its fixture) instead of only checking for the generic string.
  - clippy and fmt green.

### rich-failure-update-tests — Make the acceptance tests assert the rich codes

Update `open_run_with_invalid_content_is_reviewable` (the one that was renamed from the hard-error test in 0004) so that its assertion is:

```rust
assert!(
    view.report.issues.iter().any(|i| i.code == "dangling-dependency"
        && i.source == crate::api::IssueSource::Validator
        && i.severity == crate::api::IssueSeverity::Blocking
        && i.suggestion.is_some()),
    "expected rich Validator dangling issue; got {:?}",
    view.report.issues
);
```

Add a second small test (or a second fixture inside the same test) that hits a `DuplicateId` via the parser path and asserts the `"duplicate-task-id"` code appears as `Validator`.

If the mapping helper lives in `ingestion.rs`, add a unit test for it there (one per `TaskGraphError` variant) that does **not** require a full `OpenRun`.

- **Depends on:** rich-failure-change-sig-and-map
- **Done when:**
  - `cargo test -p makina-core open_run_with_invalid_content_is_reviewable` passes with the new assertions.
  - `cargo test -p makina-core ingestion::tests` (if you added mapping tests) passes.
  - The generic string `"interpreter-failed"` may still be asserted for pure transport errors, but never for the dangling/dup fixtures used by the plan-0004/0007 tests.
  - Full test + clippy + fmt green.

---

## 0023 — Reinterpret and Gate Race Hardening

### harden-reinterpret-status-recheck — Add the missing post-await Pending check under the second lock

In `crates/makina-core/src/orchestrator.rs`, inside `reinterpret_run` (the fn at ~923):

After the line that does the second

```rust
let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;
```

**immediately** insert (before any mutation of `entry.graph` or `entry.report`):

```rust
if entry.status != RunStatus::Pending {
    return Err(ApiError::InvalidCommand {
        reason: "reinterpret only valid for Pending runs".into(),
    });
}
```

Update the comment above the second lock to say:

```rust
// Second lookup + re-check (defensive for races with Cancel or with a StartRun
// that became legal because this re-interpret cleared the last blocker).
// We deliberately do not hold the registry lock across the await above.
```

The early return that already exists before the await (the first check) must stay.

Do not introduce a new event type; keep emitting `RunOpened` on success.

- **Depends on:** (can be independent of 0021/0022)
- **Done when:**
  - `cargo test -p makina-core reinterpret` passes.
  - `grep -n 'status != RunStatus::Pending' crates/makina-core/src/orchestrator.rs` reports the check **inside the block that obtained the mutable entry from the second lock** (i.e. after the `get_mut` that follows the await).
  - The existing test `reinterpret_rejected_when_not_pending` still passes.
  - clippy and fmt green.

### harden-reinterpret-concurrent-comment — Document last-writer-wins for concurrent re-interprets

In the same function, add a `///` or `//` comment (visible in rustdoc or clearly greppable) near the top of `reinterpret_run` or right before the first lock:

```rust
// Concurrent ReinterpretRun calls on the same run are not serialized beyond the
// registry lock. Last writer wins (the second swap overwrites the first's graph+report).
// This is the same tolerance already present for concurrent OpenRun of the same slug
// from two TUI instances.  A per-run in-flight flag can be added later if needed.
```

- **Depends on:** harden-reinterpret-status-recheck
- **Done when:**
  - `grep -n 'Last writer wins' crates/makina-core/src/orchestrator.rs` succeeds.
  - The comment is inside `reinterpret_run`.
  - Tests + clippy + fmt still green (a comment cannot break them).

---

## 0024 — TUI Report Fidelity (suggestions visible)

### tui-render-suggestions — Append suggestion text in the ingestion panel

In `crates/makina/src/ui.rs`, inside `render_ingestion_panel` (the fn at ~866):

Locate the `Line::from(vec![Span::styled( format!(...` that currently builds the issue line.

Change the format (and the preceding computation) to:

```rust
let suffix = issue.suggestion.as_ref()
    .map(|s| format!(" — suggestion: {}", s))
    .unwrap_or_default();
Line::from(vec![Span::styled(
    format!("  [{}] {} — {}{}", source, issue.code, issue.message, suffix),
    Style::default().fg(color),
)])
```

Also update the `///` doc comment on the function to say that each line ends with the suggestion (when present) in addition to the code and message.

The rest of the function (border colour, title, scroll, etc.) is unchanged.

- **Depends on:** (independent, but lands after the core produces suggestions on real issues)
- **Done when:**
  - `grep -n 'suggestion:' crates/makina/src/ui.rs` shows the new suffix logic inside `render_ingestion_panel`.
  - `cargo test -p makina` (the ui render tests) still pass.

### tui-suggestion-tests — Extend render tests to prove suggestions and Warning colour appear

In the same file, in `mod tests`:

1. Modify (or add a new test next to) `render_ingestion_panel_shows_blocking_issue` so that the `IngestionIssue` literal in the fixture has `suggestion: Some("write a concrete acceptance criterion".into())`.

   After `terminal.draw`, assert that `screen_of(&terminal)` contains the suggestion text (the exact phrase or at least the word "suggestion").

2. Add or extend a test (can be the same function or a sibling) that creates a report containing a `Warning` issue and asserts that some cell has `fg == Color::Yellow` (mirror the existing red check).

The test names must be discoverable by the cargo test filter used in "Done when".

- **Depends on:** tui-render-suggestions
- **Done when:**
  - `cargo test -p makina render_ingestion_panel -- --nocapture` runs the two render tests and they pass.
  - The suggestion text appears in the captured screen for the blocking case.
  - A yellow cell is asserted for a Warning case.
  - `cargo test -p makina`, clippy (`--all-targets -- -D warnings`), and fmt pass.

---

## Cross-cutting verification (run after all tasks)

- `cargo test -p makina-core` (all the open_run / reinterpret / start_run tests must be green; the new lint and rich-failure tests must be among them).
- `cargo test -p makina` (the ui render + event key tests).
- `cargo clippy -p makina-core -p makina --all-targets -- -D warnings`
- `cargo fmt -- --check`
- `grep -n 'lint_source' crates/makina-core/src/orchestrator.rs` (the call site)
- `grep -n 'status != RunStatus::Pending' crates/makina-core/src/orchestrator.rs` (the re-check after the second get_mut)
- `grep -n 'suggestion:' crates/makina/src/ui.rs` (the render append)
- The test `open_run_with_bad_convention_source_produces_lint_issues_in_report` exists and passes.
- The test that used to be `open_run_with_invalid_content_is_reviewable` now asserts at least one `Validator` code with a non-None suggestion.

All of the above must be true with zero manual intervention. If any step requires a human to decide a name, a string, or a helper location that is not written in the task above, the task description is incomplete.

---

**End of plan 0007 TASKS.** When every "Done when" bullet is green, the ingestion review gate is faithful to the 0004 promises and the races + fidelity gaps identified by the review are closed.
