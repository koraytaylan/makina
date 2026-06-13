# Architecture — Plan 0024

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). This plan touches `makina-core` (`api.rs`, the Developer/Reviewer
> actors), `makina-acp` (`backend.rs`), and the `makina` TUI (`app.rs`, `ui.rs`).

## Current shape (what exists)

- **The turn boundary lives in the actors.** Both `crates/makina-core/src/actors/
  developer.rs` (`Developer::handle` for `Develop`) and `.../reviewer.rs`
  (`Reviewer::handle` for `Review`) call `session.prompt(Prompt::new(prompt_text))`
  and then drain a `ResponseStream` in a `loop`, breaking on
  `ResponseEvent::TurnComplete`. The wall-clock between the `prompt` call and that
  break is the turn duration — currently unmeasured.
- **The model is held by the actor.** Each actor struct stores
  `assignment: Option<RoleAssignment>` (`developer.rs` `Developer`, `reviewer.rs`
  `Reviewer`); `RoleAssignment::model: Option<String>` (`config.rs`). It is passed
  to `session_config_for(...)` (`roles.rs`) when the session opens, and the live
  agent re-advertises its model set via `SessionCapabilities` (surfaced as
  `Event::SessionCapabilities`, `api.rs`).
- **`run`/`task`/`sink` are in the message.** The `Develop`/`Review` messages carry
  `run: api::RunId`, `sink: EventSink`, `idle_secs: Option<u64>`; the handler
  builds `task_id` from `msg.task.id`. The actor already publishes
  `api::Event::AgentExchange { … }` and `api::Event::TaskIdle { … }` through
  `(msg.sink)(…)`.
- **ACP drops turn metadata.** `crates/makina-acp/src/backend.rs` `run_turn`
  matches `Ok(AcpResponseChunk::TurnComplete(_reason))` and forwards a unit
  `ResponseEvent::TurnComplete`, dropping the `StopReason`. The reason comes from
  `crates/makina-acp/src/protocol.rs` `PromptResult { stop_reason: StopReason }` —
  which has **no** usage field. `api.rs`'s `ExchangeEvent::TurnComplete` and
  `backend.rs`'s `ResponseEvent::TurnComplete` are both unit variants.
- **The TUI consumes events in `app.rs`.** `App::update` → the `Event` match arm
  (the `Event::AgentExchange` / `Event::TaskIdle` block near `app.rs:1623`) folds
  live events into `App`. The exchange/detail header is rendered by
  `render_exchange_pane` (`ui.rs`, ~`ui.rs:789`), which already builds a
  `gate ×{} · review ×{}` line in **three** branches (no-log, empty-log, and the
  populated-`Some(log)` branch) and calls the `task_activity_indicators(app, task)`
  helper (plan 0015) for the idle/countdown spans.

## 0071 — Role-turn metrics event

Edits in `crates/makina-core/src/api.rs`,
`crates/makina-core/src/backend.rs`, `crates/makina-acp/src/backend.rs`, and the
Developer/Reviewer actors.

### Event + type in `api.rs`

Add the usage struct and the event variant next to the existing `Event` arms
(`api.rs:629`, after `Event::TaskIdle`):

```rust
/// Token usage for a single agent turn, when the backend reports it.
///
/// Both fields are `Option` because a backend may emit one count without the
/// other. Makina never *estimates* these — an absent count stays `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageStats {
    /// Prompt/input tokens consumed by the turn, if reported.
    pub input_tokens: Option<u64>,
    /// Completion/output tokens produced by the turn, if reported.
    pub output_tokens: Option<u64>,
}

// … within `pub enum Event { … }`:

/// Metrics for one completed agent role-turn (developer or reviewer).
///
/// Emitted once per turn on `TurnComplete` so the TUI can show which model
/// answered and how long it took. `usage` lights up only if the backend
/// reported token counts; it is `None` otherwise (no estimation).
RoleTurnMetrics {
    /// The Run the turn belongs to.
    run: RunId,
    /// The task whose agent took the turn.
    task: TaskId,
    /// Which role took the turn (Developer or Reviewer).
    role: AgentRole,
    /// The model that answered (resolved from the role assignment, with a
    /// capabilities fallback). Never empty.
    model: String,
    /// Wall-clock duration of the turn, in milliseconds.
    duration_ms: u64,
    /// Token usage, when the backend reported it.
    usage: Option<UsageStats>,
},
```

`UsageStats` derives `Serialize`/`Deserialize` so it can ride the existing event
channel; `Event` already derives them.

### Carry usage out of the ACP boundary

`ResponseEvent::TurnComplete` (`crates/makina-core/src/backend.rs:226`) is a unit
variant. This plan gives it an optional payload field,
`TurnComplete { usage: Option<api::UsageStats> }`, so the actor can read `usage`
directly off the terminal item.

> **Blast radius — this is a non-exhaustive-match-breaking change.** Turning the
> unit variant `ResponseEvent::TurnComplete` into a struct variant breaks **every**
> `match` arm and construction site of it across `makina-core`. Grep
> `ResponseEvent::TurnComplete` to enumerate them and update each: the definition +
> Debug/serialize tests in `backend.rs` (~`backend.rs:399`/`461`/`532`/`548`/`563`),
> the drain loops in the actors `developer.rs` (~`:362`/`553`/`627`) and
> `reviewer.rs` (~`:332`), the interpreter drain loop `interpreter.rs:754` and the
> session-config drain in `roles.rs:577`, the `orchestrator.rs:1814` construction,
> and the `backend/noop.rs` test backend's appends/matches
> (~`noop.rs:247`/`248`/`269`/`338`/`353`/`371`/`409`/`585`). Construction sites
> become `TurnComplete { usage: None }`; match arms that ignore usage become
> `TurnComplete { .. }`; only the two actor arms read `usage`. **Keeping the build
> green — all `TurnComplete` match arms and construction sites updated — is an
> explicit Done-when** (same spirit as plan 0014's `failure_reason` blast-radius
> note). `api::ExchangeEvent::TurnComplete` stays a **unit** variant — only the
> internal `backend::ResponseEvent` grows the field.

In `crates/makina-acp/src/backend.rs` `run_turn`, the
`Ok(AcpResponseChunk::TurnComplete(_reason))` arm currently drops the reason.
Extend the ACP `PromptResult` (`crates/makina-acp/src/protocol.rs:420`) with an
optional, untyped-tolerant usage object — e.g.

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
    pub stop_reason: StopReason,
    /// Optional token usage, when the agent reports it under `_meta`/`usage`.
    /// Absent for agents that don't emit it (the MVP case).
    #[serde(default)]
    pub usage: Option<TurnUsage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnUsage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
}
```

and carry it through `AcpResponseChunk::TurnComplete` (`client.rs:232`,
`TurnComplete(StopReason)`) so `run_turn` can map it into
`ResponseEvent::TurnComplete { usage: Option<api::UsageStats> }`. When the field is
absent the serde default makes it `None` — **no estimation, no failure**. If
threading the field through `client.rs` is heavier than warranted for the MVP, the
fallback is to map `usage` to `None` at `run_turn` and leave the parse wired but
dormant; the actor and UI still compile and behave identically (usage simply never
lights up). State which path the task takes in its "Done when".

### Measure + emit in the actors

In `developer.rs` `Developer::handle`, capture the start instant immediately before
`session.prompt(...)` and emit on the `TurnComplete` break:

```rust
let turn_start = std::time::Instant::now();
let stream = match session.prompt(Prompt::new(prompt_text)).await { /* … */ };
// … drain loop …
Some(Ok(ResponseEvent::TurnComplete { usage })) => {
    (msg.sink)(api::Event::AgentExchange {
        run: msg.run,
        task: task_id.clone(),
        role: api::AgentRole::Developer,
        event: api::ExchangeEvent::TurnComplete,
    });
    let model = self
        .assignment
        .as_ref()
        .and_then(|a| a.model.clone())
        .or_else(|| current_model_from(session.capabilities()))
        .unwrap_or_else(|| "(default)".to_string());
    (msg.sink)(api::Event::RoleTurnMetrics {
        run: msg.run,
        task: task_id.clone(),
        role: api::AgentRole::Developer,
        model,
        duration_ms: turn_start.elapsed().as_millis() as u64,
        usage,
    });
    break;
}
```

`reviewer.rs` gets the identical change with `AgentRole::Reviewer`. `current_model_from`
is a small free fn that reads the current `model`-category `ConfigOptionView`'s
`current_value` out of `SessionCapabilities` (returns `Option<String>`); place it in
`roles.rs` (shared) so both actors use it. `std::time::Duration` is already imported
in both actors; add `std::time::Instant`.

> The idle-timeout / error / `None` exits do **not** emit `RoleTurnMetrics` — a
> turn that never completed has no completion metric.

## 0072 — Render per-role metrics

Edits in `crates/makina/src/app.rs` and `crates/makina/src/ui.rs`.

### Accumulate in `app.rs`

Add a per-`(RunId, TaskId)`-per-role store to `App` (alongside `exchange_logs`,
`task_last_activity_tick`, … near `app.rs:687`):

