# Architecture — Plan 0047 (deltas)

> The concrete deltas. This plan touches
> `crates/makina/src/templates/todo/cargo_toml`,
> `crates/makina/src/templates/todo/main_rs`,
> `crates/makina/src/templates/todo/makina_config_toml`,
> `crates/makina/src/templates/todo/plan_scope_md`,
> `crates/makina/src/templates/todo/plan_architecture_md`,
> `crates/makina/src/templates/todo/plan_tasks_md`,
> `crates/makina/src/templates/todo/plan_status_md`,
> `crates/makina/src/scaffold.rs`, `crates/makina/src/lib.rs`,
> `crates/makina/src/cli.rs`, `crates/makina/src/main.rs`,
> `crates/makina/tests/scaffold_integration_test.rs`, and `README.md`.
> Line numbers are hints; locate by symbol.

## 0001 — Embedded `todo` Template

Today `crates/makina/src/templates/` contains exactly one file,
`plans_readme.md`, and `folder_init::initialize_folder` embeds it via
`include_str!("templates/plans_readme.md")`
(`crates/makina/src/folder_init.rs:75`) to write `docs/plans/README.md`. There
is no runnable sample project to stamp out.

**Edits:**

**Add the neutral-named template files under
`crates/makina/src/templates/todo/`.** Seven files, each written to its real
destination by the scaffolder in WS0002:

- `cargo_toml` → `Cargo.toml` (a standalone `[package] name = "todo"`,
  `edition = "2021"` crate)
- `main_rs` → `src/main.rs` (a `Task` struct + `main` + a passing `#[test]`)
- `makina_config_toml` → `.makina/config.toml` (base_branch=develop,
  concurrency=2, three gates)
- `plan_scope_md` / `plan_architecture_md` / `plan_tasks_md` /
  `plan_status_md` →
  `docs/plans/0001-Todo-Starter/{SCOPE,ARCHITECTURE,TASKS,STATUS}.md`

```toml
# cargo_toml → Cargo.toml
[package]
name = "todo"
version = "0.1.0"
edition = "2021"

[dependencies]
```

**Properties that make this safe:**

- Files stored under neutral names (no literal `Cargo.toml` in the src tree)
  cannot be mistaken for a workspace member (`Cargo.toml` root uses
  `members = ["crates/*"]`, matching only direct children) and mirror the
  existing `plans_readme.md`→`README.md` rename pattern.
- Adding static files touches no compiled code, so all three gates stay
  trivially green.
- `src/main.rs` is pre-formatted and clippy-clean with a passing test, so the
  scaffolded project passes its own gates out of the box.

## 0002 — Folder Bootstrap & Conflict Rules

Today `initialize_folder` (`crates/makina/src/folder_init.rs:16`) is only
invoked from the in-TUI `AppEvent::InitializeFolderSelected` handler
(`crates/makina/src/event.rs:425`); it has no conflict check and stamps only
git structure + `docs/plans/README.md`. Its private `fn run_git`
(`crates/makina/src/folder_init.rs:82`) and its `commit.gpgsign=false` shield
(`folder_init.rs:39`) are set unconditionally on a fresh repo, so a follow-up
commit on `develop` is shielded against a host with `commit.gpgsign = true`.

**Edits:**

**New `crates/makina/src/scaffold.rs`** (registered `pub mod scaffold;` in
`crates/makina/src/lib.rs` next to `pub mod folder_init;` at `lib.rs:32`):

```rust
pub struct ScaffoldReport {
    pub created: Vec<std::path::PathBuf>,
    pub instructions: String,
}

/// Bootstrap a brand-new, immediately-runnable project at `target` from `template`.
pub fn scaffold_project(target: &std::path::Path, template: &str) -> Result<ScaffoldReport, String> {
    // 1. Refuse a file target or a non-empty directory (never clobber existing work).
    // 2. create_dir_all(target); folder_init::initialize_folder(target)?;
    // 3. git checkout develop; write every embedded template file to its dest;
    // 4. git add -A && commit on develop with a pinned GIT_* identity.
}
```

