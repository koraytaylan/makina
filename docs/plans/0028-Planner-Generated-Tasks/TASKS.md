# Makina Plan 0028 — Planner-Generated TASKS.md Fallback

When an opened or discovered plan directory has a `SCOPE.md` and an
`ARCHITECTURE.md` but **no `TASKS.md`**, the planner **auto-generates** the task
graph from that spec and the run opens — no manual review gate. Today the open
path hard-fails on the missing file (`could not read task list …/TASKS.md` in
`CoreApi::interpret_and_seed`); this plan adds a planner GENERATE seam and routes
the missing-`TASKS.md` read into it.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0079 — Planner generate on missing TASKS.md

### planner-generate-prompt — Generate system prompt + `ModelInterpreter::generate`

Give the planner a *generative* mode: a system prompt that authors a
Makina-convention task graph from a SCOPE/ARCHITECTURE brief (instead of
transcribing an existing list), and a `ModelInterpreter` entry that drives one
agent session with it and funnels the output through the existing
deserialize/validate path.

**Steps:**

1. In `crates/makina-core/src/interpreter.rs`, add
   `pub const PLANNER_GENERATE_SYSTEM_PROMPT: &str` next to `PLANNER_SYSTEM_PROMPT`.
   It must instruct the model to **decompose** a `SCOPE`/`ARCHITECTURE` brief into
   a dependency-ordered task graph and output **only** the same JSON object schema
   `PLANNER_SYSTEM_PROMPT` documents (slug + `tasks[]` with
   `id,title,description,done_when,depends_on,section,state:"new",
   gate_iterations:0,review_iterations:0,created_at,updated_at`; no
   `started_at`/`finished_at`; no nulls; no fences). Document that it shares the
   interpret path's schema so `parse_model_response` validates it unchanged.

2. Add an inherent method
   `pub async fn generate(&self, slug: &str, brief: &str, system_prompt_override: Option<&str>) -> Result<TaskGraph, InterpretError>`
   to `impl ModelInterpreter`. It mirrors `interpret` but: (a) builds the
   `SessionConfig.system_prompt` from `PLANNER_GENERATE_SYSTEM_PROMPT`, appending
   `system_prompt_override` (a `\n\n`-joined append) when `Some`; (b) sends a
   prompt that frames `brief` as the plan to draft from, not a finished list; (c)
   collects `ResponseEvent::TextChunk` until `TurnComplete`, ignoring side-channel
   events; (d) drops the stream, best-effort `terminate()`s, and returns
   `parse_model_response(&raw)` — the **same** funnel `interpret` uses. Factor the
   system-prompt assembly out into a small pure helper
   `fn generate_system_prompt(override_: Option<&str>) -> String` (matching the
   `Some(extra) => format!("{PLANNER_GENERATE_SYSTEM_PROMPT}\n\n{extra}")` /
   `None => PLANNER_GENERATE_SYSTEM_PROMPT.to_string()` logic) so the override
   composition is unit-testable directly — the recording `NoopBackend` only captures
   prompts (`recorded_prompts`), not the spawned `SessionConfig`, so the system
   prompt cannot be asserted through the backend.

3. Do **not** change the `TaskListInterpreter` trait's `interpret` contract,
   `StructuredTextInterpreter`, `parse_model_response`, or
   `PLANNER_SYSTEM_PROMPT`. `generate` is additive on `ModelInterpreter` only.

4. Add tests in `interpreter.rs` (reuse `NoopBackend::with_responses` and the
   existing `valid_task_graph_json` helper):

   ```rust
   #[tokio::test]
   async fn generate_drafts_graph_from_brief() {
       /* ModelInterpreter::new(NoopBackend::with_responses([valid_task_graph_json("p")]));
          generate("p", "# Scope … \n# Architecture …", None).await
          => Ok(graph) with the canned tasks; graph.validate() passes */
   }
   #[test]
   fn generate_system_prompt_appends_override() {
       /* pure helper, no backend: generate_system_prompt(Some("EXTRA RULES"))
          starts_with(PLANNER_GENERATE_SYSTEM_PROMPT) && contains("EXTRA RULES")
          && contains("\n\nEXTRA RULES"); generate_system_prompt(None)
          == PLANNER_GENERATE_SYSTEM_PROMPT. (NoopBackend records only prompts,
          not the spawned SessionConfig, so the override is asserted on the
          prompt-builder output directly — the same pattern the roles.rs
          session_config_for tests use for system_prompt.) */
   }
   #[tokio::test]
   async fn generate_validates_output() {
       /* backend returns JSON with a dangling depends_on => generate returns
          InterpretError::ValidationFailed (same parse_model_response funnel) */
   }
   ```

