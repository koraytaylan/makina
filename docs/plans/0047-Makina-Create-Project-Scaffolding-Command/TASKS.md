# XAgent Plan 0047 — Project Scaffolding Command — makina create <path> [--template <name>]

This plan adds a zero-risk first-run path: it commits a complete `todo` starter template under `crates/makina/src/templates/todo/` (a minimal Rust binary crate, a committed `.makina/config.toml`, and a `docs/plans/0001-Todo-Starter` triad with junior-executable Depends-on/Done-when tasks) stored under neutral file names and embedded via `include_str!`; it adds `crates/makina/src/scaffold.rs` whose `scaffold_project` refuses a non-empty target, reuses `folder_init::initialize_folder` to bootstrap git + `main`/`develop` + `docs/plans/README.md`, then writes and commits the template content on `develop`; it extends the hand-rolled parser in `crates/makina/src/cli.rs` with a `Create { path, template }` action (required path, `--template` defaulting to and only accepting `todo`, unknown template lists available templates) plus a headless `Create`/`CreateError` dispatch in `crates/makina/src/main.rs` that prints what it created and never launches the TUI; it adds a hermetic integration test using `makina_core::test_support` git helpers that asserts the git structure, the sample config, and the plan triad; and it refreshes `README.md` to recommend `makina create` as the safe first-run path — all with the three quality gates green.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md) for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}`).
- **Depends on** lists *direct* prerequisites only; "—" means none.
- **Done when** is the verifiable criterion, and every task keeps the gate
  commands green — the full forms are `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`;
  abbreviated as "cargo test / clippy / fmt green" thereafter.
- GPU tests self-skip without an adapter.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0001 — Embedded `todo` Template

### add-todo-template-files — Commit The Embeddable `todo` Starter Template Under Neutral File Names

There is no runnable sample project for a `create` command to stamp out: `crates/makina/src/templates/` holds only `plans_readme.md`, embedded via `include_str!("templates/plans_readme.md")` in `folder_init::initialize_folder` (`crates/makina/src/folder_init.rs:75`). This task commits a complete `todo` project as STATIC files under neutral names (so no literal `Cargo.toml` sits in the src tree — the root workspace uses `members = ["crates/*"]`, matching only direct children, and the existing `plans_readme.md`→`README.md` rename pattern is the house convention). These files are plain data; nothing embeds them yet, so this task compiles alone.

**Steps:**

1. Create directory `crates/makina/src/templates/todo/`.

2. Create `crates/makina/src/templates/todo/cargo_toml` (destined for `Cargo.toml`) with exactly:

   ```toml
   [package]
   name = "todo"
   version = "0.1.0"
   edition = "2021"

   [dependencies]
   ```

3. Create `crates/makina/src/templates/todo/main_rs` (destined for `src/main.rs`) with exactly this fmt- and clippy-clean skeleton carrying a passing test:

   ```rust
   //! A tiny todo list — the Makina `todo` starter project.

   /// A single todo item.
   #[derive(Debug, Clone, PartialEq, Eq)]
   pub struct Task {
       pub title: String,
       pub done: bool,
   }

   impl Task {
       /// Create a new, not-yet-done task.
       pub fn new(title: &str) -> Self {
           Self {
               title: title.to_string(),
               done: false,
           }
       }
   }

   fn main() {
       let tasks = [Task::new("write my first Makina task list")];
       for task in &tasks {
           let mark = if task.done { "x" } else { " " };
           println!("[{mark}] {}", task.title);
       }
   }

   #[cfg(test)]
   mod tests {
       use super::*;

       #[test]
       fn new_task_is_not_done() {
           let t = Task::new("demo");
           assert_eq!(t.title, "demo");
           assert!(!t.done);
       }
   }
   ```

4. Create `crates/makina/src/templates/todo/makina_config_toml` (destined for `.makina/config.toml`) with `base_branch = "develop"`, `concurrency = 2`, a `[caps]` table (`gate_iterations = 5`, `reviewer_iterations = 3`, `wall_clock_secs = 1200`), and three `[[gates]]` tables — `name = "test" / command = "cargo test"`, `name = "clippy" / command = "cargo clippy -- -D warnings"`, `name = "fmt" / command = "cargo fmt --check"` — mirroring the shipped `.makina/config.toml` (`base_branch` at `.makina/config.toml:24`, `concurrency` at `.makina/config.toml:30`) but standalone with a short header comment.

5. Create `crates/makina/src/templates/todo/plan_tasks_md` (destined for `docs/plans/0001-Todo-Starter/TASKS.md`) as a valid implement-plan task list: a `# ` title, a `## 0001 — Todo Core` workstream header, and exactly three tasks each as `### {kebab-id} — {Title}` with a `**Steps:**` list and the two closing bullets. Use verbatim:

   ```markdown
   # Todo Starter — Plan 0001

   A tiny starter plan for the scaffolded `todo` project. Each task keeps `cargo test`, `cargo clippy -- -D warnings`, and `cargo fmt --check` green.

   ## 0001 — Todo Core

   ### add-task-toggle — Add A `Task::toggle` Method
   Add a `toggle(&mut self)` method to `Task` in `src/main.rs` that flips `done`, with a unit test.
   - **Depends on:** —
   - **Done when:** `Task::toggle` flips `done` and a unit test proves it; cargo test / clippy / fmt pass.

   ### add-task-count — Add A `count_open` Helper
   Add a free `count_open(tasks: &[Task]) -> usize` returning how many tasks are not done, with a unit test.
   - **Depends on:** —
   - **Done when:** `count_open` returns the open-task count and a unit test proves it; cargo test / clippy / fmt pass.

   ### print-open-summary — Print An Open-Task Summary In `main`
   Use `count_open` in `main` to print a trailing `N task(s) open` line.
   - **Depends on:** add-task-toggle, add-task-count
   - **Done when:** `main` prints the open-task summary and cargo test / clippy / fmt pass.
   ```

