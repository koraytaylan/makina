# Architecture — Plan 0028

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). This plan touches `crates/makina-core/src/interpreter.rs` (the
> generate prompt + entry) and `crates/makina-core/src/orchestrator.rs` (the
> missing-`TASKS.md` branch in the open path).

## Current shape (what exists)

- **The open path** (`crates/makina-core/src/orchestrator.rs`,
  `CoreApi::open_run` → `CoreApi::interpret_and_seed`): `open_run` prefers a
  persisted `.tasks/{slug}.json` artifact; on `Ok(None)`/corrupt it falls back to
  `interpret_and_seed(slug, task_list_path, repo_root, true)`, which **reads the
  `.md`**, lints it, and calls `self.state.interpreter.interpret(slug, &text)`.
  The read uses `tokio::fs::read_to_string` and maps **any** failure to
  `ApiError::InvalidCommand { reason: "could not read task list `…`: {e}" }` —
  the only hard error in the open path.
- **The interpreter seam** (`crates/makina-core/src/interpreter.rs`): the
  `TaskListInterpreter` trait has one method,
  `interpret(slug, source_text) -> Result<TaskGraph, InterpretError>`.
  `StructuredTextInterpreter` parses Markdown deterministically;
  `ModelInterpreter` (with `PLANNER_SYSTEM_PROMPT`, `ModelInterpreter::new` /
  `with_system_prompt` / `with_working_dir`) spawns a one-shot agent session,
  sends "Interpret the following task list …", collects `TextChunk`s until
  `TurnComplete`, and runs the shared `parse_model_response` (→
  `extract_json_object` → `serde_json` → `TaskGraph::validate()`).
- **Slug / path helpers** (`orchestrator.rs`): `run_slug(&Path)` (lowercased file
  stem, plan-scoped) and `plan_slug(&Path)` (parent-dir name). The TUI already
  recognises a plan-style path elsewhere by `file_name() == "TASKS.md"`
  (case-insensitive) — see `run_label` in `crates/makina/src/ui.rs`.
- **What is missing:** there is **no** generate-from-spec entry — every
  interpreter call assumes `source_text` *is* a finished task list. A missing
  `TASKS.md` therefore dead-ends at the `read_to_string` error above.
- **Introduced by sibling plans (not on `develop` today):** plan 0025 adds
  `RoleAssignment.system_prompt: Option<String>` + `system_prompt_mode`
  (append-by-default) and resolves the planner's effective prompt; plan 0027
  discovers and opens TASKS-less plan dirs (it is the caller that reaches this
  path). This plan references those as *introduced by* 00XX and does not
  re-declare them.

## 0079 — Planner generate on missing `TASKS.md`

Two edits: a **generate seam on the interpreter** (`interpreter.rs`) and a
**missing-`TASKS.md` branch in the open path** (`orchestrator.rs`).

### A. Generate seam — `interpreter.rs`

The interpret prompt is framed "convert this task list to JSON" and is wrong for
generation. Add a sibling constant that frames "author the task graph from a
scope + architecture brief", emitting the **same** JSON schema:

```rust
/// System prompt for the planner's GENERATE path: draft a Makina-convention
/// task graph from a plan dir's SCOPE/ARCHITECTURE brief (no existing TASKS.md).
///
/// Unlike [`PLANNER_SYSTEM_PROMPT`] (which transcribes an existing task list),
/// this instructs the model to DECOMPOSE the brief into tasks. The output schema
/// is identical, so [`parse_model_response`] validates it unchanged.
pub const PLANNER_GENERATE_SYSTEM_PROMPT: &str = "\
You are the Planner component of Makina … Given a project SCOPE and ARCHITECTURE \
brief (Markdown), DECOMPOSE the work into a dependency-ordered task graph and \
output ONLY the JSON object with the same schema as the Makina runtime artifact \
(slug, tasks[ id,title,description,done_when,depends_on,section,state=\"new\",… ]). \
Each task's done_when MUST be a verifiable acceptance check. Output ONLY JSON.";
```

Add a generative entry to `ModelInterpreter` that reuses the existing session +
collect + parse machinery but swaps the prompt. The simplest shape mirrors
`interpret` but takes a *brief* and uses the generate prompt; keep
`parse_model_response` as the single deserialize/validate funnel:

