//! Task-list interpreter seam and deterministic reference implementation.
//!
//! # Seam
//!
//! [`TaskListInterpreter`] is the trait every interpreter must implement.  It
//! takes raw structured-text source (the markdown conventions defined in
//! `docs/spec/structured-text-convention.md`) and returns a validated
//! [`TaskGraph`].
//!
//! The seam exists so that:
//! - **Task 14 (this task)** ships a deterministic, zero-cost reference
//!   implementation ([`StructuredTextInterpreter`]) that parses the markdown
//!   directly.  No network, no model, fully deterministic — safe for tests.
//! - **Task 18 (`planner-model-mechanism`)** adds a model-backed implementation
//!   behind the same trait by calling a language model to produce the graph.
//!   Swapping interpreters is a one-line change in [`PlannerArgs`].
//! - **Task 17 (`dependency-detection`)** augments the `depends_on` arrays with
//!   inferred edges by wrapping or extending an existing interpreter.  The trait
//!   boundary keeps task 17 isolated from the parser.
//!
//! # Why a trait and not an enum?
//!
//! Using a trait (`Arc<dyn TaskListInterpreter>`) keeps each concern in its own
//! compilation unit and makes the model-backed variant fully opt-in at runtime
//! (e.g. disabled in tests and offline environments).
//!
//! # Contract
//!
//! Implementors MUST:
//! - Be `Send + Sync` so they can live inside an actor (`Arc<dyn …>`).
//! - Return a [`TaskGraph`] that passes [`TaskGraph::validate()`] — if it does
//!   not, wrap the error in [`InterpretError::ValidationFailed`].
//! - Return [`InterpretError`] on any parse or semantic failure; never panic.

use async_trait::async_trait;
use chrono::Utc;
use thiserror::Error;

use crate::task::{Task, TaskGraph, TaskGraphError, TaskId, TaskState};

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors that [`TaskListInterpreter::interpret`] may return.
///
/// Callers (e.g. the [`Planner`](crate::actors::planner::Planner) actor) convert
/// these into a user-visible error message and surface them in the reply.
#[derive(Debug, Error)]
pub enum InterpretError {
    /// The source text does not conform to the structured-text convention.
    ///
    /// `context` is a short human-readable description of what was expected
    /// (e.g. `"task heading missing '—' separator"`) and `location` is a
    /// 1-based line number (or `0` if the location is unknown).
    #[error("parse error at line {location}: {context}")]
    ParseError {
        /// 1-based source line where the error was detected (0 = unknown).
        location: usize,
        /// Human-readable description of the problem.
        context: String,
    },