6. Create `crates/makina/src/templates/todo/plan_scope_md` (destined for `docs/plans/0001-Todo-Starter/SCOPE.md`): a `# Scope — Plan 0001` heading, a one-line `>` mission blockquote, a `## Why this plan` paragraph, an `## In scope` bullet for workstream `0001 — Todo Core`, and an `## Out of scope` line — mirroring the house style of `docs/plans/0046-Front-Door-First-Five-Minutes/SCOPE.md`.

7. Create `crates/makina/src/templates/todo/plan_architecture_md` (destined for `docs/plans/0001-Todo-Starter/ARCHITECTURE.md`): a `# Architecture — Plan 0001` heading and a `## 0001 — Todo Core` section describing the three edits to `src/main.rs` (`Task::toggle`, `count_open`, the summary line).

8. Create `crates/makina/src/templates/todo/plan_status_md` (destined for `docs/plans/0001-Todo-Starter/STATUS.md`): a `# Plan 0001 — Todo Starter — status` heading, a `**Status:** 📋 Planned` line, and a workstream table row `| 0001 | Todo Core | add-task-toggle, add-task-count, print-open-summary | 📋 Planned |`.

9. Run the full gate commands.

- **Depends on:** —
- **Done when:** All seven files exist under `crates/makina/src/templates/todo/` with the content above; `main_rs` is fmt- and clippy-clean and its `new_task_is_not_done` test would pass; `plan_tasks_md` contains three `### ` task headings each with a `**Depends on:**` and a `**Done when:**` bullet; no file in the src tree is literally named `Cargo.toml`. cargo test / clippy / fmt green.

---

## 0002 — Folder Bootstrap & Conflict Rules

### add-scaffold-module — Add The `scaffold_project` Bootstrap With Conflict Rules

`initialize_folder` (`crates/makina/src/folder_init.rs:16`) is only driven from the in-TUI handler (`crates/makina/src/event.rs:425`), has no conflict check, and stamps only git + `docs/plans/README.md`. This task adds a headless `scaffold_project` that refuses to clobber existing work, reuses `initialize_folder` for the git bootstrap, then writes the embedded `todo` template files (from `add-todo-template-files`) and commits them on `develop`. `initialize_folder` sets a `commit.gpgsign=false` shield and a repo-local identity unconditionally on a fresh repo (`folder_init.rs:39`), so the follow-up commit is hermetic. `initialize_folder` stays in the `makina` crate (no relocation): it, its only caller `event.rs:425`, and the new `create` dispatch all live in this crate, so no layering change is needed.

