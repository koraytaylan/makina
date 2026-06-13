# Makina Plan 0024 — Per-Role Metrics in the Content Pane

Surface, per completed agent role-turn, **which model** answered and **how long**
the turn took — and a token-usage figure that lights up only if the backend reports
it (no estimation). The metric rides a new `Event::RoleTurnMetrics` emitted by the
Developer/Reviewer actors and renders as a line in the task-detail header next to
the existing `gate ×n · review ×m` counters.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0071 — Role-turn metrics event

### role-turn-metrics-event — Carry model + duration (+ optional usage) to the TUI

Add the metrics type and event, measure the turn wall-clock in the Developer and
Reviewer actors, resolve the model from the role assignment, and (best-effort)
read a usage object out of the ACP turn-complete metadata into `Option<UsageStats>`
without any estimation.

**Steps:**

1. In `crates/makina-core/src/api.rs`, add a public
   `struct UsageStats { input_tokens: Option<u64>, output_tokens: Option<u64> }`
   (`#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]`) and a new
   variant on the `Event` enum (after `Event::TaskIdle`):
   `RoleTurnMetrics { run: RunId, task: TaskId, role: AgentRole, model: String,
   duration_ms: u64, usage: Option<UsageStats> }`, documented as one-per-turn,
   `usage` `None` unless the backend reported counts.

2. Carry an optional usage payload off the terminal stream item. In
   `crates/makina-core/src/backend.rs`, change `ResponseEvent::TurnComplete` (the
   unit variant) to `TurnComplete { usage: Option<api::UsageStats> }`. **This is a
   non-exhaustive-match-breaking change: it breaks every `match` arm and
   construction site of `ResponseEvent::TurnComplete` across `makina-core`.** Grep
   `ResponseEvent::TurnComplete` to enumerate them and update each one:
   - **Construction sites** → `TurnComplete { usage: None }`: `backend.rs`
     (~`:399`/`532`/`563`), `orchestrator.rs:1814`, and the `backend/noop.rs` test
     backend's appends (~`noop.rs:269`; the conditional at `:247`/`:248`).
   - **Match / `matches!` arms** that ignore usage → `TurnComplete { .. }`:
     `roles.rs:577`, `interpreter.rs:754`, the `backend.rs` tests
     (~`:461`/`548`), and the `backend/noop.rs` tests
     (~`:338`/`353`/`371`/`409`/`585`).
   - **The two actor drain arms** read `usage`: `developer.rs` (~`:362`/`553`/`627`)
     and `reviewer.rs` (~`:332`) — bind `{ usage }` and thread it into the metrics
     event (step 5). Construction-only test arms pass `usage: None`.

   Keep `api::ExchangeEvent::TurnComplete` a **unit** variant — only the internal
   `backend::ResponseEvent` grows the field. The build must stay green: every
   `TurnComplete` match arm and construction site is updated.

3. In `crates/makina-acp/src/protocol.rs`, add an optional
   `usage: Option<TurnUsage>` (`#[serde(default)]`) to `PromptResult` and a
   `struct TurnUsage { input_tokens: Option<u64>, output_tokens: Option<u64> }`
   (camelCase, all `#[serde(default)]`). Carry it through
   `AcpResponseChunk::TurnComplete` (`crates/makina-acp/src/client.rs`,
   `TurnComplete(StopReason)`) so `run_turn` (`crates/makina-acp/src/backend.rs`,
   the `Ok(AcpResponseChunk::TurnComplete(_reason))` arm) maps it into
   `ResponseEvent::TurnComplete { usage }` (mapping `TurnUsage` → `api::UsageStats`).
   Absent metadata ⇒ serde default ⇒ `None`. (If threading the field through
   `client.rs` proves heavier than the MVP warrants, map `usage` to `None` at
   `run_turn` and say so in this task's Done-when; the parse stays wired but dormant
   and behaviour is identical — usage just never lights up.)

4. In `crates/makina-core/src/roles.rs`, add a free fn
   `current_model_from(caps: Option<&api::SessionCapabilities>) -> Option<String>`
   that returns the `current_value` (as a `String`) of the `model`-category
   `ConfigOptionView`, if any.

5. In `crates/makina-core/src/actors/developer.rs` `Developer::handle`, add
   `use std::time::Instant;`, capture `let turn_start = Instant::now();` immediately
   before `session.prompt(...)`, and in the `ResponseEvent::TurnComplete { usage }`
   arm — right after publishing `ExchangeEvent::TurnComplete` and before `break` —
   emit `api::Event::RoleTurnMetrics` with `role: api::AgentRole::Developer`,
   `model = self.assignment.as_ref().and_then(|a| a.model.clone())
   .or_else(|| current_model_from(session.capabilities().as_ref()))
   .unwrap_or_else(|| "(default)".into())`,
   `duration_ms = turn_start.elapsed().as_millis() as u64`, and `usage`. Apply the
   identical change in `crates/makina-core/src/actors/reviewer.rs` with
   `AgentRole::Reviewer`. The idle-timeout, error, and `None` exits emit **no**
   metrics.

