use std::fs;
use std::path::Path;

use makina_core::plan::{
    FilesystemPlanFileSource, GitTreePlanFileSource, PlanCandidate, PlanKey, PlanReservations,
    RootRollupState, digest_list_v1, digest_records_v1, load_plan, load_plan_path,
    validate_root_rollup, validate_root_rollup_state,
};

const SOURCE: &str = "makina.source-digest.v1";
const EXECUTABLE: &str = "makina.executable-digest.v1";

#[test]
fn golden_digest_vectors_match_bootstrap_python() {
    let vectors: serde_json::Value = serde_json::from_str(include_str!(
        "../../../.claude/workflows/fixtures/plan-digest-v1.json"
    ))
    .unwrap();
    assert_eq!(digest_records_v1(SOURCE, &[]), vectors["empty"]["source"]);
    assert_eq!(
        digest_records_v1(EXECUTABLE, &[]),
        vectors["empty"]["executable"]
    );
    assert_eq!(
        digest_list_v1(SOURCE, "values", &[]),
        vectors["list"]["source"]
    );
    assert_eq!(
        digest_list_v1(EXECUTABLE, "values", &[]),
        vectors["list"]["executable"]
    );
    let unicode = vec![("title", "Makina — 日本語".as_bytes().to_vec())];
    assert_eq!(
        digest_records_v1(SOURCE, &unicode),
        vectors["unicode"]["source"]
    );
    assert_eq!(
        digest_records_v1(EXECUTABLE, &unicode),
        vectors["unicode"]["executable"]
    );
    let boundary = vec![("x", vec![b'a'; 256])];
    assert_eq!(
        digest_records_v1(SOURCE, &boundary),
        vectors["boundary"]["source"]
    );
    assert_eq!(
        digest_records_v1(EXECUTABLE, &boundary),
        vectors["boundary"]["executable"]
    );
}

#[test]
fn independent_python_encoder_matches_whole_plan_digest() {
    let repo = fixture_repo();
    let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    let PlanCandidate::Plan(plan) = load_plan(
        &source,
        PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        &PlanReservations::default(),
    )
    .unwrap() else {
        panic!()
    };
    let script = r#"
import hashlib, struct, sys
from pathlib import Path
root=Path(sys.argv[1]); rel='docs/plans/0049-Sample'; p=root/rel
def frame(tag,val):
 t=tag.encode(); return struct.pack('>I',len(t))+t+struct.pack('>Q',len(val))+val
def listed(items): return struct.pack('>Q',len(items))+b''.join(frame('item',x) for x in items)
def digest(domain,records):
 h=hashlib.sha256((domain+'\0').encode())
 for tag,val in records: h.update(frame(tag,val))
 return h.hexdigest()
def scalar(v): return v.strip().strip('"')
task_path=next((p/'tasks').glob('*.md')); text=task_path.read_text(); y,body=text[4:].split('\n---',1)
data={}; lines=y.splitlines(); i=0
while i<len(lines):
 line=lines[i]
 if ': ' in line:
  k,v=line.split(': ',1); data[k]=scalar(v)
 elif line.endswith(':'):
  k=line[:-1]; vals=[]; i+=1
  while i<len(lines) and lines[i].startswith('  - '): vals.append(scalar(lines[i][4:])); i+=1
  data[k]=vals; continue
 i+=1
for k in ('depends_on','touches'):
 if data.get(k)=='[]': data[k]=[]
 if isinstance(data.get(k),str) and data[k].startswith('['): data[k]=[x.strip() for x in data[k][1:-1].split(',') if x.strip()]
record=b''.join([frame('path',str(Path(rel)/'tasks'/task_path.name).encode()),frame('id',data['id'].encode()),frame('title',data['title'].encode()),frame('workstream',data['workstream'].encode()),frame('kind',data['kind'].encode()),frame('gated',data['gated'].encode()),frame('body',body.encode()),frame('dependencies',listed([x.encode() for x in data['depends_on']])),frame('touches',listed(sorted(x.encode() for x in data['touches'])))])
common=[('plan-key',rel.encode()),('title',b'Sample'),('task',record)]
status=(p/'STATUS.md').read_text(); anchors={}
for line in status.splitlines():
 if line.startswith('- **'):
  k,v=line[4:].split(':** ',1); anchors[k]=v
source=common+[('scope',(p/'SCOPE.md').read_bytes()),('architecture',(p/'ARCHITECTURE.md').read_bytes()),('status:goal',anchors['Goal'].encode()),('status:root-cause',anchors['Root cause'].encode()),('status:approach',anchors['Approach'].encode()),('status:outcome',anchors['Outcome'].encode())]
print(digest('makina.source-digest.v1',source)); print(digest('makina.executable-digest.v1',common))
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script, repo.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let hashes = String::from_utf8(output.stdout).unwrap();
    let mut hashes = hashes.lines();
    assert_eq!(hashes.next(), Some(plan.source_digest.as_str()));
    assert_eq!(hashes.next(), Some(plan.executable_digest.as_str()));
}