**Its own private `fn run_git`** mirrors `folder_init::run_git` (that one is
private to its module) so scaffold need not widen `folder_init`'s API.

**Properties that make this safe:**

- The conflict rule refuses any file target and any non-empty directory, so
  scaffolding can never overwrite existing files (directly preventing the
  `934d1b3` clobber class).
- `initialize_folder` is idempotent and already sets the
  `commit.gpgsign=false` shield and a repo-local identity when the host has
  none, so the develop-branch content commit succeeds hermetically.
- Content lands on `develop` (the base branch Makina drives); `main` stays at
  the bootstrap empty commit, matching Makina's develop-centric workflow.

## 0003 — Create Subcommand CLI Surface

Today `parse_args` (`crates/makina/src/cli.rs:16`) matches only `-h/--help`,
`-V/--version`, `--doctor`, and returns `CliAction::Unknown(other)` otherwise;
`help_text` (`cli.rs:27`) lists only OPTIONS. The `main` dispatch
(`crates/makina/src/main.rs:68`) routes `Unknown` to `eprintln!` +
`std::process::exit(2)` (`main.rs:80-82`) and `LaunchTui` falls through to TUI
startup (`main.rs:84`).

**Edits:**

**Extend the parser (`cli.rs`).**

```rust
pub const AVAILABLE_TEMPLATES: &[&str] = &["todo"];

pub enum CliAction {
    LaunchTui, ShowHelp, ShowVersion, RunDoctor,
    Create { path: String, template: String },
    CreateError(String),
    Unknown(String),
}
// parse_args: Some("create") => parse_create(&args[1..])
```

**Dispatch in `main.rs`** (new arms in the
`match makina::cli::parse_args(&args)` at `main.rs:68`, before `LaunchTui`):

```rust
CliAction::Create { path, template } => {
    match makina::scaffold::scaffold_project(std::path::Path::new(&path), &template) {
        Ok(report) => { println!("{report}"); return; }
        Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
    }
}
CliAction::CreateError(msg) => {
    eprintln!("error: {msg}\n\n{}", makina::cli::help_text());
    std::process::exit(2);
}
```

**Properties that make this safe:**

- `parse_args` stays a pure function, so every new branch (path, default
  template, missing path, unknown template) is unit-tested without a TTY.
- `Create`/`CreateError` return/exit before any config load or TUI init, so
  scaffolding is fully headless and a bare `makina` is byte-for-byte
  unchanged.
- Unknown templates are rejected in the parser AND defensively re-checked in
  `scaffold_project`.

## 0004 — Parser & End-to-End Scaffold Tests

Today no test exercises scaffolding (the seam does not exist). The hermetic
helpers already ship: `run_git` (`crates/makina-core/src/test_support.rs:10`),
`setup_temp_repo` (`test_support.rs:28`), `init_git_repo_with_identity`
(`test_support.rs:37`), gated behind
`#[cfg(any(test, feature = "test-support"))]` (`crates/makina-core/src/lib.rs:34`);
the `makina` crate's `[dev-dependencies]` already enables
`makina-core = { features = ["test-support"] }`.

**Edits:**

**New `crates/makina/tests/scaffold_integration_test.rs`** with three tests: a
fast structural end-to-end assertion, a conflict-rule assertion, and an
`#[ignore]`d full-compile test.

```rust
use makina_core::test_support::run_git;
// scaffold into a tempdir, assert git structure + files + plan triad parses
```

**Properties that make this safe:**

- The structural test reuses the shipped hermetic `run_git`, so it is
  deterministic and shielded against host git config — no new dependency.
- The slow full-compile of the scaffolded crate is `#[ignore]`d so the default
  `cargo test` gate stays fast; it is run explicitly when validating the
  template compiles.
- Parser unit tests for the `create` action live with the parser in WS0003, so
  both the pure and the end-to-end surfaces are covered.

