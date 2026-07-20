use std::{fs, path::Path, process::Command};

use makina_core::plan::{
    FilesystemPlanFileSource, PlanCandidate, PlanKey, PlanReservations, load_plan,
};

#[test]
fn workflow_fixture_loads_through_the_shared_plan_contract() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    let destination = repo.path().join("docs/plans/0050-Workflow-Fixture");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.claude/workflows/fixtures/plan-bundle-v1"),
        &destination,
    );
    let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    let key = PlanKey::parse("docs/plans/0050-Workflow-Fixture").unwrap();
    let PlanCandidate::Plan(plan) = load_plan(&source, key, &PlanReservations::default()).unwrap()
    else {
        panic!("fixture must be a plan")
    };
    assert_eq!(plan.tasks.len(), 1);
    assert_eq!(plan.tasks[0].frontmatter.id.as_str(), "contract-client");
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

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
