# Runtime Artifact Schema — `.makina/tasks/{slug}.json`

Version: 1.0
Status: Normative

---

## 1. Purpose

This document specifies the JSON artifact that the Makina Supervisor writes and reads during orchestration. It is the **source of truth at runtime**: all state, dependency edges, iteration counts, and timestamps live here.

The input that produced the artifact is the structured-text task list (see [`structured-text-convention.md`](structured-text-convention.md)). Once the Planner has emitted the artifact, all orchestration uses the JSON file — the markdown is no longer consulted.

---

## 2. File Location and Ownership

| Property | Value |
|----------|-------|
| Path | `.makina/tasks/{slug}.json` inside the repository |
| Naming | File stem equals the `slug` field (e.g. `my-feature.json` for `slug: "my-feature"`) |
| Committed | Yes — the file is committed to version control so that every state change is a reviewable diff |
| Writer | **Supervisor only** — no other actor or tool should write this file |
| Reader | Any actor or tool (Planner seeds it; Developer/Reviewer read it; TUI displays it) |

The Supervisor is the **sole writer**. Writing from outside the Supervisor corrupts the orchestration invariants.

---

## 3. Top-Level Object

The file contains a single JSON object:

```json
{
  "slug": "<string>",
  "tasks": [ /* array of Task objects */ ]
}
```

| Field | JSON type | Required | Meaning |
|-------|-----------|----------|---------|
| `slug` | string | yes | File-stem identifier matching the file name (e.g. `"my-feature"`). Kebab-case. |
| `tasks` | array | yes | Ordered list of `Task` objects. Preserves the authored order from the structured-text input; order is used for display, not scheduling. |

---

## 4. Task Object

Each element of the `tasks` array is a Task object. Fields are listed in their canonical serialization order.

| Field | JSON type | Required | Meaning |
|-------|-----------|----------|---------|
| `id` | string | yes | Stable kebab-case identifier, unique within the graph (e.g. `"task-model"`). See §4.1. |
| `title` | string | yes | Short human-readable title taken verbatim from the task-list document. |
| `description` | string | yes | Longer description of what the task entails. |
| `done_when` | string | yes | Acceptance criterion copied verbatim from the structured-text `Done when` field. Used by gates and the Reviewer as the ground truth for completion. |
| `depends_on` | array of strings | yes | Task ids that must be in state `done` before this task becomes `ready`. May be an empty array (`[]`). See §5. |
| `section` | string | **omitted when absent** | Four-digit section hint assigned by the Planner (e.g. `"0003"`). Present only after the Planner has processed the task. Omitted — **not `null`** — when absent. |
| `state` | string | yes | Current lifecycle state. One of the six values in §6. Defaults to `"new"` on creation. |
| `gate_iterations` | integer (≥ 0) | yes | How many times this task has cycled through the Developer → gate → failed-gate loop. See §7. |
| `review_iterations` | integer (≥ 0) | yes | How many times this task has cycled through the Developer → Reviewer → changes-requested loop. See §7. |
| `created_at` | string (RFC3339) | yes | When the task was first added to the graph. See §8. |
| `updated_at` | string (RFC3339) | yes | When the task record was last modified (state change, counter increment, etc.). See §8. |
| `started_at` | string (RFC3339) | **omitted when absent** | When a Developer agent first picked up this task. Omitted — **not `null`** — until then. |
| `finished_at` | string (RFC3339) | **omitted when absent** | When the task reached a terminal state (`done` or `failed`). Omitted — **not `null`** — until then. |

### 4.1 Task ID Rules

Task ids in the JSON artifact follow the same rules as in the structured-text convention (§4 of that document):

- Lowercase ASCII letters (`a–z`), digits (`0–9`), and hyphens (`-`).
- Must start and end with a lowercase letter or digit.
- No consecutive hyphens.
- Minimum two characters.
- Unique within the graph.

### 4.2 Omitted vs `null`

The fields `section`, `started_at`, and `finished_at` use `skip_serializing_if = "Option::is_none"`. A conformant artifact **omits** these keys entirely when they have no value. A `null` JSON value for any of these fields is **not** a valid artifact.

Deserializing a field that is absent restores it to `None` (via `#[serde(default)]`). A deserializer MUST accept artifacts that omit these fields.

---

## 5. Dependency-Edge Model

