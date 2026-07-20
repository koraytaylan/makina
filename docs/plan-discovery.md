# Plan discovery and selection

Makina discovers plans by canonical repository-relative directory identity (`PlanKey`), never by a task-list filepath.

## Candidate rule

A directory under a configured plan root is a new-format candidate when it contains `tasks/`. A valid executable bundle also contains `SCOPE.md`, `ARCHITECTURE.md`, `STATUS.md`, and at least one ordinary `tasks/*.md` file, with no nested task directories. Candidate validation is accumulated and shown deterministically; invalid candidates remain visible but cannot start.

A directory without `tasks/` is not a candidate. This deliberately makes historical pre-cutover plan directories inert. A directory combining `tasks/` with the former monolithic task-list format is malformed, not a precedence or compatibility case.

```text
docs/plans/
├── STATUS.md
├── 0048-per-task-plan-documents/
│   ├── SCOPE.md
│   ├── ARCHITECTURE.md
│   ├── STATUS.md
│   └── tasks/
│       ├── 0101-define-schema.md
│       └── 0201-project-runtime.md
└── 0001-historical/          # no tasks/; inert record
```

## Source state

Discovery is read-only. It loads committed source when available, validates the bundle and root roll-up, and reports:

- `AwaitingCommit`: the bundle exists only in working-tree bytes and cannot be registered or run.
- `Unregistered`: a valid committed bundle has no exact Phase-R registration.
- `Ready`: registration binds the exact source/ref and immutable validation-base OID; at least one ungated dependency-ready task can be considered for execution.
- invalid, active, blocked, retained, or complete states derived from typed source and coordinator-owned Git evidence.

Opening a plan projects immutable task documents into a runtime graph and overlays only compatible volatile checkpoint state. Starting rereads source, refs, worktrees, and checkpoints while holding the repository lease, closing the open-to-start race. JSON-only completion is ignored.

## UI and API identity

Discovery results, commands, events, run metadata, tabs, and runtime paths carry `plan_dir`/`PlanKey`. Task source paths remain useful navigation metadata but are not plan identities. Equal task IDs in different plans cannot collide because refs, worktrees, runs, and checkpoints are plan-qualified.

See [the authoring contract](plans/README.md) for the bundle schema.
