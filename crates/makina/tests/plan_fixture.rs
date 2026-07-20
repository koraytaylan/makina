use std::path::{Path, PathBuf};

pub struct Task {
    pub id: String,
    pub title: String,
    pub gated: bool,
    pub depends_on: Vec<String>,
    pub body: String,
}

struct Source(Vec<u8>);

impl makina_core::plan::PlanFileSource for Source {
    fn read_file(&self, _: &Path) -> Result<Vec<u8>, makina_core::plan::PlanDocumentError> {
        Ok(self.0.clone())
    }
    fn object_format(&self) -> makina_core::plan::GitObjectFormat {
        makina_core::plan::GitObjectFormat::Sha1
    }
    fn validation_base_oid(&self) -> Option<&str> {
        None
    }
    fn is_tracked_ordinary_file(
        &self,
        _: &Path,
    ) -> Result<bool, makina_core::plan::PlanDocumentError> {
        Ok(false)
    }
}

pub fn entry(
    mut dir: PathBuf,
    slug: String,
    tasks: Vec<Task>,
) -> makina_core::orchestrator::PlanEntry {
    let relative = PathBuf::from("docs").join("plans").join(
        dir.file_name()
            .filter(|name| {
                makina_core::plan::PlanKey::parse(PathBuf::from("docs/plans").join(name)).is_ok()
            })
            .unwrap_or_else(|| std::ffi::OsStr::new("0001-test")),
    );
    let key = makina_core::plan::PlanKey::parse(relative).unwrap();
    if makina_core::plan::PlanKey::parse(&dir).is_err() {
        dir = key.relative_dir.clone();
    }
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = makina_core::plan::GitTreePlanFileSource::new(repository, "HEAD").unwrap();
    let fixture = makina_core::plan::PlanKey::parse(
        "docs/plans/0048-Per-Task-Plan-Documents-And-Transactional-Status",
    )
    .unwrap();
    let makina_core::plan::PlanCandidate::Plan(document) = makina_core::plan::load_plan(
        &source,
        fixture,
        &makina_core::plan::PlanReservations::default(),
    )
    .unwrap() else {
        panic!("fixture plan missing")
    };
    let mut document = *document;
    document.key = key.clone();
    document.title = slug.clone();
    document.scope.body = "This plan implements tab-based focus navigation.".into();
    document.architecture.body = "The focus model supports keyboard navigation.".into();
    document.tasks = tasks
        .into_iter()
        .enumerate()
        .map(|(index, task)| {
            let deps = if task.depends_on.is_empty() {
                "[]".into()
            } else {
                format!("\n{}", task.depends_on.iter().map(|id| format!("  - {id}")).collect::<Vec<_>>().join("\n"))
            };
            let text = format!(
                "---\nid: {}\ntitle: {}\nworkstream: \"0001\"\nkind: task\ndepends_on: {}\ngated: {}\ntouches:\n  - src/**\nstatus: planned\nmerged_as: \"\"\n---\n# {}\n\n## Context\n\n{}\n\n**Steps:**\n\n1. Test.\n\n- **Done when:** Tested.\n",
                task.id, task.title, deps, task.gated, task.title, task.body
            );
            makina_core::plan::parse_task_document(
                &Source(text.into_bytes()),
                key.relative_dir.join("tasks").join(format!("01{:02}-{}.md", index + 1, task.id)),
            )
            .unwrap()
        })
        .collect();
    makina_core::orchestrator::PlanEntry {
        dir,
        key,
        slug,
        state: makina_core::orchestrator::PlanDiscoveryState::Ready,
        document: Some(document),
        diagnostics: makina_core::plan::PlanValidationReport::default(),
    }
}
