use std::path::{Path, PathBuf};
use std::process::Command;

const LEGACY_FILENAME: &str = "TASKS.md";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllowedCategory {
    HistoricalPlan,
    DatedReview,
    Plan0048Cutover,
    NegativeCutoverTest,
}

fn historical_plan(path: &Path) -> bool {
    let Some(name) = path
        .components()
        .nth(2)
        .and_then(|part| part.as_os_str().to_str())
    else {
        return false;
    };
    name.get(..4)
        .and_then(|number| number.parse::<u16>().ok())
        .is_some_and(|number| number < 48)
}

fn dated_review(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let bytes = name.as_bytes();
    bytes.len() >= 11
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && bytes[10] == b'-'
}

fn allowed_category(path: &Path) -> Option<AllowedCategory> {
    if path.starts_with("docs/plans/") && historical_plan(path) {
        return Some(AllowedCategory::HistoricalPlan);
    }
    if path.starts_with("docs/reviews/") && dated_review(path) {
        return Some(AllowedCategory::DatedReview);
    }
    if path.starts_with("docs/plans/0048-Per-Task-Plan-Documents-And-Transactional-Status/") {
        return Some(AllowedCategory::Plan0048Cutover);
    }
    if matches!(
        path.to_str(),
        Some(
            "crates/makina/tests/scaffold_integration_test.rs"
                | "crates/makina-core/tests/plan_bundle.rs"
                | "crates/makina-core/tests/plan_open_reconciliation.rs"
                | "crates/makina-core/tests/legacy_contract_search.rs"
        )
    ) {
        return Some(AllowedCategory::NegativeCutoverTest);
    }
    None
}

#[test]
fn legacy_monolith_filename_is_confined_to_inert_history_and_negative_tests() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.parent().unwrap().parent().unwrap();
    let output = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
        .expect("run git ls-files");
    assert!(output.status.success(), "git ls-files failed");

    let mut forbidden = Vec::new();
    for encoded in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let Ok(relative) = std::str::from_utf8(encoded) else {
            continue;
        };
        let relative = Path::new(relative);
        let Ok(bytes) = std::fs::read(root.join(relative)) else {
            continue;
        };
        if !bytes
            .windows(LEGACY_FILENAME.len())
            .any(|window| window == LEGACY_FILENAME.as_bytes())
        {
            continue;
        }
        if allowed_category(relative).is_none() {
            forbidden.push(relative.to_path_buf());
        }
    }

    assert!(
        forbidden.is_empty(),
        "live legacy plan-contract filename found outside the narrow historical/cutover allowlist:\n{}",
        forbidden
            .iter()
            .map(|path| format!("- {}", path.display()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn allowlist_categories_are_structural_and_do_not_cover_live_surfaces() {
    assert_eq!(
        allowed_category(Path::new("docs/plans/0047-Historical/TASKS.md")),
        Some(AllowedCategory::HistoricalPlan)
    );
    assert_eq!(
        allowed_category(Path::new("docs/reviews/2026-07-20-cutover.md")),
        Some(AllowedCategory::DatedReview)
    );
    for forbidden in [
        "crates/makina/src/templates/todo/plan_task_md",
        ".claude/workflows/create-plan.js",
        ".claude/skills/create-plan/SKILL.md",
        "README.md",
        "docs/plans/README.md",
        "docs/spec/project-discovery.md",
    ] {
        assert_eq!(allowed_category(Path::new(forbidden)), None, "{forbidden}");
    }
}
