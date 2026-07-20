use std::path::{Path, PathBuf};

use makina_core::plan::{
    AuthoredTaskStatus, FilesystemPlanFileSource, GitObjectFormat, GitObjectId, PlanDocumentError,
    PlanFileSource, RepoChange, RepoPattern, TaskKind, parse_task_document,
};

struct MemorySource {
    bytes: Vec<u8>,
    format: GitObjectFormat,
    base: Option<String>,
    tracked: bool,
}

impl PlanFileSource for MemorySource {
    fn read_file(&self, _: &Path) -> Result<Vec<u8>, PlanDocumentError> {
        Ok(self.bytes.clone())
    }
    fn object_format(&self) -> GitObjectFormat {
        self.format
    }
    fn validation_base_oid(&self) -> Option<&str> {
        self.base.as_deref()
    }
    fn is_tracked_ordinary_file(&self, _: &Path) -> Result<bool, PlanDocumentError> {
        Ok(self.tracked)
    }
}

fn fixture() -> Vec<u8> {
    include_bytes!("fixtures/plan-documents/valid/0102-define-schema.md").to_vec()
}

fn source(bytes: Vec<u8>) -> MemorySource {
    MemorySource {
        bytes,
        format: GitObjectFormat::Sha1,
        base: None,
        tracked: false,
    }
}

fn replace(bytes: Vec<u8>, from: &str, to: &str) -> Vec<u8> {
    String::from_utf8(bytes)
        .unwrap()
        .replace(from, to)
        .into_bytes()
}

#[test]
fn valid_document_round_trips_and_preserves_body() {
    let initial_source = source(fixture());
    let first = parse_task_document(&initial_source, "tasks/0102-define-schema.md").unwrap();
    let rendered = first.render();
    let second_source = source(rendered.into_bytes());
    let second = parse_task_document(&second_source, "tasks/0102-define-schema.md").unwrap();
    assert_eq!(first, second);
    assert_eq!(first.body, second.body);
}

#[test]
fn canonical_scalars_round_trip_unicode_and_yaml_punctuation() {
    let initial_source = source(fixture());
    let mut document = parse_task_document(&initial_source, "tasks/0102-define-schema.md").unwrap();
    let title = "Résumé & \"schema\": β #1";
    document.frontmatter.title = title.to_owned();
    document.body = document
        .body
        .replacen("# Define schema", &format!("# {title}"), 1);
    let rendered = document.render();
    let reparsed = parse_task_document(
        &source(rendered.into_bytes()),
        "tasks/0102-define-schema.md",
    )
    .unwrap();
    assert_eq!(reparsed.frontmatter.title, title);
    assert_eq!(reparsed.body, document.body);
}

#[test]
fn all_kinds_and_authored_statuses_are_accepted_when_coherent() {
    for kind in ["task", "spike", "chore"] {
        let bytes = replace(fixture(), "kind: task", &format!("kind: {kind}"));
        parse_task_document(&source(bytes), "tasks/0102-define-schema.md").unwrap();
    }
    for status in ["planned", "in-progress", "blocked", "dropped"] {
        let bytes = replace(fixture(), "status: planned", &format!("status: {status}"));
        parse_task_document(&source(bytes), "tasks/0102-define-schema.md").unwrap();
    }
    let bytes = replace(
        replace(fixture(), "status: planned", "status: done"),
        "merged_as: \"\"",
        &format!("merged_as: {}", "a".repeat(40)),
    );
    parse_task_document(&source(bytes), "tasks/0102-define-schema.md").unwrap();
}

#[test]
fn merge_evidence_matches_repository_object_format() {
    GitObjectId::parse("a".repeat(40), GitObjectFormat::Sha1).unwrap();
    GitObjectId::parse("b".repeat(64), GitObjectFormat::Sha256).unwrap();
    assert!(GitObjectId::parse("a".repeat(40), GitObjectFormat::Sha256).is_err());
    assert!(GitObjectId::parse("b".repeat(64), GitObjectFormat::Sha1).is_err());

    let incoherent = replace(fixture(), "status: planned", "status: done");
    assert!(parse_task_document(&source(incoherent), "tasks/0102-define-schema.md").is_err());
}

