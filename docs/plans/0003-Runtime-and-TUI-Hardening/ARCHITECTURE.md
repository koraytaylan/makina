# Architecture — Plan 0003 (deltas)

> Deltas to [`0001-Initial/ARCHITECTURE.md`](../0001-Initial/ARCHITECTURE.md)
> and plan 0002. File:line references were re-grounded against the just-merged
> plan-0002 code (develop @ `e134161`); symbol names are the stable anchors.

Four workstreams: **A. `.makina/` workspace + run identity**, **B. logging &
diagnostics**, **C. scheduler robustness**, **D. TUI presentation & views**.
B depends on A (run-id + paths); D's error pane depends on B; the rest are
largely independent.

---

## A. Unified `.makina/` workspace + run identity

### The target layout
```
.makina/
  config.toml            committed   (was ./makina.toml)
  tasks/{slug}.json      committed   (was ./.tasks/{slug}.json)
  runs/{run-id}/         gitignored
    run.json             (slug, status, started/ended, task→worktree map)
    audit.jsonl          (was ./.tasks/{slug}/audit.jsonl)
    logs/{task_slug}.log
  worktrees/{task-id}/   gitignored  (was ./.worktrees/{task-id}/)
  .gitignore             (ignores runs/ and worktrees/)
```

### Centralize path construction (`crates/makina-core/src/paths.rs`, new)
Today path construction is scattered but rooted at the single
`WorktreeManager.repo_root` (`worktree.rs:130`, sourced from
`std::env::current_dir()` in `main.rs:68`). Introduce a small `paths` module
that owns the `.makina/` layout, and route the existing helpers through it:
- `config.rs:565` `PathBuf::from("makina.toml")` → `paths::config_file(repo_root)` = `.makina/config.toml`.
- `persist.rs:99` `tasks_path` and `:117`/`:151` (`temp_path`, tasks dir) → `paths::task_graph(repo_root, slug)` = `.makina/tasks/{slug}.json`.
- `worktree.rs:313` `worktree_path` → `paths::worktree(repo_root, task_id)` = `.makina/worktrees/{task-id}`.
- `audit.rs:224` `repo_root.join(".tasks").join(slug)` + `:239` → `paths::audit_log(repo_root, run_id)` = `.makina/runs/{run-id}/audit.jsonl` (see run identity + decision in SCOPE).
The change is mostly mechanical: ~5 functional call sites + the docs/test string
updates the relocation recon enumerated (~120 references, the vast majority
doc-comments/test fixtures).

### Run identity (`crates/makina-core/src/api.rs`)
`RunId(u64)` (`api.rs:42`) is a session counter, not persisted. Add a
persistent, sortable **run id** — a ULID-style string (timestamp-prefixed so
`ls runs/` is chronological); add the `uuid`/`ulid` crate. Keep `RunId(u64)` as
the in-memory handle; the string id keys `.makina/runs/{run-id}/` and is
threaded to the Supervisor (it already carries `run_slug`; add `run_id`
alongside — `supervisor.rs:~436` registration site) and into `run.json`.