## 0005 — README Quickstart 'Try It Safely'

Today the Caution section (`README.md:156`) tells first-timers to
`git clone /path/to/repo /tmp/repo-trial` (`README.md:165`) immediately before
the Run section (`README.md:168`); there is no `makina create`
recommendation, so the safest first run (a brand-new project in an unrelated
folder) is undocumented.

**Edits:**

**Add a 'Try it safely' Quickstart** (immediately before `## Run`,
`README.md:168`):

````markdown
## Try it safely

The safest first run creates a brand-new project OUTSIDE this repo:

```bash
makina create ~/tmp/todo --template todo
cd ~/tmp/todo && makina
```
````

**Rewrite the Caution block** so `makina create` is the primary recommendation
and the throwaway-clone recipe is the secondary fallback.

**Properties that make this safe:**

- Documentation-only; no compiled surface, so the gates stay green.
- The flag descriptions are diffed against `cli.rs` `help_text`, so docs
  cannot drift from code.

## Test strategy

- **0002 (scaffold conflict rules).** In `crates/makina/src/scaffold.rs`:
  `refuses_non_empty_directory` asserts `scaffold_project` errors (message
  contains `non-empty`) when the target directory already has a file, and
  `refuses_unknown_template` asserts an unknown template errors listing `todo`
  (red before: the module did not exist).
- **0003 (CLI parser).** In `crates/makina/src/cli.rs`:
  `parse_args_handles_create_subcommand` covers `create <path>` (default
  template `todo`), `create <path> --template todo`, the missing-path
  `CreateError`, and the unknown-template `CreateError` (message names the
  available `todo`).
- **0004 (end-to-end scaffold).** In
  `crates/makina/tests/scaffold_integration_test.rs` using
  `makina_core::test_support::run_git`:
  `scaffold_creates_runnable_todo_project` asserts `.git`, `main`+`develop`,
  HEAD on `develop` carrying the scaffold commit,
  `Cargo.toml`/`src/main.rs`/`.makina/config.toml`
  (`base_branch = "develop"`), and the `0001-Todo-Starter` triad with
  `Depends on:`/`Done when:` lines; `scaffold_refuses_non_empty_directory`
  asserts the conflict rule; the `#[ignore]`d
  `scaffolded_todo_project_passes_its_own_gates` compiles and runs the stamped
  project's `cargo test`.
- The `main.rs` dispatch is verified by running
  `cargo run -p makina -- create <tmp-path>` (scaffolds and exits) and
  `-- create <path> --template nope` (error + help, exit 2).
- All tasks keep `cargo test`, `cargo clippy --all-targets -- -D warnings`,
  and `cargo fmt --check` green.

## Interaction with prior work

- **0046 — Front Door / First Five Minutes.** WS0003 extends the hand-rolled
  parser (`cli.rs` `CliAction`/`parse_args`/`help_text`) and the `main.rs`
  argv dispatch (`main.rs:68`) that plan 0046 shipped, mirroring its
  small-clean triad house style.
- **0043 — Multi-Folder / First-Run Config.** WS0002 reuses
  `folder_init::initialize_folder` (`folder_init.rs:16`) — the git +
  `main`/`develop` + `docs/plans/README.md` bootstrap and its
  `commit.gpgsign=false` shield — unchanged, so scaffolding inherits its
  verified behavior and its only prior caller (`event.rs:425`) is untouched.
- **0045 — Hermetic Git Test Support.** WS0004's integration test uses
  `makina_core::test_support::run_git` (`test_support.rs:10`), already a
  `makina` dev-dependency via the `test-support` feature, so the end-to-end
  assertions are hermetic against host git config.
- **Incident `934d1b3`.** The whole plan exists to prevent the
  workspace-clobber class that commit repaired: throwaway experiments are
  created in their own folder OUTSIDE the repo, and `scaffold_project` refuses
  any non-empty target so it can never overwrite existing work.