**Steps:**

1. Create `crates/makina/src/scaffold.rs` and register it by adding `pub mod scaffold;` in `crates/makina/src/lib.rs` next to `pub mod folder_init;` (`lib.rs:32`).

2. Define the report type and a `Display` impl:

   ```rust
   use std::fmt;
   use std::path::{Path, PathBuf};
   use std::process::Command;

   /// What a scaffold created, plus the run instructions to print.
   pub struct ScaffoldReport {
       pub created: Vec<PathBuf>,
       pub instructions: String,
   }

   impl fmt::Display for ScaffoldReport {
       fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
           writeln!(f, "Created project with:")?;
           for p in &self.created {
               writeln!(f, "  {}", p.display())?;
           }
           write!(f, "{}", self.instructions)
       }
   }
   ```

3. Add a private git helper mirroring `folder_init::run_git` (that one is private to its module, so it cannot be reused):

   ```rust
   fn run_git(dir: &Path, args: &[&str]) -> Result<(), String> {
       let out = Command::new("git")
           .args(args)
           .current_dir(dir)
           .output()
           .map_err(|e| format!("failed to run git: {e}"))?;
       if !out.status.success() {
           return Err(format!("git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr)));
       }
       Ok(())
   }
   ```

4. Add a `const TODO_FILES: &[(&str, &str)]` mapping each destination (relative path) to its embedded content via `include_str!`, e.g.:

   ```rust
   const TODO_FILES: &[(&str, &str)] = &[
       ("Cargo.toml", include_str!("templates/todo/cargo_toml")),
       ("src/main.rs", include_str!("templates/todo/main_rs")),
       (".makina/config.toml", include_str!("templates/todo/makina_config_toml")),
       ("docs/plans/0001-Todo-Starter/SCOPE.md", include_str!("templates/todo/plan_scope_md")),
       ("docs/plans/0001-Todo-Starter/ARCHITECTURE.md", include_str!("templates/todo/plan_architecture_md")),
       ("docs/plans/0001-Todo-Starter/TASKS.md", include_str!("templates/todo/plan_tasks_md")),
       ("docs/plans/0001-Todo-Starter/STATUS.md", include_str!("templates/todo/plan_status_md")),
   ];
   ```

5. Implement `scaffold_project`:

   ```rust
   pub const AVAILABLE_TEMPLATES: &[&str] = &["todo"];

   /// Bootstrap a brand-new, immediately-runnable project at `target` from `template`.
   pub fn scaffold_project(target: &Path, template: &str) -> Result<ScaffoldReport, String> {
       if !AVAILABLE_TEMPLATES.contains(&template) {
           return Err(format!(
               "unknown template '{template}'; available: {}",
               AVAILABLE_TEMPLATES.join(", ")
           ));
       }
       // Conflict rule: never clobber existing work.
       if target.exists() {
           if target.is_file() {
               return Err(format!("refusing to scaffold: {} is a file", target.display()));
           }
           let mut entries = std::fs::read_dir(target)
               .map_err(|e| format!("failed to read {}: {e}", target.display()))?;
           if entries.next().is_some() {
               return Err(format!(
                   "refusing to scaffold into non-empty directory {}",
                   target.display()
               ));
           }
       }
       std::fs::create_dir_all(target)
           .map_err(|e| format!("failed to create {}: {e}", target.display()))?;
       // Reuse the shipped bootstrap: git init, main+develop, docs/plans/README.md,
       // and the commit.gpgsign=false shield + repo-local identity.
       crate::folder_init::initialize_folder(target)?;
       // Put initial content on the base branch Makina drives.
       run_git(target, &["checkout", "develop"])?;
       let mut created = Vec::new();
       for (dest, contents) in TODO_FILES {
           let path = target.join(dest);
           if let Some(parent) = path.parent() {
               std::fs::create_dir_all(parent)
                   .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
           }
           std::fs::write(&path, contents)
               .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
           created.push(path);
       }
       run_git(target, &["add", "-A"])?;
       let out = Command::new("git")
           .args(["commit", "-m", "chore: scaffold todo project"])
           .env("GIT_AUTHOR_NAME", "Makina")
           .env("GIT_AUTHOR_EMAIL", "makina@localhost")
           .env("GIT_COMMITTER_NAME", "Makina")
           .env("GIT_COMMITTER_EMAIL", "makina@localhost")
           .current_dir(target)
           .output()
           .map_err(|e| format!("failed to run git: {e}"))?;
       if !out.status.success() {
           return Err(format!("git commit failed: {}", String::from_utf8_lossy(&out.stderr)));
       }
       let instructions = format!("\nNext:\n  cd {} && makina\n", target.display());
       Ok(ScaffoldReport { created, instructions })
   }
   ```

