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
    ("Cargo.toml", include_str!("templates/todo/cargo_toml")),
    ("src/main.rs", include_str!("templates/todo/main_rs")),
    (
        "docs/plans/STATUS.md",
        include_str!("templates/todo/plans_status_md"),
    ),
    (
        ".makina/config.toml",
        include_str!("templates/todo/makina_config_toml"),
    ),
    (
        "docs/plans/0001-Todo-Starter/SCOPE.md",
        include_str!("templates/todo/plan_scope_md"),
    ),
    (
        "docs/plans/0001-Todo-Starter/ARCHITECTURE.md",
        include_str!("templates/todo/plan_architecture_md"),
    ),
    (
        "docs/plans/0001-Todo-Starter/tasks/0101-add-task-toggle.md",
        include_str!("templates/todo/plan_task_md"),
    ),
    (
        "docs/plans/0001-Todo-Starter/STATUS.md",
        include_str!("templates/todo/plan_status_md"),
    ),
];

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

/// Bootstrap a brand-new, immediately-runnable project at `target` from `template`.
pub async fn scaffold_project(target: &Path, template: &str) -> Result<ScaffoldReport, String> {
    scaffold_project_inner(target, template, false, false).await
}

#[cfg(feature = "test-util")]
pub async fn scaffold_project_with_test_hooks(
    target: &Path,
    template: &str,
    hooks: ScaffoldTestHooks,
) -> Result<ScaffoldReport, String> {
    scaffold_project_inner(
        target,
        template,
        hooks.fail_before_initial_commit,
        hooks.lose_registration_response,
    )
    .await
}

async fn scaffold_project_inner(
    target: &Path,
    template: &str,
    fail_before_initial_commit: bool,
    lose_registration_response: bool,
) -> Result<ScaffoldReport, String> {
    if !AVAILABLE_TEMPLATES.contains(&template) {
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
    let owned_destination = !target.exists();
    std::fs::create_dir_all(target)
        .map_err(|e| format!("failed to create {}: {e}", target.display()))?;
    let result = scaffold_owned(
        target,
        fail_before_initial_commit,
        lose_registration_response,
    )
    .await;
    let published = git_output(target, &["log", "-1", "--format=%s", "develop"])
        .is_ok_and(|subject| subject == "chore: scaffold todo project");
    if result.is_err() && owned_destination && !published {
        let _ = std::fs::remove_dir_all(target);
    }
    result
}

async fn scaffold_owned(
    target: &Path,
    fail_before_initial_commit: bool,
    lose_registration_response: bool,
) -> Result<ScaffoldReport, String> {
    // Reuse the shipped bootstrap: git init, main+develop, docs/plans/README.md,
    // and the commit.gpgsign=false shield + repo-local identity.
    crate::folder_init::initialize_folder(target)?;
    // Put initial content on the base branch Makina drives.
    run_git(target, &["checkout", "develop"])?;
    let template_base_oid = git_output(target, &["rev-parse", "develop"])?;
    let template_base_short_oid = git_output(target, &["rev-parse", "--short", "develop"])?;
    let mut created = vec![target.join("docs/plans/README.md")];
    for (dest, contents) in TODO_FILES {
        let path = target.join(dest);
        let contents = contents
            .replace("{{BASE_OID}}", &template_base_oid)
            .replace("{{BASE_SHORT_OID}}", &template_base_short_oid);
        create_file(&path, &contents)?;
        created.push(path);
    }
    if fail_before_initial_commit {
        return Err("injected pre-publication scaffold failure".into());
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
        return Err(format!(
            "git commit failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let authored_oid = git_output(target, &["rev-parse", "develop"])?;
    let key = PlanKey::parse("docs/plans/0001-Todo-Starter").map_err(|e| e.to_string())?;
    let source = FilesystemPlanFileSource::new(target, Some(authored_oid.clone()))
        .map_err(|e| e.to_string())?;
    let plan = match load_plan(&source, key.clone(), &PlanReservations::default())
        .map_err(|report| format!("scaffold plan is invalid: {:?}", report.diagnostics))?
    {
        PlanCandidate::Plan(plan) => *plan,
        PlanCandidate::NotCandidate => return Err("scaffold plan is not a candidate".into()),
    };
    let coordinator = AuthoringCoordinator::new(
        target.to_path_buf(),
        "develop".into(),
        Arc::new(RepositoryLeaseRegistry::new()),
    );
    let registration = coordinator
        .publish_committed(key, authored_oid.clone(), plan.source_digest.to_string())
        .await
        .map_err(|e| format!("failed to register scaffold plan: {e}"))?;
    let CommandOutcome::PlanRegistered { registration_oid } = registration else {
        return Err("scaffold plan was not committed before registration".into());
    };
    if lose_registration_response {
        // The first successful response is deliberately discarded. Recovery
        // repeats the exact request and must observe the already-published R.
        let replay = coordinator
            .publish_committed(
                PlanKey::parse("docs/plans/0001-Todo-Starter").map_err(|e| e.to_string())?,
                authored_oid.clone(),
                plan.source_digest.to_string(),
            )
            .await
            .map_err(|e| format!("failed to recover lost scaffold registration response: {e}"))?;
        if !matches!(
            replay,
            CommandOutcome::PlanRegistered {
                registration_oid: replay_oid
            } if replay_oid == registration_oid
        ) {
            return Err("registration response-loss recovery did not return exact R".into());
        }
    }
    run_git(target, &["branch", "workspace", &authored_oid])?;
    run_git(target, &["checkout", "workspace"])?;
    let instructions = format!(
        "\nCreated exact registration {registration_oid} for the develop base.\nThe workspace branch is checked out at the authored scaffold commit; Makina may advance develop safely.\n\nNext:\n  cd {} && makina\n",
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
