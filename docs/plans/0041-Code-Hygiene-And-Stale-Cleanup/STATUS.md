# Plan 0041 — Code-Hygiene-And-Stale-Cleanup — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** 📋 Planned.

_Last updated: 2026-06-26, against develop._

- **Goal:** Eight code-hygiene improvements shipped: dual helpers extracted and deduped (drain_agent_turn from Developer/Reviewer — returning a shared DrainError each actor maps into its own DeveloperError/ReviewerError, combine_output from gate/merge), run.json made atomic, doc comments fixed (state_machine.rs counts at the in-test sites, run_metadata.rs claim), three stale production dead_code allows removed with the test-only PlaceholderApi::empty handled separately, SettingsCommit validation deduplicated, and the stub-event tangle fixed by re-dispatching palette IO events through resolve_io so the "Retry failed task" action actually issues Command::RetryTask/RetryFailedTasks.
- **Root cause:** NICE-TO-HAVE-tier code quality debt accumulated during implementation of plans 0001–0037: duplication arose from parallel actor implementations, atomic write discipline was inconsistent, doc comments drifted from code as new events were added, cleanup pragmas lingered, validation logic was duplicated for two dispatch paths, and stub events were left for exhaustiveness rather than cleaned up.
- **Approach:** Organized as three minimal, sequenced workstreams: extract shared helpers (drain_agent_turn, combine_output) to eliminate duplication and establish single sources of truth; make atomicity and documentation consistent with the codebase's actual practices; clean up dead code, dedup validation, and remove unreachable stubs. All changes are low-risk (localized, no API changes, high test coverage) and individually small enough to review and land quickly.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Shared-Helper-Extraction | `extract-drain-agent-turn`, `extract-combine-output` | 📋 Planned |
| 0002 | Atomicity-And-Doc-Fixes | `atomicity-run-json`, `fix-state-machine-docs`, `fix-run-metadata-docs` | 📋 Planned |
| 0003 | Dead-Code-Cleanup-And-Deduplication | `remove-stale-dead-code-allows`, `deduplicate-settings-validation`, `resolve-stub-events` | 📋 Planned |