#[test]
fn rejects_unknown_duplicate_and_forbidden_yaml() {
    let unknown = replace(
        fixture(),
        "id: define-schema",
        "extra: no\nid: define-schema",
    );
    assert!(parse_task_document(&source(unknown), "tasks/0102-define-schema.md").is_err());

    let duplicate = replace(
        fixture(),
        "title: Define schema",
        "title: One\ntitle: Define schema",
    );
    assert!(parse_task_document(&source(duplicate), "tasks/0102-define-schema.md").is_err());

    for insertion in [
        "anchor: &bad value\n",
        "alias: *bad\n",
        "tagged: !thing value\n",
        "<<: *base\n",
        "%YAML 1.2\n",
    ] {
        let bytes = replace(
            fixture(),
            "id: define-schema",
            &format!("{insertion}id: define-schema"),
        );
        assert!(
            parse_task_document(&source(bytes), "tasks/0102-define-schema.md").is_err(),
            "accepted {insertion:?}"
        );
    }
}

#[test]
fn validates_filename_body_and_repository_paths() {
    assert!(parse_task_document(&source(fixture()), "tasks/0202-define-schema.md").is_err());
    assert!(parse_task_document(&source(fixture()), "tasks/0102-wrong.md").is_err());

    let body = replace(fixture(), "# Define schema", "# Wrong");
    assert!(parse_task_document(&source(body), "tasks/0102-define-schema.md").is_err());

    for unsafe_path in [
        "../escape",
        "/absolute",
        ".git/config",
        ".makina/runtime.json",
        "src/**/bad",
    ] {
        let bytes = replace(fixture(), "crates/makina-core/src/plan.rs", unsafe_path);
        assert!(
            parse_task_document(&source(bytes), "tasks/0102-define-schema.md").is_err(),
            "accepted {unsafe_path}"
        );
    }
}

#[test]
fn tracked_makina_exceptions_remain_typed() {
    let config = replace(
        fixture(),
        "crates/makina-core/src/plan.rs",
        ".makina/config.toml",
    );
    let authored =
        parse_task_document(&source(config.clone()), "tasks/0102-define-schema.md").unwrap();
    assert_eq!(
        authored.frontmatter.touches,
        vec![RepoPattern::TrackedMakinaConfigCandidate]
    );

    let registered_source = MemorySource {
        bytes: config,
        format: GitObjectFormat::Sha1,
        base: Some("a".repeat(40)),
        tracked: true,
    };
    let registered =
        parse_task_document(&registered_source, "tasks/0102-define-schema.md").unwrap();
    assert!(matches!(
        registered.frontmatter.touches[0],
        RepoPattern::TrackedMakinaConfig { .. }
    ));

    let deletion = replace(
        replace(fixture(), "kind: task", "kind: chore"),
        "crates/makina-core/src/plan.rs",
        ".makina/obsolete.json",
    );
    let authored = parse_task_document(&source(deletion), "tasks/0102-define-schema.md").unwrap();
    assert!(matches!(
        authored.frontmatter.touches[0],
        RepoPattern::TrackedMakinaDeletionCandidate(_)
    ));
}

#[test]
fn tracked_makina_policy_is_exact_and_candidates_cannot_execute() {
    let candidate = RepoPattern::TrackedMakinaConfigCandidate;
    assert!(!candidate.is_executable());
    assert!(!candidate.permits_change(RepoChange::ModifiedOrdinaryFile));
    let candidate = RepoPattern::TrackedMakinaDeletionCandidate(".makina/old".into());
    assert!(!candidate.is_executable());
    assert!(!candidate.permits_change(RepoChange::Deleted));

    let config = RepoPattern::TrackedMakinaConfig {
        validation_base: "a".repeat(40),
    };
    let deletion = RepoPattern::TrackedMakinaDeletion {
        path: ".makina/old".into(),
        validation_base: "a".repeat(40),
    };
    for change in [
        RepoChange::Deleted,
        RepoChange::Added,
        RepoChange::RenamedOrCopied,
        RepoChange::TypeChanged,
        RepoChange::Unmerged,
        RepoChange::Submodule,
    ] {
        assert!(!config.permits_change(change));
    }
    assert!(config.permits_change(RepoChange::ModifiedOrdinaryFile));
    for change in [
        RepoChange::ModifiedOrdinaryFile,
        RepoChange::Added,
        RepoChange::RenamedOrCopied,
        RepoChange::TypeChanged,
        RepoChange::Unmerged,
        RepoChange::Submodule,
    ] {
        assert!(!deletion.permits_change(change));
    }
    assert!(deletion.permits_change(RepoChange::Deleted));
}

