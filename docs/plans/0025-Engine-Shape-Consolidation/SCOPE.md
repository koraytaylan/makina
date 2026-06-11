# Scope — Plan 0025

> What this plan delivers, what it leaves out, and the decisions behind it.
> Findings from the full-codebase review of 2026-06-11. **This is a refactor
> plan: no behavior visible through `makina_core::api` changes.**

## Why this plan

The engine pays for two execution shapes while only one does work. The
production entrypoint `run_graph_inner` (`supervisor.rs:869–969`) hand-builds
a `DriverContext` (`supervisor.rs:931–947`) that duplicates
`Supervisor::driver_context` (`supervisor.rs:738–785`), then drives the
scheduler itself — raw futures on a `JoinSet` (`supervisor.rs:1184`). The
per-run actor tree it spawns first is ceremony: the `Supervisor` hub receives
`SetSpokes` but the run never sends it another message; the `Planner` is
spawned and never messaged ("the ref is not stored", `supervisor.rs:913–923`);
the `RunReadyTasks` ask path exists effectively only for tests (its only
senders are the six integration suites, e.g. `develop_review_loop.rs:225`,
`termination_caps.rs:288`). kameo supervision therefore supervises actors
that do no work, while the real work — the task drivers — lives in `JoinSet`
futures it cannot restart. The two-shape tax shows up as concrete defects:

- **Option-itis.** `Supervisor` carries six `Option` fields for two-phase
  init (`supervisor.rs:241–308`); `worktree_manager` is `Option` yet set
  unconditionally in `on_start` (`supervisor.rs:343–353`).
- **An early-return leak.** If the `SetSpokes` ask fails, the `?` at
  `supervisor.rs:900–909` returns before `root.kill()`
  (`supervisor.rs:952`), leaking the per-run actor tree.
- **A spoke ref that anchors nothing.** `Developer` stores
  `ActorRef<Supervisor>` (`developer.rs:82`, set at `developer.rs:130`) and
  never uses it.

Persistence is held together by a comment. Nineteen hand-placed
`ctx.persist().await` calls (between `supervisor.rs:1094` and
`supervisor.rs:2079`) are governed by "Any new graph-mutation block must be
followed by `ctx.persist().await`" (`supervisor.rs:1506–1508`) — an invariant
no compiler checks. Each call clones the whole graph and pretty-prints it
(`supervisor.rs:542–557`, `persist.rs:167`). The same copy-discipline governs
logging: seven hand-copied `tracing::info!("task state transition")` blocks
(`supervisor.rs:1644`, `:1671`, `:1813`, `:1868`, `:1895`, `:1997`, `:2039`)
with inconsistent coverage — the worktree-create `HardError`
(`supervisor.rs:1604`), reviewer-dispatch `HardError` (`supervisor.rs:1712`),
hard merge-error (`supervisor.rs:1761`), wall-clock cap
(`supervisor.rs:1321`), and `Skipped` transitions (`supervisor.rs:1256–1266`)
are all unlogged.

And load-bearing duplication is spread across the crate:

- `combine_output` is byte-identical in `gate.rs:240–253` and
  `merge.rs:363–376`. (The review also flagged `extract_json_object` as
  duplicated; that one has since been consolidated into
  `json.rs:12` — used by `interpreter.rs:781` and `roles.rs:308` — and is the
  precedent this plan extends.)
- **Three** git runners: `worktree.rs:358–375` (`run_git`),
  `merge.rs:329–352` (`run_git_raw`/`run_git_checked`), and
  `developer.rs:383` (`run_git_in`), with separate error types. Teardown
  classifies failures by sniffing stderr substrings that git localizes
  (`is_not_found_stderr`, `worktree.rs:418–430` — "no such file or
  directory", "did not match any"), and the teardown control flow at
  `worktree.rs:286–292` / `:309–313` pattern-matches only
  `GitCommandFailed`, silently swallowing `WorktreeError::Io`.
- The branch format `task/{plan_slug}--{task_id}` is formatted independently
  at `worktree.rs:198`, `worktree.rs:271`, and `supervisor.rs:1738` — where
  the driver re-derives a name its own `WorktreeHandle.branch` already
  carries. The commit message `task({id}): {title}` is duplicated in
  `developer.rs:375` and `squash_commit_message_locked`
  (`supervisor.rs:2127–2136`).
- Minor hygiene: mutation helpers silently no-op on a missing task
  (`if let Ok(task)`, `supervisor.rs:2164–2201`) while their sibling read
  helpers propagate; manual `api::TaskId(task_id.0.clone())` conversions
  (`supervisor.rs:511`, `:522`; `developer.rs:207`; `reviewer.rs:184`)
  bypass the documented `From` bridge (`api.rs:92–102`); test fixtures are
  `pub` in production code (`supervision.rs:201–263`); terminal-state docs
  omit `Skipped` (`state_machine.rs:255` doc, the `:430`/`:447` test
  names/coverage, and the `api.rs:114–121` table) even though `is_terminal`
  includes it (`state_machine.rs:246–251`).

One shape, one writer, one home for each shared fact: a big maintainability
payoff with zero api-visible behavior change.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0077–0079):

- **0077 — One engine shape.** Delete the inert per-run actor tree:
  `run_graph` stays a plain function; Developer/Reviewer turns become plain
  async calls (panic-isolated to keep task-level failure semantics); the
  `Supervisor` actor, its messages, and the per-run `Planner` spawn go away,
  taking the Option-itis and the `SetSpokes` early-return leak with them.
  Port the six `RunReadyTasks` integration suites to drive `run_graph`.
