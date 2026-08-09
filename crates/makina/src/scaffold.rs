use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use makina_core::api::CommandOutcome;
use makina_core::orchestrator::AuthoringCoordinator;
use makina_core::plan::{
    FilesystemPlanFileSource, PlanCandidate, PlanKey, PlanReservations, load_plan,
};
use makina_core::repository_lease::RepositoryLeaseRegistry;

/// What a scaffold created, plus the run instructions to print.
#[derive(Debug)]
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

/// Available templates for scaffolding.
pub const AVAILABLE_TEMPLATES: &[&str] = &["todo"];

#[cfg(feature = "test-util")]
#[derive(Clone, Copy, Debug, Default)]
pub struct ScaffoldTestHooks {
    pub fail_before_initial_commit: bool,
    pub lose_registration_response: bool,
}

/// Template files mapped to their destinations.
const TODO_FILES: &[(&str, &str)] = &[
    // Build output MUST be ignored: the gates (`cargo test`/`clippy`) run in the
    // task worktree, and the Developer's `git add -A` would otherwise sweep
    // `target/` and a generated `Cargo.lock` onto the task branch — undeclared
    // paths that fail the review-acceptance footprint check on EVERY task.
    (".gitignore", include_str!("templates/todo/gitignore")),
    ("Cargo.toml", include_str!("templates/todo/cargo_toml")),
    ("src/main.rs", include_str!("templates/todo/main_rs")),
    ("src/list.rs", include_str!("templates/todo/src_list_rs")),
    (
        "src/render.rs",
        include_str!("templates/todo/src_render_rs"),
    ),
    ("src/tags.rs", include_str!("templates/todo/src_tags_rs")),
    (
        "src/filter.rs",
        include_str!("templates/todo/src_filter_rs"),
    ),
    (
        "src/by_status.rs",
        include_str!("templates/todo/src_by_status_rs"),
    ),
    (
        "src/by_title.rs",
        include_str!("templates/todo/src_by_title_rs"),
    ),
    (
        "docs/plans/STATUS.md",
        include_str!("templates/todo/plans_status_md"),
    ),
    (
        ".makina/config.toml",
        include_str!("templates/todo/makina_config_toml"),
    ),
    // Plan 0001 — sequential chain.
    (
        "docs/plans/0001-todo-core/SCOPE.md",
        include_str!("templates/todo/plan_0001_scope_md"),
    ),
    (
        "docs/plans/0001-todo-core/ARCHITECTURE.md",
        include_str!("templates/todo/plan_0001_architecture_md"),
    ),
    (
        "docs/plans/0001-todo-core/STATUS.md",
        include_str!("templates/todo/plan_0001_status_md"),
    ),
    (
        "docs/plans/0001-todo-core/tasks/0101-add-task-toggle.md",
        include_str!("templates/todo/plan_0001_task_0101_md"),
    ),
    (
        "docs/plans/0001-todo-core/tasks/0102-add-count-open.md",
        include_str!("templates/todo/plan_0001_task_0102_md"),
    ),
    (
        "docs/plans/0001-todo-core/tasks/0103-add-print-summary.md",
        include_str!("templates/todo/plan_0001_task_0103_md"),
    ),
    // Plan 0002 — three independent parallel tasks.
    (
        "docs/plans/0002-todo-features/SCOPE.md",
        include_str!("templates/todo/plan_0002_scope_md"),
    ),
    (
        "docs/plans/0002-todo-features/ARCHITECTURE.md",
        include_str!("templates/todo/plan_0002_architecture_md"),
    ),
    (
        "docs/plans/0002-todo-features/STATUS.md",
        include_str!("templates/todo/plan_0002_status_md"),
    ),
    (
        "docs/plans/0002-todo-features/tasks/0201-add-list-module.md",
        include_str!("templates/todo/plan_0002_task_0201_md"),
    ),
    (
        "docs/plans/0002-todo-features/tasks/0202-add-render-module.md",
        include_str!("templates/todo/plan_0002_task_0202_md"),
    ),
    (
        "docs/plans/0002-todo-features/tasks/0203-add-tags-module.md",
        include_str!("templates/todo/plan_0002_task_0203_md"),
    ),
    // Plan 0003 — mixed fan-out/fan-in.
    (
        "docs/plans/0003-todo-integration/SCOPE.md",
        include_str!("templates/todo/plan_0003_scope_md"),
    ),
    (
        "docs/plans/0003-todo-integration/ARCHITECTURE.md",
        include_str!("templates/todo/plan_0003_architecture_md"),
    ),
    (
        "docs/plans/0003-todo-integration/STATUS.md",
        include_str!("templates/todo/plan_0003_status_md"),
    ),
    (
        "docs/plans/0003-todo-integration/tasks/0301-add-filter-enum.md",
        include_str!("templates/todo/plan_0003_task_0301_md"),
    ),
    (
        "docs/plans/0003-todo-integration/tasks/0302-add-by-status-filter.md",
        include_str!("templates/todo/plan_0003_task_0302_md"),
    ),
    (
        "docs/plans/0003-todo-integration/tasks/0303-add-by-title-filter.md",
        include_str!("templates/todo/plan_0003_task_0303_md"),
    ),
    (
        "docs/plans/0003-todo-integration/tasks/0304-wire-filters-into-main.md",
        include_str!("templates/todo/plan_0003_task_0304_md"),
    ),
];