6. Add tests:

   ```rust
   #[test]
   fn metrics_event_round_trips() { /* api.rs: serde round-trip RoleTurnMetrics with usage:None and usage:Some(UsageStats{Some,Some}) */ }
   #[test]
   fn metrics_event_carries_model_and_duration() { /* developer.rs: backend stream = one chunk + TurnComplete{usage:None}; assignment.model=Some("m"); drain handle; assert a RoleTurnMetrics{model:"m", duration_ms>=0, role:Developer} reached the sink */ }
   #[test]
   fn usage_is_none_when_backend_omits_it() { /* makina-acp: a PromptResult JSON without `usage` deserializes to usage:None; run_turn yields TurnComplete{usage:None} (no estimation) */ }
   ```

- **Depends on:** — (uses plan 0022's timestamp plumbing only as context; adds an
  independent per-turn duration)
- **Done when:** the three tests pass; `UsageStats` and `Event::RoleTurnMetrics`
  exist and round-trip; **the build stays green — every `match` arm and
  construction site of `ResponseEvent::TurnComplete` enumerated by grep is updated
  to the new struct-variant shape** (construction sites `{ usage: None }`,
  usage-ignoring arms `{ .. }`, the two actor arms binding `usage`), and
  `api::ExchangeEvent::TurnComplete` stays a unit variant; the Developer and
  Reviewer actors emit `RoleTurnMetrics` on turn completion with the
  assignment-resolved model and a measured `duration_ms`; the ACP backend parses a
  usage object into `Option<UsageStats>` (or maps to `None` per the documented
  fallback) with **no** estimation; cargo test/clippy/fmt green.

---

## 0072 — Render per-role metrics

### render-role-metrics — Show duration/model/tokens in the detail header

Accumulate the latest `RoleTurnMetrics` per `(task, role)` in `App` and render a
per-role line in the task-detail header — model + duration always, tokens only when
present.

**Steps:**

1. In `crates/makina/src/app.rs`, add a field to the `App` struct (near
   `exchange_logs` / `task_last_activity_tick`):
   `pub role_metrics: HashMap<(RunId, TaskId), HashMap<AgentRole, RoleTurnMetric>>`,
   and a small `pub struct RoleTurnMetric { pub model: String, pub duration_ms: u64,
   pub usage: Option<makina_core::api::UsageStats> }`. Initialise the map empty in
   the `App::new` struct literal only — it is the sole struct-constructing
   constructor; `App::with_config` delegates to `App::new` (`Self::new(...)`) and so
   inherits the empty map without a separate edit.

2. Add a match arm to the `Event` handling in `App::update` (next to the
   `Event::TaskIdle` arm): on `Event::RoleTurnMetrics { run, task, role, model,
   duration_ms, usage }`, `self.role_metrics.entry((*run, task.clone())).or_default()
   .insert(role.clone(), RoleTurnMetric { model: model.clone(), duration_ms:
   *duration_ms, usage: usage.clone() })`.

3. In `crates/makina/src/ui.rs`, add helpers `role_label(&AgentRole) -> &'static str`
   (`"developer"`/`"reviewer"`), `fmt_duration(ms: u64) -> String` (e.g. `1.8s`,
   `2m 04s`), and `role_metric_lines(app, task) -> Vec<Line<'static>>` that, for the
   focused task's `role_metrics`, builds one line per role:
   `"{role} · {model} · {duration}"`, appending `" · {in}→{out} tok"` **only** when
   `usage` is `Some` and both `input_tokens` and `output_tokens` are present
   (`Color::DarkGray`).

4. In `render_exchange_pane` (`ui.rs`), push `role_metric_lines(app, task)` into the
   detail block immediately after the `gate ×{} · review ×{}` line in **all three**
   branches (the no-log `None` branch, the empty-log branch, and the `Some(log)`
   branch) — the same three sites that already call `task_activity_indicators` — so
   the metrics appear with or without exchange text.

5. Add tests:

   ```rust
   #[test]
   fn update_records_role_metrics_per_role() { /* app.rs: feed RoleTurnMetrics for Developer then Reviewer on the same (run,task); assert role_metrics has both entries with their models */ }
   #[test]
   fn header_shows_model_and_duration() { /* ui.rs: fixture App, focused task w/ a Developer RoleTurnMetric(model "gpt-x", 1800ms, usage:None); render exchange pane to a Buffer; assert buffer contains "gpt-x" and a duration token and "developer", and does NOT contain "tok" */ }
   #[test]
   fn tokens_shown_only_when_present() { /* same fixture but usage:Some(UsageStats{input:Some(100),output:Some(40)}); render; assert buffer contains "→" and "tok" */ }
   ```

- **Depends on:** role-turn-metrics-event (and plan 0021's header layout for the
  detail-block region)
- **Done when:** the three tests pass; `App` accumulates the latest
  `RoleTurnMetrics` per `(task, role)`; the task-detail header renders
  `{role} · {model} · {duration}` for each role of the focused task and appends the
  `{in}→{out} tok` figure **only** when usage is present; cargo test/clippy/fmt
  green.

---

**End of plan 0024 TASKS.** When every "Done when" bullet is green, the content
pane tells the user which model answered each role-turn and how long it took — and,
the moment a backend starts reporting token usage, the same line lights up with the
counts without any further change.