#[test]
fn mutation_api_only_accepts_coherent_bookkeeping() {
    let source = source(fixture());
    let mut doc =
        parse_task_document(&source, PathBuf::from("tasks/0102-define-schema.md")).unwrap();
    assert!(
        doc.update_bookkeeping(AuthoredTaskStatus::Done, None)
            .is_err()
    );
    let oid = GitObjectId::parse("f".repeat(40), GitObjectFormat::Sha1).unwrap();
    doc.update_bookkeeping(AuthoredTaskStatus::Done, Some(oid))
        .unwrap();
    assert_eq!(doc.frontmatter.status, AuthoredTaskStatus::Done);
    assert_eq!(doc.frontmatter.kind, TaskKind::Task);
}

#[test]
fn parses_the_plan_0048_schema_task_as_authored() {
    let bytes = include_bytes!(concat!(
        "../../../docs/plans/0048-Per-Task-Plan-Documents-And-Transactional-Status/",
        "tasks/0102-define-task-document-schema.md"
    ))
    .to_vec();
    let source = source(bytes);
    let document = parse_task_document(
        &source,
        "docs/plans/0048-Per-Task-Plan-Documents-And-Transactional-Status/tasks/0102-define-task-document-schema.md",
    )
    .unwrap();
    assert_eq!(
        document.frontmatter.id.as_str(),
        "define-task-document-schema"
    );
}

#[test]
fn rejects_each_missing_done_when_body_category() {
    for (from, to) in [
        ("# Define schema", "# Different"),
        ("**Steps:**", "**Actions:**"),
        (
            "- **Done when:** parsing and rendering preserve the Markdown body exactly.",
            "Completion text.",
        ),
    ] {
        let bytes = replace(fixture(), from, to);
        assert!(parse_task_document(&source(bytes), "tasks/0102-define-schema.md").is_err());
    }
    let no_ordered = replace(
        replace(
            fixture(),
            "1. Parse and validate the document.",
            "Parse the document.",
        ),
        "2. Render canonical frontmatter.",
        "Render canonical frontmatter.",
    );
    assert!(parse_task_document(&source(no_ordered), "tasks/0102-define-schema.md").is_err());
}

#[test]
fn rejects_limits_controls_duplicate_dependencies_and_bad_globs() {
    let oversized = vec![b'x'; 1024 * 1024 + 1];
    assert!(parse_task_document(&source(oversized), "tasks/0102-define-schema.md").is_err());
    let duplicate = replace(
        fixture(),
        "depends_on: []",
        "depends_on:\n  - prior-task\n  - prior-task",
    );
    assert!(parse_task_document(&source(duplicate), "tasks/0102-define-schema.md").is_err());
    let control = replace(
        fixture(),
        "title: Define schema",
        "title: \"bad\\u0001title\"",
    );
    assert!(parse_task_document(&source(control), "tasks/0102-define-schema.md").is_err());
    for pattern in ["src/a*b", "src/**/file", "src/***", ".makina/**"] {
        let bytes = replace(fixture(), "crates/makina-core/src/plan.rs", pattern);
        assert!(parse_task_document(&source(bytes), "tasks/0102-define-schema.md").is_err());
    }
    for pattern in ["crates/*/src", "crates/makina-core/**"] {
        let bytes = replace(fixture(), "crates/makina-core/src/plan.rs", pattern);
        parse_task_document(&source(bytes), "tasks/0102-define-schema.md").unwrap();
    }
}

#[test]
fn rejects_filename_bounds_and_self_inconsistent_dependencies() {
    for path in [
        "tasks/0002-define-schema.md",
        "tasks/0100-define-schema.md",
        "tasks/01100-define-schema.md",
        "tasks/0102-Define-schema.md",
        "tasks/0102-define--schema.md",
    ] {
        assert!(
            parse_task_document(&source(fixture()), path).is_err(),
            "accepted {path}"
        );
    }
}

