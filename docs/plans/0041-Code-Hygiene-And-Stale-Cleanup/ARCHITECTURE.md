# Architecture — Plan 0041 (deltas)

> The concrete deltas. This plan touches
> `crates/makina-core/src/actors/developer.rs`,
> `crates/makina-core/src/actors/reviewer.rs`,
> `crates/makina-core/src/actors/agent_turn.rs`,
> `crates/makina-core/src/actors/mod.rs`,
> `crates/makina-core/src/gate.rs`, `crates/makina-core/src/merge.rs`,
> `crates/makina-core/src/cmd_output.rs`, `crates/makina-core/src/lib.rs`,
> `crates/makina-core/src/run_metadata.rs`,
> `crates/makina-core/src/persist.rs`,
> `crates/makina-core/src/state_machine.rs`,
> `crates/makina-core/src/orchestrator.rs`,
> `crates/makina/src/placeholder.rs`, `crates/makina/src/app.rs`,
> `crates/makina/src/ui.rs`, `crates/makina/src/event.rs`,
> `crates/makina/src/settings_validation.rs`, and `crates/makina/src/lib.rs`.
> Line numbers are hints; locate by symbol.

## 0001 — Shared-Helper-Extraction

Today the Developer actor (`crates/makina-core/src/actors/developer.rs:289–417`)
and the Reviewer actor (`crates/makina-core/src/actors/reviewer.rs:259–386`) each
implement an identical idle-timeout watchdog loop: `loop { tokio::time::timeout(idle_secs, events.next()).await }`
→ dispatch `ThoughtChunk` / `ToolCall` / `TextChunk` to the sink, accumulate
output, break on `TurnComplete`, then emit `RoleTurnMetrics`. The two sites differ
only in the role name and error type — roughly 130 duplicated lines. Separately,
`combine_output` is defined twice with identical bodies and identical unit tests:
at `crates/makina-core/src/gate.rs:241` and `crates/makina-core/src/merge.rs:482`.
A reader encounters both patterns twice with no apparent reason for the repetition,
and any fix to one site must be mirrored in the other.

**Edits:**

**Extract `drain_agent_turn` into a shared actor helper.** Create
`crates/makina-core/src/actors/agent_turn.rs` (declared `mod agent_turn;` in
`crates/makina-core/src/actors/mod.rs`) holding a `pub(crate)` helper that owns the
drain loop verbatim:

```rust
/// Drain an agent response stream until `TurnComplete`, forwarding chunks and metrics.
/// Handles the idle timeout, accumulates the turn's text, and emits a `RoleTurnMetrics`
/// event. Extracted verbatim from the Developer/Reviewer loops; the two roles differ
/// only in `role` and the role error enum, so this is the single source of truth for both.
pub(crate) async fn drain_agent_turn<S: AgentSession + ?Sized>(
    session: &mut S,
    events: &mut ResponseStream,
    role: api::AgentRole,
    task_id: TaskId,
    idle_secs: Option<u64>,
    sink: &(dyn Fn(api::Event) + Send),
    run: RunId,
    assignment: Option<&RoleAssignment>,
) -> Result<(String, Option<api::UsageStats>), DrainError> {
    // .. idle-timeout loop body, lifted from developer.rs:289–417 ..
}
```

The return is `(String, Option<api::UsageStats>)` — the accumulated response text and
the turn's usage (the `usage` field of `ResponseEvent::TurnComplete`, `backend.rs:245`).
The error is a NEW shared `DrainError` enum (e.g. `{ IdleTimeout { idle_secs }, Stream(String),
EndedUnexpectedly }`) defined in `agent_turn.rs`, because the two actors surface DISTINCT
error enums — `DeveloperError` (`developer.rs:70`) and `ReviewerError` (`reviewer.rs:67`)
— so one concrete fn cannot return both. (The names `TokenUsage` / `DeterminismError`
do NOT exist in the crate.) Both `developer.rs:289–417` and `reviewer.rs:259–386` replace
their inline loop with a `drain_agent_turn(..)` call, bind `(output, usage)`, map any
`DrainError` into their own `DeveloperError`/`ReviewerError`, and continue with their
respective branch-commit / verdict-parse logic. New unit tests drive the existing
`crate::backend::noop::NoopBackend` session seam (already used by the actor tests at
`developer.rs:841`/`857`) rather than a new mock.

**Extract `combine_output` into a shared command-output module.** Create
`crates/makina-core/src/cmd_output.rs` (declared `mod cmd_output;` in
`crates/makina-core/src/lib.rs`) and move the function there once:

```rust
/// Merge stdout and stderr into a single formatted string, labeling the stderr block.
/// Identical to the bodies previously inlined in gate.rs and merge.rs; the unit test
/// moves here with it so coverage is preserved at the single definition site.
pub(crate) fn combine_output(stdout: &[u8], stderr: &[u8]) -> String {
    // .. body from gate.rs:241 / merge.rs:482 (identical) ..
}
```

Delete both inline definitions and their unit tests; add
`use crate::cmd_output::combine_output;` to `gate.rs` and `merge.rs`.

**Properties that make this safe:**

- `drain_agent_turn` is lifted verbatim from the Developer loop and the Reviewer
  loop is byte-for-byte equivalent, so its behavior is identical to both inline
  loops today — idle timeout, side-channel forwarding, and `RoleTurnMetrics`
  emission are preserved exactly.
- `combine_output` was identical in both sources, so moving it to one module
  preserves semantics and the surviving unit test still exercises the same logic.
- Both helpers are `pub(crate)` internal utilities; no public API or supervisor
  contract changes.
- Existing Developer/Reviewer/gate/merge tests exercise the helpers indirectly and
  stay green; new unit tests on the helpers keep `cargo test` / clippy / fmt clean.

## 0002 — Atomicity-And-Doc-Fixes

Today `write_run_metadata` (`crates/makina-core/src/run_metadata.rs:173–185`) writes
`run.json` in a single `tokio::fs::write` call straight to the destination, so a
crash mid-write can leave a truncated file. By contrast `persist_graph`
(`crates/makina-core/src/persist.rs:173–205`) writes atomically: it writes to a
`.tmp` file then `tokio::fs::rename`s it onto the canonical path (same filesystem →
atomic). Two stale doc comments also drift from the code: the module/test docs in
`crates/makina-core/src/state_machine.rs:325–336` and `:412–415` still cite
"19 legal / 65 illegal / 84 pairs" (7×12) even though plan 0017 added
`RetryRequested` and `DependencyReset`, taking the table to 21 / 77 / 98 (7×14);
and `crates/makina-core/src/run_metadata.rs:69–71` claims
`WorktreeManager::worktree_path` "still returns the pre-relocation path," which is
false since plan 0029 — it delegates to `paths::worktree`, returning the relocated
off-repo path.

**Edits:**

**Make `run.json` atomic via temp+rename.** Rework `write_run_metadata` to mirror
the `persist_graph` discipline, using a process-global atomic sequence for the
temp-file name:

```rust
pub async fn write_run_metadata(meta: &RunMetadata, repo_root: &Path) -> std::io::Result<()> {
    let dir = paths::run_dir(repo_root, &meta.run_uid);
    tokio::fs::create_dir_all(&dir).await?;
    let mut json = serde_json::to_string_pretty(meta)?;
    json.push('\n');

    // Atomic write: same pattern as persist_graph. Write to a per-process/seq temp
    // file, then rename onto the canonical path. A crash leaves a stray .tmp, never a
    // truncated run.json; stray temps are cleaned up by the next run or an explicit prune.
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("run.json.tmp.{}.{}", std::process::id(), seq));
    tokio::fs::write(&tmp, json.as_bytes()).await?;
    if let Err(e) = tokio::fs::rename(&tmp, dir.join("run.json")).await {
        let _ = tokio::fs::remove_file(&tmp).await; // best-effort cleanup
        return Err(e);
    }
    Ok(())
}
```

**Fix the `state_machine.rs` counts.** The stale "19 / 65 / 84" and "7×12" prose lives
in the `#[cfg(test)] mod` doc and in-test comments — NOT the module-level doc
(`:1–70`), which carries no counts. Update each stale site (`:325`, `:327–328`,
`:335–336`, `:346`, `:407`, `:412–413`) so the prose matches the exhaustive test's
assertions:

```rust
//! Cartesian product of all 7 states × all 14 events (98 pairs total). For each
//! pair it asserts the exact expected outcome: `Ok(target)` for the 21 legal
//! transitions and `Err(IllegalTransition)` for the remaining 77 pairs.
//!
//! Plan 0002 added `MergeConflict`; plan 0017 added `RetryRequested` and
//! `DependencyReset`, growing the table to 7 states, 14 events, 98 pairs —
//! 21 legal, 77 illegal.
```

Apply the same 21 / 77 / 98 correction to the in-test comments (the `7×12` →
`7×14` and "totalling 84 assertions" → "totalling 98 assertions" prose at `:407`
and `:412–413`). The assertions themselves already read `total == 98` (`:430`),
`legal_count == 21` (`:457`), and `illegal_count == 77` (`:458`).