```rust
impl ModelInterpreter {
    /// Draft a fresh [`TaskGraph`] for `slug` from a SCOPE/ARCHITECTURE `brief`
    /// (no existing task list). Spawns one session with the GENERATE prompt,
    /// collects TextChunks until TurnComplete, then runs `parse_model_response`.
    ///
    /// `system_prompt_override` (Some when plan 0025 resolved a planner
    /// `RoleAssignment.system_prompt`) is appended to / replaces the generate
    /// prompt per the role's `system_prompt_mode`; None ⇒ the bare constant.
    pub async fn generate(
        &self,
        slug: &str,
        brief: &str,
        system_prompt_override: Option<&str>,
    ) -> Result<TaskGraph, InterpretError> {
        // Pure, directly unit-testable (NoopBackend records only prompts, not the
        // spawned SessionConfig, so the override is asserted on this output).
        let system_prompt = generate_system_prompt(system_prompt_override);
        let config = SessionConfig { system_prompt, working_dir: self.working_dir.clone(),
                                     mode: None, model: None, effort: None, extra: None };
        let mut session = self.backend.spawn(config).await?;
        let prompt = format!(
            "Draft the Makina task graph (slug `{slug}`) for the following plan. \
             Output ONLY the JSON task-graph object:\n\n{brief}"
        );
        let mut stream = session.prompt(Prompt::new(prompt)).await?;
        let mut raw = String::new();
        while let Some(item) = stream.next().await {
            match item? {
                ResponseEvent::TextChunk { text } => raw.push_str(&text),
                ResponseEvent::TurnComplete => break,
                _ => {}
            }
        }
        drop(stream);
        let _ = session.terminate().await;
        parse_model_response(&raw) // same extract → serde_json → validate funnel
    }
}
```

where the system-prompt assembly is a small pure helper:

```rust
/// Compose the generate session's system prompt: the base
/// `PLANNER_GENERATE_SYSTEM_PROMPT`, with the planner role override (plan 0025)
/// appended `\n\n`-joined when present. Pure ⇒ directly unit-testable.
fn generate_system_prompt(override_: Option<&str>) -> String {
    match override_ {
        Some(extra) => format!("{PLANNER_GENERATE_SYSTEM_PROMPT}\n\n{extra}"),
        None => PLANNER_GENERATE_SYSTEM_PROMPT.to_string(),
    }
}
```

Notes:
- `generate` is a `ModelInterpreter` inherent method, not a `TaskListInterpreter`
  trait method — the trait's `interpret(slug, source_text)` contract (an existing
  list) is unchanged, so `StructuredTextInterpreter`/`EdgeInferrer` are untouched.
- It reuses `parse_model_response` verbatim, so fence-stripping, prose-stripping,
  `ModelResponseInvalid`, and `ValidationFailed` all behave identically to
  `interpret`.
- The override-append is verified against `generate_system_prompt`'s output
  directly: the recording `NoopBackend` exposes only `recorded_prompts()`, not the
  spawned `SessionConfig`, so the system prompt is **not** observable through the
  backend — mirroring how the `roles.rs` `session_config_for` tests assert on the
  composed `system_prompt` directly.

### B. Missing-`TASKS.md` branch — `orchestrator.rs`

Branch `interpret_and_seed`'s read failure. Today it is unconditional:

```rust
let text = tokio::fs::read_to_string(task_list_path).await
    .map_err(|e| ApiError::InvalidCommand { reason: format!("could not read task list `{}`: {e}", …) })?;
```

Change it so a **`NotFound` on a plan-style `TASKS.md`** routes to generation
instead of erroring. Add a small helper and the branch:

```rust
/// True when `path` is a plan-style task list (`file_name == "TASKS.md"`,
/// case-insensitive) inside a plan directory — the only shape we auto-generate.
fn is_plan_tasks_path(path: &std::path::Path) -> bool {
    path.file_name().and_then(|s| s.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("TASKS.md"))
}
```

```rust
let text = match tokio::fs::read_to_string(task_list_path).await {
    Ok(text) => text,
    // Missing TASKS.md in a plan dir ⇒ generate the graph from the spec.
    Err(e) if e.kind() == std::io::ErrorKind::NotFound
        && is_plan_tasks_path(task_list_path) =>
    {
        return self
            .generate_and_seed(slug, task_list_path, repo_root, seed_persist)
            .await;
    }
    Err(e) => {
        return Err(ApiError::InvalidCommand {
            reason: format!("could not read task list `{}`: {e}", task_list_path.display()),
        });
    }
};
```

Add the sibling `generate_and_seed`, mirroring `interpret_and_seed`'s shape:

```rust
/// Generate a task graph for a TASKS-less plan dir from its SCOPE/ARCHITECTURE,
/// write the drafted `TASKS.md` into the dir (the auditable record), then
/// seed-persist the graph — returning `(graph, issues)` exactly like
/// `interpret_and_seed` so `open_run` registers a Pending run uniformly.
async fn generate_and_seed(
    &self,
    slug: &str,
    task_list_path: &std::path::Path,
    repo_root: &std::path::Path,
    seed_persist: bool,
) -> Result<(TaskGraph, Vec<crate::ingestion::IngestionIssue>), ApiError> {
    let dir = task_list_path.parent().unwrap_or(task_list_path);
    // 1. Collect the spec brief (SCOPE.md + ARCHITECTURE.md; either may be absent
    //    but at least one must exist, else fall through to a reviewable issue).
    let brief = read_plan_brief(dir).await; // joins the present spec files
    // 2. Draft the graph via the planner generate path (model-backed planner
    //    interpreter; deterministic fallback yields a clear issue, no panic).
    //    The planner system_prompt override (plan 0025) is threaded in here.
    let graph = /* self.state.planner_interpreter as ModelInterpreter → generate */;
    // 3. Write TASKS.md back into the dir from the graph (best-effort; warn-only
    //    like seed-persist — a write failure must not block the open).
    // 4. seed-persist the graph (reuse persist_graph, best-effort) and return.
    Ok((graph, vec![]))
}
```