#[test]
fn loads_complete_bundle_and_validates_root_row() {
    let repo = fixture_repo();
    let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    let key = PlanKey::parse("docs/plans/0049-Sample").unwrap();
    let PlanCandidate::Plan(plan) = load_plan(&source, key, &PlanReservations::default()).unwrap()
    else {
        panic!("expected plan")
    };
    assert_eq!(plan.tasks.len(), 1);
    assert_eq!(plan.workstreams.iter().collect::<Vec<_>>(), vec!["0001"]);
    assert_eq!(plan.source_digest.as_str().len(), 64);
    assert_eq!(plan.executable_digest.as_str().len(), 64);
    let vectors: serde_json::Value = serde_json::from_str(include_str!(
        "../../../.claude/workflows/fixtures/plan-digest-v1.json"
    ))
    .unwrap();
    assert_eq!(plan.source_digest.as_str(), vectors["whole_plan"]["source"]);
    assert_eq!(
        plan.executable_digest.as_str(),
        vectors["whole_plan"]["executable"]
    );
    let row = "| 0049 | Sample | 📋 Planned | 0/1 | one sample plan validates. | [status](0049-Sample/STATUS.md) |";
    assert!(validate_root_rollup(&plan, row).is_empty());
}

#[test]
fn historical_without_tasks_is_not_a_candidate_and_mixed_is_invalid() {
    let repo = fixture_repo();
    fs::rename(
        repo.path().join("docs/plans/0049-Sample/tasks"),
        repo.path().join("saved-tasks"),
    )
    .unwrap();
    let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    let key = PlanKey::parse("docs/plans/0049-Sample").unwrap();
    assert!(matches!(
        load_plan(&source, key, &PlanReservations::default()).unwrap(),
        PlanCandidate::NotCandidate
    ));

    fs::rename(
        repo.path().join("saved-tasks"),
        repo.path().join("docs/plans/0049-Sample/tasks"),
    )
    .unwrap();
    fs::write(
        repo.path().join("docs/plans/0049-Sample/TASKS.md"),
        "legacy",
    )
    .unwrap();
    let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    let report = load_plan(
        &source,
        PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        &PlanReservations::default(),
    )
    .unwrap_err();
    assert!(
        report
            .diagnostics
            .iter()
            .any(|item| item.code == "mixed-plan-format")
    );
}

#[test]
fn accumulates_unknown_dependencies_cycles_status_drift_and_reservations() {
    let repo = fixture_repo();
    let task_path = repo
        .path()
        .join("docs/plans/0049-Sample/tasks/0101-first-task.md");
    let task = fs::read_to_string(&task_path)
        .unwrap()
        .replace("depends_on: []", "depends_on: [first-task, absent-task]");
    fs::write(task_path, task).unwrap();
    fs::write(
        repo.path().join("docs/plans/0049-Sample/STATUS.md"),
        fs::read_to_string(repo.path().join("docs/plans/0049-Sample/STATUS.md"))
            .unwrap()
            .replace("0/1 tasks", "1/1 tasks"),
    )
    .unwrap();
    let mut reservations = PlanReservations::default();
    reservations
        .verified_registrations
        .insert("0049".into(), vec!["phase-r".into()]);
    let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    let report = load_plan(
        &source,
        PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        &reservations,
    )
    .unwrap_err();
    for code in [
        "self-dependency",
        "unknown-dependency",
        "reserved-plan-number",
        "status-count-drift",
    ] {
        assert!(
            report.diagnostics.iter().any(|item| item.code == code),
            "missing {code}"
        );
    }
    let mut sorted = report.diagnostics.clone();
    sorted.sort();
    assert_eq!(report.diagnostics, sorted);
}

