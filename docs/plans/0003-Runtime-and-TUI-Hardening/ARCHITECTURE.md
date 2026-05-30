# Architecture — Plan 0003 (deltas)

> Deltas to [`0001-Initial/ARCHITECTURE.md`](../0001-Initial/ARCHITECTURE.md)
> and plan 0002. File:line references were re-grounded against the just-merged
> plan-0002 code (develop @ `e134161`); symbol names are the stable anchors.

Five workstreams: **A. `.makina/` workspace + run identity**, **B. logging &
diagnostics**, **C. scheduler robustness**, **D. TUI presentation & views**,
**E. run lifecycle & cleanup**. B depends on A (run-id + paths); D's error pane
depends on B; E's worktree naming depends on A (run-slug + paths); the rest are
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
  stderr fed by the tracing TUI layer (B). Replaces the **in-frame** `eprintln!`
  frame-bypass: any `tracing::error!`/`tracing::warn!` raised while a ratatui
  frame is live now flows into the pane instead of stdout/stderr, so nothing
  writes outside the ratatui frame *during the live-frame region*. The three
  fatal `main.rs` `eprintln!` sites are **intentionally exempt** and stay as
  `eprintln!`: the config-load failure (`main.rs:43`, before `Tui::init()`), the
  terminal-init failure itself (`main.rs:131`), and the post-`tui.restore()`
  error print (`main.rs:143`) — none of these runs while a live ratatui frame
  exists to render into, so each must write directly to stderr. (Line numbers
  are hints; these sites are pre-`Tui::init()`, the init failure, and
  post-`tui.restore()` respectively.)
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

## E. Run lifecycle & cleanup

### Today
The ACP client kills only the **direct** child on teardown: `AcpClient::shutdown`
(`client.rs:451`) and `Drop` (`client.rs:468`) call `child.start_kill()`, and the
process is spawned with `kill_on_drop(true)` (`client.rs:487`). None of these
reach grok's **descendants** (its MCP servers / model workers), and `Drop` does
not run on an out-of-band `SIGINT`/`SIGTERM`/`SIGKILL` of the TUI — so a quit can
leave live agent processes behind (observed in the dogfood). Separately,
`WorktreeManager::create` (`worktree.rs:179`) guards against a pre-existing
worktree path (`:190`) or branch (`:199`) by returning `GitCommandFailed` "rather
than silently clobbering" — so a worktree+branch left by an interrupted run
(`task/{id}` + `.worktrees/{id}/`) makes the **next** run fail in setup, which on
current code halts the whole run.

### No-orphan shutdown
Spawn each agent as its **own process-group leader** (`process_group(0)`, Unix) so
one pgid covers grok and every descendant; on teardown send the kill to the
**negative pgid** (`killpg`, via a new `nix` dep) instead of just the child. A
process-wide registry of live agent pgids plus a `kill_all_agents()` reaper (in
`makina-acp`) is the single primitive every exit path calls: the clean `q` quit
(after cancelling in-flight runs), the TUI panic hook (`tui.rs`), and new
`SIGINT`/`SIGTERM` handlers (`main.rs`, `tokio::signal`). In-TUI Ctrl-C is already
a clean quit (`event.rs:337`); the signal handlers cover external termination.

### Plan-scoped worktrees + reclaim-on-conflict
The worktree/branch namespace becomes plan-scoped: dir
`.makina/worktrees/{plan_slug}--{task_id}` and branch `task/{plan_slug}--{task_id}`,
where `plan_slug` is the lowercased-kebab of the task-list's parent dir
(`0003-runtime-and-tui-hardening`), derived beside `run_slug` and threaded through
the same `DriverContext` sites as `run_uid`. `--` is the delimiter (kebab parts
never contain a double hyphen). Because that namespace is now unambiguously
Makina-owned transient state, `create` is made **idempotent**: on a pre-existing
slot it reclaims (`remove()` — already idempotent — then re-`add`) and recreates
**fresh off the current `base_branch`** (reset, not resume), eliminating the
stale-worktree poisoning. Reclaim is confined to the plan-scoped namespace, so it
can never touch a user's branch.

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
- **No-orphan mechanism (decided):** process-group spawn + `killpg` + a
  `kill_all_agents()` reaper wired into every exit path — not bare
  `kill_on_drop`, which leaves grok's descendants orphaned and never fires on a
  signal.
- **Worktree reclaim on conflict (decided):** Option A — **reset**. A
  pre-existing plan-scoped slot is reclaimed and recreated fresh off the current
  `base_branch`; the interrupted attempt's (never-merged) work is discarded.
  Resuming the partial worktree (Option B) was rejected: stale base, possibly
  dirty/half-written state, and coupling to FSM recovery.
