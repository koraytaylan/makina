# Structured-Text Task List Convention

Version: 1.0  
Status: Normative

---

## 1. Purpose

This document specifies the markdown convention used to write **task lists** that the Makina Planner parses into a runtime task graph (`.tasks/{slug}.json`). It is the contract between human authors and the Planner.

The **canonical real-world example** is
[`docs/plans/0001-Initial/TASKS.md`](../plan/0001-Initial/TASKS.md).

---

## 2. Roles of the Two Artifacts

| Artifact | Owned by | Mutable by | Authority |
|----------|----------|------------|-----------|
| `TASKS.md` (or any `*.md` task list) | Human author | Human | Reference input only |
| `.tasks/{slug}.json` | Planner / Supervisor | Planner only | Source of truth at runtime |

The structured-text file is **read-only input** to the Planner. Once the Planner has emitted `.tasks/{slug}.json`, all orchestration (state, dependency edges, iteration counts, timestamps) lives in the JSON artifact. Edits to the markdown file do **not** automatically update a running task graph.

---

## 3. Document Structure

### 3.1 Grammar (EBNF-ish)

```
document        = title-line blank-line preamble separator section+

title-line      = "# " title-text newline
preamble        = preamble-block+
                  (* one or more paragraphs and/or block-lists *)

separator       = "---" newline

section         = section-heading blank-line task+ separator?
                  (* separator is required between sections;
                     the final section has no trailing separator *)

section-heading = "## " section-id " — " section-title newline
section-id      = DIGIT DIGIT DIGIT DIGIT          (* four-digit number *)
section-title   = TEXT                             (* human-readable title *)

task            = task-heading blank-line task-body blank-line?
task-heading    = "### " task-id " — " task-title newline
task-id         = kebab-id                         (* see §4 *)
task-title      = TEXT

task-body       = description-paragraph+ field-list
description-paragraph = paragraph newline+

field-list      = depends-field newline done-field
depends-field   = "- **Depends on:** " depends-value
depends-value   = em-dash                          (* "—": no dependencies *)
                | id-list                          (* one or more kebab-ids *)
id-list         = kebab-id ("," SPACE kebab-id)* continuation-line*
continuation-line = SPACE SPACE kebab-id ("," SPACE kebab-id)* newline
                  (* soft-wrapped continuation lines start with two spaces *)

done-field      = "- **Done when:** " done-text
done-text       = TEXT (newline SPACE SPACE TEXT)* (* continuation allowed *)

em-dash         = "—"   (* Unicode U+2014 *)
kebab-id        = [a-z] ([a-z0-9] | "-")* [a-z0-9]
                  (* lowercase letters, digits, hyphens; must start/end with
                     a lowercase letter or digit; minimum 2 characters *)
```

> **Note on soft-wrapping.** In TASKS.md the `Depends on` line for tasks with multiple dependencies wraps naturally at the column limit. The Planner treats any text after `**Depends on:**` up to — but not including — the next `**Done when:**` bullet as part of the dependency value. Comma-separated ids that appear on the wrapped portion of the line are still part of the same field.

### 3.2 Document-level rules

1. **Title (H1):** Exactly one `# …` heading at the top of the file. Required.
2. **Preamble:** Free-form paragraphs and/or bullet lists between the title and the first `---` separator. Required; must include a `**Conventions**` block that restates (or defers to) the semantics in §6.
3. **Section separators:** A `---` rule appears *between* sections (i.e., after all tasks in a section and before the next `## …` heading). The **final section has no trailing separator**.
4. **Section order:** Sections are ordered by their four-digit numeric id, ascending.
5. **Task order within a section:** Tasks appear in the order the author judges appropriate for narrative clarity. No ordering constraint is enforced by this spec.

### 3.3 Section-heading format

```
## NNNN — Title
```

- `NNNN` is a zero-padded four-digit integer (e.g., `0002`, `0007`).
- The separator between the id and title is ` — ` (space, U+2014 em-dash, space).
- The id namespace is global to the project; each four-digit code appears at most once across all task-list files.

### 3.4 Task-heading format

```
### {task-id} — {human title}
```

- `{task-id}` follows the rules in §4.
- The separator is ` — ` (space, U+2014 em-dash, space), identical to section headings.
- `{human title}` is a short, human-readable label. No length limit is enforced, but brevity is preferred.

---

## 4. Task ID Rules

A **task id** is the stable, kebab-case identifier for a task.

| Property | Rule |
|----------|------|
| Character set | Lowercase ASCII letters (`a–z`), digits (`0–9`), hyphens (`-`) |
| Start / end | Must start and end with a lowercase letter or digit (not a hyphen) |
| Minimum length | 2 characters |
| Case | Always lowercase; no uppercase letters |
| Uniqueness | Must be unique across **all sections of all task-list files** in the project |
| Stability | Once published, a task id must not be renamed (it becomes the branch name `task/{id}` and worktree path `.worktrees/{id}/`) |

Examples of valid ids: `workspace-scaffold`, `task-model`, `e2e-run`  
Examples of invalid ids: `WorkspaceScaffold` (uppercase), `-task` (leading hyphen), `t` (single character), `task--model` (double hyphen is technically valid per the grammar but discouraged)

---

## 5. Task Fields

Each task has exactly three components:

### 5.1 Description (required)

One or more paragraphs of free-form markdown immediately below the task heading. May include inline code, links, and emphasis. Ends at the start of the `field-list`.

### 5.2 `Depends on` field (required)

```
- **Depends on:** {value}
```