#[test]
fn loads_plan_0048_as_real_world_input() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = FilesystemPlanFileSource::new(&repo_root, None).unwrap();
    let key =
        PlanKey::parse("docs/plans/0048-Per-Task-Plan-Documents-And-Transactional-Status").unwrap();
    let PlanCandidate::Plan(plan) = load_plan(&source, key, &PlanReservations::default()).unwrap()
    else {
        panic!("expected plan")
    };
    assert_eq!(plan.tasks.len(), 15);
}

#[test]
fn completed_plan_0048_resolves_tracked_exceptions_from_status_validation_base() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let key =
        PlanKey::parse("docs/plans/0048-Per-Task-Plan-Documents-And-Transactional-Status").unwrap();

    let git_source = GitTreePlanFileSource::new(&repo_root, "HEAD").unwrap();
    let PlanCandidate::Plan(from_head) =
        load_plan(&git_source, key.clone(), &PlanReservations::default()).unwrap()
    else {
        panic!("expected completed Plan 0048 from HEAD")
    };
    assert_eq!(from_head.status.done, 15);
    assert_eq!(
        from_head
            .status
            .validation_base_oid
            .as_ref()
            .unwrap()
            .as_str(),
        "5d59e89a34b66a42c58cd9a1f58d4ee036e90ef8"
    );

    let filesystem_source = FilesystemPlanFileSource::new(
        &repo_root,
        Some("5d59e89a34b66a42c58cd9a1f58d4ee036e90ef8".into()),
    )
    .unwrap();
    assert!(load_plan(&filesystem_source, key, &PlanReservations::default()).is_ok());
}

#[test]
fn registered_source_base_must_agree_with_status_validation_base() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo_root)
        .output()
        .unwrap();
    let source = FilesystemPlanFileSource::new(
        &repo_root,
        Some(String::from_utf8(head.stdout).unwrap().trim().into()),
    )
    .unwrap();
    let report = load_plan(
        &source,
        PlanKey::parse("docs/plans/0048-Per-Task-Plan-Documents-And-Transactional-Status").unwrap(),
        &PlanReservations::default(),
    )
    .unwrap_err();
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "validation-base-mismatch")
    );
}

#[test]
fn digests_ignore_bookkeeping_but_bind_executable_fields_and_body() {
    let repo = fixture_repo();
    let key = || PlanKey::parse("docs/plans/0049-Sample").unwrap();
    let load = || {
        let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
        let PlanCandidate::Plan(plan) =
            load_plan(&source, key(), &PlanReservations::default()).unwrap()
        else {
            panic!()
        };
        plan
    };
    let original = load();
    let task_path = repo
        .path()
        .join("docs/plans/0049-Sample/tasks/0101-first-task.md");
    fs::write(
        &task_path,
        fs::read_to_string(&task_path)
            .unwrap()
            .replace("status: planned", "status: blocked"),
    )
    .unwrap();
    let status_path = repo.path().join("docs/plans/0049-Sample/STATUS.md");
    let status = fs::read_to_string(&status_path)
        .unwrap()
        .replace("📋 Planned", "⛔ Blocked")
        .replace("0 blocked", "1 blocked")
        .replace("`planned`; run —; base `develop` @ `aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa`; validation base —; mode —; final integration —.", "`integration-blocked`; run `01ARZ3NDEKTSV4RRFFQ69G5FAV`; base `develop` @ `aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa`; validation base `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb`; mode `Squash`; final integration —.")
        .replace(
            "**Exceptions:** —.",
            "**Exceptions:** first-task — test reason.",
        );
    fs::write(&status_path, status).unwrap();
    let bookkeeping = load();
    assert_eq!(original.source_digest, bookkeeping.source_digest);
    assert_eq!(original.executable_digest, bookkeeping.executable_digest);

    fs::write(
        &task_path,
        fs::read_to_string(&task_path)
            .unwrap()
            .replace("gated: false", "gated: true"),
    )
    .unwrap();
    let gated = load();
    assert_ne!(bookkeeping.source_digest, gated.source_digest);
    assert_ne!(bookkeeping.executable_digest, gated.executable_digest);
}

