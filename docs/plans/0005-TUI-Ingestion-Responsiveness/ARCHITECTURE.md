# Architecture — Plan 0005 (deltas)

> Deltas to plan 0004 (and 0007 hardening) plus the TUI wiring introduced for task 28/31. File:line references are grounded against `develop` immediately after the plan-0007 ingest gate work; symbol names are the stable anchors. The freeze symptom was observed when opening `docs/plans/0006-Exchange-Thoughts-and-Tools/TASKS.md` (and any other non-trivial `.md`) via the file browser.

Two tightly-coupled workstreams that eliminate the blocking model call from the only interactive open path:

- **A. Deterministic ingestion policy for the TUI** (0029) — the one-line (plus comment) change that makes OpenRun/ReinterpretRun always use the local `StructuredTextInterpreter + EdgeInferrer`.
- **B. Status feedback + planner wiring** (0030 + 0031) — make the separation visible to the user and make `planner.mechanism` actually drive a real `Planner` instance in the actor tree used by `run_graph`.

Workstream C (0032) is pure test + doc hygiene that can be done in parallel once A lands.

---

## The call sites that mattered

```
TUI file browser
  BrowserActivate (file)
    resolve_io
      api.execute(OpenRun { path })   <--- the blocking await (event.rs:243)
        CoreApi::open_run
          interpret_and_seed
            interpreter.interpret(...)   <--- here: if ModelInterpreter this does the ACP spawn + prompt + full stream drain
      ... later ...
    resolve_api_event on the RunOpened event
      api.run(id)   <--- second await

main.rs:110
  let interpreter = build_ingestion_interpreter(&config.planner.mechanism, Some(backend))?;
  CoreApi::new(interpreter, ...)     <--- this interpreter is *only* used for OpenRun/Reinterpret (ingestion)

run_graph (supervisor.rs + orchestrator.rs:839)
  spawn Supervisor
  SetSpokes { backend, ... }         <--- no planner interpreter today
  // dev/reviewer spawned per-task inside task_driver
  // Planner is *never* spawned in the prod run path (only in actor smoke tests)
```

The model path was therefore *only* exercised for TUI users opening task lists (the exact opposite of the intended "planner uses model to author" story).

## The separation we codify

- **Ingestion interpreter** (passed to `CoreApi`): always the offline deterministic path for the shipping TUI. Fast, no network, works with `None` backend, feeds `lint_source` cleanly, produces the artifact.
- **Planner interpreter** (passed to `PlannerArgs`): respects `config.planner.mechanism`. Built with `build_planner_interpreter` (which may return `ModelInterpreter` wrapped by `EdgeInferrer`). Spawned under the `RootSupervisor` so it is torn down with the run and can participate in any future restart/supervision.

In `main.rs` we will therefore see two constructions:

```rust
// Ingestion (OpenRun etc.) — always det for responsiveness
let ingestion = Arc::new(EdgeInferrer::new(
    Arc::new(StructuredTextInterpreter::new())
)) as Arc<dyn TaskListInterpreter>;

// Planner (the actor spoke, for InterpretTaskList flows) — honours mechanism
let planner_interp = match build_planner_interpreter(&config.planner.mechanism, Some(backend.clone())) {
    Ok(p) => p,
    Err(e) => { warn...; Arc::new(EdgeInferrer::new(Arc::new(StructuredTextInterpreter::new()))) }
};
```

The `CoreApi` ctor keeps receiving only the ingestion one (its `interpreter` field is the ingestion seam).

## Changes to the actor launch path (for 0031)

`run_graph` signature grows one parameter (or we add a small `PlannerHandle` newtype):

```rust
pub async fn run_graph(
    ...
    planner_interpreter: Arc<dyn TaskListInterpreter>,
    ...
)
```

Inside (after the existing supervisor + SetSpokes):

```rust
let planner_ref = RootSupervisor::spawn_child::<Planner>(
    &root,
    PlannerArgs {
        supervisor: supervisor_ref.clone(),
        interpreter: planner_interpreter,
    },
    RestartConfig::default(),
).await;

// If the supervisor or ctx ever needs to talk *to* the planner (today it doesn't;
// the planner is given the supervisor ref and pushes SetTaskGraph),
// we can store planner_ref in a new DriverContext field or on the Supervisor.
// For this plan we only need it spawned so that root.kill() covers it and the
// mechanism choice is exercised in the real tree.
```

