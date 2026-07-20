# Runtime checkpoint schema

Version: 1

Status: Normative

Makina may persist a volatile checkpoint below the external per-project state root at `checkpoints/<safe PlanKey>/checkpoint.json`. The resolver must be outside the repository and reject unavailable, insecure, or repository-contained state roots.

Validated plan documents and coordinator-owned Git evidence are authoritative. A checkpoint cannot define tasks, dependencies, instructions, gates, footprints, authored status, landing OIDs, plan status, or completion. Open validates source first and overlays only compatible volatile fields. Start rereads source, refs, worktrees, and checkpoints under the repository lease. JSON-only `done` never establishes durable completion.

## Closed object

```json
{
  "schema_version": 1,
  "identity": {
    "plan_dir": "docs/plans/NNNN-slug",
    "executable_digest": "<64 lowercase hex>",
    "task_ids": ["task-id"],
    "task_source_paths": ["docs/plans/NNNN-slug/tasks/0101-task-id.md"]
  },
  "tasks": [
    {
      "id": "task-id",
      "state": "ready",
      "gate_iterations": 0,
      "review_iterations": 0
    }
  ],
  "active_refs": [],
  "active_worktrees": []
}
```

Unknown or missing fields are invalid. Identity must exactly match the freshly loaded plan's canonical directory, executable digest, ordered IDs, and ordered source paths. Each task checkpoint has only its ID, volatile scheduler state, and non-negative gate/review iteration counts. Supported runtime states are `new`, `ready`, `in-progress`, `in-review`, `done`, and `failed`; source/Git reconciliation decides whether a restored terminal state is justified.

`active_refs` and `active_worktrees` carry typed recovery evidence. A digest/identity mismatch with active evidence is retained for explicit recovery, not silently overwritten. A clean incompatible checkpoint may be archived before a new one is written.

There is no compatibility read or migration from repository-local runtime artifacts. Runtime logs, transcripts, checkpoints, worktree metadata, and contract handoff data remain external and plan-qualified.

See [the sample checkpoint](examples/sample-run.tasks.json) and [the plan authoring contract](../plans/README.md).