#[test]
fn plan_key_rejects_unsafe_or_noncanonical_ref_names() {
    for basename in [
        "0049-bad.lock",
        "0049-bad.",
        "0049-bad@{x",
        "0049-bad name",
        "0049-bad~name",
        "049-short",
    ] {
        assert!(
            PlanKey::parse(format!("docs/plans/{basename}")).is_err(),
            "accepted {basename}"
        );
    }
    assert!(PlanKey::parse(format!("docs/plans/0049-{}", "a".repeat(196))).is_err());
}

#[test]
fn candidate_classification_precedes_key_validation_and_rejects_bad_tasks_entry() {
    let repo = fixture_repo();
    fs::rename(
        repo.path().join("docs/plans/0049-Sample"),
        repo.path().join("docs/plans/bad name"),
    )
    .unwrap();
    let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    assert!(
        load_plan_path(&source, "docs/plans/bad name", &PlanReservations::default())
            .unwrap_err()
            .diagnostics
            .iter()
            .any(|item| item.code == "invalid-plan-key")
    );
    fs::remove_dir_all(repo.path().join("docs/plans/bad name/tasks")).unwrap();
    fs::write(
        repo.path().join("docs/plans/bad name/tasks"),
        "not a directory",
    )
    .unwrap();
    assert!(
        load_plan_path(&source, "docs/plans/bad name", &PlanReservations::default())
            .unwrap_err()
            .diagnostics
            .iter()
            .any(|item| item.code == "malformed-tasks-entry")
    );
}

#[test]
fn own_registration_is_reusable_but_another_identity_collides() {
    let repo = fixture_repo();
    let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    let key = PlanKey::parse("docs/plans/0049-Sample").unwrap();
    let mut reservations = PlanReservations::default();
    reservations
        .verified_registrations
        .insert("0049".into(), vec![key.ref_name()]);
    assert!(
        load_plan(&source, key.clone(), &reservations)
            .unwrap_err()
            .diagnostics
            .iter()
            .any(|item| item.code == "missing-registration-validation-base")
    );
    let status_path = repo.path().join("docs/plans/0049-Sample/STATUS.md");
    fs::write(
        &status_path,
        fs::read_to_string(&status_path).unwrap().replace(
            "validation base —",
            "validation base `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb`",
        ),
    )
    .unwrap();
    assert!(load_plan(&source, key.clone(), &reservations).is_ok());
    reservations
        .verified_registrations
        .get_mut("0049")
        .unwrap()
        .push("refs/heads/plan/0049-Other".into());
    assert!(
        load_plan(&source, key, &reservations)
            .unwrap_err()
            .diagnostics
            .iter()
            .any(|item| item.code == "reserved-plan-number")
    );
}

#[test]
fn duplicate_workstreams_and_status_anchors_are_diagnostics() {
    let repo = fixture_repo();
    let scope = repo.path().join("docs/plans/0049-Sample/SCOPE.md");
    fs::write(
        &scope,
        format!(
            "{}\n- **0001 — Core.**\n",
            fs::read_to_string(&scope).unwrap()
        ),
    )
    .unwrap();
    let status = repo.path().join("docs/plans/0049-Sample/STATUS.md");
    fs::write(
        &status,
        fs::read_to_string(&status)
            .unwrap()
            .replace("- **Goal:**", "- **Goal:** duplicate.\n- **Goal:**"),
    )
    .unwrap();
    let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    let report = load_plan(
        &source,
        PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        &PlanReservations::default(),
    )
    .unwrap_err();
    assert!(
        report
            .diagnostics
            .iter()
            .any(|item| item.code == "duplicate-scope-workstream")
    );
    assert!(
        report
            .diagnostics
            .iter()
            .any(|item| item.code == "invalid-plan-status")
    );
}