The `depends_on` array encodes directed edges in the task dependency graph. Each element is a task id string.

```json
"depends_on": ["workspace-scaffold", "core-api-surface"]
```

**Semantics:**
- The author declares *direct structural prerequisites* in the structured-text `Depends on` field (see §5.2 of the structured-text convention).
- The **Planner augments** these edges with additional edges inferred from file/area overlap analysis. These inferred edges appear **only in the JSON artifact**, not in the markdown input.
- Therefore `depends_on` in the artifact may contain more ids than what the author listed.
- An empty array (`[]`) means the task has no prerequisites and is immediately eligible to become `ready` once added.

**Validation rule:** every id in `depends_on` must resolve to a task present in the same `tasks` array. A dangling reference is a structural error (see §9).

---

## 6. State Values

The `state` field holds one of exactly six kebab-case strings:

| Value | Meaning |
|-------|---------|
| `"new"` | Registered in the graph; one or more `depends_on` prerequisites have not yet reached `done`. Default for newly created tasks. |
| `"ready"` | All prerequisites are `done`; eligible to be picked up by a Developer agent. See §6.1. |
| `"in-progress"` | A Developer agent is actively working on the task. |
| `"in-review"` | The Developer agent has finished; a Reviewer agent is evaluating the output. |
| `"done"` | Accepted by the Reviewer (or auto-approved). **Terminal.** |
| `"failed"` | Permanently failed after exhausting retry/gate limits, or an unrecoverable error occurred. **Terminal.** |

Lifecycle diagram (from [`task.rs`](../../crates/makina-core/src/task.rs) module doc):

```
new ──► ready ──► in-progress ──► in-review ──► done
                      │  ▲            │
                      │  └────────────┘
                      │  (changes requested: in-review → in-progress)
                      │
                      ▼
                   failed  ◄── in-progress  (gate-cap reached / hard error)
                   failed  ◄── in-review    (review-cap reached)
```

State-transition logic lives in the `task-state-machine` task; this document specifies the state **values** only.

### 6.1 The "Ready" Definition (Scheduling Contract)

> A task is **`ready`** (eligible for dispatch to a Developer agent) when **all tasks in its `depends_on` array are in state `done`**.

This is the scheduling contract. The computation of which tasks are currently ready is performed by the orchestrator (not stored in the artifact), but the definition is anchored here so that any component reading the artifact can derive the ready set independently.

Corollary: a task with an empty `depends_on` array has no prerequisites and transitions to `ready` as soon as it is added to a live graph.

---

## 7. Iteration Counts

| Field | Meaning | Valid range |
|-------|---------|-------------|
| `gate_iterations` | Incremented each time a task completes a Developer cycle but fails the automated gate check. | `[0, gate_cap]` where `gate_cap` is configured globally; reaching the cap moves the task to `failed`. |
| `review_iterations` | Incremented each time the Reviewer requests changes (task cycles back from `in-review` to `in-progress`). | `[0, review_cap]` where `review_cap` is configured globally; reaching the cap moves the task to `failed`. |

Both counts start at `0` when the task is created. The termination caps (`gate_cap`, `review_cap`) are runtime configuration values; their definition is deferred to the `termination-caps` task. Neither count is reset if a task is retried: they are cumulative across the lifetime of the task.

---

## 8. Timestamps

All timestamps are **RFC3339 strings in UTC**, e.g. `"2026-05-28T10:00:00Z"`. Sub-second precision is allowed (`"2026-05-28T10:00:00.123Z"`).

| Field | Present when | Example |
|-------|-------------|---------|
| `created_at` | Always | `"2026-05-01T10:00:00Z"` |
| `updated_at` | Always; updated on every mutation | `"2026-05-03T10:00:00Z"` |
| `started_at` | Once a Developer agent claims the task | `"2026-05-02T08:00:00Z"` |
| `finished_at` | Once the task reaches `done` or `failed` | `"2026-05-02T17:30:00Z"` |

`started_at` and `finished_at` are **omitted** (not `null`) until their lifecycle point occurs.

---

## 9. Validation Checklist

A conformant artifact passes all of the following checks. This mirrors the rules enforced by `TaskGraph::validate()` in `crates/makina-core/src/task.rs`, plus documented invariants.