| Value | Meaning |
|-------|---------|
| `—` (U+2014) | The task has no declared prerequisites |
| `id1, id2, …` | Comma-separated list of task ids (direct structural prerequisites) |

- Each referenced id must resolve to a task defined in the same task list (or a task list in scope for this project — forward references within the same file are allowed).
- The value may soft-wrap; continuation text is treated as part of the same field until the next `- **Done when:**` bullet.
- **Semantics:** the ids listed are *direct structural prerequisites* only. The Planner infers additional dependency edges for tasks that touch the same files or areas; those edges do not need to appear here.

### 5.3 `Done when` field (required)

```
- **Done when:** {acceptance criterion}
```

- Free-form text describing a verifiable acceptance condition.
- May soft-wrap; continuation text is treated as part of the same field.
- Used by gates and the Reviewer actor as the ground truth for task completion.

### 5.4 Optionality summary

| Field | Required? |
|-------|-----------|
| Description | Yes (at least one paragraph) |
| `Depends on` | Yes (use `—` for none) |
| `Done when` | Yes |

No other fields are currently defined. Unknown bullet lines in the field list are treated as parse errors by a conformant Planner.

---

## 6. Planner Semantics

These rules are authoritative for how the Planner must interpret a conformant task list.

1. **Reference input only.** The structured-text file is read once by the Planner to produce `.tasks/{slug}.json`. All subsequent orchestration uses the JSON artifact.

2. **`Depends on` = direct structural prerequisites.** The edges declared in the field are the minimum dependency graph from the author's perspective. Tasks in a later slice generally assume earlier slices are complete even when not listed.

3. **Automatic dependency inference.** The Planner adds further dependency edges for tasks that touch the same files or areas (detected by heuristic or model reasoning). These inferred edges appear only in the JSON artifact, not in the markdown.

4. **`Done when` = acceptance criterion.** The Planner copies this text verbatim into the JSON artifact. Gates and the Reviewer actor use it as the concrete check for task completion.

5. **Section id as ordering hint.** The four-digit section id is a scheduling hint: tasks in section `0003` should generally complete before tasks in section `0004` begin, although the Planner may decide otherwise based on the computed dependency graph.

6. **Uniqueness enforcement.** The Planner rejects a task list in which any task id appears more than once.

---

## 7. Worked Example

The following is a minimal, self-contained task list demonstrating every rule.

```markdown
# Example Project — Build Task List

Structured-text task list for Example Project.

**Conventions**
- Each task has a stable kebab-case **id** (also used for its branch
  `task/{id}` and worktree `.worktrees/{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- The Planner adds further dependency edges automatically.
- **Done when** is the acceptance check.

---

## 0001 — Foundation

### init-repo — Initialise the repository
Create the Git repository, add `.gitignore`, and push an initial commit.
- **Depends on:** —
- **Done when:** `git log` shows the initial commit and `.gitignore` is
  present.

### add-ci — Add CI pipeline
Add a GitHub Actions workflow that runs `cargo test` on every push.
- **Depends on:** init-repo
- **Done when:** a push to `main` triggers the CI workflow and it passes.

---

## 0002 — Core Library

### core-lib — Create core library crate
Scaffold the `core` crate with a public API module and passing unit
tests.
- **Depends on:** init-repo, add-ci
- **Done when:** `cargo test -p core` passes and the public API module
  is documented.
```

### 7.1 What the example demonstrates

| Rule | Location in example |
|------|---------------------|
| H1 title | Line 1 |
| Preamble with `**Conventions**` block | Lines 3–10 |
| `---` separator between sections | Lines 12 and 26 |
| Section heading `## NNNN — Title` | Lines 14, 28 |
| Task heading `### {id} — {title}` | Lines 16, 21, 30 |
| `Depends on: —` (no dependencies) | Line 19 |
| Single dependency | Line 24 |
| Multiple dependencies | Line 34 |
| Soft-wrapped `Done when` | Lines 18–20 |
| No trailing separator after last section | (absent after `## 0002`) |

---

## 8. Conformance Checklist

A reviewer (human or automated) verifies conformance by checking each item.

### 8.1 Document level

- [ ] Exactly one H1 (`# …`) title at the top.
- [ ] Preamble present between the title and the first `---`.
- [ ] Preamble includes a `**Conventions**` block.
- [ ] Sections are separated by `---` rules (no trailing `---` after the last section).
- [ ] Section ids are four-digit, zero-padded, and appear in ascending order.

### 8.2 Section level

- [ ] Each section heading matches `## NNNN — Title` (space + em-dash + space).
- [ ] Each section id is unique within the document.

### 8.3 Task level

- [ ] Each task heading matches `### {task-id} — {title}`.
- [ ] `{task-id}` is kebab-case (lowercase, no leading/trailing hyphen).
- [ ] Each task id is unique across all sections.
- [ ] At least one description paragraph is present.
- [ ] `- **Depends on:**` field is present; value is `—` or a comma-separated list of valid task ids.
- [ ] `- **Done when:**` field is present; value is non-empty text.
- [ ] No additional bullet fields appear in the field list.

### 8.4 Cross-reference

- [ ] Every id listed in a `Depends on` field resolves to a task in scope.
- [ ] No circular dependency chains exist in the declared graph.

---

## 9. Out of Scope (FUTURE)

The following features are **not** part of this convention and must not be added by a conformant parser:

- Task priorities or urgency levels
- Per-task tags or labels
- Per-task gate override configuration
- Multiple `Done when` criteria per task
- Inline status markers (`[x]`, `[ ]`)

These are tracked as future work in
[`docs/plans/0001-Initial/FUTURE.md`](../plan/0001-Initial/FUTURE.md).