**Fix the stale `run_metadata.rs` claim.** Delete or correct the lines 69–71 comment
so it stops asserting the pre-relocation behavior:

```rust
// `WorktreeManager::worktree_path` delegates to `paths::worktree`, returning the
// off-repo relocated path (since plan 0029) — not the pre-relocation in-repo path.
```

**Properties that make this safe:**

- The temp+rename write is the exact proven, idempotent technique `persist_graph`
  already uses; a same-filesystem `rename` is atomic, so readers never observe a
  partial `run.json`.
- The doc edits change comments only — no logic moves — and the corrected counts are
  the ones the existing exhaustive transition test already asserts, so the prose now
  matches the code.
- `run.json` remains a historical record, not the source of truth for task state, so
  the change strengthens consistency without altering recovery semantics.
- All existing run-metadata and state-machine tests stay green; `cargo test` /
  clippy / fmt clean.

## 0003 — Dead-Code-Cleanup-And-Deduplication

Today three symbols carry `#[allow(dead_code)]` despite being used in production code:
`is_plan_convention_dir` (`crates/makina-core/src/orchestrator.rs:190`, used at `:1312`),
`format_tasks_section` (`crates/makina/src/ui.rs:2170`, used at `:1999`), and
`accordion_section_order` (`crates/makina/src/app.rs:1771`, used at `:1802`/`1855`/`1872`).
A fourth, `PlaceholderApi::empty` (`crates/makina/src/placeholder.rs:73`, allow at `:72`,
in the TUI crate — NOT makina-core), is test-only: every caller lives in a `#[cfg(test)]`
module, so `cargo clippy --all-targets -- -D warnings` (which also compiles the lib
target WITHOUT `cfg(test)`) would flag `dead_code` if its allow were simply removed —
it is kept allowed or `#[cfg(test)]`-gated instead. `SettingsCommit` validation is
duplicated: `App::update` (the `SettingsCommit` arm, `crates/makina/src/app.rs:2856–2963`,
~108 lines) and `event::commit_settings` (`crates/makina/src/event.rs:613`) validate the
same fields (gate_iterations, reviewer_iterations, wall_clock_secs, idle_secs, concurrency),
so the two must be kept in sync by hand; they differ only in error-surfacing
(`commit_settings` early-returns `Some(reason)` at `event.rs:620–687`; `App::update`
accumulates into `settings.error` with a `has_error` short-circuit at `app.rs:2859–2949`
and keeps the modal open). And the retry/discover palette wiring is broken, with a
structural dispatch gap on top. Retry has TWO variants: the real `AppEvent::RetryFocused`
(`crates/makina/src/app.rs:807`), handled ONLY in `resolve_io` at
`crates/makina/src/event.rs:346` (→ `retry_focused`, `event.rs:1028`, which issues
`Command::RetryTask`/`RetryFailedTasks`) and bound to the `r`/`R` key — its `App::update`
arm at `app.rs:2464` is a deliberate NO-OP — and a separate stub `AppEvent::RetryFocusedTask`
(`crates/makina/src/app.rs:900`). The command palette wires "Retry failed task" to the
STUB (`crates/makina/src/app.rs:482`), not the real `RetryFocused`, so the palette's
retry action falls through to the `App::update` arm at `crates/makina/src/app.rs:2967`
and emits "retry not yet available (plan 0017)". CRITICALLY, repointing the palette
action at `RetryFocused` alone does NOT fix it: the palette dispatch path is
`CommandPaletteExecute` → `resolve_io` EXTRACTS the action's event and returns it
(`event.rs:389–409`) → the loop calls `app.update(extractedEvent)` DIRECTLY
(`event.rs:224–226`) with NO second `resolve_io` pass, so a palette-dispatched
`RetryFocused` reaches only the no-op `App::update` arm. (The glm-5.2 review item 22's
claim that the palette wires to the real events is factually wrong.) The fix RE-DISPATCHES
palette IO events through `resolve_io` (the loop reprocesses anything received over
`background_tx` through `resolve_io`, `event.rs:156`/`224`). Discovery has a SINGLE variant
`AppEvent::DiscoverProject` (`crates/makina/src/app.rs:902`): the palette wires "Discover
project" to it (`crates/makina/src/app.rs:486`), and `resolve_io`
(`crates/makina/src/event.rs:414`) intercepts it, runs the real `discover_project`, and
returns `Tick`, while the duplicate `App::update` arm at `crates/makina/src/app.rs:2971`
("project discovery not yet available (plan 0025)") is dead code.

**Edits:**

