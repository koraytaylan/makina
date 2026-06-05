# Scope — Plan 0005

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

After plan 0004 (which introduced `build_ingestion_interpreter` defaulting to the model `OneShotAgent` path via `config.planner.mechanism`) and the 0007 hardening, opening any task-list `.md` via the TUI file browser (`[o]` → select file → Enter) performs a full model round-trip:

- `BrowserActivate` → `resolve_io` → `api.execute(OpenRun { task_list_path })` (blocking `.await`)
- Inside: `interpret_and_seed` → `interpreter.interpret` (for model: spawn AcpBackend session, send prompt with the *entire* source text, drain `TextChunk`s until `TurnComplete`, `parse_model_response`)
- Then `resolve_api_event` does another `api.run(id).await` on the `RunOpened` event.
- Only after the awaits complete does `app.update`, status message, and `tui.draw` happen.

There are no intermediate `Tick`, no "Interpreting..." status, and the select! loop is stuck. For small well-formed lists the latency is annoying; for large or richly backtick'd documents (e.g. `docs/plans/0006-Exchange-Thoughts-and-Tools/TASKS.md` with its detailed specs, code blocks, repeated crate/file/type names) the model takes many seconds of real time. The UI appears frozen (user's "I thought the app crashed" until they noticed background work).

The persisted artifact makes *subsequent* opens fast, but first open, `ReinterpretRun`, deleted artifacts, or brand-new task lists always pay the cost. The deterministic `StructuredTextInterpreter` (+ `EdgeInferrer`) already exists precisely for ingesting conforming structured-text `.md` files: it is pure, local, sub-100 ms even on the plan docs, produces identical `TaskGraph` shape, and feeds the same `lint_source` / `validate` / `qualify` paths.

The model mechanism was intended for the *Planner* spoke (LLM-authored or LLM-normalised task lists). Using it for routine TUI OpenRun conflates two concerns and violates "responsive TUI" and "deterministic governance is the wedge".

This plan efficiently fixes the root cause with a one-line policy change in the TUI wiring + supporting polish, while making the planner mechanism actually control the Planner actor in real runs.

## In scope

Exactly the tasks in [TASKS.md](TASKS.md) (sections 0029–0032):

- **0029 — Force deterministic ingestion in the TUI.** In `crates/makina/src/main.rs` (the real binary wiring), always construct the interpreter passed to `CoreApi` (the one used by every `OpenRun`/`ReinterpretRun`) as `EdgeInferrer(StructuredTextInterpreter)`, ignoring `config.planner.mechanism`. Emit a single startup log line explaining the separation. The build helper may still be used in tests that explicitly want the model ingestion path.
- **0030 — Surface immediate status on file activation.** In `resolve_io` (or the BrowserActivate arm), return a transient status message containing "Interpreting …" / "Opening …" using the file stem *before* or alongside the `execute` await. After the (now-fast) call, the existing "Opened {run}" or error message follows. Update the browser-activate tests to assert the interpreting status appears for file entries.
- **0031 — Wire planner mechanism into the real actor tree.** Extend `run_graph`, `SupervisorArgs` (and the test `execution_core_api` helpers) to accept a `planner_interpreter: Arc<dyn TaskListInterpreter>`. In `main.rs` (and the orchestrator test helpers that currently hard-code det for planner), call `build_planner_interpreter(&config.planner.mechanism, Some(backend))` (with fallback) and pass it through. In `run_graph`, after `SetSpokes`, spawn the `Planner` child under the `RootSupervisor` using the supplied interpreter (so it participates in root kill, restart policy, etc.). This makes the mechanism live for any `InterpretTaskList` traffic (current or future) without affecting TUI OpenRun latency.
- **0032 — Tests, comments, and cross-plan hygiene.** Add a focused test (e.g. `tui_uses_deterministic_ingestion_interpreter`) that constructs the real app path (or a thin wrapper) and asserts the ingestion interpreter used by its `CoreApi` is the det+edge one (via behaviour: works with `None` backend, produces expected graph for a sample without any model call). Update every comment that said "model interpreter is now the default per config for task-list files in the TUI". Update the shifted plan 0006/0007 docs only for their own renumbering (no functional change). Keep `cargo test -p makina -p makina-core` (including e2e OpenRun paths and planner-actor tests) green.

VISION principles served: **"thin shell, maximalist core"** (all the interpreter choice logic stays in core; TUI main just picks the fast policy), **"deterministic governance is the wedge"** (OpenRun of user .md files is now reliably the fast deterministic path; model is opt-in for planner authoring flows), **"no guessing on ambiguity"** (users see an explicit "Interpreting …" line and never experience unexplained UI freeze on open), and **"progress over perfection"** (one targeted policy change + status + wiring makes the symptom disappear immediately; larger "background every model call" or "generate from prose" UI can come later).

