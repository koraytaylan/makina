# Plan 0043 — Multi-Folder Project Workspace — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** 📋 Planned.

_Last updated: 2026-07-01, against develop._

- **Goal:** Makina evolves from managing a single repo_root to supporting multiple concurrently opened folders, with persistent workspace state and a 3-level sidebar tree, and can bootstrap an empty folder into a Makina-ready project (git + `docs/plans/` + an authoring-guide README).
- **Root cause:** The architecture assumes a single project folder at launch time, with no workspace persistence and no multi-folder support. Users cannot organize multiple projects within Makina, and there is no way to make an empty folder Makina-ready.
- **Approach:** Implement multi-folder support as a vertical slice: (1) persist opened folders at the home level, (2) extend the sidebar tree to a 3-level hierarchy (Folder → Plan → Task) that is navigable and shows empty folders distinctly, (3) scope plan discovery per folder, (4) add command-palette actions for opening, closing, and initializing folders, (5) implement a file-picker modal for folder selection, and (6) bootstrap uninitialized folders with git + an empty initial commit + a develop branch + a `docs/plans/README.md` that is a complete plan-authoring guide (plans are authored externally with the user's agent CLI, not inside Makina). Each workstream is largely independent and integrated at the app-state and event-handling layers.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Workspace Persistence and Multi-Folder State | `add-workspace-persistence` | 📋 Planned |
| 0002 | Sidebar Tree Refactor to 3-Level Hierarchy | `extend-treenode-enum`, `refactor-visible-tree-nodes`, `rewire-tree-navigation-for-folders` | 📋 Planned |
| 0003 | Command Palette Extensions | `extend-appevent-for-folders`, `add-palette-actions` | 📋 Planned |
| 0004 | Folder Browser and File-Picker UI | `implement-folder-browser-mode`, `handle-folder-open-close-events` | 📋 Planned |
| 0005 | Folder Initialization Flow | `create-folder-init-module`, `handle-initialize-folder-event` | 📋 Planned |
| 0006 | Plan Auto-Discovery Per Folder | `add-discover-plans-per-folder`, `update-discovery-event-and-handler`, `update-sidebar-rendering-for-folders` | 📋 Planned |
| 0007 | Multi-Folder Workflow Integration Verification | `verify-multi-folder-integration` | 📋 Planned |
