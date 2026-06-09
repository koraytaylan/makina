# Architecture — Plan 0010 (deltas)

> Make the on-disk run record complete and authoritative, then load + replay it
> into the same views used live. Line numbers are hints; locate by symbol.

## Current state

```
live run ─ Event::AgentExchange ─┬─ broadcast ─ App::update ─ ExchangeLog (in-memory only)
                                 └─ make_sink (orchestrator.rs:388) ─ {task_id}_transcript.jsonl  ← write only

startup: main.rs:151  initial_runs = api.runs().await        // RunView from registry + artifact
         main.rs:152  App::new(api, initial_runs)            // exchange_logs = HashMap::new()  (app.rs:612)
```

- `make_sink` serialises each `api::ExchangeEvent` with `serde_json::to_string`
  and appends a line — but produces no files in any existing run, so step one is
  to **prove or fix** it.
- `RunMetadata` (`run_metadata.rs:23–42`) persists only `run_uid`, `run_slug`,
  `status`, `started_at`, `ended_at`. Final task states and iteration counts are
  held in the live graph/registry, not on disk in a form the TUI loads.
- `RunView`/`TaskView` (`api.rs`) have no exchange field; `App::new` starts with
  an empty `exchange_logs`.

## 0034 — Authoritative exchange transcript

- **Verify the sink fires.** Add a `makina-core` test that drives a run (mock
  backend emitting prompt + thought + tool + segmented response + turn-complete)
  and asserts `{task_id}_transcript.jsonl` exists under
  `paths::run_logs_dir(repo_root, run_uid)` and every line parses back into
  `ExchangeEvent`. If the existing path is silently dropping writes (e.g. the
  `runs.lock()` lookup misses because the registry entry is created after the
  first event, or `run_logs_dir` isn't created), fix it here.
- **Stable line schema.** `ExchangeEvent` must serialise as a self-describing,
  tagged enum so a line is decodable without external context. Confirm/add
  `#[serde(tag = "type", rename_all = "snake_case")]` (or equivalent) on
  `api::ExchangeEvent`, and prepend an optional first-line header
  `{"v":1,"task_id":…,"role":…}` or carry `role`/version per line — whichever
  keeps the reducer trivial. Document the schema in
  `docs/spec/runtime-artifact-schema.md`.
- **Order + segments.** Because plan 0009 segments responses, the event stream
  already carries chunks in true order; persisting events verbatim preserves it.
  No reordering on read.

## 0035 — Complete run snapshot

Goal: a finished run reconstructs every UI element from disk alone.

- Extend the persisted run record (either `RunMetadata` or a sibling
  `tasks-snapshot.json` next to `run.json`) with, per task: `id`, `title`,
  `final_state: TaskState`, `gate_iterations`, `review_iterations`, and
  `depends_on` (for the dependency views). Keep it **additive + versioned**;
  older `run.json` files without the field still load (the task-list artifact
  remains the fallback source for titles/deps).
- The ingestion report is already derivable from the task-list artifact; confirm
  `runs()` can rebuild it for a finished run without the registry, or persist the
  computed `IngestionReport` alongside.
- Define a `RunSnapshot` (in `makina-core`) that `CoreApi::runs()` builds from
  disk for runs not present in the live registry, producing a `RunView`
  identical in shape to a live one.

## 0036 — Load & replay on open

- **Loader.** Add `CoreApi::load_exchange(run_uid, task_id) -> ExchangeLog` (or a
  `makina/src/replay.rs` helper) that reads the task transcript line-by-line,
  deserialises each `ExchangeEvent`, and folds it through the **same**
  `ExchangeLog` mutators used by `App::update`:

  ```
  PromptSent    → add_prompt
  ResponseChunk → append_chunk          // segments exactly as live (plan 0009)
  ThoughtChunk  → append_thought
  ToolCall      → start_tool
  ToolCallUpdate→ update_tool
  TurnComplete  → complete_turn
  ```

  Factor the existing live match arm (app.rs `AgentExchange`) and the reducer so
  both call one function — guaranteeing replay == live.

- **Wiring.** When a run is selected (or in `App::new` for the initially-selected
  run), lazily populate `exchange_logs` for that run's tasks via the loader,
  honouring `EXCHANGE_LOG_CAP` (keep the tail, set a "truncated" flag the header
  can show). Cache per `(run_uid, task_id)` so re-selecting is free.

- **Snapshot path.** `api.runs()` returns live `RunView`s plus disk
  `RunSnapshot`s for finished runs, so the sidebar lists historical runs and
  selecting one shows its task list, states, report, dependency views, and — via
  the loader — its full Exchange pane.

```
disk .makina/runs/{uid}/ ──► loader ──► RunView (0035 snapshot) + ExchangeLog (0034 transcript)
                                            └────────────► same ui::render as a live run (plan 0009)
```

## Test strategy

- `transcript_is_written_and_parses` (0034): drive a run; assert file exists and
  every line round-trips to `ExchangeEvent`.
- `replay_reducer_matches_live` (0036): feed an event sequence through the live
  match arm and through the loader; assert the resulting `ExchangeLog`s are
  equal — the core guarantee that replay cannot diverge.
- `open_finished_run_reconstructs_view` (0035/0036): given a fixture run dir,
  assert `runs()` yields a `RunView` with the right task states/iterations and
  that selecting it populates a non-empty `ExchangeLog`.
- Back-compat: an old `run.json` without the new fields still loads (falls back
  to the task-list artifact).

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay green.

## Interaction with prior plans

- **0008** built the sink this plan verifies/hardens and consumes.
- **0009** defines the exchange model (segmentation + markup) that replay
  reproduces; this plan must land after it.
- **0006** ephemeral in-memory log becomes the live half of a shared reducer.

## Future work (not in this plan)

- Timed "replay as it happened" playback.
- Cross-run search/diff; cost/token accounting; transcript compaction.