#[test]
fn root_rollup_is_unique_exact_and_allows_unregistered_absence() {
    let repo = fixture_repo();
    let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    let PlanCandidate::Plan(plan) = load_plan(
        &source,
        PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        &PlanReservations::default(),
    )
    .unwrap() else {
        panic!()
    };
    assert!(validate_root_rollup_state(&plan, "", RootRollupState::Unregistered).is_empty());
    let row = "| 0049 | Sample | 📋 Planned | 0/1 | one sample plan validates. | [status](0049-Sample/STATUS.md) |";
    assert!(!validate_root_rollup(&plan, &format!("{row}\n{row}")).is_empty());
    assert!(
        !validate_root_rollup(
            &plan,
            &row.replace("0049-Sample/STATUS.md", "wrong/STATUS.md")
        )
        .is_empty()
    );
}

#[test]
fn all_six_integration_states_accept_only_coherent_evidence() {
    let oid_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let oid_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let oid_c = "cccccccccccccccccccccccccccccccccccccccc";
    let run = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    for (state, display, task_status, merged, progress, validation, mode, final_oid, exceptions) in [
        (
            "planned",
            "📋 Planned",
            "planned",
            "",
            "0/1 tasks done; 0 blocked; 0 dropped",
            "—",
            "—",
            "—",
            "—.",
        ),
        (
            "assembling",
            "🚧 In Progress",
            "in-progress",
            "",
            "0/1 tasks done; 0 blocked; 0 dropped",
            oid_b,
            "—",
            "—",
            "—.",
        ),
        (
            "awaiting-integration",
            "🚧 In Progress",
            "planned",
            "",
            "0/1 tasks done; 0 blocked; 0 dropped",
            oid_b,
            "—",
            "—",
            "—.",
        ),
        (
            "finalization-pending",
            "🚧 In Progress",
            "planned",
            "",
            "0/1 tasks done; 0 blocked; 0 dropped",
            oid_b,
            "Squash",
            "—",
            "—.",
        ),
        (
            "integration-blocked",
            "⛔ Blocked",
            "blocked",
            "",
            "0/1 tasks done; 1 blocked; 0 dropped",
            oid_b,
            "Squash",
            "—",
            "first-task — reason.",
        ),
        (
            "complete",
            "✅ Complete",
            "done",
            oid_c,
            "1/1 tasks done; 0 blocked; 0 dropped",
            oid_b,
            "Squash",
            oid_c,
            "—.",
        ),
    ] {
        let repo = fixture_repo();
        let task_path = repo
            .path()
            .join("docs/plans/0049-Sample/tasks/0101-first-task.md");
        let task = fs::read_to_string(&task_path)
            .unwrap()
            .replace("status: planned", &format!("status: {task_status}"))
            .replace("merged_as: \"\"", &format!("merged_as: \"{merged}\""));
        fs::write(task_path, task).unwrap();
        let status_path = repo.path().join("docs/plans/0049-Sample/STATUS.md");
        let mut status = fs::read_to_string(&status_path)
            .unwrap()
            .replace("📋 Planned", display)
            .replace("0/1 tasks done; 0 blocked; 0 dropped", progress)
            .replace(
                "**Exceptions:** —.",
                &format!("**Exceptions:** {exceptions}"),
            );
        let run_field = if state == "planned" { "—" } else { run };
        let validation = if validation == "—" {
            "—".into()
        } else {
            format!("`{validation}`")
        };
        let mode = if mode == "—" {
            "—".into()
        } else {
            format!("`{mode}`")
        };
        let final_oid = if final_oid == "—" {
            "—".into()
        } else {
            format!("`{final_oid}`")
        };
        let integration = format!(
            "`{state}`; run {run_field}; base `develop` @ `{oid_a}`; validation base {validation}; mode {mode}; final integration {final_oid}."
        );
        status = status
            .lines()
            .map(|line| {
                if line.starts_with("- **Integration:**") {
                    format!("- **Integration:** {integration}")
                } else {
                    line.into()
                }
            })
            .collect::<Vec<String>>()
            .join("\n");
        fs::write(status_path, status).unwrap();
        let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
        assert!(
            load_plan(
                &source,
                PlanKey::parse("docs/plans/0049-Sample").unwrap(),
                &PlanReservations::default()
            )
            .is_ok(),
            "state {state} rejected"
        );
    }
}

