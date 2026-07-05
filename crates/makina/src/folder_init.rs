//! Folder initialization: bootstrap git repo and docs/plans structure.

use std::path::Path;
use std::process::Command;

/// Initialize a folder for use with Makina.
///
/// On success, the folder will have:
/// - A `.git` directory (created if absent)
/// - At least one commit on `main` (an empty initial commit if freshly created)
/// - A `develop` branch
/// - A `docs/plans/README.md` containing the plan authoring guide
///
/// The function is idempotent: running it twice on an already-initialized folder
/// succeeds with no further changes.
pub fn initialize_folder(folder: &Path) -> Result<(), String> {
    // 1. Ensure a git repository exists.
    if !folder.join(".git").exists() {
        run_git(folder, &["init"])?;
    }

    // 2. Ensure there is at least one commit, on `main`. A fresh `git init`
    //    leaves HEAD unborn — no commit exists, so NO branch can be created yet
    //    (`git branch`/`checkout -b` need a commit to point at). Bootstrap only
    //    when HEAD is unborn, so re-running on an initialized repo is a no-op.
    if run_git(folder, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err() {
        // Leave a repo-local identity behind when the machine has none configured,
        // so later commits in this repo (e.g. by agents) don't fail on identity.
        if run_git(folder, &["config", "user.email"]).is_err() {
            run_git(folder, &["config", "user.email", "makina@localhost"])?;
            run_git(folder, &["config", "user.name", "Makina"])?;
        }
        // Pin the bootstrap commit's identity via env (env beats config): the commit
        // is Makina's, and resolving identity from ambient config here is racy when
        // the process's HOME changes between the probe above and this commit.
        let output = Command::new("git")
            .args([
                "commit",
                "--allow-empty",
                "-m",
                "chore: initialize repository",
            ])
            .env("GIT_AUTHOR_NAME", "Makina")
            .env("GIT_AUTHOR_EMAIL", "makina@localhost")
            .env("GIT_COMMITTER_NAME", "Makina")
            .env("GIT_COMMITTER_EMAIL", "makina@localhost")
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
    std::fs::write(plans_dir.join("README.md"), PLANS_README)
        .map_err(|e| format!("failed to write docs/plans/README.md: {e}"))?;

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

    #[test]
    fn test_initialize_folder_creates_git_structure() {
        // Create a temporary directory
        let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
        let folder = temp_dir.path();

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
            readme_content.contains("## TASKS.md contract"),
            "README should contain '## TASKS.md contract' heading"
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
}
