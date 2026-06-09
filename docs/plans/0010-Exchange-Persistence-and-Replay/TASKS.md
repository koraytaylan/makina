# Makina Plan 0010 — Exchange Persistence & Replay

Make the on-disk run record complete and authoritative, then load and replay it
so opening a repo with previous runs shows their task list **and** full Exchange
pane — reconstructed through the same renderer used live.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the schema and load/replay flow.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).
- **This plan depends on plan 0009** for the response-segmentation behaviour in
  `ExchangeLog::append_chunk`; the replay reducer reuses it so replay renders
  identically to live.

---

## 0034 — Authoritative exchange transcript

### verify-and-harden-transcript-persistence — Prove the sink writes and lock the schema

Plan 0008 added a transcript sink in `make_sink`
(`crates/makina-core/src/orchestrator.rs`) that appends each
`Event::AgentExchange` as JSONL, but no `*_transcript.jsonl` file exists in any
current run — so first prove it fires (or fix it), then make the line schema
stable and self-describing.

**Steps:**

1. Read `make_sink` in `crates/makina-core/src/orchestrator.rs` and confirm the
   `Event::AgentExchange` interception path: it looks up `run_uid` from
   `state.runs.lock()` and appends to
   `paths::run_logs_dir(&repo_root, &run_uid)/{task_id}_transcript.jsonl`.

2. Add a test in `orchestrator.rs` (or `makina-core/tests/`) that drives a short
   run through a mock backend emitting a prompt, a thought, a tool call+update,
   two response chunks, and a turn-complete, then asserts the transcript file
   exists and every line deserialises:

   ```rust
   #[tokio::test]
   async fn transcript_is_written_and_parses() {
       // build CoreState with a temp repo_root + mock backend; open + start a run
       // … drive to completion …
       let path = paths::run_logs_dir(&repo_root, &run_uid).unwrap().join(format!("{task_id}_transcript.jsonl"));
       let body = std::fs::read_to_string(&path).expect("transcript exists");
       for line in body.lines() {
           let _ev: api::ExchangeEvent = serde_json::from_str(line).expect("line parses");
       }
   }
   ```

   If the file is missing, fix the cause in `make_sink` (e.g. the registry entry
   is created after the first event, or `run_logs_dir` is not created before the
   open) so the test passes.

3. Ensure `api::ExchangeEvent` (in `crates/makina-core/src/api.rs`) serialises as
   a self-describing tagged enum so a line decodes standalone. Confirm/add:

   ```rust
   #[derive(Debug, Clone, Serialize, Deserialize)]
   #[serde(tag = "type", rename_all = "snake_case")]
   pub enum ExchangeEvent { /* PromptSent, ResponseChunk, ThoughtChunk, ToolCall, ToolCallUpdate, TurnComplete */ }
   ```

4. Document the transcript line schema in `docs/spec/runtime-artifact-schema.md`
   (one short section: path, one JSON object per line, the `type` tag and fields,
   ordering guarantee = arrival order incl. response segments).

- **Depends on:** —
- **Done when:** `transcript_is_written_and_parses` passes; `grep -n 'serde(tag = "type"' crates/makina-core/src/api.rs` matches the `ExchangeEvent` enum; `runtime-artifact-schema.md` documents the transcript; cargo test/clippy/fmt green.

---

## 0035 — Complete run snapshot

### persist-complete-run-snapshot — Persist everything the UI shows for a finished run

`run.json` (`crates/makina-core/src/run_metadata.rs`) stores only run_uid / slug /
status / timestamps. A finished run's per-task final state and iteration counts
live only in the in-memory graph. Persist them so a closed run reconstructs from
disk.

**Steps:**

1. In `crates/makina-core/src/run_metadata.rs`, add a versioned per-task snapshot
   written alongside `run.json` (a sibling `tasks-snapshot.json`), or extend
   `RunMetadata` with a `tasks` vector — keep it additive so old files still
   load:

   ```rust
   #[derive(Debug, Clone, Serialize, Deserialize)]
   pub struct TaskSnapshot {
       pub id: String,
       pub title: String,
       pub state: crate::api::TaskState,
       #[serde(default)] pub gate_iterations: u32,
       #[serde(default)] pub review_iterations: u32,
       #[serde(default)] pub depends_on: Vec<String>,
   }
   ```

2. Write the snapshot when the run reaches a terminal status (next to where
   `record_final_status` updates the registry in `orchestrator.rs`) and on
   significant task-state changes, best-effort (warn on IO error, never abort).

3. Add a `RunSnapshot` reader that, given a run directory, returns a `RunView`
   (`api.rs`) for a finished run **without** the live registry — pulling titles
   and `depends_on` from the task-list artifact as a fallback when the snapshot
   predates this change, and the `IngestionReport` from the artifact.

4. Extend `CoreApi::runs()` (`orchestrator.rs`) so its returned list includes
   disk `RunSnapshot`s for finished runs not present in the live registry.