```rust
/// Latest per-role turn metrics, keyed by (run, task) then role.
///
/// Updated on every `Event::RoleTurnMetrics`; the content pane renders the
/// most recent metric for each role of the focused task.
pub role_metrics: HashMap<(RunId, TaskId), HashMap<AgentRole, RoleTurnMetric>>,
```

where `RoleTurnMetric` is a tiny local view holding `model: String`,
`duration_ms: u64`, `usage: Option<makina_core::api::UsageStats>` (or store the
event fields directly). Initialise the map empty in the `App::new` struct literal
only — it is the sole struct-constructing constructor, and `App::with_config`
delegates to it via `Self::new(...)`, so no separate edit is needed there. Add an
arm to the `Event` match in `App::update` (next to the `Event::TaskIdle` arm,
`app.rs:1638`):

```rust
Event::RoleTurnMetrics { run, task, role, model, duration_ms, usage } => {
    self.role_metrics
        .entry((*run, task.clone()))
        .or_default()
        .insert(role.clone(), RoleTurnMetric {
            model: model.clone(),
            duration_ms: *duration_ms,
            usage: usage.clone(),
        });
}
```

### Render in `ui.rs`

Add a helper mirroring `task_activity_indicators` (`ui.rs:730`) that builds the
metric spans for the focused task:

```rust
/// Per-role metric lines for the focused task's detail header:
/// `developer · {model} · {dur}` (+ ` · {in}→{out} tok` only when usage Some).
fn role_metric_lines(app: &App, task: &makina_core::api::TaskView) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let Some(run) = app.selected_run() else { return lines };
    let Some(by_role) = app.role_metrics.get(&(run.id, task.id.clone())) else { return lines };
    for role in [AgentRole::Developer, AgentRole::Reviewer] {
        if let Some(m) = by_role.get(&role) {
            let mut text = format!("{} · {} · {}", role_label(&role), m.model, fmt_duration(m.duration_ms));
            if let Some(u) = &m.usage {
                if let (Some(i), Some(o)) = (u.input_tokens, u.output_tokens) {
                    text.push_str(&format!(" · {i}→{o} tok"));
                }
            }
            lines.push(Line::from(Span::styled(text, Style::default().fg(Color::DarkGray))));
        }
    }
    lines
}
```

`role_label(&AgentRole) -> &'static str` (`"developer"` / `"reviewer"`) and
`fmt_duration(ms) -> String` (e.g. `1.8s`, `2m 04s`) are small local helpers.
Push these lines into the detail block right after the `gate ×{} · review ×{}` line
in **all three** branches of `render_exchange_pane` (the no-log, empty-log, and
`Some(log)` branches) — the same three places `task_activity_indicators` is already
called — so the metrics show whether or not there is exchange text yet. Usage spans
are appended **only** when `usage` is `Some` and both counts are present.

## Testing notes

- **0071** (`api.rs` / actors): a unit test on `api.rs` asserts the event round-trips
  (serde) with `usage: None` and with `usage: Some(UsageStats { … })`. An actor test
  (mirroring `developer.rs`'s existing stream tests) drives a backend whose stream
  yields a chunk then `TurnComplete`, captures sink events, and asserts a
  `RoleTurnMetrics` with the assignment's model and a `duration_ms` ≥ 0; a second
  asserts `usage` is `None` when the backend's `TurnComplete` carries no usage. Use
  `tokio::time` virtual time if a known nonzero duration is needed.
- **0072** (`app.rs` / `ui.rs`): an `app.rs` test feeds two `RoleTurnMetrics`
  (developer + reviewer) through `App::update` and asserts `role_metrics` holds
  both. A `ui.rs` render test builds a fixture `App` with a focused task carrying a
  metric (model + duration, `usage: None`), renders the exchange pane to a `Buffer`,
  and asserts the buffer contains the model string and a duration token but **no**
  `tok` substring; a second test adds `usage: Some(...)` and asserts `→` and `tok`
  appear.

`cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`
stay green throughout.

## Interaction with prior plans

- **Plan 0022** plumbs `TaskView.started_at`/`finished_at` (per-task Gantt span);
  this plan adds an independent **per-turn** `duration_ms` and does not touch those.
- **Plan 0021** adds verbose-mode thought/tool content to the same task-detail
  header region; the metric lines render below that content in the detail block.
- **Plan 0015** (merged) supplied `task_activity_indicators` and the tick-based
  idle/countdown indicators that the metric lines sit beside.
- **Plan 0011** shipped the `RoleAssignment` model selection (`config.rs`) and the
  `SessionCapabilities` plumbing this plan reads the model from.
- **Plan 0017** adds the `FailureKind` retry surface; an incomplete turn emits no
  `RoleTurnMetrics`, so a failed/retried turn simply shows the last *completed*
  turn's metric (or none).
