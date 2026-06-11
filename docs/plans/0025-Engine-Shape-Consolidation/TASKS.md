# Makina Plan 0025 — Engine Shape Consolidation

Collapse the engine to one execution shape (a plain `run_graph` over the
scheduler — the inert per-run actor tree is deleted), replace the
comment-enforced persistence discipline with a single debounced writer that
guarantees a terminal flush, move transition logging into the one function
that performs transitions, and give shared facts (git running, output
combining, branch/commit naming, id conversion, terminal-state docs) exactly
one home. **Refactor only: no behavior visible through `makina_core::api`
changes.**

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas — including the recommendation to land this plan AFTER
plans 0018/0020/0023, which touch the same files.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0077 — One engine shape

### single-engine-shape — Delete the inert actor tree; agents become calls

`run_graph_inner` spawns a Root/Supervisor/Planner tree it never uses
(`supervisor.rs:890–923`), hand-builds a `DriverContext`
(`supervisor.rs:931–947`) duplicating `driver_context`
(`supervisor.rs:738–785`), and leaks the tree if the `SetSpokes` ask fails
(`?` at `supervisor.rs:909` vs `root.kill()` at `supervisor.rs:952`).

**Steps:**

1. In `crates/makina-core/src/actors/developer.rs` / `reviewer.rs`, convert
   the `Develop`/`Review` handlers into plain
   `pub async fn develop(…) -> Result<DevelopOutcome, String>` /
   `pub async fn review(…) -> Result<ReviewVerdict, String>`; delete the
   kameo `Actor`/`Message` impls and the never-used
   `supervisor: ActorRef<Supervisor>` field (`developer.rs:82`, `:130`).
   Keep prompts, verdict parsing, `commit_worktree`, and the sink emissions
   byte-identical.

2. In `crates/makina-core/src/actors/supervisor.rs`, give `DriverContext` a
   single constructor (`DriverContext::new`) absorbing both former
   construction sites; drop its `root`/`supervisor` fields. `task_driver`
   (`supervisor.rs:1539`) calls `develop`/`review` through a
   `tokio::spawn` panic shield: `JoinError::is_panic` maps to the same
   task-level `Err(String)` a failed ask produced, so the driver's
   `HardError` arms (`supervisor.rs:1602–1613`, `:1709–1720`) and the
   scheduler's fatal-only-on-driver-panic split are unchanged.
   `DriverGuard` (`supervisor.rs:1441–1452`) loses its spoke-kill fields,
   keeps the worktree safety net.

3. Delete the `Supervisor` actor and its messages: the struct's six-`Option`
   two-phase init (`supervisor.rs:241–308`), `on_start`
   (`supervisor.rs:343–353`), `SetSpokes`, `SetTaskGraph`, `RunReadyTasks`
   (`supervisor.rs:651–663`), `TaskGraphSnapshot`, `run_ready_tasks`,
   `driver_context`, `restore_graph`. Delete the per-run `Planner` spawn
   (`supervisor.rs:915–923`); keep `run_graph`'s `planner_interpreter`
   parameter threaded to the planner seam's real consumer. Update
   `actors/mod.rs` re-exports (`actors/mod.rs:52–59`) and its star-topology
   module docs to the real shape.

4. Add a test:

   ```rust
   #[tokio::test]
   async fn panicking_agent_turn_fails_only_its_task() { /* backend panics in develop: that task Failed, dependents Skipped, independent task Done; run_graph returns Ok */ }
   ```

- **Depends on:** —
- **Done when:** the test passes; `grep -r "RunReadyTasks\|SetSpokes" src/`
  is empty; `run_graph` spawns no actor; the SetSpokes leak is structurally
  impossible (no tree exists); cargo test/clippy/fmt green.

### port-actor-harness-tests — Six suites drive `run_graph`; fixtures fall out

The actor-message path's only senders are the integration suites
(`develop_review_loop.rs:225`, `continue_on_failure.rs:350`,
`concurrency.rs:346`, `termination_caps.rs:288`, `squash_merge.rs:450`,
`supervisor_write_path.rs:182`).

**Steps:**

1. Port each suite's harness from
   `SetTaskGraph` → `SetSpokes` → `ask(RunReadyTasks)` to: build
   `Arc<Mutex<TaskGraph>>`, call
   `run_graph(graph.clone(), …, RunControl::silent(), …)`, assert on the
   returned `RunReport` and on the shared graph the test still holds
   (replacing `TaskGraphSnapshot` reads). Every existing outcome assertion
   is preserved verbatim.

2. Shrink the `actors/mod.rs` smoke test to the `develop`/`review` call
   contract (NoopBackend canned responses → outcome/verdict), dropping the
   actor-spawn ceremony.

3. Delete `supervision.rs` (RootSupervisor, `RestartConfig`, and the `pub`
   fixtures `PlaceholderWorker`/`QueryStartCount`/`TriggerCrash`,
   `supervision.rs:201–263`) now that its last consumer is gone — or, if a
   parallel plan still references it at land time, gate the fixtures under
   `#[cfg(test)]` and file the deletion as follow-up (see 0079's residue
   check).

