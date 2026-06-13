# Scope — Plan 0028

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Opening a run is **read-then-interpret**: the orchestrator reads a task-list
file and turns its text into a `TaskGraph`. The read happens in
`CoreApi::interpret_and_seed` (`orchestrator.rs`), and if the file is missing the
whole open **hard-fails**:

```rust
let text = tokio::fs::read_to_string(task_list_path)
    .await
    .map_err(|e| ApiError::InvalidCommand {
        reason: format!("could not read task list `{}`: {e}", task_list_path.display()),
    })?;
```

So a plan directory that has a `SCOPE.md` and an `ARCHITECTURE.md` but **no
`TASKS.md`** cannot be opened at all — the user sees `could not read task list
…/TASKS.md` and a dead end, even though the planner is perfectly capable of
drafting the task graph from the spec it *does* have. The only interpreter path
that exists today **interprets an existing `TASKS.md`** into JSON
(`StructuredTextInterpreter`, `ModelInterpreter::interpret`); there is **no
generate-from-spec path**.

This is the gap covered by origin finding **#8**: a discovered/opened plan dir
with no `TASKS.md` should not be a wall. The planner already owns task-graph
authoring (`PLANNER_SYSTEM_PROMPT`, `ModelInterpreter`); this plan gives it a
second, *generative* mode and wires the missing-`TASKS.md` open path to it.

Two concrete problems:

1. **A spec-only plan dir is unopenable.** `interpret_and_seed`'s read failure
   becomes `ApiError::InvalidCommand` and the run never registers — there is no
   fallback to *generate* the list from `SCOPE.md`/`ARCHITECTURE.md`.
2. **The planner can only transcribe, not author.** `ModelInterpreter` and
   `PLANNER_SYSTEM_PROMPT` are framed entirely as "convert this Markdown task
   list into JSON"; neither can take a scope/architecture brief and *produce* a
   `TASKS.md`-convention task graph.

This plan adds a **planner GENERATE path**: when `OpenRun` targets a plan dir
whose `TASKS.md` is absent, the planner drafts the task graph from the dir's
`SCOPE.md`/`ARCHITECTURE.md` using a dedicated generate system prompt, writes the
drafted `TASKS.md` into the dir as the auditable record, ingests it via the
existing interpreter path, and the run **proceeds** — no manual review gate.

## In scope

Work items in [TASKS.md](TASKS.md) (workstream 0079):

- **0079 — Planner generate on missing `TASKS.md`.** Add the generate seam: a
  `PLANNER_GENERATE_SYSTEM_PROMPT` constant and a generative interpret entry on
  `ModelInterpreter` that takes the concatenated `SCOPE.md`/`ARCHITECTURE.md`
  brief instead of a finished task list. Branch `interpret_and_seed`'s read
  failure for a plan-style `TASKS.md` path into the generate path: collect the
  sibling specs, generate the graph, write `TASKS.md` back into the dir, and
  return the graph so `open_run` registers a Pending run exactly as the normal
  path does.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Opened/discovered plan dir lacks `TASKS.md`; `OpenRun` hard-fails on the read | `0079` |
| Planner can only transcribe an existing list, not author one from a spec | `0079` |

## Locked decisions

- **Auto-generate and run — no blocking review.** On a missing `TASKS.md` the
  planner drafts the task graph from the plan dir and the run opens
  automatically. The **written `TASKS.md` is the auditable, editable record**;
  there is no confirmation step. (Re-interpreting or editing it later rides the
  existing `ReinterpretRun` path — out of scope here.)
- **Generate uses a distinct system prompt, not the interpret prompt.**
  `PLANNER_SYSTEM_PROMPT` says "convert this Markdown into JSON"; the generate
  path needs "author a Makina-convention task graph from this scope +
  architecture brief." A new `PLANNER_GENERATE_SYSTEM_PROMPT` keeps the existing
  interpret contract untouched, and (per plan 0025) the planner role's
  `RoleAssignment.system_prompt` appends to it by default.
- **Generate emits the same JSON schema the interpret path already validates.**
  The drafted graph is the *same* `TaskGraph` JSON `ModelInterpreter` already
  parses (`parse_model_response` → `TaskGraph::validate()`); generation differs
  only in the *prompt and the input* (a spec brief, not a task list). One
  deserialize/validate path, one set of guarantees.
- **Only triggers when `TASKS.md` is genuinely absent for a plan-style path.**
  The generate branch fires solely on the `read_to_string` *NotFound* of a
  `TASKS.md`-named file inside a plan dir; every other read error (permissions,
  a present-but-broken file) keeps today's behaviour exactly. A spec-less dir
  (no `SCOPE.md`/`ARCHITECTURE.md`) still surfaces a clear, reviewable failure.
- **The drafted `TASKS.md` is written, then re-read through the normal path.**
  Generation writes `TASKS.md` to the dir and then the *existing* deterministic
  `interpret_and_seed` flow re-reads/ingests it — so the run's graph comes from
  the same interpreter the rest of Makina trusts, and the on-disk artifact and
  the in-memory graph are guaranteed consistent.

## Out of scope

- LLM-driven repo discovery of gate commands / role constraints and the
  `[discovery]` config stamp (separate plan 0027; this plan only *opens* a
  TASKS-less dir, it does not discover it).
- Re-running / editing the generated `TASKS.md` after the fact — that is the
  existing `ReinterpretRun` path (plan 0004 machinery), unchanged here.
- A manual "review the generated plan before running" gate (explicitly decided
  against — the written artifact is the record).
- Per-role `system_prompt` / `system_prompt_mode` config plumbing itself (plan
  0025); this plan *consumes* the planner's resolved prompt but does not add the
  config surface.
- Changing the deterministic `StructuredTextInterpreter` or the JSON schema.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
