# XAgent Plan 0041 — Code-Hygiene-And-Stale-Cleanup

Extract two shared helper functions (`drain_agent_turn` from Developer/Reviewer ~130 duplicate lines — returning a shared `DrainError` each actor maps into its own `DeveloperError`/`ReviewerError`, `combine_output` from gate.rs and merge.rs), make `run.json` atomic with temp+rename for consistency with `persist_graph` atomicity, fix stale doc comments in `state_machine.rs` (update 19/65/84 → 21/77/98 transition counts at the in-test sites) and `run_metadata.rs` (worktree-path claim), remove stale `#[allow(dead_code)]` on three production-used symbols (`is_plan_convention_dir`, `format_tasks_section`, `accordion_section_order`) and handle the test-only `PlaceholderApi::empty` separately so the lib build stays dead-code-clean, deduplicate `SettingsCommit` validation between `App::update` and `event::commit_settings` (each keeping its own error-surfacing), and fix the stub-event tangle (re-dispatch palette IO events through `resolve_io` so the "Retry failed task" action actually issues `Command::RetryTask`/`RetryFailedTasks`, repoint that action at the real `RetryFocused`, delete the orphaned `RetryFocusedTask` stub, and delete the dead `DiscoverProject` `App::update` arm while keeping the real `DiscoverProject` variant).

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md) for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}`).
- **Depends on** lists *direct* prerequisites only; "—" means none.
- **Done when** is the verifiable criterion, and every task keeps the gate
  commands green — the full forms are `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`;
  abbreviated as "cargo test/clippy/fmt green" thereafter.
- GPU tests self-skip without an adapter.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0001 — Shared-Helper-Extraction

### extract-drain-agent-turn — Extract drain_agent_turn Helper

The Developer and Reviewer actors implement an identical response-stream drain loop (idle timeout, side-channel forwarding, metrics emission). `developer.rs:289–417` and `reviewer.rs:259–386` are nearly identical, differing only in the role name and the role error enum (`DeveloperError` at `developer.rs:70` vs `ReviewerError` at `reviewer.rs:67` — two DISTINCT types). This duplication is ~130 lines and creates a maintenance burden: any change to the pattern must be made in two places, and readers encounter unexplained repetition. A shared `drain_agent_turn` helper will consolidate the logic and make the actors simpler; because the two roles surface different error enums, the helper returns a shared `DrainError` (or is generic over the role error) that each actor maps into its own enum.

**Steps:**

1. Create a new file `crates/makina-core/src/actors/agent_turn.rs` (or add to `actors/mod.rs` if the crate prefers a single file for helpers).
2. Define `pub(crate) async fn drain_agent_turn(...)`. NOTE: the error/usage type names in early drafts (`DeterminismError`, `TokenUsage`) DO NOT EXIST in this crate — verify with grep before writing. The real per-actor error enums are `DeveloperError` (`developer.rs:70`) and `ReviewerError` (`reviewer.rs:67`) — two DISTINCT enums, so one concrete fn cannot return both. The real usage value is `Option<api::UsageStats>` (it is the `usage` field of `ResponseEvent::TurnComplete`, `backend.rs:245`). Resolve the error mismatch with ONE of:
   - (a) Introduce a shared `DrainError` enum in `agent_turn.rs` (e.g. `enum DrainError { IdleTimeout { idle_secs: u64 }, Stream(String), EndedUnexpectedly }`) and give the helper return type `Result<(String, Option<api::UsageStats>), DrainError>`; each actor maps `DrainError` into its own `DeveloperError` / `ReviewerError` at the call site (the current loop returns `DeveloperError::IdleTimeout { idle_secs }` for the watchdog, `DeveloperError::Other(format!("…stream error: {e}"))` for `Some(Err(e))`, and `DeveloperError::Other("…stream ended unexpectedly")` for `None`).
   - (b) Make the helper generic over the role error type via a closure/trait that constructs the role error from a `DrainError`.
   Use signature shape `async fn drain_agent_turn<S: AgentSession + ?Sized>(session: &mut S, events: &mut ResponseStream, role: api::AgentRole, task_id: api::TaskId, idle_secs: Option<u64>, sink: &(dyn Fn(api::Event) + Send), run: api::RunId, assignment: Option<&RoleAssignment>) -> Result<(String, Option<api::UsageStats>), DrainError>`. The confirmed types `AgentSession` (`backend.rs:311`), `ResponseStream` (`backend.rs:261`), `ResponseEvent` (`backend.rs:180`), `api::AgentRole` (`api.rs:533`), and `RoleAssignment` (`config.rs:157`) are correct — note `TaskId`/`RunId` are `api::TaskId`/`api::RunId` (qualified `api::`, as built in `developer.rs`).
3. Copy the idle-timeout loop from `developer.rs:289–417` verbatim into the helper body. The loop handles: timeout-with-idle-secs (returns the idle-timeout error after `session.terminate()` and a `TaskIdle` emit), dispatch of TextChunk/ThoughtChunk/ToolCall/ToolCallUpdate/CurrentModeUpdate, and `TurnComplete { usage }` which emits `RoleTurnMetrics` (with the `model` inferred from `assignment`/`session.capabilities()`) and breaks carrying `usage`; accumulation of `TextChunk` text into the returned `String`; and the `Some(Err)` / `None` error arms. The break must surface `(output, usage)`.
4. Replace the loop body in `developer.rs:289–417` with a call to `drain_agent_turn(...)`, binding the returned `(output, usage)` (mapping any `DrainError` into `DeveloperError`) and continuing with the current branch-commit logic.
5. Replace the loop body in `reviewer.rs:259–386` with an identical call to `drain_agent_turn(...)` (mapping `DrainError` into `ReviewerError`), and parse the output into a review verdict as before.
6. Add unit tests to `agent_turn.rs` driving the existing in-crate `NoopBackend` session seam (`crate::backend::noop::NoopBackend`, e.g. `NoopBackend::with_responses(vec!["hello".into()])`, already used by the developer/reviewer tests at `developer.rs:841`/`857`) rather than inventing a new session mock: `test_drain_agent_turn_forwards_text_chunks` verifies text chunks are forwarded and accumulated; `test_drain_agent_turn_emits_metrics` verifies RoleTurnMetrics is emitted with the correct role and timing; `test_drain_agent_turn_handles_idle_timeout` verifies the timeout fires and the session is terminated.
7. Run `cargo test` to ensure existing Developer/Reviewer tests pass (they exercise the helper indirectly via the actors).

- **Depends on:** —
- **Done when:** The `drain_agent_turn` helper is extracted with return type `Result<(String, Option<api::UsageStats>), DrainError>` (or generic over the role error), and both `developer.rs` and `reviewer.rs` call it, each mapping `DrainError` into its own `DeveloperError`/`ReviewerError`. Idle timeout, side-channel forwarding, and RoleTurnMetrics emission all work as before. Tests `test_drain_agent_turn_*` (driving the `NoopBackend` seam) pass. All existing Developer/Reviewer tests remain green. cargo test/clippy/fmt green.

---

### extract-combine-output — Extract combine_output Helper

`combine_output` is defined identically in both `gate.rs:241` and `merge.rs:482`, merging stdout and stderr into a single formatted string with a 'stderr:' label for stderr. Each file has its own unit test. Moving the function to a shared internal module eliminates duplication and establishes a single source of truth for the logic.

**Steps:**

1. Create a new file `crates/makina-core/src/cmd_output.rs`.
2. Copy the `combine_output` function from `gate.rs:241–256` (or `merge.rs:482–496`, they are identical) into `cmd_output.rs` as `pub(crate) fn combine_output(stdout: &[u8], stderr: &[u8]) -> String { ... }`.
3. Copy one of the unit test blocks (e.g., `gate.rs:394–398`) into a `#[cfg(test)]` mod in `cmd_output.rs`.
4. In `gate.rs`, delete the `combine_output` function and its unit test; add `use crate::cmd_output::combine_output;` near the top of the file.
5. In `merge.rs`, delete the `combine_output` function and its unit test; add `use crate::cmd_output::combine_output;`.
6. Add `mod cmd_output;` to `crates/makina-core/src/lib.rs` if not already present, or to the appropriate parent module.
7. Run `cargo test` to verify the tests still pass and the function is used correctly in both sites.

