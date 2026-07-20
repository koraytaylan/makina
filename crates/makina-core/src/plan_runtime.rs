//! Deterministic projection from validated plan documents into scheduler state.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::plan::PlanDocument;
use crate::task::{
    AuthoredRepoPattern, AuthoredSeedEvidence, AuthoredTaskMetadata, Task, TaskGraph, TaskId,
    TaskState, seed_authored_state,
};

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedTaskGraph {
    pub plan_dir: PathBuf,
    pub executable_digest: String,
    pub graph: TaskGraph,
}

impl ProjectedTaskGraph {
    pub fn from_document(plan: &PlanDocument, projected_at: DateTime<Utc>) -> Self {
        let slug = format!("{}-{}", plan.key.number, plan.key.slug);
        let mut authored = BTreeMap::new();
        let tasks = plan
            .tasks
            .iter()
            .map(|source| {
                let fm = &source.frontmatter;
                let seed =
                    seed_authored_state(fm.status, fm.gated, AuthoredSeedEvidence::default());
                authored.insert(
                    TaskId::new(fm.id.as_str()),
                    AuthoredTaskMetadata {
                        source_path: source.source_path.clone(),
                        workstream: fm.workstream.as_str().to_owned(),
                        kind: fm.kind.to_string(),
                        gated: fm.gated,
                        touches: fm.touches.iter().map(AuthoredRepoPattern::from).collect(),
                        status: fm.status,
                        merged_as: fm.merged_as.as_ref().map(|oid| oid.as_str().to_owned()),
                        branch_base_oid: None,
                        collision_dependencies: Vec::new(),
                        seed,
                    },
                );
                Task {
                    id: TaskId::new(fm.id.as_str()),
                    title: fm.title.clone(),
                    description: source.body.clone(),
                    done_when: done_when(&source.body).unwrap_or_default().to_owned(),
                    depends_on: fm
                        .depends_on
                        .iter()
                        .map(|id| TaskId::new(id.as_str()))
                        .collect(),
                    section: Some(fm.workstream.as_str().to_owned()),
                    // Authored completion is evidence to reconcile, never checkpoint truth.
                    state: seed.runtime_state().unwrap_or(TaskState::New),
                    gate_iterations: 0,
                    review_iterations: 0,
                    created_at: projected_at,
                    updated_at: projected_at,
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                }
            })
            .collect();
        let mut graph = TaskGraph {
            slug,
            tasks,
            authored,
        };
        crate::dependency::infer_authored_footprint_edges(&mut graph)
            .expect("validated authored task order cannot produce a collision cycle");
        Self {
            plan_dir: plan.key.relative_dir.clone(),
            executable_digest: plan.executable_digest.as_str().to_owned(),
            graph,
        }
    }

    pub fn is_dispatchable(&self, id: &TaskId) -> bool {
        self.graph.is_authored_dispatchable(id)
    }
}

fn done_when(body: &str) -> Option<&str> {
    body.lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("- **Done when:**").map(str::trim))
        .filter(|value| !value.is_empty())
}