- **Depends on:** —
- **Done when:** `PLANNER_GENERATE_SYSTEM_PROMPT` exists and documents the shared
  schema; `ModelInterpreter::generate` drafts + validates a `TaskGraph` from a
  brief and rejects invalid output via `parse_model_response`; the pure
  `generate_system_prompt(Some/None)` helper composes the override append
  (`PLANNER_GENERATE_SYSTEM_PROMPT` prefix + `\n\n` + override) and is asserted
  directly (not through the backend, which records only prompts); the interpret
  path / trait / schema are unchanged; the three tests pass; cargo test/clippy/fmt
  green.

---

### planner-generate-on-open — Route missing `TASKS.md` to generate, write it, open the run

Branch `CoreApi::interpret_and_seed`'s read failure: a `NotFound` on a plan-style
`TASKS.md` path generates the graph from the dir's spec, writes the drafted
`TASKS.md` back, and returns the same `(graph, issues)` so `open_run` registers a
Pending run uniformly — instead of `ApiError::InvalidCommand`.

**Steps:**

1. In `crates/makina-core/src/orchestrator.rs`, add a free helper
   `fn is_plan_tasks_path(path: &std::path::Path) -> bool` that returns true when
   `path.file_name()` equals `"TASKS.md"` (case-insensitive) — the same plan-style
   test `run_label` uses in `crates/makina/src/ui.rs`.

2. In `CoreApi::interpret_and_seed`, replace the unconditional
   `tokio::fs::read_to_string(task_list_path).await.map_err(…InvalidCommand…)?`
   with a `match`: on `Ok(text)` continue as today; on
   `Err(e) if e.kind() == std::io::ErrorKind::NotFound && is_plan_tasks_path(task_list_path)`
   `return self.generate_and_seed(slug, task_list_path, repo_root, seed_persist).await`;
   on any other `Err(e)` return the **unchanged** `ApiError::InvalidCommand`
   ("could not read task list `…`: {e}"). Keep all I/O lock-free, as today.