- **Depends on:** —
- **Done when:** `combine_output` is defined once in `cmd_output.rs` as a `pub(crate)` utility. Both `gate.rs` and `merge.rs` import and call it. The shared unit test passes. All gate and merge tests remain green. cargo test/clippy/fmt green.

---

## 0002 — Atomicity-And-Doc-Fixes

### atomicity-run-json — Make run.json Atomic with Temp+Rename

`run_metadata.rs:write_run_metadata` writes `run.json` in a single `tokio::fs::write` call. A crash mid-write leaves a truncated file. `persist_graph` (`persist.rs:173–205`) uses atomic temp+rename. For consistency and safety, `run.json` should adopt the same pattern. Although `run.json` is a historical record (not the source of truth for task state), the uniformity in persistence discipline strengthens the codebase.

**Steps:**

1. In `crates/makina-core/src/run_metadata.rs`, locate the `write_run_metadata` function (line 173 onwards).
2. Identify any process-global sequence counter for temp-file naming. If `persist.rs` exports a counter or helper, reuse it. Otherwise, introduce a local `static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);` in the run_metadata module.
3. Replace the single `tokio::fs::write(&dest, json.as_bytes()).await?;` with: (1) generate a temp path `tmp = dir.join(format!("run.json.tmp.{}.{}", std::process::id(), next_seq))` using the counter; (2) write to `tmp`; (3) rename `tmp` to `dest` atomically; (4) on rename error, attempt best-effort cleanup of `tmp`.
4. Document the temp-file cleanup idiom with a comment: e.g., "Temp files left behind by crashed writes are cleaned up by the next run or explicit prune."
5. Add a unit test `test_write_run_metadata_is_atomic` that verifies the write uses temp+rename (inspect the filesystem between write and rename, or mock the operations).
6. Run `cargo test` and verify existing run-metadata tests still pass.

