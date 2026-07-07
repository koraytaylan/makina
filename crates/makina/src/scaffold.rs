use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// Template files mapped to their destinations.
const TODO_FILES: &[(&str, &str)] = &[
    ("Cargo.toml", include_str!("templates/todo/cargo_toml")),
    ("src/main.rs", include_str!("templates/todo/main_rs")),
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
        "docs/plans/0001-Todo-Starter/TASKS.md",
        include_str!("templates/todo/plan_tasks_md"),
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
        return Err(format!(
            "git commit failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let instructions = format!("\nNext:\n  cd {} && makina\n", target.display());
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
        let err = super::scaffold_project(&target, "todo").expect_err("must refuse");
        assert!(
            err.contains("non-empty"),
            "error explains the conflict: {err}"
        );
    }

    #[test]
    fn refuses_unknown_template() {
        let tmp = tempfile::tempdir().unwrap();
        let err = super::scaffold_project(&tmp.path().join("x"), "nope").expect_err("must refuse");
        assert!(err.contains("todo"), "lists available templates: {err}");
    }
}