    /// The resulting [`TaskGraph`] did not pass [`TaskGraph::validate()`].
    ///
    /// This covers duplicate task ids and dangling `depends_on` references.
    #[error("task graph validation failed: {0}")]
    ValidationFailed(#[from] TaskGraphError),
}

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Converts structured-text source into a validated runtime [`TaskGraph`].
///
/// # Implementations
///
/// | Type | Added in | Notes |
/// |------|----------|-------|
/// | [`StructuredTextInterpreter`] | Task 14 | Deterministic markdown parser; no model. |
/// | `ModelInterpreter` (pending)  | Task 18 | Makes a real model call to produce the graph. |
///
/// # Seam for task 17
///
/// Task 17 (`dependency-detection`) will augment `depends_on` arrays with
/// inferred edges.  The recommended approach is a decorator that wraps any
/// existing `TaskListInterpreter` and post-processes its output — the trait
/// boundary makes this composable without modifying the base implementation.
///
/// # Seam for task 18
///
/// Task 18 (`planner-model-mechanism`) will provide a `ModelInterpreter` that
/// calls a language model and returns a `TaskGraph`.  The [`Planner`] actor
/// receives its interpreter via dependency injection (`Arc<dyn
/// TaskListInterpreter>` in [`PlannerArgs`]); replacing the interpreter is a
/// one-line change at the call site.
///
/// [`Planner`]: crate::actors::planner::Planner
#[async_trait]
pub trait TaskListInterpreter: Send + Sync {
    /// Interpret structured task-list text into a runtime task graph.
    ///
    /// # Parameters
    ///
    /// - `slug` — The file-stem identifier that will become `TaskGraph::slug`
    ///   (e.g. `"my-feature"` from `my-feature.md`).
    /// - `source_text` — The full text of the task-list document, conforming to
    ///   `docs/spec/structured-text-convention.md`.
    ///
    /// # Contract
    ///
    /// Returns a [`TaskGraph`] that has already passed [`TaskGraph::validate()`].
    /// Any structural problem (duplicate id, dangling dep) surfaces as
    /// [`InterpretError::ValidationFailed`]; any syntax problem surfaces as
    /// [`InterpretError::ParseError`].
    ///
    /// Implementors MUST NOT write to `.tasks/` or any other file — the
    /// Supervisor is the sole writer of the artifact (via [`SetTaskGraph`]).
    ///
    /// [`SetTaskGraph`]: crate::actors::supervisor::SetTaskGraph
    async fn interpret(&self, slug: &str, source_text: &str) -> Result<TaskGraph, InterpretError>;
}

// ── Reference implementation ──────────────────────────────────────────────────

/// Deterministic reference implementation of [`TaskListInterpreter`].
///
/// Parses the structured-text convention documented in
/// `docs/spec/structured-text-convention.md` without making any model calls.
/// Every field is extracted directly from the markdown source:
///
/// | Field | Source |
/// |-------|--------|
/// | `id` | `### {id} — …` heading |
/// | `title` | `### … — {title}` heading |
/// | `description` | Paragraph(s) between the heading and the field list |
/// | `done_when` | `- **Done when:** {text}` (may soft-wrap) |
/// | `depends_on` | `- **Depends on:** {ids}` (only explicit ids; `—` → `[]`) |
/// | `section` | The `NNNN` of the enclosing `## NNNN — …` heading |
/// | `state` | Always [`TaskState::New`] (freshly created) |
/// | `gate_iterations` | `0` |
/// | `review_iterations` | `0` |
/// | `created_at` / `updated_at` | `Utc::now()` at parse time |
/// | `started_at` / `finished_at` | `None` |
///
/// # Scope
///
/// This implementation captures **only** the explicitly-written `Depends on`
/// edges.  Cross-cutting edge inference (tasks touching the same files/areas) is
/// the concern of task 17 (`dependency-detection`), which will post-process the
/// graph produced here.
///
/// # Future model-backed interpreter
///
/// Task 18 (`planner-model-mechanism`) will add a `ModelInterpreter` that calls a
/// language model.  Both implementations share the same [`TaskListInterpreter`]
/// trait; the [`Planner`] actor accepts either via `Arc<dyn TaskListInterpreter>`.
///
/// [`Planner`]: crate::actors::planner::Planner
pub struct StructuredTextInterpreter;

impl StructuredTextInterpreter {
    /// Create a new `StructuredTextInterpreter`.
    pub fn new() -> Self {
        Self
    }
}

impl Default for StructuredTextInterpreter {
    fn default() -> Self {
        Self::new()
    }
}

// ── Parser internals ──────────────────────────────────────────────────────────

/// Internal: raw task data collected from a single `###` block before it is
/// turned into a [`Task`].
#[derive(Debug, Default)]
struct RawTask {
    id: String,
    title: String,
    /// 1-based line number of the `###` heading (for error messages).
    heading_line: usize,
    section: String,
    description_lines: Vec<String>,
    depends_on_raw: Option<String>,
    done_when_raw: Option<String>,
}

/// State machine used while scanning the document line by line.
#[derive(Debug, PartialEq)]
enum ParseState {
    /// Before the first `---` separator (title / preamble).
    Preamble,
    /// Inside a `## NNNN — …` section, before any `###` task heading.
    InSection { section_id: String },
    /// Inside a `###` task block, collecting description lines.
    InTaskDescription,
    /// Saw `- **Depends on:**`; reading the multi-line value.
    InDependsOn,
    /// Saw `- **Done when:**`; reading the multi-line value.
    InDoneWhen,
}

#[async_trait]
impl TaskListInterpreter for StructuredTextInterpreter {
    async fn interpret(&self, slug: &str, source_text: &str) -> Result<TaskGraph, InterpretError> {
        parse_structured_text(slug, source_text)
    }
}

/// Pure parsing logic — separated from the async trait impl for easy unit testing.
pub(crate) fn parse_structured_text(
    slug: &str,
    source_text: &str,
) -> Result<TaskGraph, InterpretError> {
    let mut state = ParseState::Preamble;
    let mut raw_tasks: Vec<RawTask> = Vec::new();
    let mut current_task: Option<RawTask> = None;

    for (idx, raw_line) in source_text.lines().enumerate() {
        let line_no = idx + 1; // 1-based

        // ── Section heading: `## NNNN — Title` ───────────────────────────────
        if let Some(rest) = raw_line.strip_prefix("## ") {
            // Commit any in-progress task before entering the new section.
            if let Some(task) = current_task.take() {
                finish_task(task, &mut raw_tasks, line_no)?;
            }

            // Extract the section id from `NNNN — …`.
            let (section_id, _section_title) = split_em_dash(rest, line_no, "section heading")?;
            let section_id = section_id.trim().to_string();
            state = ParseState::InSection {
                section_id: section_id.clone(),
            };
            continue;
        }

        // ── Task heading: `### {id} — {title}` ───────────────────────────────
        if let Some(rest) = raw_line.strip_prefix("### ") {
            // Commit the previous task (if any) before starting a new one.
            if let Some(prev) = current_task.take() {
                finish_task(prev, &mut raw_tasks, line_no)?;
            }

            let section_id = match &state {
                ParseState::InSection { section_id } => section_id.clone(),
                ParseState::InTaskDescription
                | ParseState::InDependsOn
                | ParseState::InDoneWhen => {
                    // We're still inside a previous task's block — pick up the section from
                    // the last committed task, or default to empty.
                    raw_tasks
                        .last()
                        .map(|t| t.section.clone())
                        .unwrap_or_default()
                }
                ParseState::Preamble => {
                    return Err(InterpretError::ParseError {
                        location: line_no,
                        context: "task heading found before any section heading".to_string(),
                    });
                }
            };

            let (task_id, task_title) = split_em_dash(rest, line_no, "task heading")?;
            let task_id = task_id.trim().to_string();
            let task_title = task_title.trim().to_string();

            current_task = Some(RawTask {
                id: task_id,
                title: task_title,
                heading_line: line_no,
                section: section_id,
                ..Default::default()
            });
            state = ParseState::InTaskDescription;
            continue;
        }

        // ── Field: `- **Depends on:** …` ─────────────────────────────────────
        if let Some(rest) = raw_line.strip_prefix("- **Depends on:** ") {
            let task = current_task
                .as_mut()
                .ok_or_else(|| InterpretError::ParseError {
                    location: line_no,
                    context: "`- **Depends on:**` found outside a task block".to_string(),
                })?;
            task.depends_on_raw = Some(rest.to_string());
            state = ParseState::InDependsOn;
            continue;
        }

        // ── Field: `- **Done when:** …` ───────────────────────────────────────
        if let Some(rest) = raw_line.strip_prefix("- **Done when:** ") {
            let task = current_task
                .as_mut()
                .ok_or_else(|| InterpretError::ParseError {
                    location: line_no,
                    context: "`- **Done when:**` found outside a task block".to_string(),
                })?;
            task.done_when_raw = Some(rest.to_string());
            state = ParseState::InDoneWhen;
            continue;
        }

        // ── Separator: `---` ──────────────────────────────────────────────────
        if raw_line == "---" {
            // Commit any in-progress task.
            if let Some(task) = current_task.take() {
                finish_task(task, &mut raw_tasks, line_no)?;
            }
            // After a separator we expect either a new section or EOF; stay in the
            // current section context (the next `##` will override state).
            if let ParseState::InSection { section_id } = &state {
                state = ParseState::InSection {
                    section_id: section_id.clone(),
                };
            } else {
                state = ParseState::Preamble;
            }
            continue;
        }

        // ── Continuation lines for multi-line fields ──────────────────────────
        match &state {
            ParseState::InDependsOn => {
                // A continuation line starts with two spaces (soft-wrap) or may be
                // a blank line (acts as natural separator — stop the field).
                if raw_line.trim().is_empty() {
                    // blank line ends the field
                    state = ParseState::InTaskDescription;
                } else {
                    // Append to depends_on_raw regardless of indentation.
                    if let Some(task) = current_task.as_mut() {
                        let existing = task.depends_on_raw.get_or_insert_with(String::new);
                        existing.push(' ');
                        existing.push_str(raw_line.trim());
                    }
                }
                continue;
            }
            ParseState::InDoneWhen => {
                if raw_line.trim().is_empty() {
                    state = ParseState::InTaskDescription;
                } else {
                    // Check for unexpected new bullet that is not a known field.
                    // (Unknown bullets become a parse error per the spec.)
                    if raw_line.starts_with("- **") {
                        return Err(InterpretError::ParseError {
                            location: line_no,
                            context: format!(
                                "unexpected bullet field `{}` in task block; \
                                 only `Depends on` and `Done when` are allowed",
                                raw_line.trim()
                            ),
                        });
                    }
                    if let Some(task) = current_task.as_mut() {
                        let existing = task.done_when_raw.get_or_insert_with(String::new);
                        existing.push(' ');
                        existing.push_str(raw_line.trim());
                    }
                }
                continue;
            }
            ParseState::InTaskDescription => {
                // Check for unexpected bullet fields (only Depends on / Done when are valid).
                if raw_line.starts_with("- **") {
                    return Err(InterpretError::ParseError {
                        location: line_no,
                        context: format!(
                            "unexpected bullet field `{}` in task block; \
                             only `- **Depends on:**` and `- **Done when:**` are allowed",
                            raw_line.trim()
                        ),
                    });
                }
                // Collect description lines (skip leading blanks; stop at field list).
                if let Some(task) = current_task.as_mut() {
                    let trimmed = raw_line.trim();
                    if !trimmed.is_empty() || !task.description_lines.is_empty() {
                        task.description_lines.push(raw_line.to_string());
                    }
                }
                continue;
            }
            _ => {
                // Preamble / InSection: ignore non-heading, non-separator lines.
            }
        }
    }

    // ── Commit final in-progress task ─────────────────────────────────────────
    if let Some(task) = current_task.take() {
        let eof_line = source_text.lines().count() + 1;
        finish_task(task, &mut raw_tasks, eof_line)?;
    }

    // ── Convert raw tasks → domain Tasks ─────────────────────────────────────
    let now = Utc::now();
    let mut tasks: Vec<Task> = Vec::with_capacity(raw_tasks.len());

    for raw in raw_tasks {
        let depends_on =
            parse_depends_on_field(raw.depends_on_raw.as_deref(), raw.heading_line, &raw.id)?;

        let done_when = raw
            .done_when_raw
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| InterpretError::ParseError {
                location: raw.heading_line,
                context: format!("task `{}` is missing the `- **Done when:**` field", raw.id),
            })?
            .trim()
            .to_string();

        // Trim trailing blank lines from description; join into one string.
        let mut desc_lines = raw.description_lines;
        while desc_lines
            .last()
            .map(|l: &String| l.trim().is_empty())
            .unwrap_or(false)
        {
            desc_lines.pop();
        }
        let description = desc_lines
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();

        if description.is_empty() {
            return Err(InterpretError::ParseError {
                location: raw.heading_line,
                context: format!("task `{}` has no description paragraph", raw.id),
            });
        }

        tasks.push(Task {
            id: TaskId::new(raw.id),
            title: raw.title,
            description,
            done_when,
            depends_on,
            section: if raw.section.is_empty() {
                None
            } else {
                Some(raw.section)
            },
            state: TaskState::New,
            gate_iterations: 0,
            review_iterations: 0,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
        });
    }

