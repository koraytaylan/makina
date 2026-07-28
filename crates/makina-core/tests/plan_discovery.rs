use std::fs;
use std::path::Path;

use makina_core::orchestrator::{PlanDiscoveryState, discover_plans, load_authoritative_plan};

#[test]
fn working_candidate_moves_from_awaiting_commit_to_unregistered() {
    let repo = fixture_repo();
    let entries = discover_plans(repo.path());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state, PlanDiscoveryState::AwaitingCommit);
    assert!(entries[0].document.is_some());

    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "add plan"]);
    let entries = discover_plans(repo.path());
    assert_eq!(entries[0].state, PlanDiscoveryState::Unregistered);
}

#[test]
fn invalid_candidate_is_visible_with_shared_diagnostics() {
    let repo = fixture_repo();
    fs::write(
        repo.path()
            .join("docs/plans/0049-Sample/tasks/0101-sample.md"),
        "not frontmatter\n",
    )
    .unwrap();
    let entries = discover_plans(repo.path());
    assert_eq!(entries[0].state, PlanDiscoveryState::Invalid);
    assert!(entries[0].document.is_none());
    assert!(!entries[0].diagnostics.is_empty());
}

#[test]
fn historical_directory_is_inert_but_reserves_its_number() {
    let repo = fixture_repo();
    fs::remove_dir_all(repo.path().join("docs/plans/0049-Sample/tasks")).unwrap();
    assert!(discover_plans(repo.path()).is_empty());

    copy_tree(&fixture_path(), &repo.path().join("docs/plans/0049-Other"));
    let entries = discover_plans(repo.path());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state, PlanDiscoveryState::Invalid);
    assert!(
        entries[0]
            .diagnostics
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "reserved-plan-number")
    );
}

#[test]
fn configured_base_candidate_is_visible_without_a_working_directory() {
    let repo = fixture_repo();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "add base plan"]);
    git(repo.path(), &["switch", "-q", "-c", "operator"]);
    git(repo.path(), &["rm", "-q", "-r", "."]);
    git(repo.path(), &["commit", "-qm", "remove operator copy"]);

    let entries = discover_plans(repo.path());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state, PlanDiscoveryState::Unregistered);
    assert!(entries[0].document.is_some());

    // The authoritative loader is also the restart path: it must recover the
    // complete task document from the configured base even though the current
    // checkout has no plan directory.
    let loaded = load_authoritative_plan(repo.path(), &entries[0].key).unwrap();
    assert_eq!(loaded.tasks[0].body, entries[0].tasks()[0].body);
}

#[test]
fn arbitrary_commit_after_registration_is_rejected() {
    let repo = fixture_repo();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "source"]);
    let base = git_output(repo.path(), &["rev-parse", "HEAD"]);
    // This deliberately malformed R is sufficient to make the retained ref a
    // scanner candidate; the following arbitrary commit must never be treated
    // as active lifecycle evidence.
    git(
        repo.path(),
        &[
            "commit",
            "--allow-empty",
            "-qm",
            &format!(
                "register\n\nMakina-Phase: plan-registration\nMakina-Plan: 0049-Sample\nMakina-Source-Digest: x\nMakina-Executable-Digest: y\nMakina-Validation-Base: {base}"
            ),
        ],
    );
    git(repo.path(), &["branch", "plan/0049-Sample"]);
    git(repo.path(), &["switch", "-q", "plan/0049-Sample"]);
    git(
        repo.path(),
        &["commit", "--allow-empty", "-qm", "arbitrary"],
    );

    let entries = discover_plans(repo.path());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state, PlanDiscoveryState::Invalid);
}