#[cfg(unix)]
#[test]
fn filesystem_source_rejects_component_final_symlinks_and_non_files() {
    use std::fs;
    use std::os::unix::fs::symlink;

    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "-q"]);
    fs::create_dir_all(repo.path().join("real/tasks")).unwrap();
    fs::write(
        repo.path().join("real/tasks/0102-define-schema.md"),
        fixture(),
    )
    .unwrap();
    symlink("real", repo.path().join("linked")).unwrap();
    symlink(
        "0102-define-schema.md",
        repo.path().join("real/tasks/0103-link.md"),
    )
    .unwrap();
    fs::create_dir(repo.path().join("real/tasks/0104-dir.md")).unwrap();
    let source = FilesystemPlanFileSource::new(repo.path(), None).unwrap();
    assert!(
        source
            .read_file(Path::new("linked/tasks/0102-define-schema.md"))
            .is_err()
    );
    assert!(
        source
            .read_file(Path::new("real/tasks/0103-link.md"))
            .is_err()
    );
    assert!(
        source
            .read_file(Path::new("real/tasks/0104-dir.md"))
            .is_err()
    );
    assert!(source.read_file(Path::new("../escape.md")).is_err());
}

#[test]
fn tracked_lookup_is_pinned_to_base_and_survives_worktree_deletion() {
    use std::fs;

    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "-q"]);
    run_git(repo.path(), &["config", "user.email", "test@example.com"]);
    run_git(repo.path(), &["config", "user.name", "Test"]);
    fs::create_dir_all(repo.path().join(".makina")).unwrap();
    fs::create_dir_all(repo.path().join("tasks")).unwrap();
    fs::write(repo.path().join(".makina/config.toml"), "base = true\n").unwrap();
    let task = replace(
        fixture(),
        "crates/makina-core/src/plan.rs",
        ".makina/config.toml",
    );
    fs::write(repo.path().join("tasks/0102-define-schema.md"), task).unwrap();
    run_git(repo.path(), &["add", "."]);
    run_git(repo.path(), &["commit", "-qm", "base"]);
    let base = git_output(repo.path(), &["rev-parse", "HEAD"]);

    fs::remove_file(repo.path().join(".makina/config.toml")).unwrap();
    run_git(repo.path(), &["add", "-u"]);
    let source = FilesystemPlanFileSource::new(repo.path(), Some(base.clone())).unwrap();
    let document = parse_task_document(&source, "tasks/0102-define-schema.md").unwrap();
    assert!(matches!(
        document.frontmatter.touches[0],
        RepoPattern::TrackedMakinaConfig { .. }
    ));

    let head_without_config = {
        run_git(repo.path(), &["commit", "-qm", "delete in head"]);
        git_output(repo.path(), &["rev-parse", "HEAD"])
    };
    let wrong = FilesystemPlanFileSource::new(repo.path(), Some(head_without_config)).unwrap();
    assert!(parse_task_document(&wrong, "tasks/0102-define-schema.md").is_err());
}

#[test]
fn tracked_lookup_is_literal_and_rejects_gitlinks() {
    use std::fs;

    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "-q"]);
    run_git(repo.path(), &["config", "user.email", "test@example.com"]);
    run_git(repo.path(), &["config", "user.name", "Test"]);
    fs::create_dir_all(repo.path().join(".makina")).unwrap();
    fs::write(repo.path().join(".makina/[literal]*.txt"), "ordinary\n").unwrap();
    run_git(repo.path(), &["add", "."]);
    run_git(repo.path(), &["commit", "-qm", "ordinary"]);
    let commit = git_output(repo.path(), &["rev-parse", "HEAD"]);
    run_git(
        repo.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{commit},.makina/gitlink"),
        ],
    );
    run_git(repo.path(), &["commit", "-qm", "gitlink"]);
    let base = git_output(repo.path(), &["rev-parse", "HEAD"]);
    let source = FilesystemPlanFileSource::new(repo.path(), Some(base)).unwrap();
    assert!(
        source
            .is_tracked_ordinary_file(Path::new(".makina/[literal]*.txt"))
            .unwrap()
    );
    assert!(
        !source
            .is_tracked_ordinary_file(Path::new(".makina/gitlink"))
            .unwrap()
    );
}

fn run_git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn git_output(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
