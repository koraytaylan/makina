use makina_core::orchestrator::{AuthoringCoordinator, PlanContractCoordinator};
use makina_core::orchestrator::{PlanDiscoveryState, discover_plans};
use makina_core::plan::{AuthoredTaskStatus, PlanIntegrationState, PlanKey, TaskId};
use makina_core::test_support::run_git;
use makina_core::{
    checkpoint::{CheckpointIdentity, checkpoint_path, load_checkpoint, persist_checkpoint},
    plan_runtime::ProjectedTaskGraph,
    repository_lease::RepositoryLeaseRegistry,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;

static HOME_LOCK: Mutex<()> = Mutex::const_new(());

struct RestoreHome(Option<std::ffi::OsString>);

impl Drop for RestoreHome {
    fn drop(&mut self) {
        unsafe {
            match self.0.take() {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

fn use_test_home(path: &std::path::Path) -> RestoreHome {
    std::fs::create_dir_all(path).expect("create isolated HOME");
    let prior = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", path) };
    RestoreHome(prior)
}

fn workspace_snapshot(
    root: &std::path::Path,
) -> (String, Vec<u8>, Vec<u8>, BTreeMap<String, Vec<u8>>) {
    fn visit(root: &std::path::Path, dir: &std::path::Path, files: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            // Skip .git (version control internals) and transient Makina
            // runtime state (runs/, worktrees/, checkpoints/) which now lives
            // in-repo under .makina/ but is gitignored and must not appear in
            // the workspace comparison.
            if path == root.join(".git") {
                continue;
            }
            if path == root.join(".makina").join("runs")
                || path == root.join(".makina").join("worktrees")
                || path == root.join(".makina").join("checkpoints")
            {
                continue;
            }
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let head = run_git(root, &["rev-parse", "HEAD"]);
    let index = run_git(root, &["diff", "--cached", "--binary"]);
    let worktree = run_git(root, &["diff", "--binary"]);
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    (
        String::from_utf8_lossy(&head.stdout).trim().into(),
        index.stdout,
        worktree.stdout,
        files,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn scaffold_creates_runnable_todo_project() {
    let _lock = HOME_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let _home = use_test_home(&tmp.path().join("home"));
    let target = tmp.path().join("todo");
    let report = makina::scaffold::scaffold_project(&target, "todo")
        .await
        .expect("scaffold_project should succeed on an empty target");

    assert!(target.join(".git").exists(), ".git must exist");
    let branches = run_git(&target, &["branch", "--format=%(refname:short)"]);
    let branches = String::from_utf8_lossy(&branches.stdout);
    assert!(branches.contains("main"), "main branch: {branches}");
    assert!(branches.contains("develop"), "develop branch: {branches}");

    let head = run_git(&target, &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "workspace");
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

    let plan_dir = target.join("docs/plans/0001-todo-core");
    assert!(!plan_dir.join("TASKS.md").exists(), "no legacy TASKS.md");
    assert!(
        plan_dir.join("tasks/0101-add-task-toggle.md").exists(),
        "per-task document"
    );
    assert!(plan_dir.join("SCOPE.md").exists(), "SCOPE.md");
    assert!(plan_dir.join("ARCHITECTURE.md").exists(), "ARCHITECTURE.md");
    assert!(plan_dir.join("STATUS.md").exists(), "STATUS.md");
    assert!(
        target
            .join("docs/plans/0002-todo-features/SCOPE.md")
            .exists(),
        "plan 0002 SCOPE.md"
    );
    assert!(
        target
            .join("docs/plans/0003-todo-integration/SCOPE.md")
            .exists(),
        "plan 0003 SCOPE.md"
    );

    let plans = discover_plans(&target);
    assert_eq!(plans.len(), 3, "three loader-valid scaffold plans");
    for plan in &plans {
        assert_eq!(
            plan.state,
            PlanDiscoveryState::Ready,
            "exact R is ready for {}",
            plan.key.relative_dir.display()
        );
    }
    let plan0001 = plans
        .iter()
        .find(|p| p.key.number == "0001")
        .expect("plan 0001 present");
    assert_eq!(
        plan0001.tasks().len(),
        3,
        "three per-task documents in plan 0001"
    );
    let registered = run_git(&target, &["rev-parse", "refs/heads/plan/0001-todo-core"]);
    let registered = String::from_utf8_lossy(&registered.stdout)
        .trim()
        .to_owned();
    let develop = run_git(&target, &["rev-parse", "develop"]);
    assert_ne!(
        registered,
        String::from_utf8_lossy(&develop.stdout).trim(),
        "exact registration R is retained separately from develop"
    );
    let workspace = run_git(&target, &["rev-parse", "workspace"]);
    assert_eq!(
        workspace.stdout, develop.stdout,
        "workspace stays at authored commit"
    );

    assert!(
        report.instructions.contains("cd"),
        "report tells the user to cd + run"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn scaffold_supports_a_nested_new_destination() {
    let _lock = HOME_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let _home = use_test_home(&tmp.path().join("home"));
    let target = tmp.path().join("nested/project/todo");
    makina::scaffold::scaffold_project(&target, "todo")
        .await
        .expect("nested scaffold");
    assert!(target.join(".git").is_dir());
    assert_eq!(discover_plans(&target)[0].state, PlanDiscoveryState::Ready);
}

#[tokio::test(flavor = "current_thread")]
async fn scaffold_prepublication_failure_removes_only_a_new_destination() {
    let _lock = HOME_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let _home = use_test_home(&tmp.path().join("home"));
    let parent = tmp.path().join("existing-parent");
    std::fs::create_dir_all(&parent).unwrap();
    let target = parent.join("new-project");
    let error = makina::scaffold::scaffold_project_with_test_hooks(
        &target,
        "todo",
        makina::scaffold::ScaffoldTestHooks {
            fail_before_initial_commit: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(error.contains("injected pre-publication"));
    assert!(!target.exists(), "newly owned destination is removed");
    assert!(parent.is_dir(), "pre-existing parent is preserved");

    let existing = parent.join("existing-empty");
    std::fs::create_dir(&existing).unwrap();
    makina::scaffold::scaffold_project_with_test_hooks(
        &existing,
        "todo",
        makina::scaffold::ScaffoldTestHooks {
            fail_before_initial_commit: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(existing.is_dir(), "caller-owned destination is preserved");
}

#[tokio::test(flavor = "current_thread")]
async fn scaffold_recovers_registration_response_loss_with_exact_r() {
    let _lock = HOME_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let _home = use_test_home(&tmp.path().join("home"));
    let target = tmp.path().join("todo");
    makina::scaffold::scaffold_project_with_test_hooks(
        &target,
        "todo",
        makina::scaffold::ScaffoldTestHooks {
            lose_registration_response: true,
            ..Default::default()
        },
    )
    .await
    .expect("response-loss recovery");
    let plan_ref = "refs/heads/plan/0001-todo-core";
    let r = run_git(&target, &["rev-parse", plan_ref]);
    let count = run_git(&target, &["rev-list", "--count", plan_ref]);
    assert_eq!(
        String::from_utf8_lossy(&count.stdout).trim(),
        "3",
        "bootstrap + authored + one exact R; replay minted no R+1"
    );
    let plans = discover_plans(&target);
    let plan0001 = plans
        .iter()
        .find(|p| p.key.number == "0001")
        .expect("plan 0001 present");
    assert_eq!(plan0001.state, PlanDiscoveryState::Ready);
    assert!(!r.stdout.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn scaffold_sample_runs_claim_a_b_p_f_c_without_touching_workspace() {
    let _lock = HOME_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let _home = use_test_home(&tmp.path().join("home"));
    let target = tmp.path().join("todo");
    makina::scaffold::scaffold_project(&target, "todo")
        .await
        .expect("scaffold");
    let before = workspace_snapshot(&target);
    let plan_ref = "refs/heads/plan/0001-todo-core";
    let run = "01AAAAAAAAAAAAAAAAAAAAAAAA";
    // Extend the starter through a separate develop worktree with a fourth,
    // independent task (disjoint footprint `src/list.rs`), then refresh exact
    // R. The checked-out operator workspace remains pinned to the original
    // authored commit throughout.
    let author = tmp.path().join("author");
    run_git(
        &target,
        &["worktree", "add", author.to_str().unwrap(), "develop"],
    );
    std::fs::write(
        author.join("src/list.rs"),
        "pub fn summary() -> usize { 0 }\n",
    )
    .unwrap();
    let second = r#"---
id: add-list-summary
title: Add A List Summary
workstream: "0001"
kind: task
depends_on: []
gated: false
touches:
  - src/list.rs
status: planned
merged_as: ""
---
# Add A List Summary

Implement the list summary independently from task toggling.

**Steps:**

1. Update `summary` in `src/list.rs`.
2. Verify the focused implementation.

- **Done when:** `summary` returns the implemented value.
"#;
    std::fs::write(
        author.join("docs/plans/0001-todo-core/tasks/0104-add-list-summary.md"),
        second,
    )
    .unwrap();
    for relative in [
        "docs/plans/0001-todo-core/STATUS.md",
        "docs/plans/STATUS.md",
    ] {
        let path = author.join(relative);
        let value = std::fs::read_to_string(&path)
            .unwrap()
            .replace("0/3", "0/4");
        std::fs::write(path, value).unwrap();
    }
    run_git(&author, &["add", "."]);
    run_git(
        &author,
        &["commit", "-m", "docs: add independent sample task"],
    );
    let authored = String::from_utf8_lossy(&run_git(&target, &["rev-parse", "develop"]).stdout)
        .trim()
        .to_owned();
    let key = PlanKey::parse("docs/plans/0001-todo-core").unwrap();
    let source = makina_core::plan::GitTreePlanFileSource::new(&target, &authored).unwrap();
    let makina_core::plan::PlanCandidate::Plan(authored_plan) =
        makina_core::plan::load_plan_path(&source, key.relative_dir.clone(), &Default::default())
            .unwrap()
    else {
        panic!("four-task authored plan is invalid")
    };
    let registration = AuthoringCoordinator::new(
        target.clone(),
        "develop".into(),
        Arc::new(RepositoryLeaseRegistry::new()),
    )
    .publish_committed(
        key.clone(),
        authored,
        authored_plan.source_digest.to_string(),
    )
    .await
    .unwrap();
    let makina_core::api::CommandOutcome::PlanRegistered {
        registration_oid: r,
    } = registration
    else {
        panic!("four-task refresh did not publish exact R")
    };
    run_git(&target, &["worktree", "remove", author.to_str().unwrap()]);
    let coordinator = PlanContractCoordinator::new(&target, key, plan_ref, run).unwrap();
    let task = TaskId::parse("add-task-toggle").unwrap();
    let second_task = TaskId::parse("add-list-summary").unwrap();
    let ready = coordinator.ready_tasks().unwrap();
    assert_eq!(ready.tasks, vec![task.clone(), second_task.clone()]);
    let claim = coordinator
        .claim_task(&task, &r, "_Last updated: 2026-07-20, task claimed._")
        .await
        .unwrap();
    let task_worktree = coordinator.ensure_task_worktree(&task).await.unwrap();
    let second_claim = coordinator
        .claim_task(
            &second_task,
            &claim,
            "_Last updated: 2026-07-20, parallel task claimed._",
        )
        .await
        .unwrap();
    let second_worktree = coordinator
        .ensure_task_worktree(&second_task)
        .await
        .unwrap();
    let main_path = task_worktree.join("src/main.rs");
    let main = std::fs::read_to_string(&main_path).unwrap().replace(
        "    pub fn new(title: &str) -> Self {",
        "    pub fn toggle(&mut self) {\n        self.done = !self.done;\n    }\n\n    pub fn new(title: &str) -> Self {",
    );
    std::fs::write(&main_path, main).unwrap();
    run_git(&task_worktree, &["add", "src/main.rs"]);
    run_git(&task_worktree, &["commit", "-m", "feat: add task toggle"]);
    let a = coordinator.land_phase_a(&task).await.unwrap();
    let a_typed = coordinator.parse_oid(a.clone()).unwrap();
    let b = coordinator
        .complete_task(
            &task,
            &a,
            a_typed,
            "_Last updated: 2026-07-20, task landed._",
        )
        .await
        .unwrap();
    // Land the two sequential dependents of `add-task-toggle` so the plan can
    // finalize. Each builds on the previous one's `src/main.rs` edit.
    let count_task = TaskId::parse("add-count-open").unwrap();
    let count_claim = coordinator
        .claim_task(
            &count_task,
            &b,
            "_Last updated: 2026-07-20, sequential task claimed._",
        )
        .await
        .unwrap();
    let count_worktree = coordinator.ensure_task_worktree(&count_task).await.unwrap();
    {
        let main_path = count_worktree.join("src/main.rs");
        let main = std::fs::read_to_string(&main_path).unwrap();
        std::fs::write(
            &main_path,
            main.replace(
                "fn main() {",
                "pub fn count_open(tasks: &[Task]) -> usize {\n    tasks.iter().filter(|t| !t.done).count()\n}\n\nfn main() {",
            ),
        )
        .unwrap();
        run_git(&count_worktree, &["add", "src/main.rs"]);
        run_git(&count_worktree, &["commit", "-m", "feat: add count_open"]);
    }
    let count_a = coordinator.land_phase_a(&count_task).await.unwrap();
    let count_a_typed = coordinator.parse_oid(count_a.clone()).unwrap();
    let count_b = coordinator
        .complete_task(
            &count_task,
            &count_a,
            count_a_typed,
            "_Last updated: 2026-07-20, sequential task landed._",
        )
        .await
        .unwrap();
    let _ = count_claim;

    let summary_task = TaskId::parse("add-print-summary").unwrap();
    let summary_claim = coordinator
        .claim_task(
            &summary_task,
            &count_b,
            "_Last updated: 2026-07-20, sequential task claimed._",
        )
        .await
        .unwrap();
    let summary_worktree = coordinator
        .ensure_task_worktree(&summary_task)
        .await
        .unwrap();
    {
        let main_path = summary_worktree.join("src/main.rs");
        let main = std::fs::read_to_string(&main_path).unwrap();
        std::fs::write(
            &main_path,
            main.replace(
                "fn main() {",
                "pub fn print_summary(tasks: &[Task]) {\n    println!(\"{} open / {} total\", count_open(tasks), tasks.len());\n}\n\nfn main() {",
            ),
        )
        .unwrap();
        run_git(&summary_worktree, &["add", "src/main.rs"]);
        run_git(
            &summary_worktree,
            &["commit", "-m", "feat: add print_summary"],
        );
    }
    let summary_a = coordinator.land_phase_a(&summary_task).await.unwrap();
    let summary_a_typed = coordinator.parse_oid(summary_a.clone()).unwrap();
    let summary_b = coordinator
        .complete_task(
            &summary_task,
            &summary_a,
            summary_a_typed,
            "_Last updated: 2026-07-20, sequential task landed._",
        )
        .await
        .unwrap();
    let _ = summary_claim;

    let second_path = second_worktree.join("src/list.rs");
    std::fs::write(&second_path, "pub fn summary() -> usize { 1 }\n").unwrap();
    run_git(&second_worktree, &["add", "src/list.rs"]);
    run_git(
        &second_worktree,
        &["commit", "-m", "feat: implement list summary"],
    );
    let second_a = coordinator.land_phase_a(&second_task).await.unwrap();
    assert!(matches!(
        coordinator.inspect_task(&second_task).await.unwrap(),
        makina_core::landing::TaskEvidenceState::LandingPending { ref implementation_oid }
            if implementation_oid == &second_a
    ));
    assert_eq!(
        coordinator.land_phase_a(&second_task).await.unwrap(),
        second_a,
        "lost Phase-A response reconciles to exact retained A"
    );
    let second_a_typed = coordinator.parse_oid(second_a.clone()).unwrap();
    let second_b = coordinator
        .complete_task(
            &second_task,
            &second_a,
            second_a_typed,
            "_Last updated: 2026-07-20, interrupted landing reconciled._",
        )
        .await
        .unwrap();
    let prepared = coordinator
        .prepare_finalization(&second_b, "squash", "_Last updated: 2026-07-20, prepared._")
        .await
        .unwrap();
    let p = prepared.prepared_oid.clone();
    let f = coordinator
        .integrate_finalization(&prepared, None)
        .await
        .unwrap();
    let c = coordinator
        .complete_finalization(&prepared, &f, "_Last updated: 2026-07-20, complete._")
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run_git(&target, &["rev-parse", "develop"]).stdout).trim(),
        c
    );
    assert!(
        String::from_utf8_lossy(&run_git(&target, &["show", "develop:src/main.rs"]).stdout)
            .contains("pub fn toggle")
    );
    for (oid, phase) in [
        (claim, "task-claim"),
        (b, "task-status"),
        (count_b, "task-status"),
        (summary_b, "task-status"),
        (second_claim, "task-claim"),
        (second_b, "task-status"),
        (p, "finalization-prepared"),
        (f, "final-integration"),
        (c.clone(), "completion"),
    ] {
        let message = run_git(&target, &["show", "-s", "--format=%B", &oid]);
        assert!(
            String::from_utf8_lossy(&message.stdout).contains(&format!("Makina-Phase: {phase}"))
        );
    }
    let a_message = run_git(&target, &["show", "-s", "--format=%B", &a]);
    assert!(String::from_utf8_lossy(&a_message.stdout).contains("Makina-Task: add-task-toggle"));
    let final_source = makina_core::plan::GitTreePlanFileSource::new(&target, &c).unwrap();
    let makina_core::plan::PlanCandidate::Plan(final_plan) = makina_core::plan::load_plan_path(
        &final_source,
        "docs/plans/0001-todo-core",
        &Default::default(),
    )
    .unwrap() else {
        panic!("completed develop tree lost sample plan")
    };
    assert!(
        final_plan
            .tasks
            .iter()
            .all(|task| task.frontmatter.status == AuthoredTaskStatus::Done)
    );
    assert_eq!(
        final_plan.status.integration_state,
        PlanIntegrationState::Complete
    );
    let final_root = run_git(&target, &["show", &format!("{c}:docs/plans/STATUS.md")]);
    let final_root = String::from_utf8_lossy(&final_root.stdout);
    assert!(final_root.contains("| 0001 | Todo Core (Sequential) | Complete | 4/4 |"));
    assert_eq!(
        workspace_snapshot(&target),
        before,
        "workspace HEAD/index/tree unchanged through C"
    );
    let projected = ProjectedTaskGraph::from_document(&final_plan, chrono::Utc::now());
    persist_checkpoint(
        &target,
        CheckpointIdentity::from_plan(&final_plan),
        &projected.graph,
    )
    .await
    .unwrap();
    // Checkpoints now live in-repo under .makina/checkpoints/.
    let checkpoint = checkpoint_path(&target, &final_plan.key).unwrap();
    assert!(
        checkpoint.starts_with(target.join(".makina").join("checkpoints")),
        "checkpoint must be under repo/.makina/checkpoints, got {}",
        checkpoint.display()
    );
    assert!(
        load_checkpoint(&target, &final_plan.key)
            .await
            .unwrap()
            .is_some(),
        "checkpoint must be loadable from its in-repo path"
    );
}

#[tokio::test]
async fn scaffold_refuses_non_empty_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("occupied");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("keep.txt"), "x").unwrap();
    let err = makina::scaffold::scaffold_project(&target, "todo")
        .await
        .expect_err("must refuse a non-empty target");
    assert!(
        err.contains("non-empty"),
        "error explains the conflict: {err}"
    );
}

#[tokio::test]
#[ignore = "compiles the scaffolded crate; slow, run explicitly"]
async fn scaffolded_todo_project_passes_its_own_gates() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("todo");
    makina::scaffold::scaffold_project(&target, "todo")
        .await
        .unwrap();
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