    // ── Validate ──────────────────────────────────────────────────────────────
    let graph = TaskGraph {
        slug: slug.to_string(),
        tasks,
    };
    graph.validate()?;

    Ok(graph)
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Split `text` on the first ` — ` (space + U+2014 + space) separator.
///
/// Returns `(before, after)` as string slices.  Fails with a
/// [`InterpretError::ParseError`] if the separator is absent.
fn split_em_dash<'a>(
    text: &'a str,
    line_no: usize,
    context: &str,
) -> Result<(&'a str, &'a str), InterpretError> {
    // The spec uses U+2014 EM DASH surrounded by single spaces.
    const SEP: &str = " \u{2014} ";
    text.split_once(SEP)
        .ok_or_else(|| InterpretError::ParseError {
            location: line_no,
            context: format!("{context} is missing the ` — ` separator (space + U+2014 + space)"),
        })
}

/// Validate that a completed [`RawTask`] has the required fields and push it
/// onto `raw_tasks`.
fn finish_task(
    task: RawTask,
    raw_tasks: &mut Vec<RawTask>,
    _current_line: usize,
) -> Result<(), InterpretError> {
    // Minimal validation: must have an id (set during construction).
    if task.id.is_empty() {
        return Err(InterpretError::ParseError {
            location: task.heading_line,
            context: "task heading has an empty id".to_string(),
        });
    }
    // The `depends_on` and `done_when` fields are validated later when we have
    // all tasks available (for forward-reference resolution).
    raw_tasks.push(task);
    Ok(())
}