### Structural
- [ ] The file is valid JSON.
- [ ] The top-level value is an object with `slug` (string) and `tasks` (array).
- [ ] `slug` is non-empty.

### Task-level
- [ ] Every task object has all required fields: `id`, `title`, `description`, `done_when`, `depends_on`, `state`, `gate_iterations`, `review_iterations`, `created_at`, `updated_at`.
- [ ] No task object has a `null` value for `section`, `started_at`, or `finished_at` — these keys must be absent, not `null`.

### Uniqueness
- [ ] All `id` values across the `tasks` array are unique (`TaskGraph::validate()` check 1).

### Dependency edges
- [ ] Every id in every `depends_on` array resolves to a task present in the same `tasks` array (`TaskGraph::validate()` check 2).

### State values
- [ ] Every `state` value is one of: `"new"`, `"ready"`, `"in-progress"`, `"in-review"`, `"done"`, `"failed"`.

### Timestamps
- [ ] `created_at` and `updated_at` parse as valid RFC3339 strings.
- [ ] `started_at` and `finished_at`, when present, parse as valid RFC3339 strings.
- [ ] `created_at <= updated_at` (updated time is never before creation time).
- [ ] `started_at`, when present, is `>= created_at`.
- [ ] `finished_at`, when present, is `>= started_at`.

### Counters
- [ ] `gate_iterations` and `review_iterations` are non-negative integers.

---

## 10. Relationship to the Structured-Text Convention

| Concept | In structured-text (`.md`) | In runtime artifact (`.json`) |
|---------|---------------------------|-------------------------------|
| Task id | `### {task-id} — …` heading | `"id"` field |
| State | Not present (input only) | `"state"` field |
| `Depends on` | Author-declared edges only | `depends_on` — may include Planner-inferred edges |
| `Done when` | Acceptance criterion text | `"done_when"` field (verbatim copy) |
| Section | `## NNNN — …` heading | `"section"` field (four-digit string) |
| Timestamps | Not present | `created_at`, `updated_at`, `started_at`, `finished_at` |
| Iteration counts | Not present | `gate_iterations`, `review_iterations` |

---

## 11. Out of Scope (FUTURE)

The following are explicitly deferred and must not appear in the current artifact:

- Schema version / migration fields — deferred to a future versioning task.
- Task priorities, urgency, or labels.
- Multiple acceptance criteria per task.
- Per-task gate or review cap overrides.
- Cycle detection in the dependency graph (scheduling concern; not part of `validate()`).

A canonical sample artifact is provided at
[`docs/spec/examples/sample-run.tasks.json`](examples/sample-run.tasks.json).

---

# Exchange Transcript Schema — `.makina/runs/{run_uid}/{task_id}_transcript.jsonl`

Version: 1.0
Status: Normative

## §T.1 Purpose

The exchange transcript is the **authoritative record of all communications** between the orchestrator and agents (Developer, Reviewer) during a task's execution. It captures every prompt sent, response chunk received, thought emitted, tool call made, and turn completion event in the exact order they occurred.

The transcript enables:
- **Replaying** a finished run with 100% visual fidelity — opening a closed run in the TUI shows the same Exchange pane as it did live.
- **Auditing** — inspecting the full decision chain that led to a task outcome.
- **Analysis and debugging** — understanding why a task took a particular path.

---

## §T.2 File Location and Ownership

| Property | Value |
|----------|-------|
| Path | `.makina/runs/{run_uid}/{task_id}_transcript.jsonl` inside the repository |
| Naming | File stem equals the task id (e.g. `my-task_transcript.jsonl` for task id `"my-task"`). Extension is `.jsonl` (JSON Lines, one object per line). |
| Committed | No — transcripts are ephemeral run artifacts, not version controlled. |
| Writer | **Orchestrator only** — the `make_sink` function appends each `AgentExchange` event as it is emitted. |
| Reader | TUI (replay module), audit tools, developers (direct file inspection). |

The Orchestrator is the **sole writer**. Each `Event::AgentExchange` is appended as a single JSON line immediately upon emission, in order.

---

## §T.3 File Format: JSON Lines (JSONL)

The transcript is stored as **JSON Lines**, one `ExchangeEvent` object per line:

```jsonl
{"type":"prompt_sent","text":"Implement the solo task"}
{"type":"response_chunk","text":"I'll start by "}
{"type":"response_chunk","text":"analyzing the requirements"}
{"type":"turn_complete"}
```