#[test]
fn retained_r_only_plan_becomes_active_on_verified_claim_lineage() {
    let repo = fixture_repo();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "base source"]);
    let base = git_output(repo.path(), &["rev-parse", "HEAD"]);
    git(repo.path(), &["switch", "-q", "-c", "registration"]);
    let status = repo.path().join("docs/plans/0049-Sample/STATUS.md");
    let contents = fs::read_to_string(&status)
        .unwrap()
        .replace("validation base —", &format!("validation base `{base}`"));
    fs::write(status, contents).unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "registration tree"]);
    let document = discover_plans(repo.path())[0].document.clone().unwrap();
    git(
        repo.path(),
        &[
            "commit",
            "--amend",
            "-qm",
            &format!(
                "register\n\nMakina-Phase: plan-registration\nMakina-Plan: 0049-Sample\nMakina-Source-Digest: {}\nMakina-Executable-Digest: {}\nMakina-Validation-Base: {base}",
                document.source_digest, document.executable_digest
            ),
        ],
    );
    git(repo.path(), &["branch", "plan/0049-Sample"]);
    git(repo.path(), &["switch", "-q", "develop"]);

    let ready = discover_plans(repo.path());
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].state, PlanDiscoveryState::Ready);

    // A restart may see no trustworthy checkout copy (or a locally edited
    // one). The loader must keep using the exact retained R tree, including
    // authored task prose, rather than silently switching sources.
    let retained_body = ready[0].tasks()[0].body.clone();
    fs::write(
        repo.path()
            .join("docs/plans/0049-Sample/tasks/0101-sample.md"),
        "local checkout content that must never become retained history\n",
    )
    .unwrap();
    let key = ready[0].key.clone();
    let reloaded = load_authoritative_plan(repo.path(), &key).unwrap();
    assert_eq!(reloaded.tasks[0].body, retained_body);

    git(repo.path(), &["switch", "-q", "plan/0049-Sample"]);
    git(
        repo.path(),
        &[
            "commit",
            "--allow-empty",
            "-qm",
            "claim\n\nMakina-Phase: task-status\nMakina-Plan: 0049-Sample\nMakina-Task: sample-task\nMakina-Run: run-1",
        ],
    );
    let active = discover_plans(repo.path());
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].state, PlanDiscoveryState::Active);
}

#[test]
fn retained_full_lifecycle_lineage_is_accepted() {
    let repo = registered_repo();
    let claim = lifecycle_message("task-status", "Makina-Task: sample-task\nMakina-Run: run-1");
    commit_message(repo.path(), &claim);
    let landing = commit_message(
        repo.path(),
        "landing\n\nMakina-Plan: 0049-Sample\nMakina-Task: sample-task\nMakina-Run: run-1",
    );
    commit_message(
        repo.path(),
        &lifecycle_message(
            "task-status",
            &format!("Makina-Task: sample-task\nMakina-Run: run-1\nMakina-Landing: {landing}"),
        ),
    );
    let prepared = commit_message(
        repo.path(),
        &lifecycle_message(
            "finalization-prepared",
            "Makina-Run: run-1\nMakina-Final-Mode: squash\nMakina-Expected-Base: base",
        ),
    );
    let integrated = commit_message(
        repo.path(),
        &lifecycle_message(
            "final-integration",
            &format!("Makina-Run: run-1\nMakina-Final-Mode: squash\nMakina-Plan-Tip: {prepared}"),
        ),
    );
    commit_message(
        repo.path(),
        &lifecycle_message(
            "completion",
            &format!("Makina-Run: run-1\nMakina-Final-Commit: {integrated}"),
        ),
    );

    let entries = discover_plans(repo.path());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state, PlanDiscoveryState::Active);
}

#[test]
fn retained_disposition_and_registration_refresh_chains_are_accepted() {
    let repo = registered_repo();
    let ready = discover_plans(repo.path());
    let document = ready[0].document.as_ref().unwrap();
    let source = document.source_digest.to_string();
    let executable = document.executable_digest.to_string();
    let validation_base = document
        .status
        .validation_base_oid
        .as_ref()
        .unwrap()
        .to_string();
    commit_message(
        repo.path(),
        &lifecycle_message(
            "task-disposition",
            &format!(
                "Makina-Task: sample-task\nMakina-Run: run-1\nMakina-Previous-Source-Digest: {source}\nMakina-New-Source-Digest: {source}\nMakina-Previous-Plan-Digest: {executable}\nMakina-New-Plan-Digest: {executable}"
            ),
        ),
    );
    assert_eq!(
        discover_plans(repo.path())[0].state,
        PlanDiscoveryState::Active
    );

    let previous_registration = git_output(repo.path(), &["rev-list", "--reverse", "--all"])
        .lines()
        .find(|oid| {
            git_output(repo.path(), &["show", "-s", "--format=%B", oid])
                .contains("Makina-Phase: plan-registration")
        })
        .unwrap()
        .to_owned();
    commit_message(
        repo.path(),
        &format!(
            "refresh registration\n\nMakina-Phase: plan-registration\nMakina-Plan: 0049-Sample\nMakina-Source-Digest: {source}\nMakina-Executable-Digest: {executable}\nMakina-Validation-Base: {validation_base}\nMakina-Previous-Registration: {previous_registration}"
        ),
    );
    assert_eq!(
        discover_plans(repo.path())[0].state,
        PlanDiscoveryState::Ready
    );
}