- **Depends on:** single-engine-shape
- **Done when:** all six suites pass against `run_graph` with their outcome
  assertions intact; `continue_on_failure` still proves a panicked turn
  doesn't halt the run; no `pub` test fixture remains reachable from
  production code; cargo test/clippy/fmt green.

---

## 0078 — Single persistence writer + transition-logging choke point

### dirty-flag-persistence-writer — One debounced writer, guaranteed final flush

Nineteen hand-placed `ctx.persist().await` calls
(`supervisor.rs:1094`–`:2079`) each clone + pretty-print the whole graph
(`supervisor.rs:542–557`, `persist.rs:167`), held in sync only by the
comment at `supervisor.rs:1506–1508`.

**Steps:**

1. In `crates/makina-core/src/actors/supervisor.rs`, add
   `persist_gen: watch::Sender<u64>` to `DriverContext` and a sync
   `mark_dirty()` (`send_modify(|g| *g += 1)`); replace all 19
   `ctx.persist().await` sites with `ctx.mark_dirty()` and delete
   `persist()`.

2. Add `persistence_writer(graph, repo_root, rx)` (see ARCHITECTURE for the
   loop): startup write (replaces the seed persist at `supervisor.rs:1094`),
   ~100 ms debounce per burst, snapshot-under-lock / write-outside-lock
   (the existing discipline, `supervisor.rs:528–541`), one final write when
   the channel closes. Take the write call as an injectable async fn so the
   coalescing test can count invocations.

3. In `run_graph`, spawn the writer beside the scheduler; after
   `scheduler(…)` returns, drop the sender and `await` the writer handle
   **before** emitting the terminal `RunStatusChanged`
   (`supervisor.rs:957–966`) and returning — `.tasks/{slug}.json` is current
   before the orchestrator finalizes run metadata. Rewrite the comment
   invariant at `supervisor.rs:1506–1508` to describe `mark_dirty` +
   guaranteed terminal flush.

4. Add tests:

   ```rust
   #[tokio::test]
   async fn persistence_writer_flushes_terminal_state_on_exit() { /* slow debounce; after run_graph returns the file already holds every terminal state */ }
   #[tokio::test]
   async fn persistence_writer_coalesces_bursts() { /* N mark_dirty within the window → 1 write + final flush, counted via the injected write fn */ }
   ```

- **Depends on:** single-engine-shape
- **Done when:** both tests pass; `grep -c "ctx.persist()" supervisor.rs`
  is 0; `supervisor_write_path.rs` still proves the on-disk file reaches
  the same terminal content; cargo test/clippy/fmt green.

### transition-log-choke-point — Log in `apply_event_locked`; helpers propagate

Seven hand-copied `tracing::info!("task state transition")` blocks
(`supervisor.rs:1644`, `:1671`, `:1813`, `:1868`, `:1895`, `:1997`, `:2039`)
miss the worktree-create/reviewer-dispatch/merge `HardError`s
(`supervisor.rs:1604`, `:1712`, `:1761`), the wall-clock cap
(`supervisor.rs:1321`), and `Skipped` (`supervisor.rs:1256–1266`).

**Steps:**

1. In `apply_event_locked` (`supervisor.rs:2105–2116`), capture `from`
   before the transition and emit the single
   `tracing::info!(task, from, to, "task state transition")` after it;
   delete the seven duplicated blocks.

2. Make the silent mutation helpers strict: `increment_*_locked` /
   `mark_started_locked` / `mark_finished_locked`
   (`supervisor.rs:2164–2201`) return `Result<(), String>` via
   `task_mut_locked(…)?` like their read siblings; `?`-propagate at call
   sites.

3. Extend `tests/supervisor_tracing_transitions.rs` to assert the newly
   covered arms:

   ```rust
   #[tokio::test]
   async fn transition_logging_covers_silent_arms() { /* wall-clock-cap run logs InProgress→Failed; a failed dep's dependents log →Skipped; reviewer-dispatch hard error logs InReview→Failed */ }
   ```

- **Depends on:** dirty-flag-persistence-writer
- **Done when:** the test passes; exactly one "task state transition" emit
  site exists in `supervisor.rs`; every `apply_event_locked` call is logged
  with its real from/to; cargo test/clippy/fmt green.

---

## 0079 — Shared infrastructure

### git-runner-facade — One `Git` runner, C locale, honest teardown errors

Three git runners (`worktree.rs:358–375`, `merge.rs:329–352`,
`developer.rs:383`) with separate error types; localized stderr sniffing
(`worktree.rs:418–430`); teardown swallows `WorktreeError::Io`
(`worktree.rs:286–292`, `:309–313`).

**Steps:**

1. Add `crates/makina-core/src/git.rs`: `Git { repo_root }` with
   `run_raw`/`run` building `git -C {repo_root} {args}` with `LC_ALL=C` +
   `LANG=C` pinned; `GitError { Io, CommandFailed { command, stderr } }`;
   `GitError::is_not_found()` matching the exact C-locale messages
   (documented inline) that today's `is_not_found_stderr` guessed at.

