# Architecture — Plan 0025 (deltas)

> Edits in `crates/makina-core/src/actors/supervisor.rs`, `actors/
> developer.rs`, `actors/reviewer.rs`, `actors/planner.rs`, `actors/mod.rs`,
> `supervision.rs`, `worktree.rs`, `merge.rs`, `gate.rs`, `state_machine.rs`,
> `api.rs`, plus new `git.rs`, `naming.rs`, `output.rs` modules and the six
> integration suites under `crates/makina-core/tests/`. Line numbers are
> hints; locate by symbol. **No behavior visible through `api` changes.**

## 0077 — One engine shape

Today `run_graph_inner` (`supervisor.rs:869–969`) spawns
`RootSupervisor` + `Supervisor` + `Planner` (`supervisor.rs:890–923`), wires
the hub via `SetSpokes` (with the leak: `?` at `supervisor.rs:909` returns
before `root.kill()` at `supervisor.rs:952`), then ignores the tree and
drives `scheduler` itself with a hand-built `DriverContext`
(`supervisor.rs:931–947`) that duplicates `driver_context`
(`supervisor.rs:738–785`). The drivers spawn per-task Developer/Reviewer
actors (`supervisor.rs:1547–1566`) solely to `ask` them once per turn.

Edits:

- **`run_graph` is the only engine entrypoint.** `run_graph_inner` stops
  spawning any actor. `DriverContext` gains the one constructor
  (`DriverContext::new(graph, worktree_manager, config, backends, control,
  audit_registry, slugs…)`) and loses its `root`/`supervisor` fields; both
  former construction sites collapse into it.

- **Developer/Reviewer become plain async calls.** `actors/developer.rs` /
  `actors/reviewer.rs` keep their files, prompts, parsing, and the
  `commit_worktree` step, but the kameo `Actor`/`Message` impls and the
  unused `supervisor: ActorRef<Supervisor>` field (`developer.rs:82`,
  `:130`) go; the handlers become

  ```rust
  pub async fn develop(backend: &Arc<dyn AgentBackend>, assignment: Option<RoleAssignment>,
                       msg: Develop) -> Result<DevelopOutcome, String>
  pub async fn review(backend: &Arc<dyn AgentBackend>, assignment: Option<RoleAssignment>,
                      msg: Review) -> Result<ReviewVerdict, String>
  ```

  `task_driver` (`supervisor.rs:1539`) calls them through a panic shield so
  semantics are bit-identical to a failed ask:

  ```rust
  let handle = tokio::spawn(develop(…));
  let outcome = match handle.await {
      Ok(r) => r,
      Err(join) if join.is_panic() => Err(format!("developer panicked for {task_id}: …")),
      Err(join) => Err(join.to_string()),
  };
  ```

  An agent-turn panic stays a *task-level* hard error (the driver's existing
  `HardError` arms, `supervisor.rs:1602–1613`, `:1709–1720`); only a
  *driver* panic remains the scheduler's fatal arm — exactly today's split.
  `DriverGuard` (`supervisor.rs:1441–1452`) drops its `developer`/`reviewer`
  kill fields and keeps the worktree safety net.

- **Delete the `Supervisor` actor wholesale**: the struct
  (`supervisor.rs:241–308`, six `Option` fields), `SupervisorArgs`,
  `on_start` (`supervisor.rs:343–353` — `Option` set unconditionally),
  `SetSpokes` (`supervisor.rs:616–629`), `SetTaskGraph`, `RunReadyTasks`
  (`supervisor.rs:651–663`), `TaskGraphSnapshot`, `run_ready_tasks`
  (`supervisor.rs:695–727`), `driver_context`, and `restore_graph`. The
  Option-itis and the early-return leak are gone *by construction* — there
  is no tree to kill. The per-run `Planner` spawn (`supervisor.rs:915–923`)
  is deleted; `planner.rs` keeps `InterpretTaskList` for its real consumer
  (the planner-actor seam), and the `planner_interpreter` parameter of
  `run_graph` is threaded to wherever that consumer lives — unchanged
  signature, no silent drop.