The call site in `orchestrator.rs` (the `tokio::spawn` of `run_graph`) builds the planner interpreter exactly once using the same config + backend it already has, then passes it down.

Test helpers (`execution_core_api`, the ones in `orchestrator_read_path.rs`, `planner_actor.rs` etc.) that previously passed `None` or hard-coded `StructuredTextInterpreter` for planner cases are updated to go through `build_planner_interpreter(..., None or Some(mock))` so the mechanism path is covered.

No change to `SupervisorArgs` is strictly required if we spawn the planner at the `run_graph` level (the root owns the children). If we later want the supervisor to hold the planner ref we can add a `planner: Option<ActorRef<Planner>>` behind a feature or just do it.

## Status message (0030)

In `resolve_io` for `BrowserActivate` on a file entry, before or instead of waiting for the outcome of `execute`:

```rust
let stem = entry.path.file_stem().and_then(|s| s.to_str()).unwrap_or("task list");
let interpreting_msg = format!("Interpreting {}...", stem);
// We still perform the await so the RunOpened/RunLoaded flow happens.
let result = ...execute... .await;
let final_msg = match result { Ok(RunOpened { run }) => format!("Opened {run}"), Err(e) => format!("Open failed: {e}"), ... };
( AppEvent::CloseBrowser, Some(final_msg) )   // or return the interpreting one first if we want two status updates
```

Because the det path is now <50 ms in practice for real task lists, returning the "Interpreting..." as the status and letting the subsequent `RunLoaded` + sidebar update be the success affordance is sufficient. If we want two distinct messages we can do an extra `app.update(StatusMessage(...))` before the await (the redraw will show it briefly).

The browser tests already assert on status messages returned from activate; we add the literal expectation for an "Interpreting" or "Opening" prefix when the entry is a file.

## Data flow after the change (no more model on the hot path)

```
BrowserActivate(file)
  resolve_io
    update(StatusMessage("Interpreting foo.md..."))   // visible immediately
    execute(OpenRun)  // now: read .md, StructuredTextInterpreter::interpret (pure), EdgeInferrer::infer_edges (local), seed-persist, register, broadcast RunOpened
  resolve_api_event(RunOpened) -> RunLoaded(full view from the fast graph)
  app.update(...) + redraw   // sidebar populates, tasks visible, all in <100 ms
```

The model (when enabled) is only reached when something explicitly calls `build_planner_interpreter` and sends `InterpretTaskList` to a live Planner ref. That flow can (and should) surface its own progress via the normal `AgentExchange` / Exchange pane once plan 0006 (the shifted Exchange work) lands.

## Test impact

- All existing `OpenRun` e2e / orchestrator tests that used the default api helper continue to work (they already mostly forced the det path via `build_ingestion_interpreter(..., None)` or the `execution_core_api` helper).
- New test asserts the *shipping* construction in main.rs produces a det ingestion interpreter (by behaviour or by a small "is_det_ingestion" seam exposed only under test cfg).
- Planner actor tests that want model behaviour continue to pass an explicit `Some(backend)` to `build_planner_interpreter`.
- No change to `cargo test -p makina-acp` or real_cli paths (they talk to a real agent but never through the ingestion interpreter for OpenRun).

## Future evolution (not in this plan)

If we ever want a "use model to re-interpret this task list" button, it can be a distinct `Command` that builds a one-shot `ModelInterpreter`, runs it in a spawned task, and feeds `RunLoaded` (or a new `RunReinterpreted`) event back through the api stream. The TUI would then show a proper activity indicator in the run row or error pane. That is left for a later plan (or the shifted Exchange plan if it wants to surface planner thoughts during authoring).

## References into shifted plans

- The workstreams that used to be plan 0005 (Exchange thoughts/tools, 0025–0028) are now under plan 0006 after the renumber.
- The hardening work (0021–0024) that used to be plan 0006 is now plan 0007.
- No functional deltas to those plans' TASKS; only their titles, "Plan 000X" headers, and "end of plan 000X TASKS" footers are updated as part of the shift.