Key constraints, mirroring the existing code's discipline:
- **No lock held across I/O.** Spec read, generate (a model session), `TASKS.md`
  write, and `persist_graph` all happen with no registry lock held — exactly as
  `interpret_and_seed` does today; `open_run` takes the registry lock only for
  the brief insert afterwards.
- **Same return contract.** `generate_and_seed` returns
  `(TaskGraph, Vec<IngestionIssue>)`; on a generation/validation failure it
  returns an **empty graph + a `Blocking` `IngestionIssue`** (reusing the
  `interpreter-failed`-style item) so the run opens as a *reviewable* Pending run
  rather than a hard `ApiError` — matching how `interpret_and_seed` degrades on
  an interpret error. A dir with no spec at all yields a clear issue
  ("no SCOPE.md/ARCHITECTURE.md to generate from"), not a panic.
- **The written `TASKS.md` is the record; the graph is re-derived consistently.**
  The graph returned is the one parsed from the model output; the on-disk
  `TASKS.md` is rendered from that same graph, so the auditable artifact and the
  registered graph agree. (Writing it via the structured-text rendering of the
  graph keeps the file editable + re-interpretable through the normal path.)

## Wiring the planner generate interpreter

`CoreState` already holds a `planner_interpreter: Arc<dyn TaskListInterpreter>`
(separate from the deterministic ingestion `interpreter`). The generate call
needs the *concrete* `ModelInterpreter::generate` entry. Reach it via a narrow
seam — either a new trait method with a default that returns
`InterpretError::MechanismNotSupported` (so `StructuredTextInterpreter` cleanly
declines and the run opens with a reviewable issue), or a downcast/`enum` on the
planner interpreter. Pick the trait-default variant: it keeps the deterministic
offline path honest (no model ⇒ explicit "cannot generate" issue) and avoids
`Any` downcasting. `main.rs` already builds `planner_interpreter` via
`build_planner_interpreter(&config.planner.mechanism, Some(backend))`, so the
model-backed generate path is available in the shipping binary whenever a planner
provider is configured.

## Test strategy

- `missing_tasks_triggers_planner_generate`: a temp plan dir with `SCOPE.md` +
  `ARCHITECTURE.md` and **no** `TASKS.md`; a stub backend (`NoopBackend::
  with_responses`) returns valid task-graph JSON; assert the generate path runs
  (the read `NotFound` does **not** become `ApiError::InvalidCommand`) and a
  non-empty `TaskGraph` is produced + validated.
- `generated_graph_is_ingested_and_run_opens`: drive `OpenRun` end-to-end against
  that dir; assert `CommandOutcome::RunOpened`, the registered run is `Pending`
  with the generated tasks, and a `TASKS.md` now exists in the dir.
- `no_tasks_md_does_not_hard_error`: with the generate backend wired, opening a
  TASKS-less plan dir returns `Ok(RunOpened)`, never `Err(InvalidCommand)`; with
  the deterministic-only planner interpreter, it still opens (Pending) carrying a
  reviewable "cannot generate" issue rather than a hard error.
- `non_tasks_md_missing_file_still_errors`: a missing `.tasks/x.json` (not a
  plan-style `TASKS.md`) still returns `ApiError::InvalidCommand` — the generate
  branch is correctly scoped.
- `model_generate_validates_output`: `ModelInterpreter::generate` with a dangling
  `depends_on` JSON returns `InterpretError::ValidationFailed` (proves the shared
  `parse_model_response` funnel applies to generate).
- `generate_system_prompt_appends_override`: a pure assertion on
  `generate_system_prompt` — `Some("EXTRA RULES")` ⇒ starts with
  `PLANNER_GENERATE_SYSTEM_PROMPT` and contains `"\n\nEXTRA RULES"`; `None` ⇒ the
  bare constant. (No backend; `NoopBackend` records only prompts, not the spawned
  `SessionConfig`.)

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay green.
Use `NoopBackend::with_responses` for the model and `tempfile` dirs for the spec
fixtures — no real network, no real model.

## Interaction with prior plans

- Depends on plan 0025's planner `RoleAssignment.system_prompt` /
  `system_prompt_mode` to resolve the override threaded into `generate`; when
  that field is absent the bare `PLANNER_GENERATE_SYSTEM_PROMPT` is used.
- Plan 0027 is the caller that *discovers* and opens a TASKS-less plan dir; this
  plan makes that open succeed by generating rather than erroring.
- Reuses `ModelInterpreter`/`parse_model_response`/`PLANNER_SYSTEM_PROMPT`'s
  schema (the generate output is the same `TaskGraph` JSON), `persist_graph`
  (seed-persist), and the `IngestionIssue`/`IngestionReport` degrade-to-Pending
  pattern from `interpret_and_seed` (plans 0004/0007).
