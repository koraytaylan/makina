//! Folder initialization: bootstrap git repo and docs/plans structure.

use std::path::Path;
use std::process::Command;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitIdentity {
    pub(crate) name: String,
    pub(crate) email: String,
}

fn global_git_config_value(key: &str) -> Result<Option<String>, String> {
    let output = Command::new("git")
        .args(["config", "--global", "--includes", "--get", key])
        .output()
        .map_err(|error| format!("failed to run git while reading global {key}: {error}"))?;
    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        return Ok((!value.is_empty()).then_some(value));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    Err(format!(
        "failed to read global Git {key}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

pub(crate) fn global_git_identity() -> Result<GitIdentity, String> {
    let name = global_git_config_value("user.name")?;
    let email = global_git_config_value("user.email")?;
    let mut missing = Vec::new();
    if name.is_none() {
        missing.push("user.name");
    }
    if email.is_none() {
        missing.push("user.email");
    }
    if !missing.is_empty() {
        return Err(format!(
            "global Git identity is not configured (missing {}). Configure it before creating a project:\n  git config --global user.name \"Your Name\"\n  git config --global user.email \"you@example.com\"",
            missing.join(" and ")
        ));
    }
    Ok(GitIdentity {
        name: name.expect("checked above"),
        email: email.expect("checked above"),
    })
}

/// Initialize a folder for use with Makina.
///
/// On success, the folder will have:
/// - A `.git` directory (created if absent)
/// - At least one commit on `main` (an empty initial commit if freshly created)
/// - A `develop` branch
/// - A `docs/plans/README.md` containing the plan authoring guide
/// - A `.gitignore` covering build output, when the folder had none
///
/// The function is idempotent: running it twice on an already-initialized folder
/// succeeds with no further changes.
pub fn initialize_folder(folder: &Path) -> Result<(), String> {
    initialize_folder_with_identity(folder, None)
}

pub(crate) fn initialize_folder_with_identity(
    folder: &Path,
    identity: Option<&GitIdentity>,
) -> Result<(), String> {
    // 1. Ensure a git repository exists.
    if !folder.join(".git").exists() {
        run_git(folder, &["init"])?;
    }

    // 2. Ensure there is at least one commit, on `main`. A fresh `git init`
    //    leaves HEAD unborn — no commit exists, so NO branch can be created yet
    //    (`git branch`/`checkout -b` need a commit to point at). Bootstrap only
    //    when HEAD is unborn, so re-running on an initialized repo is a no-op.
    if run_git(folder, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err() {
        // Shield this repo (and every worktree spawned from it) against a host
        // with `commit.gpgsign = true` and no signing key configured for
        // automation: engine-spawned commits (this bootstrap commit, and later
        // the Developer/merge actors) must never block on GPG, so disable
        // signing locally rather than relying on ambient config — mirrors the
        // hermetic shield in makina_core::test_support::init_git_repo_with_identity.
        run_git(folder, &["config", "commit.gpgsign", "false"])?;
        let mut commit = Command::new("git");
        commit.args([
            "commit",
            "--allow-empty",
            "-m",
            "chore: initialize repository",
        ]);
        if let Some(identity) = identity {
            commit
                .env("GIT_AUTHOR_NAME", &identity.name)
                .env("GIT_AUTHOR_EMAIL", &identity.email)
                .env("GIT_COMMITTER_NAME", &identity.name)
                .env("GIT_COMMITTER_EMAIL", &identity.email);
        }
        let output = commit
            .current_dir(folder)
            .output()
            .map_err(|e| format!("failed to run git: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git failed: {}", stderr));
        }
        // Name the initial branch `main` regardless of the user's init.defaultBranch.
        run_git(folder, &["branch", "-M", "main"])?;
    }

    // 3. Ensure a `develop` branch exists (created off main, only when absent).
    if run_git(folder, &["rev-parse", "--verify", "--quiet", "develop"]).is_err() {
        run_git(folder, &["branch", "develop"])?;
    }

    // 4. Create docs/plans/ and write the authoring-guide README verbatim from a
    //    committed template (see step 2). include_str! keeps the guide in a real
    //    Markdown file humans can edit and writes it with correct newlines.
    let plans_dir = folder.join("docs").join("plans");
    std::fs::create_dir_all(&plans_dir).map_err(|e| format!("failed to create docs/plans: {e}"))?;
    const PLANS_README: &str = include_str!("templates/plans_readme.md");
    let readme = plans_dir.join("README.md");
    if !readme.exists() {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&readme)
            .map_err(|e| format!("failed to create docs/plans/README.md: {e}"))?;
        file.write_all(PLANS_README.as_bytes())
            .map_err(|e| format!("failed to write docs/plans/README.md: {e}"))?;
    }

    // 5. Ensure the project ignores build output, for the same reason it needs
    //    a `develop` branch: Makina is about to drive agents in it.
    //
    //    A task tells its agent to build and verify, and the developer actor
    //    then commits the whole worktree (`git add -A`). In a repository with
    //    no ignore rules that sweeps every artifact the build produced into the
    //    task branch — hundreds of files no plan declared — and the footprint
    //    check rejects the task for touching them. The correction rounds cannot
    //    converge, because rebuilding is what the task asked for, so the first
    //    task that compiles anything fails the run.
    //
    //    Only written when the folder has no `.gitignore` at all: a project
    //    that already has one has already made these decisions.
    let gitignore = folder.join(".gitignore");
    if !gitignore.exists() {
        const PROJECT_GITIGNORE: &str = include_str!("templates/project_gitignore");
        std::fs::write(&gitignore, PROJECT_GITIGNORE)
            .map_err(|e| format!("failed to write .gitignore: {e}"))?;
    }

    // 6. Ensure .makina/.gitignore exists so transient runtime state (runs,
    //    worktrees, checkpoints) is never committed accidentally. Only creates
    //    the file if it doesn't already exist; never overwrites a user's
    //    custom rules.
    let makina_dir = folder.join(".makina");
    std::fs::create_dir_all(&makina_dir).map_err(|e| format!("failed to create .makina: {e}"))?;
    let gitignore = makina_dir.join(".gitignore");
    if !gitignore.exists() {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&gitignore)
            .map_err(|e| format!("failed to create .makina/.gitignore: {e}"))?;
        file.write_all(b"/runs/\n/worktrees/\n/checkpoints/\n")
            .map_err(|e| format!("failed to write .makina/.gitignore: {e}"))?;
    }

    Ok(())
}

fn run_git(folder: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(folder)
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git failed: {}", stderr));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn configure_test_identity(folder: &Path) {
        for args in [
            &["init"][..],
            &["config", "user.name", "Folder Init Test"][..],
            &["config", "user.email", "folder-init@example.invalid"][..],
        ] {
            let output = Command::new("git")
                .args(args)
                .current_dir(folder)
                .output()
                .expect("configure test Git identity");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn test_initialize_folder_creates_git_structure() {
        // Create a temporary directory
        let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
        let folder = temp_dir.path();
        configure_test_identity(folder);

        // Initialize the folder
        initialize_folder(folder).expect("initialize_folder failed");

        // Verify .git exists
        assert!(folder.join(".git").exists(), ".git directory should exist");

        // Verify HEAD resolves to a commit
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(folder)
            .output()
            .expect("git rev-parse HEAD failed");
        assert!(output.status.success(), "git rev-parse HEAD should succeed");

        // Verify both main and develop branches exist
        let output = Command::new("git")
            .args(["branch"])
            .current_dir(folder)
            .output()
            .expect("git branch failed");
        let branches = String::from_utf8_lossy(&output.stdout);
        assert!(
            branches.contains("main"),
            "main branch should exist, got: {}",
            branches
        );
        assert!(
            branches.contains("develop"),
            "develop branch should exist, got: {}",
            branches
        );

        // Verify docs/plans/README.md exists and contains the required headings
        let readme_path = folder.join("docs").join("plans").join("README.md");
        assert!(readme_path.exists(), "docs/plans/README.md should exist");

        let readme_content = fs::read_to_string(&readme_path).expect("failed to read README.md");
        assert!(
            readme_content.contains("## Layout"),
            "README should contain '## Layout' heading"
        );
        assert!(
            readme_content.contains("## Task document contract"),
            "README should contain the task document contract"
        );

        // Verify that the README does not contain the literal substring \n (it should use real newlines)
        assert!(
            !readme_content.contains("\\n"),
            "README should not contain literal '\\n' substring"
        );
    }

    #[test]
    fn test_initialize_folder_is_idempotent() {
        let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
        let folder = temp_dir.path();
        configure_test_identity(folder);

        // Initialize once
        initialize_folder(folder).expect("first initialize_folder failed");

        // Initialize again
        initialize_folder(folder).expect("second initialize_folder failed");

        // Verify everything still exists and is correct
        assert!(
            folder.join(".git").exists(),
            ".git directory should still exist"
        );

        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(folder)
            .output()
            .expect("git rev-parse HEAD failed");
        assert!(output.status.success(), "HEAD should still resolve");

        let output = Command::new("git")
            .args(["branch"])
            .current_dir(folder)
            .output()
            .expect("git branch failed");
        let branches = String::from_utf8_lossy(&output.stdout);
        assert!(branches.contains("main"));
        assert!(branches.contains("develop"));
    }

    /// A project Makina is about to drive must ignore build output.
    ///
    /// Without this the first task that compiles anything fails the run: the
    /// developer actor commits the whole worktree, the build's artifacts go
    /// with it, and the footprint check rejects the task for hundreds of paths
    /// no plan declared and no correction round can remove.
    #[test]
    fn initializing_a_folder_ignores_build_output() {
        let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
        let folder = temp_dir.path();
        configure_test_identity(folder);

        initialize_folder(folder).expect("initialize_folder failed");

        let ignore = fs::read_to_string(folder.join(".gitignore")).expect(".gitignore is written");
        assert!(ignore.contains("/target/"), "Rust build output: {ignore}");
        assert!(
            ignore.contains("node_modules/"),
            "JS dependencies: {ignore}"
        );
        assert!(ignore.contains("__pycache__/"), "Python bytecode: {ignore}");

        // And git actually honours it: a build directory left in the tree is
        // not something `git add -A` can pick up.
        fs::create_dir_all(folder.join("target/debug")).expect("fake build output");
        fs::write(folder.join("target/debug/artifact"), "binary").expect("fake artifact");
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(folder)
            .output()
            .expect("git status");
        let status = String::from_utf8_lossy(&output.stdout);
        assert!(
            !status.contains("target/"),
            "build output must not show as a change: {status}"
        );
    }

    /// A project that already has ignore rules has already made these
    /// decisions; Makina does not rewrite them.
    #[test]
    fn an_existing_gitignore_is_left_alone() {
        let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
        let folder = temp_dir.path();
        configure_test_identity(folder);
        fs::write(folder.join(".gitignore"), "/my-own-build-dir\n").expect("seed .gitignore");

        initialize_folder(folder).expect("initialize_folder failed");

        assert_eq!(
            fs::read_to_string(folder.join(".gitignore")).expect("still there"),
            "/my-own-build-dir\n",
        );
    }
}