/// Plan directories scaffolded by the `todo` template, in registration order.
const TODO_PLAN_DIRS: &[&str] = &[
    "docs/plans/0001-todo-core",
    "docs/plans/0002-todo-features",
    "docs/plans/0003-todo-integration",
];

const EMPTY_PROJECT_COMMIT: &str = "chore: initialize Makina project";
const TODO_PROJECT_COMMIT: &str = "chore: scaffold todo project";

/// Run a git command in the given directory.
fn run_git(dir: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

fn git_output(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn commit_as(
    dir: &Path,
    message: &str,
    identity: &crate::folder_init::GitIdentity,
) -> Result<(), String> {
    let output = Command::new("git")
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_NAME", &identity.name)
        .env("GIT_AUTHOR_EMAIL", &identity.email)
        .env("GIT_COMMITTER_NAME", &identity.name)
        .env("GIT_COMMITTER_EMAIL", &identity.email)
        .current_dir(dir)
        .output()
        .map_err(|error| format!("failed to run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn create_file(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("refusing to replace {}: {e}", path.display()))?;
    file.write_all(contents.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", path.display()))
}

/// Bootstrap a brand-new project, optionally adding an explicit template.
pub async fn create_project(
    target: &Path,
    template: Option<&str>,
) -> Result<ScaffoldReport, String> {
    scaffold_project_inner(target, template, false, false).await
}

/// Bootstrap a brand-new, immediately-runnable project from an explicit template.
pub async fn scaffold_project(target: &Path, template: &str) -> Result<ScaffoldReport, String> {
    create_project(target, Some(template)).await
}

#[cfg(feature = "test-util")]
pub async fn scaffold_project_with_test_hooks(
    target: &Path,
    template: &str,
    hooks: ScaffoldTestHooks,
) -> Result<ScaffoldReport, String> {
    scaffold_project_inner(
        target,
        Some(template),
        hooks.fail_before_initial_commit,
        hooks.lose_registration_response,
    )
    .await
}

async fn scaffold_project_inner(
    target: &Path,
    template: Option<&str>,
    fail_before_initial_commit: bool,
    lose_registration_response: bool,
) -> Result<ScaffoldReport, String> {
    if let Some(template) = template
        && !AVAILABLE_TEMPLATES.contains(&template)
    {
        return Err(format!(
            "unknown template '{template}'; available: {}",
            AVAILABLE_TEMPLATES.join(", ")
        ));
    }
    // Conflict rule: never clobber existing work.
    if target.exists() {
        if target.is_file() {
            return Err(format!(
                "refusing to scaffold: {} is a file",
                target.display()
            ));
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
    let identity = crate::folder_init::global_git_identity()?;
    let owned_destination = !target.exists();
    std::fs::create_dir_all(target)
        .map_err(|e| format!("failed to create {}: {e}", target.display()))?;
    let result = scaffold_owned(
        target,
        template,
        &identity,
        fail_before_initial_commit,
        lose_registration_response,
    )
    .await;
    let published_subject = if template.is_some() {
        TODO_PROJECT_COMMIT
    } else {
        EMPTY_PROJECT_COMMIT
    };
    let published = git_output(target, &["log", "-1", "--format=%s", "develop"])
        .is_ok_and(|subject| subject == published_subject);
    if result.is_err() && owned_destination && !published {
        let _ = std::fs::remove_dir_all(target);
    }
    result
}

async fn scaffold_owned(
    target: &Path,
    template: Option<&str>,
    identity: &crate::folder_init::GitIdentity,
    fail_before_initial_commit: bool,
    lose_registration_response: bool,
) -> Result<ScaffoldReport, String> {
    // Reuse the shipped bootstrap: git init, main+develop, docs/plans/README.md,
    // and the commit.gpgsign=false shield.
    crate::folder_init::initialize_folder_with_identity(target, Some(identity))?;
    // Put initial content on the base branch Makina drives.
    run_git(target, &["checkout", "develop"])?;
    let mut created = vec![
        target.join("docs/plans/README.md"),
        target.join(".makina/.gitignore"),
        target.join(".gitignore"),
    ];
    if template == Some("todo") {
        let template_base_oid = git_output(target, &["rev-parse", "develop"])?;
        let template_base_short_oid = git_output(target, &["rev-parse", "--short", "develop"])?;
        for (dest, contents) in TODO_FILES {
            let path = target.join(dest);
            let contents = contents
                .replace("{{BASE_OID}}", &template_base_oid)
                .replace("{{BASE_SHORT_OID}}", &template_base_short_oid);
            // A template is a more specific project than the bootstrap it is
            // layered on, so where the two describe the same file the template
            // wins — its `.gitignore` knows the project is Rust. Replacing is
            // safe here and nowhere else: `scaffold_project` refuses a
            // non-empty destination, so anything already present was written
            // moments ago by this same run.
            if path.exists() {
                std::fs::remove_file(&path)
                    .map_err(|e| format!("failed to replace {}: {e}", path.display()))?;
            }
            create_file(&path, &contents)?;
            created.push(path);
        }
    }
    if fail_before_initial_commit {
        return Err("injected pre-publication scaffold failure".into());
    }
    run_git(target, &["add", "-A"])?;
    let commit_message = if template.is_some() {
        TODO_PROJECT_COMMIT
    } else {
        EMPTY_PROJECT_COMMIT
    };
    commit_as(target, commit_message, identity)?;
    let authored_oid = git_output(target, &["rev-parse", "develop"])?;
    // Keep only the two repository branches created by folder initialization.
    // The operator stays on `main`, while Makina owns `develop` as its base.
    // Both begin at the exact same authored scaffold commit.
    run_git(target, &["branch", "-f", "main", &authored_oid])?;
    run_git(target, &["checkout", "main"])?;
    if template.is_none() {
        let instructions = format!(
            "\nNo template was applied; add plans under docs/plans when you are ready.\nmain is checked out at the initialized project commit; develop is ready for Makina.\n\nNext:\n  cd {} && makina\n",
            target.display()
        );
        return Ok(ScaffoldReport {
            created,
            instructions,
        });
    }
    let coordinator = AuthoringCoordinator::new(
        target.to_path_buf(),
        "develop".into(),
        Arc::new(RepositoryLeaseRegistry::new()),
    );
    let mut registration_oids = Vec::with_capacity(TODO_PLAN_DIRS.len());
    for (index, plan_dir) in TODO_PLAN_DIRS.iter().enumerate() {
        let key = PlanKey::parse(plan_dir).map_err(|e| e.to_string())?;
        let source = FilesystemPlanFileSource::new(target, Some(authored_oid.clone()))
            .map_err(|e| e.to_string())?;
        let plan = match load_plan(&source, key.clone(), &PlanReservations::default()).map_err(
            |report| {
                format!(
                    "scaffold plan {plan_dir} is invalid: {:?}",
                    report.diagnostics
                )
            },
        )? {
            PlanCandidate::Plan(plan) => *plan,
            PlanCandidate::NotCandidate => {
                return Err(format!("scaffold plan {plan_dir} is not a candidate"));
            }
        };
        let registration = coordinator
            .publish_committed_with_identity(
                key,
                authored_oid.clone(),
                plan.source_digest.to_string(),
                &identity.name,
                &identity.email,
            )
            .await
            .map_err(|e| format!("failed to register scaffold plan {plan_dir}: {e}"))?;
        let CommandOutcome::PlanRegistered { registration_oid } = registration else {
            return Err(format!(
                "scaffold plan {plan_dir} was not committed before registration"
            ));
        };
        if index == 0 && lose_registration_response {
            // The first successful response is deliberately discarded. Recovery
            // repeats the exact request and must observe the already-published R.
            let replay = coordinator
                .publish_committed_with_identity(
                    PlanKey::parse(plan_dir).map_err(|e| e.to_string())?,
                    authored_oid.clone(),
                    plan.source_digest.to_string(),
                    &identity.name,
                    &identity.email,
                )
                .await
                .map_err(|e| {
                    format!("failed to recover lost scaffold registration response: {e}")
                })?;
            if !matches!(
                replay,
                CommandOutcome::PlanRegistered {
                    registration_oid: replay_oid
                } if replay_oid == registration_oid
            ) {
                return Err("registration response-loss recovery did not return exact R".into());
            }
        }
        registration_oids.push(registration_oid);
    }
    let instructions = format!(
        "\nCreated {} exact registrations ({}) for the develop base.\nmain is checked out at the authored scaffold commit; Makina may advance develop safely.\n\nNext:\n  cd {} && makina\n",
        registration_oids.len(),
        registration_oids.join(", "),
        target.display()
    );
    Ok(ScaffoldReport {
        created,
        instructions,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn refuses_non_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("occupied");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.txt"), "x").unwrap();
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(super::scaffold_project(&target, "todo"))
            .expect_err("must refuse");
        assert!(
            err.contains("non-empty"),
            "error explains the conflict: {err}"
        );
    }

    #[test]
    fn refuses_unknown_template() {
        let tmp = tempfile::tempdir().unwrap();
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(super::scaffold_project(&tmp.path().join("x"), "nope"))
            .expect_err("must refuse");
        assert!(err.contains("todo"), "lists available templates: {err}");
    }
}