2. Adopt it in `worktree.rs` (delete `run_git`; wrap `GitError` into
   `WorktreeError` via `From` so callers keep matching the same failure
   classes — invalid-id / git-failed / io), `merge.rs` (delete
   `run_git_raw`/`run_git_checked`, same for `MergeError`), and
   `developer.rs` (delete `run_git_in`; construct `Git` on the worktree
   path).

3. Fix `WorktreeManager::remove`: replace the two `if let
   Err(GitCommandFailed…)` checks with explicit matches —
   `Ok` | not-found → continue, **any other `Err` (including `Io`) →
   propagate**.

4. Add tests:

   ```rust
   #[test]
   fn git_runner_pins_c_locale() { /* constructed Command env contains LC_ALL=C */ }
   #[tokio::test]
   async fn remove_propagates_io_errors() { /* an Io-class failure from remove() returns Err, not Ok(()) */ }
   #[tokio::test]
   async fn not_found_classification_matches_gone_objects_only() { /* branch -D on a missing branch → is_not_found; a real failure → not */ }
   ```

- **Depends on:** —
- **Done when:** the tests pass; one git-spawn site exists in makina-core
  (grep `Command::new("git")` → `git.rs` only, plus test helpers); the
  worktree suite (`tests/worktree.rs`) is still green; cargo
  test/clippy/fmt green.

### naming-and-output-helpers — One home per derived fact

Branch format triplicated (`worktree.rs:198`, `:271`, `supervisor.rs:1738` —
where the driver already holds `worktree.branch`); commit message duplicated
(`developer.rs:375`, `supervisor.rs:2127–2136`); `combine_output`
byte-identical twice (`gate.rs:240–253`, `merge.rs:363–376`); manual id
conversions bypass the bridge (`supervisor.rs:511`, `:522`,
`developer.rs:207`, `reviewer.rs:184` vs `api.rs:92–102`).

**Steps:**

1. Add `crates/makina-core/src/naming.rs` with `task_branch(plan_slug,
   task_id)` and `squash_commit_message(id, title)`; adopt in
   `worktree.rs:198`/`:271`, `developer.rs:375`, and
   `squash_commit_message_locked`. At `supervisor.rs:1738`, use
   `worktree.branch` from the driver's `WorktreeHandle` instead of
   re-deriving.

2. Add `crates/makina-core/src/output.rs` with the one `combine_output`
   (doc covers both gate- and merge-output semantics); delete both copies
   (precedent: `json.rs:12`).

3. Replace the four manual `api::TaskId(task_id.0.clone())` constructions
   with `.into()` through the documented `From` impls.

4. Add a test:

   ```rust
   #[test]
   fn branch_and_commit_formats_are_stable() { /* task_branch("p","t") == "task/p--t"; squash_commit_message == "task({id}): {title}" byte-for-byte */ }
   ```

- **Depends on:** —
- **Done when:** the test passes; each format string exists exactly once in
  `src/` (grep `task/{` and `task({`); existing branch/commit assertions in
  `tests/squash_merge.rs` and `tests/worktree.rs` are untouched and green;
  cargo test/clippy/fmt green.

### skipped-docs-and-fixtures — Close the documentation drift

`Skipped` is terminal in code (`state_machine.rs:246–251`) but missing from
the `legal_events` doc (`state_machine.rs:255`), the terminal-state tests
(`state_machine.rs:430`, `:447`), and the `api::TaskState` table
(`api.rs:114–121`, vs the variant at `api.rs:137–138`).

**Steps:**

1. Fix the `legal_events` doc to name `Done`, `Failed`, **and `Skipped`**.

2. Rename/extend the tests: `terminal_states_are_done_and_failed` →
   `terminal_states_are_done_failed_and_skipped` (assert
   `is_terminal(Skipped)`); `terminal_states_have_no_legal_events` iterates
   `[Done, Failed, Skipped]`.

3. Add the `Skipped` row to the `api::TaskState` state-meanings table.

4. Residue check from 0077: confirm no `pub` test fixture
   (`PlaceholderWorker`/`QueryStartCount`/`TriggerCrash`) remains reachable
   from production code; if `supervision.rs` survived for a parallel plan,
   move the fixtures under `#[cfg(test)]` here.

- **Depends on:** port-actor-harness-tests
- **Done when:** the renamed tests pass and cover `Skipped`; the api table
  documents all seven states; `grep -rn "PlaceholderWorker" src/` hits only
  `cfg(test)` code or nothing; cargo test/clippy/fmt green.

---

**End of plan 0025 TASKS.** When every "Done when" bullet is green, the
engine has one shape (a function over a scheduler, with the `AgentBackend`
trait as its seam), persistence is a single writer whose terminal flush is
guaranteed rather than comment-enforced, every state transition is logged at
its one choke point, and git running, naming, output combining, id
conversion, and terminal-state documentation each live in exactly one place
— with zero change observable through the api.