#[test]
fn retained_lifecycle_rejects_skips_duplicates_mismatches_and_out_of_order_phases() {
    let invalid_sequences = [
        vec![lifecycle_message(
            "task-status",
            "Makina-Task: sample-task\nMakina-Run: run-1\nMakina-Landing: missing-a",
        )],
        vec![
            lifecycle_message("task-status", "Makina-Task: sample-task\nMakina-Run: run-1"),
            lifecycle_message("task-status", "Makina-Task: sample-task\nMakina-Run: run-1"),
        ],
        vec![
            lifecycle_message("task-status", "Makina-Task: sample-task\nMakina-Run: run-1"),
            "landing\n\nMakina-Plan: 0049-Sample\nMakina-Task: other-task\nMakina-Run: run-1"
                .into(),
        ],
        vec![lifecycle_message(
            "final-integration",
            "Makina-Run: run-1\nMakina-Final-Mode: squash\nMakina-Plan-Tip: missing-p",
        )],
        vec![lifecycle_message(
            "completion",
            "Makina-Run: run-1\nMakina-Final-Commit: missing-f",
        )],
    ];

    for sequence in invalid_sequences {
        let repo = registered_repo();
        for message in sequence {
            commit_message(repo.path(), &message);
        }
        let entries = discover_plans(repo.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, PlanDiscoveryState::Invalid);
    }
}

fn registered_repo() -> tempfile::TempDir {
    let repo = fixture_repo();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "base source"]);
    let base = git_output(repo.path(), &["rev-parse", "HEAD"]);
    git(repo.path(), &["switch", "-q", "-c", "registration"]);
    let status = repo.path().join("docs/plans/0049-Sample/STATUS.md");
    let contents = fs::read_to_string(&status)
        .unwrap()
        .replace("validation base —", &format!("validation base `{base}`"));
    fs::write(status, contents).unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "registration tree"]);
    let document = discover_plans(repo.path())[0].document.clone().unwrap();
    git(
        repo.path(),
        &[
            "commit",
            "--amend",
            "-qm",
            &format!(
                "register\n\nMakina-Phase: plan-registration\nMakina-Plan: 0049-Sample\nMakina-Source-Digest: {}\nMakina-Executable-Digest: {}\nMakina-Validation-Base: {base}",
                document.source_digest, document.executable_digest
            ),
        ],
    );
    git(repo.path(), &["branch", "plan/0049-Sample"]);
    git(repo.path(), &["switch", "-q", "plan/0049-Sample"]);
    repo
}

fn lifecycle_message(phase: &str, trailers: &str) -> String {
    format!("{phase}\n\nMakina-Phase: {phase}\nMakina-Plan: 0049-Sample\n{trailers}")
}

fn commit_message(repo: &Path, message: &str) -> String {
    git(repo, &["commit", "--allow-empty", "-qm", message]);
    git_output(repo, &["rev-parse", "HEAD"])
}

fn fixture_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "develop"]);
    git(
        repo.path(),
        &["config", "user.email", "scanner@example.invalid"],
    );
    git(repo.path(), &["config", "user.name", "Scanner Test"]);
    copy_tree(&fixture_path(), &repo.path().join("docs/plans/0049-Sample"));
    repo
}

fn fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plan-bundles/valid/0049-Sample")
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn git(repo: &Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap()
            .success()
    );
}

fn git_output(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
