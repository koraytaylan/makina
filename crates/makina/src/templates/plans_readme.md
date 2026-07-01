# Plans

This directory holds the project's implementation plans. A plan is authored by a
human — typically with an agent CLI such as the `create-plan` skill — and then
executed by Makina. Makina does not author plans itself; it reads plans that
follow the format below.

## Layout

One directory per plan, named `NNNN-Title-Case-Kebab/`, where `NNNN` is a
zero-padded 4-digit number one greater than the highest existing plan (the first
plan is `0001`). Each plan directory contains exactly four files:

- `SCOPE.md` — why the plan exists, what is in scope, what is explicitly out of
  scope, and any locked decisions.
- `ARCHITECTURE.md` — the concrete code deltas, grouped by workstream, ideally
  with real `path/to/file.rs:line` anchors.
- `TASKS.md` — the executable task list (contract below).
- `STATUS.md` — a status marker (`📋 Planned`, `🚧 In progress`, or `✅ Done`) and a
  table mapping each workstream to its task ids.

## TASKS.md contract

- A single `# ` title line at the top.
- Workstreams are `## NNNN — Workstream Title` headings.
- Each task is a `### {kebab-id} — {Title}` heading. The separator between id and
  title is: space, EM DASH (`—`, U+2014), space — NOT a hyphen.
- `{kebab-id}` is a unique, stable, lowercase-kebab identifier; it also names the
  task's git branch.
- After the task's prose (and an optional `**Steps:**` list), every task ends with
  exactly these two bullets:
  - `- **Depends on:** id-a, id-b` — the comma-separated ids of this task's DIRECT
    prerequisites, or a single `—` if it has none.
  - `- **Done when:** <criterion>` — one falsifiable completion criterion that
    includes the project's quality gates passing.
- The `Depends on` edges must form a directed acyclic graph, and tasks must appear
  in a valid topological order: every id referenced in a `Depends on` line must
  belong to a task defined EARLIER in the file.
