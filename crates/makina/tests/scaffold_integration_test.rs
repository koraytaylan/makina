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
    assert!(
        cfg.contains("base_branch = \"develop\""),
        "config base_branch"
    );

    let plan_dir = target.join("docs/plans/0001-Todo-Starter");
    let tasks = std::fs::read_to_string(plan_dir.join("TASKS.md")).unwrap();
    assert!(tasks.contains("Depends on:"), "TASKS has Depends on");
    assert!(tasks.contains("Done when:"), "TASKS has Done when");
    assert!(plan_dir.join("SCOPE.md").exists(), "SCOPE.md");
    assert!(plan_dir.join("ARCHITECTURE.md").exists(), "ARCHITECTURE.md");
    assert!(plan_dir.join("STATUS.md").exists(), "STATUS.md");

    assert!(
        report.instructions.contains("cd"),
        "report tells the user to cd + run"
    );
}

#[test]
fn scaffold_refuses_non_empty_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("occupied");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("keep.txt"), "x").unwrap();
    let err = makina::scaffold::scaffold_project(&target, "todo")
        .expect_err("must refuse a non-empty target");
    assert!(
        err.contains("non-empty"),
        "error explains the conflict: {err}"
    );
}

#[test]
#[ignore = "compiles the scaffolded crate; slow, run explicitly"]
fn scaffolded_todo_project_passes_its_own_gates() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("todo");
    makina::scaffold::scaffold_project(&target, "todo").unwrap();
    let status = std::process::Command::new("cargo")
        .args(["test"])
        .current_dir(&target)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "scaffolded todo project's cargo test must pass"
    );
}