**Remove the three production `#[allow(dead_code)]` pragmas; handle the test-only one
separately.** Delete the attribute line above each production-used symbol —
`is_plan_convention_dir`, `format_tasks_section`, `accordion_section_order` — and let
`cargo clippy --all-targets -- -D warnings` confirm no dead-code warning reappears. For
`PlaceholderApi::empty` (test-only), do NOT blanket-delete the allow: either keep it
(with its "used by tests; production binary uses `new()`" comment) or replace it with
`#[cfg(test)]`, so the non-test lib build stays dead-code-clean.

**Deduplicate `SettingsCommit` validation into one module.** Create
`crates/makina/src/settings_validation.rs` (declared `mod settings_validation;` in
`crates/makina/src/lib.rs`) holding the single canonical validator:

```rust
/// Validated, parsed settings values shared by both commit paths.
pub struct SettingsValidation {
    pub gate_iterations: u32,
    pub reviewer_iterations: u32,
    pub wall_clock_secs: u64,
    pub idle_secs: Option<u64>,
    pub concurrency: usize,
}

/// Single source of truth for SettingsCommit validation. Lifted from the two inline
/// blocks (app.rs `App::update` and event.rs `commit_settings`); the error strings are
/// preserved verbatim so both call sites surface identical messages and can't drift.
pub fn validate_settings(settings: &SettingsState) -> Result<SettingsValidation, String> {
    // .. parsing + range checks consolidated from both sites ..
}
```

Both `App::update` (the `SettingsCommit` arm) and `event::commit_settings` replace
their inline validation with a `validate_settings(..)?` call and consume the returned
`SettingsValidation`.