3. Add `async fn generate_and_seed(&self, slug, task_list_path, repo_root, seed_persist) -> Result<(TaskGraph, Vec<IngestionIssue>), ApiError>`
   mirroring `interpret_and_seed`'s signature/return contract:
   - Resolve `dir = task_list_path.parent()`; read the brief by joining the
     present `SCOPE.md` and `ARCHITECTURE.md` (each optional). If **neither**
     exists, return an empty `TaskGraph { slug, tasks: vec![] }` plus a single
     `Blocking` `IngestionIssue` (code `"no-spec-to-generate"`, message naming the
     dir) — a reviewable Pending run, **not** an `ApiError` (mirrors how
     `interpret_and_seed` degrades on interpret failure).
   - Drive generation via the planner interpreter. Reach
     `ModelInterpreter::generate` through a narrow seam: add
     `async fn generate(&self, slug, brief, override) -> Result<TaskGraph, InterpretError>`
     to the `TaskListInterpreter` trait **with a default impl** that returns
     `InterpretError::MechanismNotSupported { mechanism: "generate".into() }`, and
     override it on `ModelInterpreter` to call the inherent `generate`. The
     deterministic `StructuredTextInterpreter`/`EdgeInferrer` keep the default ⇒
     offline opens degrade to the "cannot generate" issue rather than panicking.
     Thread the planner `system_prompt` override (plan 0025; `None` until then).
   - On `Ok(graph)`: write `TASKS.md` into `dir` rendered from `graph` (a
     structured-text serialization that round-trips through
     `StructuredTextInterpreter`), best-effort (warn-only on write error, like
     seed-persist); then, when `seed_persist`, `persist_graph(&graph, repo_root)`
     (best-effort). Return `(graph, vec![])`.
   - On `Err(e)`: return `(empty graph, vec![Blocking interpreter-style issue])`
     so the run opens reviewable (same shape as `interpret_and_seed`'s error arm).

4. Add tests in `orchestrator.rs` (use a `tempfile` repo + plan dir, write
   `SCOPE.md`/`ARCHITECTURE.md`, omit `TASKS.md`; build `CoreApi` with a planner
   interpreter backed by `NoopBackend::with_responses([valid task-graph JSON])`):

   ```rust
   #[tokio::test]
   async fn missing_tasks_triggers_planner_generate() {
       /* dir has SCOPE.md + ARCHITECTURE.md, no TASKS.md; planner=ModelInterpreter
          over canned JSON; open_run(dir/TASKS.md) => the NotFound is NOT mapped to
          InvalidCommand; returned/registered TaskGraph is non-empty + validates */
   }
   #[tokio::test]
   async fn generated_graph_is_ingested_and_run_opens() {
       /* OpenRun end-to-end => CommandOutcome::RunOpened; run is Pending with the
          generated tasks; TASKS.md now exists in the dir and re-interprets cleanly
          via StructuredTextInterpreter */
   }
   #[tokio::test]
   async fn no_tasks_md_does_not_hard_error_offline() {
       /* same dir, planner = deterministic StructuredTextInterpreter (no model):
          open still returns Ok(RunOpened) (Pending) carrying a "cannot generate"
          blocking issue — never Err(InvalidCommand) */
   }
   #[tokio::test]
   async fn non_tasks_md_missing_file_still_errors() {
       /* OpenRun on a missing ".tasks/ghost.json" (not plan-style TASKS.md) still
          returns ApiError::InvalidCommand — the generate branch is scoped */
   }
   ```

- **Depends on:** planner-generate-prompt
- **Done when:** opening a plan dir with `SCOPE.md`/`ARCHITECTURE.md` and no
  `TASKS.md` generates the task graph, writes a re-interpretable `TASKS.md` into
  the dir, seed-persists, and returns `CommandOutcome::RunOpened` (Pending) — no
  manual review; a non-`TASKS.md` missing file still errors with the unchanged
  `ApiError::InvalidCommand`; the offline/no-model path opens with a reviewable
  "cannot generate" issue rather than panicking or hard-erroring; the four tests
  pass; cargo test/clippy/fmt green.

---

### planner-generate-docs — Document the generate fallback in the convention spec

Record the new behaviour where the convention is normative so the generated
`TASKS.md` is a documented, auditable artifact.

**Steps:**

1. In `docs/spec/structured-text-convention.md`, add a short subsection (under the
   "Roles of the Two Artifacts" / document-structure material) stating that when a
   plan dir has no `TASKS.md`, Makina's planner **auto-generates** one from
   `SCOPE.md`/`ARCHITECTURE.md` using `PLANNER_GENERATE_SYSTEM_PROMPT`, writes it
   into the dir as the auditable record, and opens the run with no manual review;
   the generated file conforms to this same convention and is editable +
   re-interpretable through the normal path.

2. Cross-reference plan `docs/plans/0028-Planner-Generated-Tasks/` and note the
   `ModelInterpreter::generate` entry point and the `CoreApi::interpret_and_seed`
   `NotFound` branch so a reader can find the implementation.

   ```rust
   /* docs-only task: no code. Verify the spec edit by grepping the new text and
      confirming it matches the shipped behaviour from planner-generate-on-open. */
   ```

- **Depends on:** planner-generate-on-open
- **Done when:** `docs/spec/structured-text-convention.md` documents the
  auto-generate-on-missing-`TASKS.md` fallback (prompt constant, write-back,
  no-review, re-interpretable artifact) and accurately describes the implemented
  behaviour; no code changes; cargo test/clippy/fmt green.

---

**End of plan 0028 TASKS.** When every "Done when" bullet is green, opening a
plan directory that has only `SCOPE.md`/`ARCHITECTURE.md` no longer dead-ends:
the planner drafts the task graph, writes an editable `TASKS.md` as the auditable
record, and the run opens automatically — while every non-`TASKS.md` missing-file
case keeps its existing hard error.