### Run slug — collision-free, plan-scoped (`crates/makina-core/src/orchestrator.rs`)
`open_run` currently derives the slug as `task_list_path.file_stem()`. Because
every task list is named `TASKS.md`, that makes all plans collide on the slug
`TASKS` — so `.makina/tasks/{slug}.json` is shared across plans, and the
plan-0002 read-path will load a stale `.tasks/TASKS.json` from one plan when you
open another. Fix: derive the slug from the **parent directory name + file
stem**, lowercased and kebab-sanitized — e.g.
`docs/plans/0003-Runtime-and-TUI-Hardening/TASKS.md` →
`0003-runtime-and-tui-hardening-tasks` — falling back to the lowercased stem
when there is no usable parent. This makes the task-graph artifact (and the
read-path's `load_graph`) unique per plan. The slug must remain a valid kebab id
per `docs/spec/runtime-artifact-schema.md` §4.1. (Distinct from the per-run ULID
above: the slug keys the committed `tasks/{slug}.json`; the ULID keys the
transient `runs/{run-id}/`.)

### Config back-compat (`crates/makina-core/src/config.rs`)
`load_defaults` (`:563`) reads `.makina/config.toml`; if absent and a legacy
`./makina.toml` exists, load it with a one-time `tracing::warn!` deprecation
note. Global `~/.makina/config.toml` is unchanged.

### `.gitignore`
Add `.makina/.gitignore` ignoring `runs/` and `worktrees/`. Update the root
`.gitignore` (currently `/.worktrees/` at lines 5–8) — drop `/.worktrees/`, and
do not ignore `.makina/` itself (config + tasks are committed; the internal
`.gitignore` carves out the transient subdirs). Keep `persist.rs`'s gitignore
invariant test, updated for the new layout.

---

## B. Logging & diagnostics

### Today
There is **no `tracing_subscriber` anywhere**. `tracing::warn!` calls
(`supervisor.rs:501`, `audit.rs`, `orchestrator.rs`) go nowhere; errors
`eprintln!` past the frame (`main.rs:57/116/126`); agent stderr becomes
`AgentExchange` events held in TUI memory and lost on exit. No per-run logs.

### Logging subsystem
- **Subscriber** (in `makina`): install `tracing_subscriber` with two layers —
  a **file layer** writing per-run / per-task logs under
  `.makina/runs/{run-id}/logs/{task_slug}.log` (state transitions, gate output,
  agent exchange), and a **TUI channel layer** that feeds the error/log pane
  (workstream D). Wire alongside the existing audit-sink setup in
  `main.rs:39–106` (same `repo_root` + run plumbing).
- **Per-task transcript:** the per-run event stream the Supervisor already
  emits (the `RunControl` sink, `supervisor.rs:~202`) plus agent stderr route
  to the task's log file — reuse the `AuditRegistry` per-task-routing pattern
  (`supervisor.rs:~436`) for per-task log routing.
- **`run.json`:** write run metadata (slug, run-id, status, timing, task→worktree
  map) under `.makina/runs/{run-id}/` so a run is inspectable from disk.
- **Folded plan-0002 follow-ups:** (1) **async audit write** — `JsonlAuditSink::record`
  (`audit.rs`) currently does blocking `std::fs` on the ACP reader thread;
  offload to a background writer (bounded channel + writer task) so the trait
  stays sync but I/O leaves the hot path. (2) **registry eviction** — evict a
  task's `AuditRegistry` entry when its run completes (the run-id now scopes it).

---

## C. Scheduler robustness (`crates/makina-core/src/actors/supervisor.rs`)

### Today
The scheduler (`scheduler()` ~`:914`) has three `stop_launching = true` sites:
graph-advance failure (`:987`), a driver returning a hard `Err` (`:1073`), and a
driver panic (`:1118`). `next_ready_task_id` (`:1136`) requires every dep
`== Done`, so a `Failed` task's dependents already never become ready — they sit
non-terminal forever. A normal `Failed` (gate/review cap) and wall-clock-cap do
**not** stop launching.

### Continue-independents + `Skipped`
- **FSM** (`state_machine.rs`): add a `Skipped` terminal state (alongside
  `Done`/`Failed`) and a `DependencyFailed` event legal from
  `New|Ready|InProgress|InReview → Skipped`; update `transition` (`:194`),
  `is_terminal` (`:233`), and `legal_events` (`:241`); keep the FSM total. The
  plan-0002 `MergeConflict` addition (`state_machine.rs` tests ~`:598`) is the
  precedent.
- **Scheduler:** a task failure (including the hard-error paths at `:987`/`:1073`)
  must **not** set `stop_launching` — keep launching independent ready tasks.
  When a task reaches a non-`Done` terminal, mark its **transitive dependents**
  `Skipped` (via `DependencyFailed`) so they leave the ready computation and
  show a reason in the report, instead of dangling. A genuine **panic** (`:1118`)
  stays fatal (`stop_launching`). The run's final status is `Failed` if any task
  failed, but it completes all independent work first.
- **Parallelism probe:** a focused investigation task — instrument the scheduler
  to confirm `concurrency` drivers actually overlap, and root-cause the
  "sequential appearance" the dogfood showed (prime suspect: `cargo` build
  contention across worktrees sharing a target dir, or gate serialization). Fix
  follows the finding; the new timeline view (D) makes overlap observable.

---

## D. TUI presentation & views (`crates/makina/src/`)

- **Error/log pane** (new, bottom, collapsible): default collapsed with an
  error-count badge; a key toggles it; shows recent system errors + agent
  stderr fed by the tracing TUI layer (B). Replaces the `eprintln!` frame-bypass
  (`main.rs:57/116/126`). Nothing writes outside the ratatui frame anymore.
- **Exchange pane** (`ui.rs:exchange_entry_lines` ~`:453`, no ANSI handling
  today): parse ANSI SGR → ratatui styles (render the agent's colors), special-
  case unified-diff lines (+/− gutter, green/red, preserved alignment), and
  strip non-SGR control codes. No more literal `\x1b[31m`.
- **Mouse scroll** (`event.rs:320` drops mouse → `Tick`; no scroll state in
  `app.rs`): add a mouse arm; the wheel scrolls the **focused pane's** content —
  give the exchange pane a scroll offset (manual + auto-follow). Task switching
  stays on keys/sidebar.
- **Sidebar label** (`ui.rs:130`, `task_list_path.file_stem()`): show
  `{project}/{plan}` — repo-dir name + the task-list's plan folder (e.g.
  `makina / 0002-Governance-and-Persistence`).
- **`G`/`R` legend** (`ui.rs:279–288`): keep the compact columns, add a footer
  legend (`G = gate iterations · R = review iterations`); widen to `Gate`/`Rev`
  if space allows.
- **Dependency view** (`TaskView.depends_on`, `api.rs:179`, currently unrendered):
  a `v` key cycles **list → tree → timeline**. Tree shows prerequisite/blocked
  structure; timeline is a lane view over scheduling order (the parallelism
  observability tool).

---

## Decisions & open questions

Locked (see SCOPE for rationale): unified `.makina/` with internal `.gitignore`
(committed config+tasks, gitignored runs+worktrees); persistent ULID run id
(RunId u64 stays the session handle); plan-scoped collision-free run slug;
config back-compat fallback to `./makina.toml`; continue-independents +
`Skipped`; ANSI-parse exchange rendering; list/tree/timeline view.

- **Audit ledger location (decided):** `.makina/runs/{run-id}/audit.jsonl` —
  per-run, gitignored. It is a run transcript and lives beside the per-run logs;
  this is a deliberate change from plan-0002's committed, slug-keyed
  `.tasks/{slug}/audit.jsonl`. (Revisit only if a committed, diff-reviewable
  audit trail is later wanted, in which case it would move to
  `.makina/tasks/{slug}/audit.jsonl`.)
- **Run-id format (decided):** ULID (sortable, timestamp-prefixed) so
  `ls runs/` is chronological.
- **Logging volume/rotation (deferred):** per-task `.log` files are unbounded
  per run; acceptable for the MVP (transient, gitignored). Rotation/retention
  is a FUTURE concern.
