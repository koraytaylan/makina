use std::{fs, path::Path, process::Command};

use makina_core::plan::{
    GeneratedInitialStatus, GeneratedPlanBundle, GeneratedTaskDocument, GeneratedWorkstream,
    GitObjectFormat, GitObjectId, PlanCandidate, PlanFileSource, PlanKey, PlanReservations,
    RepoPattern, TaskFrontmatter, TaskId, TaskKind, TaskSequence, WorkstreamId, load_plan,
    parse_task_document,
};

fn bundle() -> GeneratedPlanBundle {
    GeneratedPlanBundle {
        key: PlanKey::parse("docs/plans/0050-Generated-Sample").unwrap(),
        title: "Generated Sample".into(),
        scope: "## In scope\n\n- **0001 \u{2014} Core.** Generate a complete bundle.".into(),
        architecture: "## 0001 \u{2014} Core\n\nUse the canonical renderer.".into(),
        initial_status: GeneratedInitialStatus {
            goal: "generate one loader-valid plan.".into(),
            root_cause: "free-form generation cannot guarantee structure.".into(),
            approach: "render a typed authoring representation.".into(),
            outcome: "the generated bundle is ready for registration.".into(),
            base_name: "develop".into(),
            base_oid: GitObjectId::parse("a".repeat(40), GitObjectFormat::Sha1).unwrap(),
            last_updated: "2026-07-20".into(),
        },
        workstreams: vec![GeneratedWorkstream {
            id: WorkstreamId::parse("0001").unwrap(),
            title: "Core".into(),
        }],
        tasks: vec![GeneratedTaskDocument {
            sequence: TaskSequence::parse("01").unwrap(),
            frontmatter: TaskFrontmatter {
                id: TaskId::parse("render-bundle").unwrap(),
                title: "Render Bundle".into(),
                workstream: WorkstreamId::parse("0001").unwrap(),
                kind: TaskKind::Task,
                depends_on: vec![],
                gated: false,
                touches: vec![RepoPattern::Glob("src/**".into())],
                status: makina_core::plan::AuthoredTaskStatus::Blocked,
                merged_as: None,
            },
            body: "# Render Bundle\n\nRender every document.\n\n**Steps:**\n\n1. Render canonical bytes.\n\n- **Done when:** the parsed bundle is reproducible."
                .into(),
        }],
    }
}

#[test]
fn canonical_render_is_sorted_and_owns_initial_bookkeeping() {
    let files = bundle().render_files().unwrap();
    assert_eq!(
        files.keys().collect::<Vec<_>>(),
        vec![
            Path::new("ARCHITECTURE.md"),
            Path::new("SCOPE.md"),
            Path::new("STATUS.md"),
            Path::new("tasks/0101-render-bundle.md"),
        ]
    );
    let task = String::from_utf8(files[Path::new("tasks/0101-render-bundle.md")].clone()).unwrap();
    assert_eq!(
        task,
        include_str!("fixtures/generated-plans/canonical-task.md")
    );
    assert!(task.contains("status: planned\nmerged_as: \"\""));
    let status = String::from_utf8(files[Path::new("STATUS.md")].clone()).unwrap();
    assert!(status.contains("**Progress:** 0/1 tasks done; 0 blocked; 0 dropped."));
}

#[test]
fn rendered_bundle_loads_and_task_parse_render_is_byte_reproducible() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    let bundle = bundle();
    for (relative, bytes) in bundle.render_files().unwrap() {
        let path = repo.path().join(&bundle.key.relative_dir).join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    let source = makina_core::plan::FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    let PlanCandidate::Plan(plan) =
        load_plan(&source, bundle.key.clone(), &PlanReservations::default()).unwrap()
    else {
        panic!("expected a complete generated plan");
    };
    assert_eq!(plan.tasks.len(), 1);
    let task_path = &plan.tasks[0].source_path;
    let original = source.read_file(task_path).unwrap();
    let parsed = parse_task_document(&source, task_path).unwrap();
    assert_eq!(parsed.render().as_bytes(), original);
    assert_eq!(
        bundle.render_files().unwrap(),
        bundle.render_files().unwrap()
    );
}

#[test]
fn authoring_rejects_tasks_outside_declared_workstreams() {
    let mut bundle = bundle();
    bundle.tasks[0].frontmatter.workstream = WorkstreamId::parse("0002").unwrap();
    assert!(bundle.render_files().is_err());
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
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