**Serialization:**
- Each line is a complete, standalone JSON object serialized with `serde_json::to_string`.
- Objects use the `#[serde(tag = "type", rename_all = "snake_case")]` tagging scheme, so the `type` field is self-describing.
- Every line is terminated with a newline (`\n`); empty lines are ignored by readers.

**Deserialization:**
- Each line parses independently via `serde_json::from_str::<ExchangeEvent>(line)`.
- Malformed lines are skipped with a warning (best-effort); a single bad line does not corrupt the whole transcript.
- No external context (schema version, role, task id) is needed — the `type` field fully qualifies the object.

---

## §T.4 ExchangeEvent Object Structure

The `ExchangeEvent` enum (in `crates/makina-core/src/api.rs`) defines the event types. Each line is serialized as a tagged union with the `type` field indicating which variant it is.

### §T.4.1 PromptSent

```json
{"type":"prompt_sent","text":"Implement the task"}
```

Emitted when the orchestrator sends a prompt to an agent (Developer or Reviewer).

| Field | Type | Meaning |
|-------|------|---------|
| `type` | `"prompt_sent"` | Event type tag |
| `text` | string | Full text of the prompt dispatched to the agent |

### §T.4.2 ResponseChunk

```json
{"type":"response_chunk","text":"I'll "}
{"type":"response_chunk","text":"start by "}
```

Emitted as the agent's response is streamed in segments. Chunks accumulate to form the full response.

| Field | Type | Meaning |
|-------|------|---------|
| `type` | `"response_chunk"` | Event type tag |
| `text` | string | A fragment of the agent's response; never empty |

### §T.4.3 ThoughtChunk

```json
{"type":"thought_chunk","text":"Let me understand "}
```

Emitted as the agent's internal reasoning is streamed in segments. Thoughts are observational only and do not contribute to the final response text.

| Field | Type | Meaning |
|-------|------|---------|
| `type` | `"thought_chunk"` | Event type tag |
| `text` | string | A fragment of the agent's reasoning; never empty |

### §T.4.4 ToolCall

```json
{"type":"tool_call","id":"bash_123","title":"run tests","kind":"execute","status":"pending"}
```

Emitted when an agent announces a tool invocation.

| Field | Type | Meaning |
|-------|------|---------|
| `type` | `"tool_call"` | Event type tag |
| `id` | string | Stable identifier correlating this call with later updates |
| `title` | string | Human-readable title (may be empty) |
| `kind` | string \| null | Optional semantic kind (e.g. `"execute"`, `"edit"`) |
| `status` | string | Lifecycle status (e.g. `"pending"`, `"running"`, `"completed"`) |

### §T.4.5 ToolCallUpdate

```json
{"type":"tool_call_update","id":"bash_123","status":"running","title":null}
```

Emitted as an incremental status or title update for a previously-announced tool call.

| Field | Type | Meaning |
|-------|------|---------|
| `type` | `"tool_call_update"` | Event type tag |
| `id` | string | The id of the `ToolCall` this updates |
| `status` | string \| null | Updated lifecycle status (if the update carried one) |
| `title` | string \| null | Updated title (if the update carried one) |

### §T.4.6 TurnComplete

```json
{"type":"turn_complete"}
```

Emitted when the agent has finished its current response turn. Signals the end of a back-and-forth cycle; the next `PromptSent` begins a new turn.

| Field | Type | Meaning |
|-------|------|---------|
| `type` | `"turn_complete"` | Event type tag |

---

## §T.5 Ordering Guarantee

Events are appended to the transcript **in the exact order they are emitted**, preserving causal order and response segmentation. Because plan 0009 segments responses into `ResponseChunk` and `ThoughtChunk` events, a reader can faithfully reconstruct the streaming experience by replaying events in order.

**Invariant:** If event A's causal origin is event B, then A appears after B in the transcript.

---

## §T.6 Back-Compat and Versioning

Currently, no schema version is stored in the transcript. If the `ExchangeEvent` enum changes, older transcripts must be read by a decoder that understands the old schema.

Future enhancements may prepend a header line or embed a version in the first event. For now, the `ExchangeEvent` type definitions in `crates/makina-core/src/api.rs` are the single source of truth.
