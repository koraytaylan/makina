# Makina Plan 0005 — TUI Ingestion Responsiveness

Live visibility of the agent's internal reasoning was valuable, but the immediate user-visible defect is that the *only* way to open a task list in the app (`[o]` browser → file) performs a potentially long, completely synchronous model round-trip with zero feedback. The root cause is the conflation of "ingest a user-provided structured `.md`" (should always be the fast deterministic parser) with "planner may use a model to author/normalise tasks".

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md) for the wiring deltas and the two-interpreter split.

**Conventions**
- Each task has a stable kebab-case **id** (also used for its branch `task/{id}` and worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only; the Planner adds further dependency edges automatically for tasks that touch the same files or areas.
- **Done when** is the verifiable acceptance check used by gates and the Reviewer. Every task must also keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check` green.
- Line numbers below are grounded against `develop` (post 0007 renumber + the shift of the old 0005/0006 plans into 0006/0007) and are hints only; locate every site by the named symbol (grep), since earlier tasks shift lines.
- When a task says "add a test that asserts X", the test must be a `#[tokio::test]` (or `#[test]`) whose name appears literally in the "Done when" list, and the assertions must be written exactly as described (use the same helper style already present in the file).
- When a task says "a grep check must pass", include the exact `grep -n '...' path` line in the "Done when" and make sure it matches after your edit.

---

## 0029 — Always-deterministic ingestion for OpenRun / ReinterpretRun

### tui-always-deterministic-ingestion — Force the CoreApi in the real binary to use Structured + EdgeInferrer for ingestion

In `crates/makina/src/main.rs`:

- Replace the existing block that does `match build_ingestion_interpreter(&config.planner.mechanism, Some(Arc::clone(&backend))) { ... }` (the one that produces the `interpreter` variable passed to `CoreApi::with_audit_registry`).
- Instead, unconditionally construct the fast path:

  ```rust
  let ingestion_interpreter: Arc<dyn makina_core::interpreter::TaskListInterpreter> =
      Arc::new(makina_core::dependency::EdgeInferrer::new(
          Arc::new(makina_core::interpreter::StructuredTextInterpreter::new()),
      ));
  ```

- Keep the previous fallback `eprintln!` / log only for the planner construction (see the next task). Remove or repurpose the "planner mechanism unavailable; falling back to deterministic interpreter" message that was tied to ingestion.
- Add a single `tracing::info!` (or `eprintln!` before the subscriber if needed) that says approximately: "Using deterministic structured-text + edge inference for TUI OpenRun/ReinterpretRun (planner mechanism only affects the Planner actor)".
- Update the big comment block immediately above the construction (the one that starts "The real, core-backed orchestrator Api. It opens Runs by reading a task-list file...") to state clearly that the TUI always uses the deterministic ingestion path for responsiveness; the model path is reserved for the Planner spoke.

The variable passed to `CoreApi::with_audit_registry( ingestion_interpreter, backend, ... )` must be the one built above.

- **Depends on:** —
- **Done when:**
  - `grep -n 'EdgeInferrer::new' crates/makina/src/main.rs` shows the deterministic construction for the value given to CoreApi.
  - `grep -n 'Using deterministic structured-text' crates/makina/src/main.rs` (or the exact log string you chose) exists and is reached on normal startup.
  - `cargo test -p makina --test e2e` (and any test that drives `OpenRun` through a real `CoreApi` constructed the way the binary does) still passes; the opened graphs are identical to before for all the golden task lists.
  - `cargo test -p makina -p makina-core`, `cargo clippy -p makina -p makina-core --all-targets -- -D warnings`, and `cargo fmt --check` are green.
  - A new test `tui_main_constructs_deterministic_ingestion_interpreter` (see 0032) will later assert the behaviour.

### tui-no-model-in-openrun-path — (grep + compile guard) prove that the TUI binary path never constructs a ModelInterpreter for ingestion

Add a compile-time or test-time assertion (a `#[cfg(test)]` helper or a simple unit test in `main.rs` or `lib.rs` under `#[cfg(test)]`) that would fail to compile or would panic at test time if `ModelInterpreter` were accidentally used for the ingestion interpreter in the `main` path.

At minimum, add a literal comment block and a `grep` check:

- In the same area of `main.rs`, right after the ingestion construction, add:

  ```rust
  // INVARIANT: the ingestion interpreter used for OpenRun/ReinterpretRun in the
  // shipping TUI is *never* a ModelInterpreter.  All model use for task-list
  // interpretation goes through the Planner actor (build_planner_interpreter).
  // If you change this, update plan 0005 and the test that asserts the invariant.
  ```

- **Done when:**
  - `grep -n 'ModelInterpreter' crates/makina/src/main.rs` returns no matches (or only appears inside a test that explicitly builds a model ingestion path for a negative test).
  - The exact `grep -n 'INVARIANT: the ingestion interpreter' crates/makina/src/main.rs` line appears in "Done when" of this task and matches after the edit.
  - `cargo test -p makina` (the binary crate tests) passes.

## 0030 — Immediate status feedback while a task list is being interpreted

### browser-activate-interpreting-status — Return an "Interpreting …" status for file activations

In `crates/makina/src/event.rs`, inside `resolve_io`, in the `BrowserActivate` match arm for `Some(entry) if !entry.is_dir`:

- Before (or instead of) performing the `execute(OpenRun)` await, compute a user-visible stem:

  ```rust
  let stem = entry.path
      .file_stem()
      .and_then(|s| s.to_str())
      .unwrap_or("task list");
  let status = format!("Interpreting {}...", stem);
  ```

- Perform the execute (it is now fast).
- Return `(AppEvent::CloseBrowser, Some(status))` (the later "Opened {run}" or error can be a follow-up status from the `RunLoaded` path, or you may choose to surface the final outcome only on error; the important thing is the user sees activity immediately).
- Update the doc comment for `resolve_io` (the paragraph about BrowserActivate) to mention that a transient "Interpreting …" status is surfaced for files.

- **Depends on:** 0029 (so that the await is known to be cheap).
- **Done when:**
  - `grep -n 'Interpreting .*...' crates/makina/src/event.rs` (or the exact format string) shows the status construction for the file case.
  - In the existing browser tests (the `mod tests` that exercise `resolve_io` with a `PlaceholderApi`), a test now asserts that activating a file entry produces a status message whose text contains "Interpreting" (or "Opening") and the stem.
  - The literal test name `browser_activate_file_produces_interpreting_status` appears in the "Done when" and the test body matches the description.
  - `cargo test -p makina --test event` (or the browser-related tests) is green.

### runloaded-clears-interpreting-status — (optional polish) ensure a successful RunLoaded overwrites any transient interpreting message with the normal ready state

If after the change the status bar is left showing the "Interpreting" line even after the run appears, add a tiny state rule: on `RunLoaded` (or on any `RunOpened` that produces a loaded view) the status is cleared unless it was an error.

- **Done when:** a render or app test (can be an addition to an existing `RunLoaded` test in `app.rs` tests) asserts that after `RunLoaded` the transient status is the empty/default one (or the normal "Ready" hint). The test name contains `runloaded_clears_interpreting_status` or is listed in this task's "Done when".

## 0031 — Make planner.mechanism control a real Planner in the execution actor tree

### run-graph-accepts-planner-interpreter — Extend the prod run launch path to carry a planner interpreter

In `crates/makina-core/src/actors/supervisor.rs` and the `run_graph` / `run_graph_inner` functions:

- Add a parameter (or extend `DriverContext` / a new small struct) so that a `planner_interpreter: Arc<dyn TaskListInterpreter>` reaches the place where spokes are created.
- After the `SetSpokes` ask (or in the same region), spawn the Planner exactly as the actor smoke test does:

  ```rust
  let planner_ref = RootSupervisor::spawn_child::<Planner>(
      &root,
      PlannerArgs {
          supervisor: supervisor_ref.clone(),
          interpreter: planner_interpreter,
      },
      RestartConfig::default(),
  ).await;
  ```

  (Store the ref only if you need it; for this plan the mere act of spawning under the root is sufficient for lifecycle and to make the mechanism choice observable.)

- Update the signature of `pub async fn run_graph(...)` and the internal `_inner` helper.
- Update the call site in `orchestrator.rs:839` (the `tokio::spawn(async move { run_graph(...) })`).

- **Done when:**
  - `grep -n 'spawn_child::<Planner>' crates/makina-core/src/actors/supervisor.rs` finds the spawn (or the equivalent location after edits).
  - `grep -n 'planner_interpreter' crates/makina-core/src/actors/supervisor.rs` (and the orchestrator call site) shows the threading.
  - `cargo test -p makina-core --test planner_actor` and the broader `cargo test -p makina-core` stay green (the test helpers will be updated in the companion task).

### main-builds-and-passes-planner-interpreter — In the TUI binary, build the planner interpreter with the real mechanism and pass it through

In `crates/makina/src/main.rs`:

- After (or alongside) the ingestion construction, add:

  ```rust
  let planner_interpreter = match makina_core::interpreter::build_planner_interpreter(
      &config.planner.mechanism,
      Some(Arc::clone(&backend)),
  ) {
      Ok(p) => p,
      Err(e) => {
          eprintln!("planner mechanism unavailable; falling back to deterministic planner: {e}");
          Arc::new(makina_core::dependency::EdgeInferrer::new(
              Arc::new(makina_core::interpreter::StructuredTextInterpreter::new()),
          )) as Arc<dyn makina_core::interpreter::TaskListInterpreter>
      }
  };
  ```

- Thread `planner_interpreter` into the place that eventually calls `run_graph` (you may need to extend `CoreApi` construction or the exit/reap paths, or simply build it in the scope where `api` lives and pass it when the run is started; the orchestrator already has access to `config` via the api state). The minimal change that makes the mechanism live is to build it here and ensure the value reaches the `run_graph` spawn site (via a new field on the api state or by changing the internal start-run helper).

- The fallback message is now specifically about the *planner*.

- **Done when:**
  - `grep -n 'build_planner_interpreter' crates/makina/src/main.rs` shows the call using `&config.planner.mechanism`.
  - The fallback message now mentions "planner" (not the generic ingestion fallback).
  - Starting the binary with a config that requests `OneShotAgent` + a working backend no longer causes model calls on `[o]` open of a `.md`; a later `StartRun` (once the planner is sent `InterpretTaskList`) will exercise the model path if that flow is triggered.
  - All normal `cargo test -p makina` invocations (which usually have no real backend) fall back cleanly.

### update-orchestrator-test-helpers-for-planner — Make the CoreApi test constructors that start real runs accept / build a planner interpreter

In `crates/makina-core/src/orchestrator.rs` (the `mod tests` helpers: `execution_core_api`, `no_gate_config`, the ones used by `open_run_*`, `reinterpret_*`, and `planner_actor` tests):

- Change (or add an overload for) the helpers that previously did `let interpreter = Arc::new(EdgeInferrer::new(Arc::new(StructuredTextInterpreter::new())));` for the planner case so that they go through `build_planner_interpreter(&PlannerMechanism::OneShotAgent, None_or_Some(mock_backend))`.
- The `CoreApi` construction for tests that only care about ingestion can keep forcing det; the ones that exercise `InterpretTaskList` via a supervisor must now be able to inject a model planner when they want the model behaviour.

- **Done when:**
  - `grep -n 'build_planner_interpreter' crates/makina-core/src/orchestrator.rs` shows at least one use inside the test helpers.
  - `cargo test -p makina-core --test planner_actor` (the tests that send `InterpretTaskList`) pass with both the det and a mock-backend model planner.
  - `cargo test -p makina-core` (the big orchestrator suite) is green.

## 0032 — Tests, comments, and renumber hygiene (cross-cutting)

### tui-uses-deterministic-ingestion-interpreter — New test that the shipping construction path is det

Add a test (can live in `crates/makina/src/app.rs` under `mod tests`, or a new `main_ingestion_test.rs`, or as a `#[test]` in a `#[cfg(test)]` module in `main.rs` that is compiled only for tests):

```rust
#[test]
fn tui_main_constructs_deterministic_ingestion_interpreter() {
    // Simulate the exact construction that main.rs performs for the api
    // (using the same EdgeInferrer + StructuredTextInterpreter literals).
    // Then create a CoreApi (or the minimal state) and assert that an
    // OpenRun of a known-good sample produces the expected graph with
    // zero backend involvement (pass None or a backend that would panic
    // if called).
    // The test must be named exactly as shown and must fail before the
    // 0029 change.
}
```

The test must not require a real ACP backend.

- **Done when:**
  - The test with the literal name `tui_main_constructs_deterministic_ingestion_interpreter` exists, is listed in this task's "Done when", and passes.
  - It would have failed (or not compiled) before the main.rs edit in 0029.

### update-comments-after-decoupling — Sweep for stale claims about model being default for OpenRun

Use `grep` (and fix) for every comment, docstring, or log that said the model/default ingestion path was used for task-list files opened by the TUI / `OpenRun` in the app.

At minimum the sites changed in 0029/0030/0031 plus the big comment in `orchestrator.rs` that describes the api interpreter.

- **Done when:**
  - `grep -n 'model.*default.*task.list\|ingestion.*model.*default' --include='*.rs' crates/ | cat` returns no matches that claim the TUI uses model for OpenRun (a few test sites that explicitly opt into model ingestion are allowed).
  - A literal `grep -n 'always use the deterministic' crates/makina/src/main.rs` (or the exact clarifying sentence you added) appears in the "Done when".

### shift-plan-5-and-6-titles-and-refs — Perform the directory + title renumber

- Rename `docs/plans/0005-Exchange-Thoughts-and-Tools/` → `docs/plans/0006-Exchange-Thoughts-and-Tools/`
- Rename `docs/plans/0006-Ingestion-Gate-Hardening/` → `docs/plans/0007-Ingestion-Gate-Hardening/`
- In all three files of the former 0005 (now 0006): change every "Makina Plan 0005", "Plan 0005", "plan 0005", "End of plan 0005 TASKS" to the 0006 equivalents. Leave the internal workstream numbers 0025–0028 and all "changes in 0025–0028" text untouched (they are stable).
- In all three files of the former 0006 (now 0007): change every "Makina Plan 0006", "Plan 0006", "plan 0006", "End of plan 0006 TASKS" to the 0007 equivalents. Leave 0021–0024 untouched.
- Update any "see plan 0005" / "post-0005" style sentences inside the shifted docs (and inside this new plan's docs if they mention the old numbers) to the new plan numbers.
- The new plan 0005's own TASKS/SCOPE/ARCH use 0029–0032 and refer to the shifted plans by their *new* numbers (0006 for Exchange, 0007 for the gate hardening).

- **Done when:**
  - `ls docs/plans/ | grep -E '000[5-7]'` shows exactly `0005-TUI-Ingestion-Responsiveness`, `0006-Exchange-Thoughts-and-Tools`, `0007-Ingestion-Gate-Hardening`.
  - `grep -n 'Makina Plan 0006 — Exchange' docs/plans/0006-Exchange-Thoughts-and-Tools/TASKS.md` matches.
  - `grep -n 'Makina Plan 0007 — Ingestion Gate Hardening' docs/plans/0007-Ingestion-Gate-Hardening/TASKS.md` matches.
  - `grep -n 'End of plan 0006 TASKS' docs/plans/0006-Exchange-Thoughts-and-Tools/TASKS.md` matches (and the 0007 equivalent).
  - No stale "plan 0005" or "plan 0006" (as a plan identifier) remains in the three plan trees except inside this new plan's docs when it talks about the *old* numbering before the shift.
  - `cargo test -p makina -p makina-core` (the tests that mention plan numbers in comments or error messages are unaffected).

**End of plan 0005 TASKS.** When every "Done when" bullet is green (plus the usual cargo test/clippy/fmt), opening any task list in the app is instant, shows an "Interpreting …" line, the planner mechanism actually controls the Planner actor, and the old conflation that caused the freeze is impossible to regress without failing the new invariant test.