/// The renderer must refuse to emit a `STATUS.md` the loader would reject.
///
/// Coordinator writes are committed to the plan ref and only re-read by the
/// *next* durable claim, so a silently unreadable render surfaces one task later
/// as an opaque failure in a different subsystem. `awaiting-integration` plus a
/// final merge mode is the exact combination that stranded a real run.
#[test]
fn render_refuses_a_status_the_loader_would_reject() {
    let repo = fixture_repo();
    let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    let PlanCandidate::Plan(plan) = load_plan(
        &source,
        PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        &PlanReservations::default(),
    )
    .unwrap() else {
        panic!("the fixture must be a plan")
    };
    let oid = makina_core::plan::GitObjectId::parse(
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        makina_core::plan::GitObjectFormat::Sha1,
    )
    .unwrap();
    let error = makina_core::plan_status::render_plan_status(
        &plan,
        &makina_core::plan_status::StatusTransition {
            integration_state: makina_core::plan::PlanIntegrationState::AwaitingIntegration,
            run: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".into()),
            validation_base: Some(oid),
            mode: Some("Squash".into()),
            final_oid: None,
            display_status: "🚧 In Progress".into(),
            last_updated: "_Last updated: 2026-07-19, against `develop` @ `aaaaaaa`._".into(),
        },
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            makina_core::plan_status::StatusEditError::UnreadableRender(_)
        ),
        "the renderer must reject its own unreadable output, got {error:?}",
    );
}

/// Every canonical spelling the coordinator writes must survive a reload.
///
/// The final merge mode has two vocabularies — a lowercase-kebab commit trailer
/// and a CamelCase STATUS field — and the display badge has one per integration
/// state. Writing the wrong vocabulary into `STATUS.md` produced plans that
/// registration accepted and the loader then refused.
#[test]
fn canonical_mode_spellings_and_badges_reload() {
    use makina_core::config::FinalMerge;
    use makina_core::plan::PlanIntegrationState;
    use makina_core::plan_status::{display_badge, final_mode_names};

    let repo = fixture_repo();
    let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    let PlanCandidate::Plan(plan) = load_plan(
        &source,
        PlanKey::parse("docs/plans/0049-Sample").unwrap(),
        &PlanReservations::default(),
    )
    .unwrap() else {
        panic!("the fixture must be a plan")
    };
    let oid = makina_core::plan::GitObjectId::parse(
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        makina_core::plan::GitObjectFormat::Sha1,
    )
    .unwrap();
    // The fixture's single task is `planned`, so the states reachable with that
    // task shape are the pre-terminal ones; each must render and reload.
    for state in [
        PlanIntegrationState::Planned,
        PlanIntegrationState::Assembling,
        PlanIntegrationState::AwaitingIntegration,
        PlanIntegrationState::FinalizationPending,
    ] {
        for mode in [
            FinalMerge::Squash,
            FinalMerge::MergeCommit,
            FinalMerge::Stage,
            FinalMerge::Manual,
        ] {
            let carries_mode = state == PlanIntegrationState::FinalizationPending;
            let rendered = makina_core::plan_status::render_plan_status(
                &plan,
                &makina_core::plan_status::StatusTransition {
                    integration_state: state,
                    run: (state != PlanIntegrationState::Planned)
                        .then(|| "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned()),
                    validation_base: (state != PlanIntegrationState::Planned).then(|| oid.clone()),
                    mode: carries_mode.then(|| final_mode_names(mode).1.to_owned()),
                    final_oid: None,
                    display_status: display_badge(state).into(),
                    last_updated: "_Last updated: 2026-07-19, against `develop` @ `aaaaaaa`._"
                        .into(),
                },
            )
            .unwrap_or_else(|error| panic!("{state:?} with {mode:?} failed to render: {error}"));
            makina_core::plan::verify_rendered_status(&plan, &rendered).unwrap_or_else(|errors| {
                panic!("{state:?} with {mode:?} will not reload: {errors:?}")
            });
        }
    }
}

fn fixture_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "-q"]);
    copy_tree(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/plan-bundles/valid/0049-Sample")
            .as_path(),
        &repo.path().join("docs/plans/0049-Sample"),
    );
    repo
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

fn run_git(repo: &Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap()
            .success()
    );
}