6. Add a `#[cfg(test)] mod tests` in `scaffold.rs` with the conflict-rule unit test:

   ```rust
   #[test]
   fn refuses_non_empty_directory() {
       let tmp = tempfile::tempdir().unwrap();
       let target = tmp.path().join("occupied");
       std::fs::create_dir_all(&target).unwrap();
       std::fs::write(target.join("keep.txt"), "x").unwrap();
       let err = super::scaffold_project(&target, "todo").expect_err("must refuse");
       assert!(err.contains("non-empty"), "error explains the conflict: {err}");
   }

   #[test]
   fn refuses_unknown_template() {
       let tmp = tempfile::tempdir().unwrap();
       let err = super::scaffold_project(&tmp.path().join("x"), "nope").expect_err("must refuse");
       assert!(err.contains("todo"), "lists available templates: {err}");
   }
   ```

7. Run the full gate commands.

- **Depends on:** add-todo-template-files
- **Done when:** `crates/makina/src/scaffold.rs` exports `scaffold_project`, `ScaffoldReport`, and `AVAILABLE_TEMPLATES`, is registered `pub mod scaffold;` in `lib.rs`, `include_str!`s every `templates/todo/*` file, refuses a file target / non-empty directory / unknown template, and on an empty target creates git structure + writes and commits the template content on `develop`; `refuses_non_empty_directory` and `refuses_unknown_template` pass (red before: the module did not exist). cargo test / clippy / fmt green.

---

## 0003 — Create Subcommand CLI Surface

### add-create-cli-action — Add The `create` Subcommand To The CLI Parser

`parse_args` (`crates/makina/src/cli.rs:16`) matches only `-h/--help`, `-V/--version`, `--doctor` and returns `CliAction::Unknown(other)` for everything else, so `makina create …` is rejected. This task adds the `Create`/`CreateError` variants, an `AVAILABLE_TEMPLATES` const, a `create` parse arm (required positional path, `--template` defaulting to and only accepting `todo`, unknown template listing available templates), and a `SUBCOMMANDS` block in `help_text`. Parsing stays pure so every branch is unit-testable without a TTY; the wiring lands separately in `wire-create-dispatch-in-main`.

**Steps:**

1. In `crates/makina/src/cli.rs`, add near the top: `pub const AVAILABLE_TEMPLATES: &[&str] = &["todo"];`.

2. Extend `enum CliAction` (`cli.rs:7`) with two variants: `Create { path: String, template: String }` and `CreateError(String)`. Keep the existing variants.

3. Add a `create` arm to `parse_args` (`cli.rs:16`) before the `Some(other) =>` catch-all: `Some("create") => parse_create(&args[1..]),`.

4. Add the `parse_create` helper:

   ```rust
   fn parse_create(rest: &[String]) -> CliAction {
       let mut path: Option<String> = None;
       let mut template = String::from("todo");
       let mut i = 0;
       while i < rest.len() {
           match rest[i].as_str() {
               "--template" => match rest.get(i + 1) {
                   Some(t) => {
                       template = t.clone();
                       i += 2;
                   }
                   None => return CliAction::CreateError("--template requires a value".to_string()),
               },
               other if !other.starts_with('-') && path.is_none() => {
                   path = Some(other.to_string());
                   i += 1;
               }
               other => return CliAction::CreateError(format!("unexpected argument: {other}")),
           }
       }
       let Some(path) = path else {
           return CliAction::CreateError("makina create requires a target <path>".to_string());
       };
       if !AVAILABLE_TEMPLATES.contains(&template.as_str()) {
           return CliAction::CreateError(format!(
               "unknown template '{template}'; available templates: {}",
               AVAILABLE_TEMPLATES.join(", ")
           ));
       }
       CliAction::Create { path, template }
   }
   ```