- **Tests port to the function.** The six suites that drove
  `SetTaskGraph`/`SetSpokes`/`RunReadyTasks` (`develop_review_loop.rs`,
  `continue_on_failure.rs`, `concurrency.rs`, `termination_caps.rs`,
  `squash_merge.rs`, `supervisor_write_path.rs`) build an
  `Arc<Mutex<TaskGraph>>`, call `run_graph(…, RunControl::silent(), …)`, and
  assert on the returned `RunReport` + the shared graph they still hold —
  the same assertions, less harness. `actors/mod.rs`'s smoke test shrinks to
  the develop/review call contract; its star-topology module docs
  (`actors/mod.rs:3–30`) are rewritten to describe the real shape.

- **`supervision.rs` loses its last production consumer.** `RootSupervisor`,
  `RestartConfig`, and the placeholder fixtures
  (`supervision.rs:201–263`) are deleted with their tests, unless the
  parallel plans still reference them at land time — in which case the
  fixtures move under `#[cfg(test)]` and deletion is a follow-up (0079
  carries the residue check).

## 0078 — Single persistence writer + transition-logging choke point

Today: 19 `ctx.persist().await` sites (`supervisor.rs:1094`–`:2079`), each a
full graph clone (`supervisor.rs:542–557`) and pretty-print
(`persist.rs:167`), held in sync by the comment at `supervisor.rs:1506–1508`.
Seven transition logs (`supervisor.rs:1644`–`:2039`) miss five transition
classes (`supervisor.rs:1604`, `:1712`, `:1761`, `:1321`, `:1256–1266`).

Edits:

- **Dirty-generation channel.** `DriverContext` replaces `persist()` with

  ```rust
  persist_gen: tokio::sync::watch::Sender<u64>,
  fn mark_dirty(&self) { self.persist_gen.send_modify(|g| *g += 1); } // sync, no await
  ```

  All 19 `ctx.persist().await` calls become `ctx.mark_dirty()` (the seed
  write at `supervisor.rs:1094` becomes the writer's startup write).

- **One writer task**, spawned by `run_graph` beside the scheduler:

  ```rust
  async fn persistence_writer(graph: Arc<Mutex<TaskGraph>>, repo_root: PathBuf,
                              mut rx: watch::Receiver<u64>) {
      loop {
          let open = rx.changed().await.is_ok();
          if open { tokio::time::sleep(PERSIST_DEBOUNCE).await; rx.mark_unchanged(); }
          let snapshot = { graph.lock().await.clone() };       // tight, no await held
          if let Err(e) = persist_graph(&snapshot, &repo_root).await { tracing::warn!(…); }
          if !open { break; }                                   // final flush done
      }
  }
  ```

  Debounce ~100 ms: a burst of transitions costs one write. Lock discipline
  is unchanged (snapshot under the lock, write outside — the existing
  `persist()` contract, `supervisor.rs:528–541`).

- **Flush ordering vs run finalization.** After `scheduler(…)` returns,
  `run_graph` drops every `persist_gen` sender (the ctx clones are gone with
  the drivers; drop the original explicitly) and `await`s the writer's
  `JoinHandle` *before* deriving/emitting the terminal `RunStatusChanged`
  (`supervisor.rs:957–966`) and returning. The orchestrator finalizes run
  metadata only after `run_graph` returns, so `.tasks/{slug}.json` is always
  current first — terminal state is guaranteed persisted even though
  intermediate writes are debounced.

- **Transition logging in `apply_event_locked`**
  (`supervisor.rs:2105–2116`):

  ```rust
  let from = task.state;
  let next = transition(from, event).map_err(…)?;
  task.state = next;
  task.updated_at = chrono::Utc::now();
  tracing::info!(task = %task_id.0, from = ?from, to = ?next, "task state transition");
  ```

  Delete the seven hand-copied blocks. Coverage becomes complete by
  construction: worktree-create/reviewer-dispatch/merge `HardError`s,
  `WallClockCapReached`, and every `DependencyFailed`→`Skipped` (applied via
  `mark_dependents_skipped`) now log. `supervisor_tracing_transitions.rs`
  asserts the new arms.

- **Strict mutation helpers.** `increment_review_iterations_locked`,
  `increment_gate_iterations_locked`, `mark_started_locked`,
  `mark_finished_locked` (`supervisor.rs:2164–2201`) return
  `Result<(), String>` via `task_mut_locked(…)?` like their read siblings
  (`supervisor.rs:2119–2124`); callers `?`-propagate. A missing task is
  always a bug upstream — no observable change on the happy path.

## 0079 — Shared infrastructure

- **`crates/makina-core/src/git.rs`** — one runner, one error, one
  classifier:

  ```rust
  pub struct Git { repo_root: PathBuf }
  pub enum GitError { Io(std::io::Error),
                      CommandFailed { command: String, stderr: String } }
  impl Git {
      fn command(&self, args: &[&str]) -> tokio::process::Command {
          // git -C {repo_root} {args}, with LC_ALL=C + LANG=C pinned so
          // stderr classification is locale-stable.
      }
      pub async fn run_raw(&self, args: &[&str]) -> Result<Output, GitError>;
      pub async fn run(&self, args: &[&str]) -> Result<String, GitError>;  // non-zero → CommandFailed
  }
  impl GitError { pub fn is_not_found(&self) -> bool { /* the exact C-locale
      messages: "is not a working tree", branch "not found"/"does not
      exist", "did not match any file(s) known to git" */ } }
  ```

  Adopters: `worktree.rs:358–375` (`run_git` deleted;
  `WorktreeError::GitCommandFailed`/`Io` become `From<GitError>` wrappers so
  the public error surface is unchanged), `merge.rs:329–352`
  (`run_git_raw`/`run_git_checked` deleted, same treatment for
  `MergeError`), `developer.rs:383` (`run_git_in` deleted — `Git` is
  constructed on the *worktree* path there). `is_not_found_stderr`
  (`worktree.rs:418–430`) is deleted in favour of `GitError::is_not_found`,
  whose patterns are tightened to the documented C-locale strings (the
  broad "no such file or directory" survives only as the specific worktree
  message, no longer a localized guess).

- **Fix the `Io` swallow in teardown.** `WorktreeManager::remove`'s two
  checks (`worktree.rs:286–292`, `:309–313`) only match `GitCommandFailed`,
  so an `Err(Io)` falls through and the function returns `Ok(())`. Replace
  with explicit matches:

  ```rust
  match remove_result {
      Ok(_) => {}
      Err(WorktreeError::Git(e)) if e.is_not_found() => {} // goal state reached
      Err(e) => return Err(e),                             // Io etc. propagate
  }
  ```

- **`crates/makina-core/src/naming.rs`** — the single source for derived
  names:

  ```rust
  pub fn task_branch(plan_slug: &str, task_id: &str) -> String   // "task/{plan_slug}--{task_id}"
  pub fn squash_commit_message(id: &TaskId, title: &str) -> String // "task({id}): {title}"
  ```

  Callers: `worktree.rs:198` and `:271` use `task_branch`;
  `supervisor.rs:1738` stops re-deriving entirely and uses
  `worktree.branch` — the `WorktreeHandle` the driver already holds
  (`worktree.rs:237–243`); `developer.rs:375` and
  `squash_commit_message_locked` (`supervisor.rs:2127–2136`) both call
  `squash_commit_message`. Formats are byte-identical to today (unit-tested
  so a drift fails).

- **`crates/makina-core/src/output.rs`** — one `combine_output`, replacing
  the byte-identical pair (`gate.rs:240–253`, `merge.rs:363–376`), following
  the `json.rs` consolidation precedent (`json.rs:12`).

- **Use the `From` bridge.** `api::TaskId(task_id.0.clone())` →
  `task_id.into()` at `supervisor.rs:511`, `:522`, `developer.rs:207`,
  `reviewer.rs:184` (`From<&crate::task::TaskId>` exists, `api.rs:98–102`).

- **Fixtures out of the production surface.** Whatever 0077 leaves of
  `supervision.rs`: `PlaceholderWorker`/`QueryStartCount`/`TriggerCrash`
  (`supervision.rs:201–263`) are deleted with the module, or — if the module
  must outlive this plan — moved under `#[cfg(test)]`.

- **`Skipped` is terminal everywhere it's written down.**
  `legal_events`' doc (`state_machine.rs:255`) gains `Skipped`; the tests
  `terminal_states_are_done_and_failed` (`state_machine.rs:430`) and
  `terminal_states_have_no_legal_events` (`state_machine.rs:447`, iterates
  `[Done, Failed]`) are renamed/extended to cover `Skipped`; the
  `api::TaskState` table (`api.rs:114–121`) gains the `Skipped` row its enum
  already has (`api.rs:137–138`).

## Test strategy

- The six ported suites are the primary net: every existing outcome
  assertion in `develop_review_loop.rs`, `continue_on_failure.rs`,
  `concurrency.rs`, `termination_caps.rs`, `squash_merge.rs`, and
  `supervisor_write_path.rs` passes against `run_graph` unchanged.
- `panicking_agent_turn_fails_only_its_task`: a backend whose develop turn
  panics yields a `Failed` task + skipped dependents while independent tasks
  complete (the `continue_on_failure` pin, now exercising the panic shield).
- `persistence_writer_flushes_terminal_state_on_exit`: drive a run, await
  `run_graph`; `.tasks/{slug}.json` holds every terminal state before the
  function returns (inject a slow debounce to prove the final flush, not
  luck, wrote it).
- `persistence_writer_coalesces_bursts`: `persistence_writer` extracted with
  an injectable write fn; N rapid `mark_dirty`s within the debounce window
  produce one write plus the final flush.
- `transition_logging_covers_silent_arms` (extends
  `supervisor_tracing_transitions.rs`): wall-clock cap and
  dependency-skip runs now emit "task state transition" records for
  `InProgress→Failed` (wall-clock) and `New→Skipped`.
- `git_runner_pins_c_locale`: the constructed command's envs contain
  `LC_ALL=C`; `not_found_classification_matches_gone_objects_only`: real
  `git branch -D` on a missing branch classifies not-found; a bogus git
  *binary* path yields `GitError::Io`, and
  `remove_propagates_io_errors` asserts `WorktreeManager::remove` returns
  that `Err` instead of `Ok(())`.
- `branch_and_commit_formats_are_stable`: `naming::task_branch("p","t") ==
  "task/p--t"`; `squash_commit_message` matches the historical
  `task({id}): {title}` byte-for-byte.
- `terminal_states_are_done_failed_and_skipped` (renamed): `Skipped` is
  asserted terminal with no legal events.

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay
green.

## Interaction with prior plans

- **Sequencing: land AFTER 0018, 0020, and 0023 — or rebase.** Those sibling
  plans from the same review touch the same hot files: 0018 (merge
  isolation) rewrites `merge.rs`/the merge-lock section of `supervisor.rs`;
  0023 (structured terminal outcomes) retypes the failure arms this plan's
  logging choke point sits in; 0020 also lands in the supervisor.
  Refactoring under them invites churn in both directions; the
  recommendation is 0018 → 0020 → 0023 → **0025**, re-verifying this plan's
  line hints against the merged tree (every site is named by symbol for
  exactly this reason).
- **0014/0015** thread `FailureReason`/idle events through the same
  transition arms; after 0078 they have *one* place to hook
  (`apply_event_locked`) instead of seven — this plan makes theirs smaller,
  not the reverse.
- **0010's persistence contract is preserved**: same `persist_graph`, same
  schema, same atomic temp-file rename (`persist.rs:155–175`); only *when*
  writes happen changes (debounced + guaranteed final flush).
- This plan deliberately does **not** restructure `RunReport` or the
  scheduler's failure-reason strings ("wall-clock-cap-reached",
  `supervisor.rs:1347`) — plan 0023 owns those.