**Re-dispatch palette IO events through `resolve_io`, repoint the retry action, and
remove the dead stub arms.** First, fix the dispatch gap: in the `CommandPaletteExecute`
arm (`crates/makina/src/event.rs:389–410`), instead of returning the extracted action
event straight to `App::update`, re-dispatch IO-backed events through `resolve_io` — send
the extracted event over `background_tx` (in scope as the arm's parameter) and return
`(AppEvent::Tick, None)`, so the event loop reprocesses it through `resolve_io`
(`event.rs:156`/`224`) and `RetryFocused` reaches `retry_focused`. Then rewire the palette:
change the "Retry failed task" action at `crates/makina/src/app.rs:482` from
`event: AppEvent::RetryFocusedTask` to `event: AppEvent::RetryFocused` (the real event
handled in `resolve_io` at `event.rs:346`). The "Discover project" action at
`crates/makina/src/app.rs:486` already points at the real `AppEvent::DiscoverProject`
and is left untouched. With the stub no longer referenced, delete the `RetryFocusedTask`
variant from `AppEvent` (`crates/makina/src/app.rs:900`) and its "not yet available" arm
in `App::update` (`crates/makina/src/app.rs:2967`). Do NOT touch the no-op `RetryFocused`
`App::update` arm at `app.rs:2464` — `RetryFocused` keeps flowing through `resolve_io`.
Finally, delete only the DEAD `DiscoverProject` arm in `App::update`
(`crates/makina/src/app.rs:2971`) — unreachable because `resolve_io` intercepts
`DiscoverProject` first — while KEEPING the `DiscoverProject` variant itself. Leave a
short comment in place of the removed arms:

```rust
// Plan 0041: palette IO actions are re-dispatched through resolve_io (event.rs
// CommandPaletteExecute arm), so the "Retry failed task" action repointed at the real
// `RetryFocused` event reaches `retry_focused` (resolve_io, event.rs:346/1028) and
// issues Command::RetryTask/RetryFailedTasks; the `RetryFocusedTask` stub variant + arm
// were removed. `DiscoverProject` is the real discovery event handled in resolve_io
// (event.rs:414), so only its dead `App::update` stub arm was removed.
```

**Properties that make this safe:**

- Removing `#[allow(dead_code)]` from genuinely used symbols cannot change behavior;
  clippy under `-D warnings` proves they are still reachable, and catches any future
  regression that the pragmas would otherwise have masked.
- Centralizing validation makes both commit paths call one function with identical
  rules and identical error strings, so a future rule change lands in exactly one
  place and the paths can no longer diverge.
- Re-dispatching palette IO events through `resolve_io` and repointing the "Retry
  failed task" action at the real `RetryFocused` makes that action actually issue
  `Command::RetryTask`/`RetryFailedTasks` (it previously hit the stub's "not yet
  available" message; a bare repoint would have silently hit the no-op `App::update`
  arm). The re-dispatch reuses the loop's existing `background_tx` → `resolve_io` path,
  so no new dispatch mechanism is introduced and the `r`/`R` key path is unchanged.
  Deleting the then-orphaned `RetryFocusedTask` variant + arm and the dead
  `DiscoverProject` stub arm removes only unreachable code; the real `RetryFocused` /
  `DiscoverProject` `resolve_io` handlers and the kept `DiscoverProject` variant are
  unchanged, so discovery keeps working.
- All existing app/event/settings tests stay green; new validation unit tests keep
  `cargo test` / clippy / fmt clean.

## Test strategy

- **0001 (shared helpers).** New unit tests in `agent_turn.rs`:
  `test_drain_agent_turn_forwards_text_chunks` asserts text chunks are forwarded and
  accumulated, `test_drain_agent_turn_emits_metrics` asserts `RoleTurnMetrics` is
  emitted with the right role and timing, and `test_drain_agent_turn_handles_idle_timeout`
  asserts the timeout fires and the session is terminated. The surviving
  `combine_output` unit test moves into `cmd_output.rs`. Existing Developer/Reviewer
  and gate/merge tests exercise both helpers indirectly and stay green.
- **0002 (atomicity + docs).** `test_write_run_metadata_is_atomic` asserts the write
  goes through a temp file and rename (no truncated `run.json` is observable), and a
  crashed write leaves only a `.tmp`. The state-machine exhaustive transition test is
  unchanged and now agrees with the corrected 21 / 77 / 98 comments; the doc-only
  `run_metadata.rs` fix has no test of its own.
- **0003 (cleanup + dedup + stubs).** `cargo clippy --all-targets -- -D warnings` AND
  `cargo clippy -p makina -- -D warnings` (the non-test lib build) prove the three
  production-used symbols are still used and that `PlaceholderApi::empty` does not trip
  `dead_code`. New `settings_validation.rs` unit tests cover each field and error case
  (`test_validate_gate_iterations_must_be_at_least_one`, etc.). After re-dispatching
  palette IO events through `resolve_io`, repointing the palette's "Retry failed task"
  action at `RetryFocused`, and removing the `RetryFocusedTask` stub variant (plus the
  dead `DiscoverProject` `App::update` arm), `cargo build` confirms `AppEvent` matches
  stay exhaustive and existing app/event tests stay green; an end-to-end palette test
  drives "Retry failed task" through `CommandPaletteExecute` + the `resolve_io`
  re-dispatch and asserts the orchestrator received `Command::RetryTask` /
  `RetryFailedTasks` (using the `PlaceholderApi::empty()` command-recorder seam, as in
  `retry_key_on_failed_task_issues_retry_task`, `event.rs:3146`) — NOT merely that the
  carried event `== RetryFocused`. Any test that dispatched the `RetryFocusedTask` stub
  is repointed at the real flow.
- All tasks keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.

## Interaction with prior work

- **0017 / task retry and re-dispatch.** 0002's state-machine doc fix records the
  `RetryRequested` + `DependencyReset` edges 0017 added (the table grew to 7×14 → 98
  pairs); the corrected prose now matches the assertions the exhaustive test has been
  enforcing since 0017. 0003 re-dispatches palette IO events through `resolve_io` and
  repoints the palette at 0017's real `RetryFocused` event (whose retry logic lives only
  in `resolve_io`, since `App::update(RetryFocused)` is a no-op), removing the orphaned
  `RetryFocusedTask` stub that shadowed it.
- **0029 / relocate-and-shorten-state.** 0002's `run_metadata.rs` doc fix retires the
  pre-relocation claim about `WorktreeManager::worktree_path`, which 0029 made false by
  routing it through `paths::worktree` to the off-repo path.
- **0023/0025 / command palette and project discovery.** 0025's `discover_project`
  handler lives in `resolve_io` (`event.rs:414`), so the palette's "Discover project"
  action (`app.rs:486` → `DiscoverProject`) only actually runs once palette IO events
  take a `resolve_io` pass (the same dispatch fix 0003 adds for retry); with that in
  place, 0003 deletes the variant's dead `App::update` stub arm and keeps the variant.
  For retry, 0003 fixes the palette's "Retry failed task" action (`app.rs:482`), which
  wrongly pointed at the `RetryFocusedTask` stub, by repointing it at the real
  `RetryFocused` event AND re-dispatching it through `resolve_io` so it reaches
  `retry_focused` rather than the no-op `App::update` arm.
- **persist.rs atomic-write discipline.** 0002's `run.json` change reuses the
  established temp+rename pattern from `persist_graph` (process-global atomic sequence,
  best-effort temp cleanup), aligning the two persistence sites without introducing a
  new write mechanism.
