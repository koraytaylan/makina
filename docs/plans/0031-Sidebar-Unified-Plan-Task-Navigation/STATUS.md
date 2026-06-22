# Plan 0031 — Sidebar-Unified Plan-Task Tree & Tabbed Content Navigation — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** ✅ Complete.

_Last updated: 2026-06-23, against develop._

- **Goal:** The plan-opening, task-viewing, and ingestion paths are unified and robust: discovered plans are navigable from the sidebar without mode-switching; multiple tasks/plans can be viewed in parallel via tabs; and malformed/missing TASKS.md is auto-repaired by the planner before deterministic execution, hardening the entire workflow while preserving the governance wedge (model at the front door only).
- **Root cause:** Three separate ergonomic and robustness gaps compound into workflow friction: (1) plan discovery is modal, forcing context-switching; (2) single-pane design prevents parallel task comparison; (3) ingestion brittleness (parse errors hard-fail) blocks opening well-scoped plans. Unifying these three surfaces eliminates mode-switching, enables side-by-side viewing, and auto-repairs ingestion blockers—all while preserving the deterministic execution model that keeps the system trustworthy and auditable.
- **Approach:** Integrate discovered plans into the persistent sidebar tree as first-class `TreeNode::Plan` variants, eliminating the modal mode; extend the App to track open tabs (keyed by task/plan identity) + an active-tab pointer, replacing the single `selected_task` with tab-based composition; add a `ModelNormalizer` that repairs/generates malformed/missing TASKS.md from the SCOPE/ARCHITECTURE brief **before** the deterministic interpreter runs, gating repair to plan-convention dirs only. All three edits are additive and backward-compatible: empty discovered-plans shows only runs; single-tab usage is the default; non-plan dirs are unchanged. The normalizer preserves the deterministic-governance wedge: only ingestion (read/parse) invokes the model; execution stays pure.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Sidebar Unified Plan-Task Tree | `tree-node-plan-variant`, `add-collapsed-plans-state`, `update-visible-tree-nodes-builder`, `extend-sidebar-rendering-for-plans`, `remove-plan-picker-mode`, `add-plan-open-keybind`, `test-sidebar-plan-integration`, `integration-plan-open-from-sidebar` | ✅ Done |
| 0002 | Tabbed Content Pane | `add-tab-content-enum`, `add-tab-state-to-app`, `add-open-tab-event`, `handle-tab-events-in-update`, `add-tab-bar-renderer`, `integrate-tab-bar-into-main-pane`, `route-task-selection-to-tabs`, `test-tab-state-operations`, `integration-tabbed-pane-navigation` | ✅ Done |
| 0003 | Model-Normalized TASKS.md Ingestion | `add-normalizer-struct`, `add-normalize-system-prompt`, `add-is-plan-convention-helper`, `integrate-normalizer-into-ingestion`, `add-normalizer-to-api-builder`, `test-normalizer-basic`, `integration-normalizer-malformed-tasks` | ✅ Done |