5. In `help_text` (`cli.rs:27`), add a `SUBCOMMANDS:` block documenting `create <path> [--template <name>]` — "Scaffold a new, runnable Makina project at <path> (default template: todo)" — and list `todo` as the available template.

6. Add to the `#[cfg(test)] mod tests` in `cli.rs` the verbatim test:

   ```rust
   #[test]
   fn parse_args_handles_create_subcommand() {
       assert_eq!(
           parse_args(&["create".into(), "/tmp/x".into()]),
           CliAction::Create { path: "/tmp/x".into(), template: "todo".into() }
       );
       assert_eq!(
           parse_args(&["create".into(), "/tmp/x".into(), "--template".into(), "todo".into()]),
           CliAction::Create { path: "/tmp/x".into(), template: "todo".into() }
       );
       match parse_args(&["create".into()]) {
           CliAction::CreateError(msg) => assert!(msg.contains("path")),
           other => panic!("expected CreateError, got {other:?}"),
       }
       match parse_args(&["create".into(), "/tmp/x".into(), "--template".into(), "nope".into()]) {
           CliAction::CreateError(msg) => assert!(msg.contains("todo")),
           other => panic!("expected CreateError, got {other:?}"),
       }
   }
   ```

7. Run the full gate commands.

- **Depends on:** —
- **Done when:** `CliAction` has `Create { path, template }` and `CreateError(String)`; `parse_args` returns `Create` for `create <path>` (template defaulting to `todo`) and `CreateError` for a missing path or an unknown template (message naming the available `todo`); `help_text` documents the `create` subcommand; `parse_args_handles_create_subcommand` passes (red before: the variants did not exist). cargo test / clippy / fmt green.

---

### wire-create-dispatch-in-main — Dispatch `create` Headlessly In `main` So It Scaffolds And Exits Without The TUI

With the parser (`add-create-cli-action`) and `scaffold_project` (`add-scaffold-module`) in place, this task wires them into `fn main` (`crates/makina/src/main.rs:62`) so `makina create <path>` scaffolds and exits before any config load or TUI init. Today the `match makina::cli::parse_args(&args)` dispatch (`main.rs:68`) has no `Create`/`CreateError` arms; `Unknown` exits 2 (`main.rs:80-82`) and `LaunchTui` falls through (`main.rs:84`). A bare `makina` must remain byte-for-byte unchanged.

**Steps:**

1. In `crates/makina/src/main.rs`, add two arms to the `match makina::cli::parse_args(&args)` block (`main.rs:68`), before the `CliAction::LaunchTui` arm (`main.rs:84`):

   ```rust
   makina::cli::CliAction::Create { path, template } => {
       match makina::scaffold::scaffold_project(std::path::Path::new(&path), &template) {
           Ok(report) => {
               println!("{report}");
               return;
           }
           Err(e) => {
               eprintln!("error: {e}");
               std::process::exit(1);
           }
       }
   }
   makina::cli::CliAction::CreateError(msg) => {
       eprintln!("error: {msg}\n\n{}", makina::cli::help_text());
       std::process::exit(2);
   }
   ```

2. Confirm both arms `return`/`exit` before the config load and TUI startup, so scaffolding never touches `Config::load_defaults_with_paths` (`main.rs:102`) or the terminal.

3. Verify the `LaunchTui` fall-through (`main.rs:84`) is unchanged, so a bare `makina` still launches the TUI exactly as before.

4. Run the full gate commands, then manually verify `cargo run -p makina -- create /tmp/makina-smoke-todo` prints the created files and next-step instructions and exits without entering the raw-mode TUI, and `cargo run -p makina -- create /tmp/x --template nope` prints an error + help and exits 2.

