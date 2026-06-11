# Scope — Plan 0019

> What this plan delivers, what it leaves out, and the decisions behind it.
> Origin: full-codebase review, 2026-06-11.

## Why this plan

`task_id` is strictly validated; the slug next to it is not — and nothing
prevents two live runs on the same slug. The combination makes the worktree
reclaim heuristic destructive.

1. **Slugs reach the filesystem unvalidated.** `WorktreeManager::create` /
   `remove` (`worktree.rs:190–215`, `:267–271`) call `validate_task_id`
   (`worktree.rs:390–413` — strict kebab-case, rejects `..` and `/`) but never
   validate `plan_slug`, which flows verbatim into `paths::worktree`
   (`paths.rs:120–125` — a plain `join(format!("{plan_slug}--{task_id}"))`) and
   into the branch name `task/{plan_slug}--{task_id}` (`worktree.rs:198`). A
   slug like `../../../tmp/evil` yields a path *outside the repository*, and
   `remove()` then runs `tokio::fs::remove_dir_all` on it
   (`worktree.rs:296–299`). The persistence layer has the same shape:
   `tasks_path` / `temp_path` (`persist.rs:103–124`) join `graph.slug`
   verbatim.

2. **The slug is model-controlled.** The trust chain is real:
   `ModelInterpreter::interpret` (`interpreter.rs:723–763`) returns whatever
   graph `parse_model_response` (`interpreter.rs:780–794`) deserializes —
   *including the model-supplied `slug`* (`TaskGraph.slug`,
   `task.rs:189–191`); `TaskGraph::validate()` checks duplicate ids and
   dangling deps only (`task.rs:218–242`), never slug safety. That slug is
   persisted immediately: `open_run`'s fresh path seed-persists the
   interpreted graph (`orchestrator.rs:798–811`) via `persist_graph`, which
   writes to `tasks_path(repo_root, &graph.slug)` (`persist.rs:176`, `:187`),
   and the supervisor re-persists it on every transition
   (`supervisor.rs:542–557`). A prompt-injected planner model controls a
   string that becomes filesystem write paths. (The *worktree* `plan_slug` is
   today derived and sanitized locally — `orchestrator.rs:163–177`,
   `sanitize_kebab` at `:182–200` — so that layer is unvalidated trust in the
   caller rather than an open hole: one refactor away from one.)

3. **No run exclusivity per slug.** Nothing stops two live runs over the same
   plan slug. `WorktreeManager::create`'s reclaim-on-conflict
   (`worktree.rs:209–216`) treats any existing `{plan_slug}--{task_id}`
   worktree/branch as "a stale slot left by a prior interrupted run" and
   **deletes it** (`remove`, then recreate fresh). With two live runs on one
   slug — open the same task list twice and press start twice — run B reclaims
   run A's *live* worktree mid-task. The "Makina-owned namespace ⇒ stale"
   heuristic is only sound when at most one run per slug is live.

This plan introduces a validated `Slug` newtype threaded through every
path/branch construction site, rejects unsafe model-supplied slugs at the
interpretation boundary, and refuses to start a second live run on a slug.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0060–0061):

- **0060 — Validated `Slug` newtype.** A `Slug` type in `makina-core` whose
  constructor enforces the artifact-schema §4.1 kebab rules; `TaskGraph.slug`
  becomes `Slug` (serde-compatible: serializes as a plain string,
  deserialization validates); `&Slug` threads through `paths::worktree`,
  `persist::tasks_path`/`temp_path`, `WorktreeManager::create`/`remove`,
  `RunMetadata`, and the orchestrator/supervisor plumbing; the model-response
  boundary rejects an unsafe slug with a typed interpret error.
- **0061 — Per-slug run exclusivity.** `StartRun` refuses when another
  non-terminal (Running/Paused) run shares the plan slug, via
  `ApiError::InvalidCommand` with a clear reason; opening for inspection stays
  allowed.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| `plan_slug`/`slug` joined verbatim into worktree paths, branch names, and `.makina/tasks/` paths; `remove_dir_all` on the result | `0060` |
| Model-supplied `graph.slug` reaches disk write paths through `parse_model_response` → `persist_graph` | `0060` |
| Two live runs on one slug: reclaim-on-conflict deletes the other run's live worktree | `0061` |

## Locked decisions

- **`Slug` enforces artifact-schema §4.1** (`docs/spec/runtime-artifact-schema.md`
  §4.1): lowercase ASCII letters/digits/hyphens, starts and ends alphanumeric,
  **no consecutive hyphens**, minimum two characters. This is exactly what the
  derived slugs already satisfy (`run_slug`'s sanitizer targets §4.1 —
  `orchestrator.rs:103–110`; real artifacts like
  `.makina/tasks/0005-tui-ingestion-responsiveness-tasks.json` conform), and
  the no-`--` rule keeps the `{plan_slug}--{task_id}` delimiter unambiguous
  (`paths.rs:105–107`). Strictly stronger than `validate_task_id` (which
  permits `--` and single chars); `validate_task_id` itself is unchanged.
- **`Slug` lives in `task.rs` alongside `TaskId`** (`task.rs:52`), because
  `TaskGraph.slug` is its primary owner and the two identifier types belong
  together. Serde via `#[serde(try_from = "String", into = "String")]` — plain
  string on the wire, validation on every deserialization (model responses
  *and* persisted artifacts).
- **Typed rejection at the model boundary.** `parse_model_response` pre-checks
  the raw JSON's `"slug"` field with `Slug::parse` and returns a new
  `InterpretError::UnsafeSlug { slug, reason }` — a first-class diagnosis for
  the prompt-injection case — before full deserialization (which would
  otherwise fold the failure into a generic `ModelResponseInvalid`).
- **Derived slugs are `Slug` by construction.** `orchestrator::run_slug` /
  `plan_slug` (`orchestrator.rs:120–150`, `:163–177`) return `Slug`; their
  sanitizer + fallback already guarantee validity, enforced with a
  `debug_assert`-backed parse.
- **Exclusivity is checked at `StartRun`, not `OpenRun`.** Opening a run for
  inspection is harmless (it creates registry state, not worktrees); only
  starting dispatches drivers that create/reclaim worktrees. The check keys on
  **`plan_slug`** (the worktree namespace `{plan_slug}--{task_id}` is what
  collides) and refuses while another run with the same plan slug is `Running`
  or `Paused`; `Pending`, `Completed`, and `Failed` peers don't block.
- **The reclaim heuristic itself is untouched.** "Makina-owned namespace ⇒
  stale" (`worktree.rs:204–216`) becomes *sound* once at most one run per slug
  is live — exclusivity is the fix, not a different heuristic.

## Out of scope

- **Sandboxing the gate commands** (plan 0024 of this review; plan 0008 covers
  the existing gate sandbox baseline).
- **Multi-process / multi-user locking.** Exclusivity here is within one
  orchestrator process (the runs registry); cross-process coordination remains
  future work ("Multi-user concurrency", `docs/plans/0001-Initial/mind-map.opml`).
- **Changing the worktree reclaim heuristic** (see locked decisions — it is
  sound under exclusivity).
- **Normalizing a safe-but-mismatched model slug** to the derived run slug
  (today `open_run` loads by derived slug while the seed persists under
  `graph.slug` — a consistency quirk, not a safety hole; only *unsafe* slugs
  are rejected here).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
