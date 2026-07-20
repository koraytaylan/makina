# Typed plan-document convention

Status: Normative

Makina's structured source is a validated plan directory containing `SCOPE.md`, `ARCHITECTURE.md`, `STATUS.md`, and non-empty `tasks/*.md`. The normative authoring grammar, examples, lifecycle ownership, and validation commands are maintained in [the plan authoring contract](../plans/README.md).

The plan directory is the stable identity. Each task is one independently reviewable Markdown document with closed YAML frontmatter and a body whose title, ordered steps, and falsifiable completion criterion are validated. Makina projects these documents into its runtime graph; it does not interpret an alternative free-form task-list grammar.

Historical plans using the pre-cutover monolithic task-list convention remain repository records but are not executable inputs. Directories without `tasks/` are inert, and mixed old/new directories are invalid. No automatic generation fallback or migration path is defined.

Model-assisted authoring must return the same typed blueprint consumed by the Rust bundle renderer and loader. It cannot introduce a second schema or write plan/status files directly.