- **Depends on:** add-create-cli-action, add-scaffold-module
- **Done when:** `main` dispatches `CliAction::Create` to `scaffold::scaffold_project` (printing the report and returning on success, `exit(1)` with the error on failure) and `CliAction::CreateError` to an error + help print with `exit(2)`, both before config load and TUI init; a bare `makina` still launches the TUI unchanged; `cargo run -p makina -- create <tmp-path>` scaffolds a runnable project headlessly. cargo test / clippy / fmt green.

---

## 0004 — Parser & End-to-End Scaffold Tests

### add-scaffold-integration-test — Add A Hermetic End-To-End Scaffold Integration Test

No test exercises scaffolding. This task adds an integration test using the shipped hermetic helper `makina_core::test_support::run_git` (`crates/makina-core/src/test_support.rs:10`), already available to the `makina` crate via the `[dev-dependencies]` `makina-core = { features = ["test-support"] }`. It scaffolds a `todo` project into a temp dir and asserts the git structure, the sample files, and that the plan triad parses; it asserts the conflict rule; and it adds an `#[ignore]`d test that compiles and runs the scaffolded project's own gates (kept `#[ignore]` so the default `cargo test` stays fast — the structural assertions are the everyday gate).

**Steps:**

1. Create `crates/makina/tests/scaffold_integration_test.rs` with the structural end-to-end test:

   ```rust
   use makina_core::test_support::run_git;

   #[test]
   fn scaffold_creates_runnable_todo_project() {
       let tmp = tempfile::tempdir().expect("tempdir");
       let target = tmp.path().join("todo");
       let report = makina::scaffold::scaffold_project(&target, "todo")
           .expect("scaffold_project should succeed on an empty target");

       assert!(target.join(".git").exists(), ".git must exist");
       let branches = run_git(&target, &["branch", "--format=%(refname:short)"]);
       let branches = String::from_utf8_lossy(&branches.stdout);
       assert!(branches.contains("main"), "main branch: {branches}");
       assert!(branches.contains("develop"), "develop branch: {branches}");

       let head = run_git(&target, &["rev-parse", "--abbrev-ref", "HEAD"]);
       assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "develop");
       let log = run_git(&target, &["log", "--oneline", "develop"]);
       assert!(
           String::from_utf8_lossy(&log.stdout).contains("scaffold"),
           "scaffold commit must exist on develop"
       );

       assert!(target.join("Cargo.toml").exists(), "Cargo.toml");
       assert!(target.join("src/main.rs").exists(), "src/main.rs");
       let cfg = std::fs::read_to_string(target.join(".makina/config.toml")).unwrap();
       assert!(cfg.contains("base_branch = \"develop\""), "config base_branch");

       let plan_dir = target.join("docs/plans/0001-Todo-Starter");
       let tasks = std::fs::read_to_string(plan_dir.join("TASKS.md")).unwrap();
       assert!(tasks.contains("Depends on:"), "TASKS has Depends on");
       assert!(tasks.contains("Done when:"), "TASKS has Done when");
       assert!(plan_dir.join("SCOPE.md").exists(), "SCOPE.md");
       assert!(plan_dir.join("ARCHITECTURE.md").exists(), "ARCHITECTURE.md");
       assert!(plan_dir.join("STATUS.md").exists(), "STATUS.md");

       assert!(report.instructions.contains("cd"), "report tells the user to cd + run");
   }
   ```

2. Add the conflict-rule integration test in the same file:

   ```rust
   #[test]
   fn scaffold_refuses_non_empty_directory() {
       let tmp = tempfile::tempdir().expect("tempdir");
       let target = tmp.path().join("occupied");
       std::fs::create_dir_all(&target).unwrap();
       std::fs::write(target.join("keep.txt"), "x").unwrap();
       let err = makina::scaffold::scaffold_project(&target, "todo")
           .expect_err("must refuse a non-empty target");
       assert!(err.contains("non-empty"), "error explains the conflict: {err}");
   }
   ```

