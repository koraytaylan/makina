# Scope — Plan 0024

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

When a Developer or Reviewer turn finishes, the TUI shows the exchange text and a
`gate ×n · review ×m` counter line — but it never says **which model** answered or
**how long** the turn took. Both facts are knowable today and both are interesting
to a user watching a multi-agent run: a slow reviewer on an expensive model is
exactly the kind of thing the content pane should surface.

Three concrete observations from the code:

1. **The model is known but unshown.** Each spoke actor holds its
   `RoleAssignment` (`crates/makina-core/src/config.rs`, `RoleAssignment`), whose
   `model: Option<String>` field is threaded into the session
   (`session_config_for`, `roles.rs`) and re-advertised by the live agent as
   `SessionCapabilities` (`api.rs`). Nothing carries it back to the content pane
   after a turn.
2. **Duration is measurable but discarded.** The Developer/Reviewer stream loops
   (`developer.rs`, `reviewer.rs`) run from `session.prompt(...)` to the
   `ResponseEvent::TurnComplete` break with no wall-clock measurement; the turn's
   elapsed time is simply lost. (Plan 0022 plumbs `TaskView.started_at` /
   `finished_at` for the whole-task Gantt span — a coarser, per-task figure — but
   there is no **per-turn** duration anywhere.)
3. **Token usage is genuinely unavailable.** The ACP turn-complete result
   (`crates/makina-acp/src/protocol.rs`, `PromptResult`) carries only
   `stop_reason`; `crates/makina-acp/src/backend.rs` maps
   `AcpResponseChunk::TurnComplete(_reason)` → a **unit** `ResponseEvent::TurnComplete`
   and explicitly drops the reason. `api.rs`'s `ExchangeEvent::TurnComplete` is a
   unit variant too. So there is **no token count to show today** — and we will not
   invent one.

This plan adds a per-role-turn **metrics event** (model + duration, plus an
optional token-usage slot that stays dark until a backend actually emits it) and
renders it as a line in the task-detail header next to the existing
`gate ×n · review ×m` counters.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0071–0072):

- **0071 — Role-turn metrics event.** Add `UsageStats` and
  `Event::RoleTurnMetrics { run, task, role, model, duration_ms, usage }` to
  `api.rs`. In `developer.rs` / `reviewer.rs`, measure wall-clock around the agent
  turn, read the model from the role assignment, and emit `RoleTurnMetrics` on
  `TurnComplete`. In `makina-acp/backend.rs`, attempt to parse a usage object from
  the ACP turn-complete metadata into `Option<UsageStats>` — `None` when absent,
  **no estimation**.
- **0072 — Render per-role metrics.** In `app.rs`, accumulate the latest
  `RoleTurnMetrics` per `(task, role)`. In `ui.rs`, render a per-role line in the
  task-detail header — `developer · {model} · {duration}` — appending
  ` · {in}→{out} tok` only when `usage` is `Some`.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Model is known (`RoleAssignment.model` / `SessionCapabilities`) but never surfaced after a turn | `0071`, `0072` |
| Per-turn duration is measurable in the actor stream loops but discarded | `0071`, `0072` |
| Token usage is dropped at the ACP boundary (`PromptResult` has only `stop_reason`) — must light up only if the backend later emits it | `0071`, `0072` |

## Locked decisions

- **Duration + model now; usage as a dark slot.** Per the product owner's #1
  metrics decision: ship **duration** and **model** today. The event carries a
  `usage: Option<UsageStats>` that the UI renders **only when `Some`**. There is
  **no token estimation** anywhere — when the backend omits usage, the slot is
  simply absent.
- **Wall-clock is measured per turn, in the actor.** Duration is the elapsed time
  between dispatching `session.prompt(...)` and observing
  `ResponseEvent::TurnComplete` in the Developer/Reviewer stream loop — captured
  with `std::time::Instant`. This is the true model-turn latency, finer-grained
  than plan 0022's per-task `started_at`/`finished_at` span (which this plan does
  not touch).
- **Model resolves from the assignment, with a capabilities fallback.** The model
  string is `self.assignment.as_ref().and_then(|a| a.model.clone())`; when the
  assignment leaves it unset, fall back to the live agent's current model option
  from `session.capabilities()`, else a stable `"(default)"` placeholder. Never
  empty.
- **The usage object is read, never synthesised.** `makina-acp/backend.rs` tries
  to deserialize a usage object from the ACP turn-complete metadata into
  `Option<UsageStats>`; absence yields `None`. This requires plumbing a richer
  turn-complete signal from `backend.rs` so the actor can attach it to the event —
  but it adds **no** wire-message or protocol round-trip.
- **Render near the counters, not a new pane.** The metrics line is appended to
  the existing task-detail block in `render_exchange_pane` (`ui.rs`), beside
  `gate ×n · review ×m` and the plan-0015 idle/countdown indicators — and below
  the verbose/tool header content that plan 0021 adds to the same region. No new
  layout constraint.

## Out of scope

- **Token estimation / pricing / cost.** If the backend does not emit usage, the
  pane shows no token figure. No heuristic token counting, no $-cost model.
- **Changing `ExchangeEvent::TurnComplete` into a non-unit variant.** Usage rides
  on the new `RoleTurnMetrics` event, not on the per-chunk exchange stream.
- **Per-task aggregate duration / the Gantt span.** Plan 0022 owns
  `TaskView.started_at`/`finished_at` and the timeline; this plan adds only the
  per-turn figure and does not modify those.
- **Planner/interpreter metrics.** The planner is a deterministic interpreter
  today (no model turn; `planner.rs`); a `RoleTurnMetrics` for it is deferred until
  a model-backed interpreter exists (planner-model-mechanism, task 18). The event
  shape leaves room for it (`AgentRole` could grow), but no planner emission is
  wired here.
- **Persisting metrics across restarts.** Metrics are live-only; they are not
  added to the on-disk snapshot.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
