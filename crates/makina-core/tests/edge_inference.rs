use std::collections::{BTreeMap, HashMap};

use chrono::Utc;
use makina_core::dependency::{
    footprint_matches, footprints_overlap, infer_authored_footprint_edges, infer_footprint_edges,
};
use makina_core::plan::AuthoredTaskStatus;
use makina_core::task::{
    AuthoredRepoPattern, AuthoredSeedOutcome, AuthoredTaskMetadata, Task, TaskGraph, TaskId,
    TaskState,
};

fn task(id: &str, deps: &[&str]) -> Task {
    let now = Utc::now();
    Task {
        id: TaskId::new(id),
        title: id.into(),
        description: String::new(),
        done_when: String::new(),
        depends_on: deps.iter().copied().map(TaskId::new).collect(),
        section: None,
        state: TaskState::New,
        gate_iterations: 0,
        review_iterations: 0,
        created_at: now,
        updated_at: now,
        started_at: None,
        finished_at: None,
        failure_reason: None,
    }
}

#[test]
fn portable_matcher_handles_literals_one_segment_stars_and_terminal_recursion() {
    assert!(footprint_matches("src/lib.rs", "src/lib.rs"));
    assert!(footprint_matches("src/*.rs", "src/lib.rs"));
    assert!(!footprint_matches("src/*.rs", "src/nested/lib.rs"));
    assert!(footprint_matches("src/**", "src/nested/lib.rs"));
    assert!(footprints_overlap("src/*.rs", "src/l*.rs"));
    assert!(footprints_overlap("src", "src/lib.rs"));
    assert!(footprints_overlap("src/**", "src/nested/*.rs"));
    assert!(!footprints_overlap("src/*.rs", "tests/*.rs"));
}

#[test]
fn collisions_follow_one_authored_topological_extension_and_stay_distinct() {
    // Authored edge makes 0300 precede 0100 despite numeric order. Kahn-ready
    // ties choose 0200 first, yielding 0200,0300,0100 without a cycle.
    let mut graph = TaskGraph {
        slug: "p".into(),
        tasks: vec![
            task("0100-a", &["0300-c"]),
            task("0200-b", &[]),
            task("0300-c", &[]),
        ],
        authored: Default::default(),
    };
    let footprints = graph
        .tasks
        .iter()
        .map(|t| (t.id.clone(), vec!["src/**".into()]))
        .collect::<HashMap<_, _>>();
    let inferred = infer_footprint_edges(&mut graph, &footprints).unwrap();
    assert!(graph.tasks[0].depends_on.contains(&TaskId::new("0300-c")));
    assert!(graph.tasks[0].depends_on.contains(&TaskId::new("0200-b")));
    assert!(graph.tasks[2].depends_on.contains(&TaskId::new("0200-b")));
    assert_eq!(
        inferred.len(),
        2,
        "authored 0100<-0300 edge is not reclassified"
    );
}

#[test]
fn shuffled_input_has_the_same_deterministic_edges() {
    fn run(tasks: Vec<Task>) -> Vec<(String, String)> {
        let mut graph = TaskGraph {
            slug: "p".into(),
            tasks,
            authored: Default::default(),
        };
        let footprints = graph
            .tasks
            .iter()
            .map(|t| (t.id.clone(), vec!["same/*".into()]))
            .collect();
        let mut edges = infer_footprint_edges(&mut graph, &footprints)
            .unwrap()
            .into_iter()
            .map(|e| (e.prerequisite.0, e.dependent.0))
            .collect::<Vec<_>>();
        edges.sort();
        edges
    }
    assert_eq!(
        run(vec![
            task("0300-c", &[]),
            task("0100-a", &[]),
            task("0200-b", &[])
        ]),
        run(vec![
            task("0200-b", &[]),
            task("0300-c", &[]),
            task("0100-a", &[])
        ])
    );
}

#[test]
fn textual_fallback_only_augments_suspicious_footprints() {
    let mut first = task("0100-a", &[]);
    first.description = "touches `shared.rs`".into();
    let mut second = task("0200-b", &[]);
    second.description = "also `shared.rs`".into();
    let metadata = |touches| AuthoredTaskMetadata {
        source_path: "task.md".into(),
        workstream: "0001".into(),
        kind: "task".into(),
        gated: false,
        touches,
        status: AuthoredTaskStatus::Planned,
        merged_as: None,
        seed: AuthoredSeedOutcome::Seeded(TaskState::New),
        collision_dependencies: vec![],
        branch_base_oid: None,
    };
    let mut authored = BTreeMap::new();
    authored.insert(first.id.clone(), metadata(vec![]));
    authored.insert(
        second.id.clone(),
        metadata(vec![AuthoredRepoPattern::Path("other.rs".into())]),
    );
    let mut graph = TaskGraph {
        slug: "p".into(),
        tasks: vec![first, second],
        authored,
    };
    let edges = infer_authored_footprint_edges(&mut graph).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].left_pattern, "<textual-fallback>");
    assert_eq!(
        graph.authored[&TaskId::new("0200-b")].collision_dependencies,
        vec![TaskId::new("0100-a")]
    );
}
