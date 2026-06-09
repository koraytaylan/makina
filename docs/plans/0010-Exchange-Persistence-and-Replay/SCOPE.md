# Scope — Plan 0010

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Open Makina in a repo that already has runs under `.makina/runs/` and you see a
task list — but the Exchange pane is empty and the detail is partial. Everything
the agents thought, the tools they called, and the answers they streamed are
gone the moment the TUI closes. Two prior decisions left this gap:

- Plan 0006 kept the rich exchange **in memory only** (`HashMap<TaskId,
  ExchangeLog>` in the TUI `App`), explicitly deferring persistence.
- Plan 0008 added a transcript sink in `orchestrator.rs::make_sink` that appends
  `Event::AgentExchange` as JSONL to
  `.makina/runs/{run_uid}/logs/{task_id}_transcript.jsonl`. But (a) no
  `*_transcript.jsonl` file exists in any current run, so the path is unverified
  and may not be firing, and (b) **nothing ever reads it back** — there is no
  loader and no `RunView` field to hold it.

The user's requirement is concrete: *"when we open the app in a repo where we
have previous run logs, it needs to display them — therefore logs need to
provide all the data needed for what we show on the UI."* This plan makes the
on-disk run record **complete and authoritative**, then **loads and replays** it
so a finished run reconstructs exactly as it looked live.

## In scope

Exactly the work items in [TASKS.md](TASKS.md) (workstreams 0034–0036):

- **0034 — Authoritative exchange transcript.** Verify the `make_sink` write
  actually fires (add a test that drives a run and asserts the file exists and
  parses); make the per-task JSONL the single source of truth for the pane — a
  stable, versioned, self-describing line schema that captures prompts, thoughts,
  tool lifecycle, response **segments** (matching plan 0009), and turn
  boundaries, in true order.
- **0035 — Complete run snapshot.** Ensure `run.json` / the run artifacts persist
  everything the UI renders for a finished run: the task list, each task's final
  `TaskState`, the gate/review iteration counts, and the ingestion report — so a
  closed run reconstructs without the live in-memory registry.
- **0036 — Load & replay on open.** A loader reads a run directory and rebuilds
  `RunView` + per-task `ExchangeLog` by folding the transcript through the *same*
  `ExchangeLog` reducer used live. Selecting a past run repopulates both the task
  list and the Exchange pane, rendered through the plan-0009 pipeline.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Logs do not include model exchanges at all | `0034` |
| Logs must carry all data the UI shows for a past run | `0035` |
| Opening a repo with previous runs must display them | `0036` |

## Locked decisions

- **The transcript JSONL is the single source of truth for the pane.** Live and
  replayed exchanges both flow through the same `ExchangeLog` methods
  (`add_prompt`/`append_chunk`/`append_thought`/`start_tool`/`update_tool`/
  `complete_turn`). There is no second rendering path — replay cannot diverge
  from live.
- **Depends on plan 0009.** The persisted order and the reducer must match
  0009's response segmentation, so a loaded run renders identically to a live
  one. 0009 lands first.
- **Reconstructable-first, additive snapshot.** Where the task-list artifact and
  graph already encode state, we reuse them; we only add to `run.json` what is
  otherwise unrecoverable for a finished run (final per-task state, iteration
  counts). Schema changes are additive and **versioned**.
- **Best-effort, non-fatal I/O.** Persistence and loading never abort a run or
  crash the TUI; failures degrade to a warning and an empty/partial pane,
  consistent with the existing logging philosophy.
- **Bounded, honest replay.** Replay respects `EXCHANGE_LOG_CAP`; if a transcript
  exceeds the cap we keep the tail and the UI indicates the log was truncated,
  rather than silently implying it is complete.
- **Lazy load.** A run's transcript is read when its run is selected, not for
  every run at startup, so opening a repo with many runs stays fast.

## Out of scope — deferred

- A live "re-run / replay-as-if-streaming" animation — this plan reconstructs the
  *final* state of the pane, not a timed playback.
- Cross-run search, diffing, or analytics over transcripts.
- Cost / token / timing annotations.
- Transcript rotation, compaction, or size management beyond the existing cap.
- Anything in plans 0009 (rendering), 0011 (providers), or 0012 (task list).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the schema and load/replay flow.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