- **Start-time reclamation (out of scope, by choice):** there is no separate
  startup sweep. After a hard `kill -9` that bypasses every exit hook, a stray
  worktree is reclaimed lazily by `create` on the next run of that task — so no
  poisoning persists, but a sweep is intentionally not built.

### Parallelism root-cause

**Verdict: NO real, reproducible cargo build-contention defect exists in the
current code. The prime suspect — concurrent `cargo` gate builds across
worktrees serializing on a *shared* target dir — does not occur, because each
worktree already resolves to its own private `target/` directory by default.
`sched-isolated-target-dirs` is therefore NOT triggered: there is nothing to
isolate, and this is recorded as an explicitly-accepted (non-)limitation, not a
fix to implement.**

**What was probed (real gate config, not the in-process test harness).** The
deterministic concurrency harness uses `CountingBackend` with an *empty* gate
list (`config_with_concurrency`, `crates/makina-core/tests/concurrency.rs`;
`develop_until_gates_pass` runs `run_gates` over no gates), so it can never
reproduce cargo contention. The probe instead reconstructed Makina's real layout
out-of-band: a `[workspace] members = ["crates/*"]` repo (identical to
`/Users/koraytaylan/Workspace/makina/Cargo.toml`), two git worktrees created at
`.makina/worktrees/{task_id}` (matching `paths::worktree`), each member crate
carrying a `build.rs` that sleeps ~4s so a real recompile is wall-clock
observable. Each gate was launched **exactly** as `GateRunner::run_gates`
launches it (`crates/makina-core/src/gate.rs`): `sh -c "cargo build"` with
`current_dir(worktree_path)` and the parent environment inherited verbatim. Two
gates were run concurrently and their start/end wall-clock stamps compared for
overlap.

**Measured results (cargo 1.94.0, `CARGO_TARGET_DIR` unset, no
`.cargo/config.toml` anywhere in the repo):**

1. **Where the target dir resolves.** `cargo metadata` inside each worktree
   reports `workspace_root` = the worktree itself and `target_directory` =
   `.makina/worktrees/{task_id}/target`. The two worktrees resolve to **two
   distinct** target dirs. A nested worktree is a full checkout of the whole
   tree (its own root `Cargo.toml` + `crates/`), so it is its own workspace and
   is **not** swallowed into the parent repo's workspace and does **not** share
   the parent's `target/`.
2. **Default (per-worktree target) — concurrent builds fully overlap.** Two
   concurrent `cargo build` gates: `A dur≈4.8s, B dur≈5.0s, wall≈5.0s, overlap
   window≈4.8s` — i.e. total wall-clock ≈ a single build. **No serialization.**
3. **Contrast — forcing a *shared* target dir DOES serialize.** Re-running the
   identical two gates with `CARGO_TARGET_DIR` pointed at one shared directory:
   `B dur≈4.6s, A dur≈9.0s, wall≈9.0s` (≈ B + B again). The second build blocked
   on cargo's build-directory lock until the first finished. This confirms the
   *mechanism* the suspect describes is real — but it only fires under sharing,
   which Makina does not do.

**Root cause of the "sequential appearance."** It is **not** cargo target-dir
contention (no sharing exists to contend on). The apparent sequencing in dogfood
runs is attributable to the scheduler/dispatch path and the cost of git worktree
setup + agent turns per task, not to a build-lock defect. The deterministic
overlap tests added by `sched-parallelism-instrument` /
`sched-parallelism-verify-test`
(`crates/makina-core/tests/concurrency.rs::drivers_overlap_under_concurrency_2`,
`driver_intervals_observable`) already prove the scheduler itself launches ≥2
drivers simultaneously at `concurrency = 2` (`max_observed() == 2` with a 2-party
barrier), so the scheduler is not the serializer either.

**Fix recommendation: none required.** No file needs changing for build
contention. Were target-dir sharing ever introduced (e.g. a repo-level
`.cargo/config.toml` with a `build.target-dir`, or a `CARGO_TARGET_DIR` exported
into the agent/gate environment), the contention in result (3) would appear; the
single point to fix it then would be `GateRunner::run_gates`
(`crates/makina-core/src/gate.rs`), by adding
`.env("CARGO_TARGET_DIR", <per-worktree path>)` to the
`tokio::process::Command` (or equivalently a per-worktree `target/` set at
worktree creation in `crates/makina-core/src/worktree.rs`). That is the design
that `sched-isolated-target-dirs` would implement — but it is **conditional on
this finding, and this finding does not confirm the defect**, so it remains
unbuilt by choice.

- **Cargo target-dir contention (accepted non-limitation):** worktrees at
  `.makina/worktrees/{task_id}` each get their own `target/` automatically, so
  concurrent `cargo` gates run in parallel with no build-lock serialization.
  Verified empirically (per-worktree targets → full overlap; only an artificially
  *shared* target dir serializes). No isolation code is added.