- **Depends on:** —
- **Done when:** `run.json` is written atomically via temp file and rename. A crashed mid-write leaves only a temporary file, not a truncated `run.json`. Test `test_write_run_metadata_is_atomic` passes. Existing run-metadata tests remain green. cargo test/clippy/fmt green.

---

### fix-state-machine-docs — Update state_machine.rs Doc Comments

Doc comments in `state_machine.rs` cite outdated transition counts: 19/65/84 pairs (7×12 events). Plan 0017 added `RetryRequested` and `DependencyReset`, bringing the counts to 21/77/98 pairs (7×14 events). The code and test assertions are correct; the comments were not updated. A reader comparing documentation to test assertions encounters a mismatch.

**Steps:**

NOTE: the module-level doc comment (`state_machine.rs:1–70`) contains NO transition counts — there is nothing to fix "around line 1". The stale "19 / 65 / 84" and "7×12" text all lives in the `#[cfg(test)] mod` doc and the in-test comments. Grep for `19`, `65`, `84`, and `7×12` to confirm the exact sites before editing.

1. In `crates/makina-core/src/state_machine.rs`, update the test-module doc at `:325` ("Cartesian product of all 7 states × all 12 events (84 pairs total)") to "... all 14 events (98 pairs total)".
2. Update `:327–328` ("the 19 legal transitions and `Err(IllegalTransition)` for the remaining 65 pairs") to "the 21 legal transitions ... remaining 77 pairs".
3. Update the growth-history comment at `:335–336` ("6 → 7 states, 11 → 12 events, 66 → 84 pairs, 15 → 19 legal, 51 → 65 illegal") so the final figures read 14 events / 98 pairs / 21 legal / 77 illegal (record that plan 0017's `RetryRequested` + `DependencyReset` grew the table further; keep the prose internally consistent).
4. Update the `legal_table()` doc at `:346` ("now 19 entries: 15 + the 4 DependencyFailed edges into Skipped") to reflect the current 21 legal entries.
5. Update the `exhaustive_transition_table` test doc at `:407` ("the 7×12 Cartesian product") to "the 7×14 Cartesian product", and at `:412–413` ("19 legal transitions and 65 illegal ones, totalling 84 assertions") to "21 legal transitions and 77 illegal ones, totalling 98 assertions".
6. Run `cargo test` and ensure all state_machine tests pass with the updated counts.

- **Depends on:** —
- **Done when:** Doc comments in `state_machine.rs` (sites `:325`, `:327–328`, `:335–336`, `:346`, `:407`, `:412–413`) cite 21 legal / 77 illegal / 98 total transitions (7 states, 14 events). The counts match the test assertions (`total == 98` at `state_machine.rs:430`, `legal_count == 21` at `:457`, `illegal_count == 77` at `:458`). cargo test/clippy/fmt green.

---

### fix-run-metadata-docs — Fix run_metadata.rs Stale Doc Comment

Lines 69–71 of `run_metadata.rs` claim `WorktreeManager::worktree_path` "still returns the pre-relocation path." This is outdated; the method now delegates to `paths::worktree` which returns the relocated off-repo path (as of plan 0029). The stale comment misleads readers about the current behavior.

**Steps:**

1. In `crates/makina-core/src/run_metadata.rs`, locate the comment at lines 69–71 that starts with "... the orchestrator stores no such map, and `WorktreeManager::worktree_path` is private and still returns the pre-relocation path."
2. Replace or delete the stale claim. Either: (1) remove the entire sentence; (2) update it to say "... and `WorktreeManager::worktree_path` delegates to `paths::worktree`, returning the off-repo relocated path (as of plan 0029)." Choose whichever keeps the comment concise and accurate.
3. Verify the comment makes sense in context (it is part of a doc comment for the RunMetadata struct).

- **Depends on:** atomicity-run-json
- **Done when:** The doc comment at lines 69–71 no longer claims `worktree_path` returns the pre-relocation path. The comment is accurate or removed. cargo test/clippy/fmt green.

---

## 0003 — Dead-Code-Cleanup-And-Deduplication

### remove-stale-dead-code-allows — Remove Stale #[allow(dead_code)] Pragmas

Three symbols carry stale `#[allow(dead_code)]` pragmas despite being used in production (non-test) code: `is_plan_convention_dir` (`orchestrator.rs:190`, used at `orchestrator.rs:1312`), `format_tasks_section` (`ui.rs:2170`, used at `ui.rs:1999`), and `accordion_section_order` (`app.rs:1771`, used at `app.rs:1802`/`1855`/`1872`). For these three, removing the pragma is a one-line change per site and clippy will verify no regression.

A FOURTH symbol — `PlaceholderApi::empty` (`crates/makina/src/placeholder.rs:73`, in the TUI crate, NOT makina-core; its `#[allow(dead_code)]` is at `placeholder.rs:72`) — is DIFFERENT and must be handled separately. It is a non-`#[cfg(test)]` `pub fn` whose ONLY callers live inside `#[cfg(test)]` modules (the source comment at `:72` says "used by tests; production binary uses `new()`"; the callers are all in test mods in `event.rs`, `ui.rs`, and `app.rs`). `cargo clippy --all-targets -- -D warnings` ALSO compiles the lib target WITHOUT `cfg(test)`, where `empty()` is unreachable → `dead_code` → the gate FAILS if the allow is simply deleted. So do NOT blanket-remove this allow.

**Steps:**

1. In `crates/makina-core/src/orchestrator.rs`, locate line 190 (`#[allow(dead_code)]` above `is_plan_convention_dir`). Delete the entire line. (Used at `orchestrator.rs:1312` in production.)
2. In `crates/makina/src/ui.rs`, locate line 2170 (`#[allow(dead_code)]` above `format_tasks_section`). Delete the entire line. (Used at `ui.rs:1999` in production.)
3. In `crates/makina/src/app.rs`, locate line 1771 (`#[allow(dead_code)]` above `accordion_section_order`). Delete the entire line. (Used at `app.rs:1802`/`1855`/`1872` in production accordion-cycle logic.)
4. Handle `PlaceholderApi::empty` (`crates/makina/src/placeholder.rs:73`) SEPARATELY — it is genuinely test-only. Pick ONE of:
   - (a) KEEP `#[allow(dead_code)]` on `empty()` (with its existing "used by tests; production binary uses `new()`" comment), leaving it untouched; or
   - (b) `#[cfg(test)]`-gate `empty()` itself (replace the `#[allow(dead_code)]` with `#[cfg(test)]`) so it is only compiled for tests, then the allow is no longer needed.
   Either way, `cargo clippy --all-targets -- -D warnings` must stay green. Verify by running clippy on the lib target without `--all-targets` too (`cargo clippy -p makina -- -D warnings`) to confirm `empty()` does not trip `dead_code` in the non-test lib build.
5. Run `cargo clippy --all-targets -- -D warnings` to verify no new warnings appear.

- **Depends on:** —
- **Done when:** The three production-used `#[allow(dead_code)]` pragmas (`is_plan_convention_dir`, `format_tasks_section`, `accordion_section_order`) are removed; `PlaceholderApi::empty` is handled per option (a) or (b) so it is NOT reported as dead code in the non-test lib build. `cargo clippy --all-targets -- -D warnings` AND `cargo clippy -p makina -- -D warnings` both show no warnings. cargo test/clippy/fmt green.

---

### deduplicate-settings-validation — Deduplicate SettingsCommit Validation

`App::update` (the `SettingsCommit` arm, `app.rs:2856–2963`, ~108 lines) and `event::commit_settings` (`event.rs:613`) both validate the same fields (gate_iterations, reviewer_iterations, wall_clock_secs, idle_secs, concurrency). Both must stay in sync; a change to validation rules requires updates in two places, creating a maintenance hazard. Extracting the validation into a shared helper eliminates duplication and ensures coherence. The two validators differ ONLY in how they surface a validation error, and the extraction must preserve each site's behavior: `event::commit_settings` early-returns `Some(reason)` on the first bad field (`event.rs:620–687`); `App::update` instead accumulates into `settings.error` with a `has_error` short-circuit and does NOT early-return from `update` (`app.rs:2859–2949`).

**Steps:**

1. Create a new file `crates/makina/src/settings_validation.rs` with a public module for validation logic.
2. Define a struct `SettingsValidation` with fields for each validated setting: `pub struct SettingsValidation { pub gate_iterations: u32, pub reviewer_iterations: u32, pub wall_clock_secs: u64, pub idle_secs: Option<u64>, pub concurrency: usize, }`.
3. Define `pub fn validate_settings(settings: &SettingsState) -> Result<SettingsValidation, String>` that validates all fields and returns `Result`. Copy the validation logic from `event.rs:613` onwards (parsing, range checks, error messages), ensuring the error messages match exactly what both sites currently use.
4. In `event.rs`, replace the validation block in `commit_settings` (`event.rs:620–687`) with `let valid = match validate_settings(app.settings.as_ref()?) { Ok(v) => v, Err(reason) => return Some(reason) };` — preserving the early-`Some(reason)`-return behavior. Bind the returned `SettingsValidation` and use its fields.
5. In `app.rs`, replace the validation block in `App::update` (SettingsCommit arm, `app.rs:2856–2963`) with a call to `validate_settings(settings)` and branch on the `Result` WITHOUT `?` (the `update` method returns `bool`, not `Result`, so no early return): on `Err(msg)`, set `settings.error = Some(msg)` and KEEP the modal open (do not clear `self.settings` or change `self.mode`); on `Ok(valid)`, apply the fields (`self.caps.* = valid.*`, `self.concurrency = valid.concurrency`), then close the modal (`self.mode = Mode::Normal; self.settings = None;`). Return `true` either way.
6. Add unit tests to `settings_validation.rs`: `test_validate_gate_iterations_must_be_at_least_one`, `test_validate_reviewer_iterations_must_be_at_least_one`, etc., covering all fields and error cases.
7. Run `cargo test` and ensure all settings-related tests pass.

- **Depends on:** remove-stale-dead-code-allows
- **Done when:** `SettingsValidation` struct and `validate_settings` function are defined in `settings_validation.rs`. Both `App::update` and `event::commit_settings` call the shared validator with identical rules and error strings, while each preserves its own error-surfacing: `commit_settings` returns `Some(reason)` on `Err`; `App::update` sets `settings.error = Some(msg)` and keeps the modal open on `Err`, and applies fields + closes the modal on `Ok`. Tests for each validation rule pass. Existing app and event tests remain green. cargo test/clippy/fmt green.

---

### resolve-stub-events — Re-Dispatch Palette IO Events and Drop Dead Stub Arms

The retry/discover stub wiring is broken, AND the palette dispatch path has a structural gap that makes the "obvious" one-line fix a silent no-op. Retry has TWO variants: the real `AppEvent::RetryFocused` (`app.rs:807`), and a separate stub `AppEvent::RetryFocusedTask` (`app.rs:900`). The command palette wires "Retry failed task" (`app.rs:482`) to the STUB, so that palette action emits "retry not yet available (plan 0017)" instead of retrying. (The source review item 22 claims the palette is wired to the *real* events — it is factually WRONG; the TASKS.md premise that the palette is wired to the stub at `app.rs:482` is correct.)

The critical subtlety: the real retry logic lives ONLY in `resolve_io`'s `retry_focused` (`event.rs:346` → `event.rs:1028`, which calls `api.execute(Command::RetryTask { .. })` / `Command::RetryFailedTasks { .. }`). `App::update(RetryFocused)` is a deliberate NO-OP (`app.rs:2464`, in the `StartRun | … | RetryFocused | OpenFocusedNode => true` arm). So simply repointing the palette action at `RetryFocused` does NOT make retry work — it converts a broken stub-message into a SILENT no-op. The reason: the palette dispatch path is `CommandPaletteExecute` → `resolve_io` EXTRACTS the action's event and `return`s it directly (`event.rs:389–409`, `(event.clone(), None)`) → the event loop calls `app.update(extractedEvent)` DIRECTLY (`event.rs:224–226`) with NO second `resolve_io` pass. So a palette-dispatched `RetryFocused` reaches only the no-op `App::update` arm and never hits `retry_focused`. (The same structural gap silently affects the palette's other IO-backed actions, e.g. an `OpenBrowser`-style intent, which also need a `resolve_io` pass.)

The real fix is therefore in the `CommandPaletteExecute` arm: IO-backed events (`RetryFocused`, and any other extracted action that needs an IO pass) must be RE-DISPATCHED through `resolve_io`, not returned straight to `App::update`. The event loop already reprocesses anything received over `background_tx` through the full `resolve_io` → `App::update` path (`event.rs:156`, `224`), so sending the extracted IO event back over `background_tx` (and returning `Tick` for the current pass) makes the loop re-run it through `resolve_io` and reach `retry_focused`. (Alternatively, special-case `RetryFocused` in the `CommandPaletteExecute` arm to run its IO inline before returning — `retry_focused(app).await`, which itself issues `Command::RetryTask` / `Command::RetryFailedTasks`; see the assertions at `event.rs:3157`/`3190`.) Discovery has a SINGLE variant `AppEvent::DiscoverProject` (`app.rs:902`); the palette wires "Discover project" (`app.rs:486`) to it, and `resolve_io` (`event.rs:414` → `discover_project`) intercepts it and returns `Tick`. With the re-dispatch fix in place, the palette `DiscoverProject` path also reaches its `resolve_io` handler; either way the duplicate `App::update` arm at `app.rs:2971` ("project discovery not yet available (plan 0025)") is unreachable dead code. Fix: re-dispatch palette IO events through `resolve_io`, repoint the retry palette action at the real `RetryFocused`, delete the now-orphaned `RetryFocusedTask` stub variant + arm, and delete the dead `DiscoverProject` `App::update` arm while keeping the `DiscoverProject` variant.

**Steps:**

1. In `crates/makina/src/app.rs`, in `CommandPalette::default_actions` (around line 462), change the "Retry failed task" action (around line 482) from `event: AppEvent::RetryFocusedTask` to `event: AppEvent::RetryFocused`. Leave the "Discover project" action (around line 486) untouched — it already points at the real `AppEvent::DiscoverProject`.
2. In `crates/makina/src/event.rs`, fix the `CommandPaletteExecute` arm (`event.rs:389–410`) so the extracted action event takes a `resolve_io` pass instead of going straight to `App::update`. Concrete approach: for a `PaletteAction::Regular { event, .. }`, send the extracted `event` over `background_tx` (`background_tx.send(event.clone()).await` — the parameter is already in scope) and `return (AppEvent::Tick, None)` for the current pass, so the event loop reprocesses it through `resolve_io` (the loop already feeds `background_rx` items through `resolve_io` at `event.rs:156`/`224`) and `RetryFocused` reaches `retry_focused`. (Acceptable alternative: special-case `RetryFocused` in this arm to call `retry_focused(app).await` inline and return its status; but prefer the general re-dispatch so every IO-backed palette action is fixed, not just retry.)
3. Update the `default_actions` doc comment (around line 460) that calls `RetryFocusedTask`/`DiscoverProject` "forward-referenced" / "stub variant" so it reflects that the palette now dispatches the real `RetryFocused` and `DiscoverProject` events (re-dispatched through `resolve_io`).
4. In the `AppEvent` enum, delete the now-unused `RetryFocusedTask` variant (around line 900). Keep the `DiscoverProject` variant (around line 902) — it is dispatched by the palette and handled by `resolve_io`.
5. In `App::update`, delete the `RetryFocusedTask` "not yet available" arm (around line 2967) and the dead `DiscoverProject` "not yet available" arm (around line 2971). Leave a short comment explaining that palette IO actions are re-dispatched through `resolve_io` (so retry hits `retry_focused`, `event.rs:1028`) and that `DiscoverProject` is handled in `resolve_io` (`event.rs:414`). Do NOT touch the no-op `RetryFocused` arm at `app.rs:2464` — `RetryFocused` still flows through `resolve_io` (key `r`/`R` and now the re-dispatched palette path), so `App::update` keeps treating it as a redraw-only no-op.
6. Run `cargo build` and address any compilation errors (the match on `AppEvent` stays exhaustive without `RetryFocusedTask`; nothing handled `RetryFocusedTask` in `resolve_io`, so no IO arm needs touching).
7. Run `cargo test`. Repoint any test that dispatched the `RetryFocusedTask` stub (or asserted its "not yet available" message) at the real `RetryFocused` flow. Add a test that drives the palette "Retry failed task" action end-to-end (open the palette with a focused `Failed` task, select that action, resolve `CommandPaletteExecute`, and let the re-dispatched event resolve) and asserts the orchestrator received `Command::RetryTask` / `Command::RetryFailedTasks` — i.e. the action actually ISSUES the retry command (use the `RetryRecordingApi` command-recorder seam — built by the `retry_app` helper at `event.rs:3121` — as in `retry_key_on_failed_task_issues_retry_task`, `event.rs:3147`; note `PlaceholderApi::empty()` is a no-op placeholder that does NOT record commands, so it is the wrong seam for this assertion). Asserting only that the carried event `== RetryFocused` is NOT sufficient.

- **Depends on:** remove-stale-dead-code-allows, deduplicate-settings-validation
- **Done when:** The palette's "Retry failed task" action ISSUES `Command::RetryTask` / `Command::RetryFailedTasks` against the orchestrator (verified by a test that drives the palette action through `CommandPaletteExecute` + the `resolve_io` re-dispatch, not merely that the carried event `== RetryFocused`); palette IO-backed events are re-dispatched through `resolve_io`; the orphaned `RetryFocusedTask` variant and its `App::update` arm are deleted; the dead `DiscoverProject` `App::update` arm is deleted while the `DiscoverProject` variant (dispatched by the palette, handled in `resolve_io`) remains; the `AppEvent` match stays exhaustive. All tests pass. cargo test/clippy/fmt green.

---

**End of plan 0041 TASKS.** When every "Done when" bullet is green, eight
code-hygiene improvements ship: two shared helpers extracted and deduped
(`drain_agent_turn` from the Developer/Reviewer stream loops — returning a shared
`DrainError` mapped into each actor's own error enum, `combine_output`
from gate.rs and merge.rs); `run.json` made atomic via temp+rename to match
`persist_graph`; stale doc comments corrected (state_machine.rs transition
counts at the in-test sites, run_metadata.rs worktree-path claim); three stale
`#[allow(dead_code)]` pragmas removed from production-used symbols while the
test-only `PlaceholderApi::empty` is handled separately; `SettingsCommit`
validation deduplicated into a single shared validator (each call site keeping
its own error-surfacing); and the stub-event tangle fixed (palette IO events
re-dispatched through `resolve_io` so the "Retry failed task" action actually
issues `Command::RetryTask`/`RetryFailedTasks`, that action repointed at the real
`RetryFocused`, the orphaned `RetryFocusedTask` stub deleted, and the dead
`DiscoverProject` `App::update` arm deleted while its real variant is kept) — all
with the gate commands green.