5. Tests:

   ```rust
   #[test]
   fn old_run_json_without_snapshot_still_loads() { /* fixture with only run.json → runs() yields a RunView */ }
   #[tokio::test]
   async fn open_finished_run_reconstructs_view() { /* fixture run dir → RunView with right states + iteration counts */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; `grep -n 'TaskSnapshot' crates/makina-core/src` matches; back-compat holds (old `run.json` loads); cargo test/clippy/fmt green.

---

## 0036 — Load and replay on open

### add-exchange-replay-reducer — One reducer for live and replayed exchanges

The live `Event::AgentExchange` handling lives inside `apply_api_event`
(`crates/makina/src/app.rs`). Factor it into a single reducer so replay cannot
diverge from live.

**Steps:**

1. In `crates/makina/src/app.rs`, find the `Event::AgentExchange { role, event, .. }`
   match inside `apply_api_event`. Extract its body into a free function:

   ```rust
   /// Apply one exchange event to a task's log. The single source of truth used
   /// by both the live event path and on-disk replay (plan 0010).
   pub fn apply_exchange_event(log: &mut ExchangeLog, role: AgentRole, event: &ExchangeEvent) {
       match event {
           ExchangeEvent::PromptSent { text }    => log.add_prompt(role, text.clone()),
           ExchangeEvent::ResponseChunk { text } => log.append_chunk(role, text.clone()),
           ExchangeEvent::ThoughtChunk { text }  => log.append_thought(role, text.clone()),
           ExchangeEvent::ToolCall { id, title, kind, status } =>
               log.start_tool(role, id.clone(), title.clone(), kind.clone(), status.clone()),
           ExchangeEvent::ToolCallUpdate { id, status, title } =>
               log.update_tool(id, status.clone(), title.clone()),
           ExchangeEvent::TurnComplete => log.complete_turn(),
       }
   }
   ```

   (Match the actual `ExchangeEvent` variant/field names — grep `enum ExchangeEvent`
   in `makina-core/src/api.rs` — and adjust arms to fit.)

2. Replace the live match body with a call to
   `apply_exchange_event(self.exchange_logs.entry(task).or_default(), role, &event)`.

3. Add a test asserting the two paths agree:

   ```rust
   #[test]
   fn replay_reducer_matches_live() {
       let events = sample_exchange_events(); // prompt, chunk, thought, tool, chunk, turn_complete
       let mut live = ExchangeLog::default();
       for (role, ev) in &events { apply_exchange_event(&mut live, *role, ev); }
       let mut replay = ExchangeLog::default();
       for (role, ev) in &events { apply_exchange_event(&mut replay, *role, ev); }
       assert_eq!(format!("{live:?}"), format!("{replay:?}"));
   }
   ```

- **Depends on:** verify-and-harden-transcript-persistence
- **Done when:** `replay_reducer_matches_live` passes; `grep -n 'pub fn apply_exchange_event' crates/makina/src/app.rs` matches and the live arm calls it; cargo test/clippy/fmt green.

### load-past-run-exchange-on-open — Read transcripts and populate the pane

**Steps:**

1. Create `crates/makina/src/replay.rs` (declare `mod replay;` in `main.rs`) with
   a loader that folds a task transcript into an `ExchangeLog`:

   ```rust
   use makina_core::api::{AgentRole, ExchangeEvent};
   use crate::app::{apply_exchange_event, ExchangeLog};

   /// Read `{task_id}_transcript.jsonl` and rebuild the task's ExchangeLog,
   /// honouring EXCHANGE_LOG_CAP (keep the tail; mark truncated).
   pub fn load_task_exchange(path: &std::path::Path, role: AgentRole) -> std::io::Result<ExchangeLog> {
       let body = std::fs::read_to_string(path)?;
       let mut log = ExchangeLog::default();
       for line in body.lines() {
           if let Ok(ev) = serde_json::from_str::<ExchangeEvent>(line) {
               apply_exchange_event(&mut log, role, &ev);
           }
       }
       Ok(log)
   }
   ```

   (The transcript carries `role` per the 0008 sink; if the persisted line does
   not include role, persist it in 0034 or pass the task's owning role here.)

2. Wire into the TUI: when a run is **selected** and its tasks have no in-memory
   exchange yet, lazily call the loader for each task (using
   `paths::run_logs_dir` via the api / a new `CoreApi::transcript_path`) and
   insert into `app.exchange_logs`. Cache by `(run_uid, task_id)` so re-selecting
   is free. Do not read transcripts for unselected runs at startup.

3. Best-effort: a missing/un-parseable transcript yields an empty log and a
   single warning, never a crash.

4. Add a test `open_finished_run_populates_exchange` that, given a fixture run
   directory containing a transcript, selects the run and asserts the focused
   task's `ExchangeLog` is non-empty and contains the expected kinds in order.

- **Depends on:** add-exchange-replay-reducer, persist-complete-run-snapshot
- **Done when:** `open_finished_run_populates_exchange` passes; `grep -n 'load_task_exchange' crates/makina/src` matches; selecting a finished run in the running TUI shows its prompts/thoughts/tools/response; cargo test/clippy/fmt green.

---

**End of plan 0010 TASKS.** When every "Done when" bullet is green, a finished run
reopened from disk shows its task list, states, iteration counts, and the full
Exchange transcript — identical to how it looked live.