- **0078 — Single persistence writer + transition-logging choke point.**
  Replace the nineteen hand-placed `ctx.persist().await` calls with a
  dirty-generation channel feeding one debounced writer task
  (snapshot-under-lock, write-outside-lock, guaranteed final flush before
  `run_graph` returns). Move transition logging into `apply_event_locked` —
  one site, complete coverage. Make the silent mutation helpers propagate.
- **0079 — Shared infrastructure.** One `Git` runner (repo-scoped,
  `LC_ALL=C`, one error type, not-found classification in one place, the
  `Io`-swallow fixed); one `combine_output` home; a `naming` module for
  branch + commit-message formats (the driver uses its
  `WorktreeHandle.branch`); `From` impls for id conversion; test fixtures
  out of the production surface; the `Skipped` doc/test omissions fixed.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Two engine shapes; `RunReadyTasks` is test-only; duplicated `DriverContext` construction; inert Planner spawn | `0077` |
| `Supervisor` Option-itis; `SetSpokes` `?` leaks the actor tree; unused spoke→hub ref | `0077` |
| 19 comment-enforced `ctx.persist()` sites; whole-graph clone + pretty-print per call | `0078` |
| Seven hand-copied transition logs; HardError / wall-clock / Skipped transitions unlogged | `0078` |
| `increment_*`/`mark_*` helpers silently no-op on a missing task | `0078` |
| `combine_output` duplicated byte-identically | `0079` |
| Three git runners, separate error enums, localized stderr sniffing, swallowed `Io` errors in teardown | `0079` |
| Branch + commit-message formats duplicated; driver ignores `WorktreeHandle.branch` | `0079` |
| Manual `api::TaskId` conversions bypass the `From` bridge | `0079` |
| `pub` test fixtures in `supervision.rs` | `0077` (deletes the module's consumer), `0079` (residue) |
| Terminal-state docs/tests omit `Skipped` | `0079` |

## Locked decisions

- **Delete the actor tree; the seam that matters is `AgentBackend`.** Read
  before locking, as the review demanded: `RunReadyTasks` has no production
  sender (grep: only the six test suites); the Planner ref is dropped at
  spawn (`supervisor.rs:915–923`); the Developer never touches its hub ref
  (`developer.rs:82`). The alternative — making the *drivers* actors so
  supervision is real — was weighed and rejected: a kameo restart restores
  the actor, not the in-flight agent session (the session lives in the
  spawned agent process), so a restarted driver still must fail its task;
  fault tolerance already lives in the FSM + scheduler
  (continue-on-failure, `DriverGuard`). Keeping `run_graph` a plain function
  deletes ceremony without losing any real resilience.
- **Preserve panic semantics exactly.** Today an agent-turn panic surfaces
  as a failed kameo ask → task-level hard error, and a *driver* panic is the
  scheduler's only fatal arm. After 0077 each agent turn runs in its own
  `tokio::spawn`, so `JoinError::is_panic` maps back to the same task-level
  `Err(String)`. `continue_on_failure.rs` is the behavioral pin and is
  ported, not weakened.
- **Persistence is best-effort but terminal state is guaranteed.** The
  writer debounces bursts (one snapshot per quiet window, replacing 19
  full-graph clone+writes per task lifecycle) and `run_graph` awaits the
  writer's final flush *before* emitting the terminal `RunStatusChanged`
  and returning — so `.tasks/{slug}.json` is current before the
  orchestrator finalizes run metadata. A missed `mark_dirty` now degrades
  to bounded staleness (healed by the next change or the final flush), not
  silent data loss; the comment invariant becomes structural.
- **Logging moves to the transition function, not beside it.**
  `apply_event_locked` (`supervisor.rs:2105–2116`) knows the real
  `from`/`to` pair; logging there covers every arm including the five
  currently silent ones. Log lines are observability, not api — coverage
  *growing* is the point, and `supervisor_tracing_transitions.rs` is
  updated to assert the new completeness.
- **`LC_ALL=C` on every git invocation.** Stderr classification is only
  sound in one locale; the shared `Git` runner pins it (and `LANG=C`),
  documents the exact upstream messages it matches, and owns
  `is_not_found` so the next caller cannot fork the patterns.
- **No api-visible behavior change.** Same events, same persisted JSON
  schema (`persist_graph` untouched), same branch/commit formats, same
  verdict/gate semantics. Tests may be restructured; assertions about
  outcomes are preserved.

## Out of scope

- Any behavior change visible through `makina_core::api` — this plan is a
  refactor, plainly: if a test asserting engine *outcomes* must change
  (beyond harness porting), the change is wrong.
- Structured terminal outcomes / failure-reason typing — **plan 0023**
  (0014 surfaces reasons; 0023 restructures them; this plan only makes
  their landing sites cleaner).
- Merge isolation and the develop-checkout strategy — **plan 0018** (the
  `Git` facade here is plumbing; 0018 owns merge semantics).
- Scheduler policy, caps, concurrency, FSM transitions (`state_machine.rs`
  logic untouched — only its docs/tests gain `Skipped`).
- The TUI and `makina-acp` crates.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits and the
sequencing recommendation relative to plans 0018/0020/0023.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