## Origin → workstream mapping

| Symptom / design flaw | Addressed by |
|---|---|
| TUI event loop `.await`s model `OpenRun` interpret with zero feedback or redraws | `0029` (root cause: always-det ingestion) + `0030` (status) |
| `config.planner.mechanism` (and model path) only affected the slow ingestion path; Planner actor always got det in practice | `0031` (proper planner wiring + build call in main + run_graph) |
| Comments and test helpers implied model was intended for routine task-list opens | `0032` (comment hygiene + explicit det-ingestion test) |
| Large plan docs (rich in backticks, long done_whens, code fences) make model latency especially visible | `0029` (det path is O(lines + backticks) local CPU, independent of content size for human-scale lists) |

## Locked decisions

- **Ingestion interpreter for the TUI (OpenRun/ReinterpretRun) is *always* deterministic + EdgeInferrer.** This is the correct, fast, offline, reviewable path for a file the user has already written in the structured-text convention. Model-backed ingestion is no longer offered for this entry point (it can be re-introduced later behind an explicit "normalise with model" or "re-interpret via planner" action if a real need appears). Tests that want the model ingestion path continue to call `build_ingestion_interpreter(OneShotAgent, Some(backend))` explicitly.
- **planner.mechanism now controls the Planner actor.** `build_planner_interpreter` (with the EdgeInferrer wrapper via `build_ingestion...` no, the planner variant) will be called in the real startup path and passed into `run_graph` → supervisor → Planner spawn. When `InterpretTaskList` is sent (today only from tests; tomorrow from a "plan from description" flow or internal re-plan), the user's configured model (or fallback det) is used. The two builders stay distinct.
- **Status is best-effort and transient.** We surface "Interpreting <stem>..." from the `BrowserActivate` resolve arm. Because the work is now local and fast we do not need a full "pending open" run placeholder or background task for this plan (that would be overkill). If in future we re-allow slow model paths we can promote the work to a spawned task + channel event.
- **No change to the `TaskListInterpreter` trait, `parse_structured_text`, or `ModelInterpreter` itself.** They remain exactly as delivered by 0004/0007. Only the *choice* at the TUI binary boundary and the actor construction boundary changes.
- **Artifact fast-path remains.** After the first (now instant) interpret, `load_graph` still short-circuits; nothing changes for already-opened runs.
- **plan numbers & work-item ids.** This plan receives 0029–0032 (next after the 0025–0028 workstreams). The shifted former plan 0005 keeps its internal 0025–0028 workstream numbers (they are stable references in its own "Done when" and "changes in 0025–0028" text).

## Out of scope — deferred to [FUTURE.md](../0001-Initial/FUTURE.md) or a later plan

- Making *every* model call (planner, dev, reviewer, future author-a-task-list) fully non-blocking with progress spinners in the Exchange pane or a global activity indicator.
- A "New run from free-form description" flow that would intentionally use the model Planner to *generate* the initial `.md` or graph (that flow can be slow and should surface thoughts/tools per the shifted plan 0006).
- Performance work on `infer_edges` / `extract_areas` / repeated `transitive_depends_on` for task lists with ≫100 tasks (current heuristic is fine for all real plans in the repo).
- Changing the `CoreApi` to accept two separate interpreters (ingestion vs. planner) at construction time — we keep the simple "the api's interpreter = ingestion" and plumb planner separately via run_graph for now.
- Any alteration to how artifacts, ReinterpretRun, or the review gate work (those are stable post-0004/0007).
- Killing in-flight model interprets on Esc / browser cancel (the det path is so fast it is irrelevant).

This plan is deliberately narrow and mechanical: one policy decision in the TUI entry point, one status line, the missing planner wiring made real, and the tests/comments that lock the behaviour in. The symptom (TUI freeze on opening any `.md`, especially the rich plan documents) disappears with minimal diff and zero user-visible behaviour change for well-formed lists.

## References

- Root cause investigation: the blocking await in `event.rs:157` (`resolve_io`) and `196` (`resolve_api_event`) around `OpenRun` + `ModelInterpreter::interpret` in `interpreter.rs:737`.
- See the shifted plan 0006 `TASKS.md` for the Exchange work that will now live under plan 0006.
- The ingestion interpreter contract and builders are in `interpreter.rs:865` (`build_ingestion_interpreter`) and `822` (`build_planner_interpreter`).