/// Parse the raw `Depends on` field value into a `Vec<TaskId>`.
///
/// Accepts:
/// - `—` (U+2014) → empty vec
/// - comma-separated kebab ids (possibly multi-line, already joined by the
///   caller into a single string)
fn parse_depends_on_field(
    raw: Option<&str>,
    heading_line: usize,
    task_id: &str,
) -> Result<Vec<TaskId>, InterpretError> {
    let raw = raw.ok_or_else(|| InterpretError::ParseError {
        location: heading_line,
        context: format!("task `{task_id}` is missing the `- **Depends on:**` field"),
    })?;

    let raw = raw.trim();

    // Em-dash means no dependencies.
    if raw == "\u{2014}" {
        return Ok(vec![]);
    }

    // Otherwise: comma-separated list of kebab ids.
    // Split on commas, trim whitespace and any trailing continuation newlines.
    let ids: Vec<TaskId> = raw
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| TaskId::new(s.to_string()))
        .collect();

    if ids.is_empty() {
        return Err(InterpretError::ParseError {
            location: heading_line,
            context: format!(
                "task `{task_id}` has an empty `Depends on` value \
                 (use `—` for no dependencies)"
            ),
        });
    }

    Ok(ids)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Worked example from the spec ──────────────────────────────────────────

    /// The worked example in `docs/spec/structured-text-convention.md` §7 must
    /// parse cleanly and produce a graph with 3 tasks and correct edges.
    #[test]
    fn spec_worked_example_parses_correctly() {
        let source = r#"# Example Project — Build Task List

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
"#;

        let graph = parse_structured_text("example", source)
            .expect("spec worked example must parse without error");

        assert_eq!(graph.slug, "example");
        assert_eq!(graph.tasks.len(), 3, "should have 3 tasks");

        graph.validate().expect("graph must be valid");

        // init-repo: no deps
        let init_repo = graph.tasks.iter().find(|t| t.id.0 == "init-repo").unwrap();
        assert_eq!(init_repo.title, "Initialise the repository");
        assert!(init_repo.depends_on.is_empty());
        assert_eq!(init_repo.section.as_deref(), Some("0001"));
        assert_eq!(init_repo.state, TaskState::New);
        assert_eq!(init_repo.gate_iterations, 0);
        assert_eq!(init_repo.review_iterations, 0);

        // add-ci: depends on init-repo
        let add_ci = graph.tasks.iter().find(|t| t.id.0 == "add-ci").unwrap();
        assert_eq!(add_ci.depends_on, vec![TaskId::new("init-repo")]);
        assert_eq!(add_ci.section.as_deref(), Some("0001"));

        // core-lib: depends on init-repo and add-ci
        let core_lib = graph.tasks.iter().find(|t| t.id.0 == "core-lib").unwrap();
        assert_eq!(
            core_lib.depends_on,
            vec![TaskId::new("init-repo"), TaskId::new("add-ci")]
        );
        assert_eq!(core_lib.section.as_deref(), Some("0002"));
        assert!(core_lib.description.contains("Scaffold"));
    }

    /// A small representative task list with multiple sections and soft-wrapped
    /// `Done when` lines.
    #[test]
    fn representative_sample_parses_and_validates() {
        let source = r#"# My Project — Task List

Preamble describing the project.

**Conventions**
- ids are kebab-case.

---

## 0001 — Bootstrap

### scaffold — Scaffold the repo
Create the repository and add the initial files needed to get started.
- **Depends on:** —
- **Done when:** `git status` shows the repository is clean.

### add-lint — Add linting
Configure linting tools for the project to catch issues early in the
development process.
- **Depends on:** scaffold
- **Done when:** running the linter produces no errors on the initial
  codebase.

---

## 0002 — Core

### core-impl — Implement core logic
Implement the core business logic for the project, including all
required data structures and algorithms.
- **Depends on:** scaffold, add-lint
- **Done when:** unit tests cover the core logic and all pass.

### docs — Write documentation
Write developer documentation for the project.
- **Depends on:** core-impl
- **Done when:** all public functions have doc comments.
"#;

        let graph =
            parse_structured_text("my-project", source).expect("sample must parse without error");

        assert_eq!(graph.slug, "my-project");
        assert_eq!(graph.tasks.len(), 4);
        graph.validate().expect("must be valid");

        // scaffold: no deps, section 0001
        let scaffold = graph.tasks.iter().find(|t| t.id.0 == "scaffold").unwrap();
        assert!(scaffold.depends_on.is_empty());
        assert_eq!(scaffold.section.as_deref(), Some("0001"));
        assert_eq!(scaffold.state, TaskState::New);

        // add-lint: depends on scaffold, section 0001
        let add_lint = graph.tasks.iter().find(|t| t.id.0 == "add-lint").unwrap();
        assert_eq!(add_lint.depends_on, vec![TaskId::new("scaffold")]);
        assert_eq!(add_lint.section.as_deref(), Some("0001"));

        // core-impl: depends on scaffold and add-lint, section 0002
        let core_impl = graph.tasks.iter().find(|t| t.id.0 == "core-impl").unwrap();
        assert_eq!(
            core_impl.depends_on,
            vec![TaskId::new("scaffold"), TaskId::new("add-lint")]
        );
        assert_eq!(core_impl.section.as_deref(), Some("0002"));

        // docs: depends on core-impl, section 0002
        let docs = graph.tasks.iter().find(|t| t.id.0 == "docs").unwrap();
        assert_eq!(docs.depends_on, vec![TaskId::new("core-impl")]);
        assert_eq!(docs.section.as_deref(), Some("0002"));

        // All tasks are New with zero counters
        for task in &graph.tasks {
            assert_eq!(task.state, TaskState::New);
            assert_eq!(task.gate_iterations, 0);
            assert_eq!(task.review_iterations, 0);
            assert!(task.started_at.is_none());
            assert!(task.finished_at.is_none());
        }
    }

    /// Soft-wrapped `Done when` produces a single, clean string.
    #[test]
    fn soft_wrapped_done_when_is_joined() {
        let source = r#"# T

Preamble.

---

## 0001 — X

### foo — Foo task
Does foo.
- **Depends on:** —
- **Done when:** the widget renders correctly in all supported browsers
  and accessibility checks pass.
"#;

        let graph = parse_structured_text("t", source).unwrap();
        let task = graph.tasks.iter().find(|t| t.id.0 == "foo").unwrap();
        assert!(
            task.done_when.contains("widget renders"),
            "done_when should contain first line"
        );
        assert!(
            task.done_when.contains("accessibility"),
            "done_when should contain continued text"
        );
    }

    /// A task with a dangling `Depends on` reference fails validation.
    #[test]
    fn dangling_depends_on_fails_with_clear_error() {
        let source = r#"# T

Preamble.

---

## 0001 — X

### bar — Bar task
Does bar.
- **Depends on:** ghost-task
- **Done when:** it works.
"#;

        let err =
            parse_structured_text("t", source).expect_err("dangling dep should cause an error");

        match &err {
            InterpretError::ValidationFailed(graph_err) => {
                let msg = graph_err.to_string();
                assert!(
                    msg.contains("ghost-task"),
                    "error should mention the missing id; got: {msg}"
                );
            }
            other => panic!("expected ValidationFailed, got: {other:?}"),
        }
    }

    /// Missing `Done when` field produces a clear `ParseError`.
    #[test]
    fn missing_done_when_is_a_parse_error() {
        let source = r#"# T

Preamble.

---

## 0001 — X

### baz — Baz task
Does baz.
- **Depends on:** —
"#;

        let err =
            parse_structured_text("t", source).expect_err("missing Done when should be an error");

        assert!(
            matches!(err, InterpretError::ParseError { .. }),
            "expected ParseError, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("baz") || msg.contains("Done when"),
            "error message should mention the task or field: {msg}"
        );
    }

    /// Duplicate task ids trigger `ValidationFailed`.
    #[test]
    fn duplicate_task_ids_fail_validation() {
        let source = r#"# T

Preamble.

---

## 0001 — X

### same-id — First task
Does the first thing.
- **Depends on:** —
- **Done when:** first thing done.

### same-id — Second task
Does the second thing.
- **Depends on:** —
- **Done when:** second thing done.
"#;

        let err = parse_structured_text("t", source).expect_err("duplicate id should fail");

        assert!(
            matches!(err, InterpretError::ValidationFailed(_)),
            "expected ValidationFailed, got: {err:?}"
        );
    }

    /// Forward references in `Depends on` resolve (task 17 adds inference on top).
    #[test]
    fn forward_reference_in_depends_on_resolves() {
        let source = r#"# T

Preamble.

---

## 0001 — X

### first — First task
Description of first.
- **Depends on:** —
- **Done when:** first is done.

### second — Second task
Description of second.
- **Depends on:** third
- **Done when:** second is done.

### third — Third task
Description of third.
- **Depends on:** first
- **Done when:** third is done.
"#;

        let graph = parse_structured_text("t", source).expect("forward reference should be valid");
        graph.validate().expect("graph must be valid");

        let second = graph.tasks.iter().find(|t| t.id.0 == "second").unwrap();
        assert_eq!(second.depends_on, vec![TaskId::new("third")]);
    }
}