3. Add the `#[ignore]`d full-compile test that runs the scaffolded project's own `cargo test`:

   ```rust
   #[test]
   #[ignore = "compiles the scaffolded crate; slow, run explicitly"]
   fn scaffolded_todo_project_passes_its_own_gates() {
       let tmp = tempfile::tempdir().expect("tempdir");
       let target = tmp.path().join("todo");
       makina::scaffold::scaffold_project(&target, "todo").unwrap();
       let status = std::process::Command::new("cargo")
           .args(["test"]).current_dir(&target).status().unwrap();
       assert!(status.success(), "scaffolded todo project's cargo test must pass");
   }
   ```

4. Run the full gate commands, then run `cargo test -p makina --test scaffold_integration_test -- --ignored scaffolded_todo_project_passes_its_own_gates` once to confirm the template crate actually compiles and its test passes.

- **Depends on:** add-scaffold-module, add-todo-template-files
- **Done when:** `crates/makina/tests/scaffold_integration_test.rs` exists; `scaffold_creates_runnable_todo_project` and `scaffold_refuses_non_empty_directory` pass under the default `cargo test` (asserting git structure, `main`+`develop`, HEAD on `develop` with the scaffold commit, `Cargo.toml`/`src/main.rs`/`.makina/config.toml` with `base_branch = "develop"`, and the `0001-Todo-Starter` triad with `Depends on:`/`Done when:` lines); the `#[ignore]`d `scaffolded_todo_project_passes_its_own_gates` passes when run explicitly. cargo test / clippy / fmt green.

---

## 0005 — README Quickstart 'Try It Safely'

### readme-quickstart-try-it-safely — Add A README 'Try It Safely' Quickstart Recommending `makina create`

The README's only experiment-safely guidance is a throwaway clone: the Caution section (`README.md:156`) tells first-timers to `git clone /path/to/repo /tmp/repo-trial` (`README.md:165`) right before the Run section (`README.md:168`). This documentation-only task adds a 'Try it safely' Quickstart recommending `makina create` into a brand-new folder OUTSIDE the makina repo — the zero-risk first-run path this plan ships — and demotes the throwaway-clone recipe to a secondary option, keeping the flag descriptions consistent with `crates/makina/src/cli.rs` `help_text`.

**Steps:**

1. In `README.md`, add a `## Try it safely` section immediately before `## Run` (`README.md:168`):

   ````markdown
   ## Try it safely

   The safest first run creates a brand-new project OUTSIDE this repo, so nothing
   Makina does can touch your working tree:

   ```bash
   makina create ~/tmp/todo --template todo   # scaffold a runnable project
   cd ~/tmp/todo && makina                     # open it in the TUI
   ```

   `makina create <path> [--template <name>]` bootstraps a git repo (with `main`
   and `develop`), a committed `.makina/config.toml`, and a starter
   `docs/plans/0001-Todo-Starter` plan. The only template today is `todo`.
   ````

2. Rewrite the existing Caution block (`README.md:156-166`) so its primary recommendation is `makina create` into an unrelated folder, and the throwaway-clone recipe (`git clone … /tmp/repo-trial`, `README.md:165`) is presented as a secondary option for experimenting on an existing repo — not the first thing a newcomer sees.

3. Ensure the `makina create` flag descriptions match `crates/makina/src/cli.rs` `help_text` (subcommand name, `--template` default `todo`, available templates).

- **Depends on:** wire-create-dispatch-in-main
- **Done when:** Documentation-only (no runtime surface, so the red-green behavioral gate is exempt). `README.md` has a `## Try it safely` section before `## Run` recommending `makina create ~/tmp/todo --template todo && cd ~/tmp/todo && makina`; the Caution block leads with `makina create` into a folder outside the repo and demotes the throwaway clone; the flag text matches `cli.rs` `help_text`. cargo test / clippy / fmt green.

---

**End of plan 0047 TASKS.** When every "Done when" bullet is green, a headless
`makina create <path> [--template todo]` scaffolds a brand-new,
immediately-runnable experiment project outside the makina repo — a git repo
with `main`/`develop`, a committed `.makina/config.toml`, a starter
`0001-Todo-Starter` plan, and a README that recommends it as the zero-risk
first run — all with the gate commands green.
