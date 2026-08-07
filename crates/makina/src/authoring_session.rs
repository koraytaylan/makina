//! Durable plan-authoring conversations.
//!
//! A plan is the *output* of a conversation, and the conversation is worth more
//! than the artifact for as long as the plan is still being shaped: the answers
//! the operator gave, the alternatives the planner discarded, the constraint
//! that only came up on the fourth question. Holding that in the tab alone made
//! it live exactly as long as the tab did — a generated bundle, an `Esc`, or a
//! restart took it all.
//!
//! So every authoring conversation is also a file. One JSON document per
//! session under `<project>/.makina/authoring/`, rewritten whenever the
//! transcript changes:
//!
//! - **Unfinished** (`plans` is empty) — a draft the operator is still working
//!   on. Re-opening the authoring tab for that project resumes it.
//! - **Finished** — it produced one or more plans, and stays reachable *through*
//!   them: opening a plan's conversation loads the session that wrote it, so a
//!   plan can be refined by continuing the discussion that created it rather
//!   than by describing it again from scratch.
//!
//! Writes are atomic (temp file + same-directory rename), so a crash mid-write
//! can never leave a half-written transcript where a whole one used to be.
//!
//! The directory ignores itself. A conversation is per-operator working state,
//! like `.makina/runs/`, and the `/authoring/` line that would otherwise have to
//! be added to every existing project's `.makina/.gitignore` would be missing
//! from exactly the projects that already have one.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app::ExchangeEntry;

/// Directory holding one project's authoring sessions, relative to its root.
const SESSIONS_DIR: &str = ".makina/authoring";

/// One plan-authoring conversation, as it is stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthoringSession {
    /// Filename stem, and the id the TUI carries while the session is live.
    pub id: String,
    /// When the conversation started (RFC 3339).
    pub created_at: String,
    /// When it was last written (RFC 3339). Orders the resume candidates.
    pub updated_at: String,
    /// Plan directories this conversation produced, oldest first, each relative
    /// to the project root (`docs/plans/0042-cache-markdown-rendering`).
    ///
    /// Empty means the conversation has not landed a plan yet — the state that
    /// makes it the one to resume when the operator asks to create a plan.
    #[serde(default)]
    pub plans: Vec<String>,
    /// The unsent composer text. Half a sentence the operator had not pressed
    /// Enter on yet is still theirs, and is exactly what a restart used to eat.
    #[serde(default)]
    pub draft: String,
    /// The conversation itself, in the order it happened.
    #[serde(default)]
    pub entries: Vec<ExchangeEntry>,
}

impl AuthoringSession {
    /// A short description of when this conversation last moved, for the status
    /// line that announces a resume.
    pub fn when(&self) -> &str {
        // The RFC 3339 stamp is already sorted date-then-time; the date alone is
        // what makes "resumed the conversation from …" readable.
        self.updated_at
            .split('T')
            .next()
            .unwrap_or(&self.updated_at)
    }

    /// Whether this conversation has yet to produce a plan.
    pub fn unfinished(&self) -> bool {
        self.plans.is_empty()
    }
}

/// The directory this project's sessions live in.
pub fn sessions_dir(project_root: &Path) -> PathBuf {
    project_root.join(SESSIONS_DIR)
}

/// A fresh session id: the UTC start time, then the writer and a counter.
///
/// The timestamp leads so a directory listing reads as a chronology without
/// opening anything. The process id and per-process sequence follow because a
/// millisecond is not a guarantee — two conversations started in the same one
/// would otherwise be the same file, and the second would silently erase the
/// first. Same reasoning as the temp names in `makina_core::persist`.
pub fn new_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%3f");
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{stamp}-{}-{sequence}", std::process::id())
}

/// The current time as an RFC 3339 stamp, for `created_at` / `updated_at`.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Write `session` under `project_root`, atomically.
///
/// Creates the directory (and its self-ignoring `.gitignore`) on first use. The
/// document lands in a temp file in the same directory and is renamed over the
/// target, so a concurrent reader sees either the previous transcript or the
/// new one, never a partial write.
pub fn save(project_root: &Path, session: &AuthoringSession) -> Result<(), String> {
    let dir = sessions_dir(project_root);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        // Best effort: a conversation that could not write its ignore rule is
        // still a conversation worth keeping, and the operator can see the
        // untracked directory and decide for themselves.
        let _ = std::fs::write(&ignore, "*\n");
    }

    let body = serde_json::to_string_pretty(session)
        .map_err(|e| format!("failed to serialize the authoring session: {e}"))?;
    let target = dir.join(format!("{}.json", session.id));
    let temp = dir.join(format!(".{}.json.tmp", session.id));
    std::fs::write(&temp, body).map_err(|e| format!("failed to write {}: {e}", temp.display()))?;
    std::fs::rename(&temp, &target)
        .map_err(|e| format!("failed to publish {}: {e}", target.display()))
}

/// Every session stored under `project_root`, most recently updated first.
///
/// Unreadable or unparseable documents are skipped rather than failing the
/// listing: one corrupt transcript must not cost the operator the others.
pub fn load_all(project_root: &Path) -> Vec<AuthoringSession> {
    let Ok(entries) = std::fs::read_dir(sessions_dir(project_root)) else {
        return Vec::new();
    };
    let mut sessions: Vec<AuthoringSession> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|body| serde_json::from_str(&body).ok())
        .collect();
    sessions.sort_by(|a: &AuthoringSession, b: &AuthoringSession| {
        b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id))
    });
    sessions
}

/// The most recent conversation that has not produced a plan yet, if any.
///
/// This is what "create a plan" resumes: a conversation that already landed a
/// plan is finished work reachable through that plan, and re-opening it as the
/// draft would silently continue an old discussion under a new intent.
pub fn latest_unfinished(project_root: &Path) -> Option<AuthoringSession> {
    load_all(project_root)
        .into_iter()
        .find(AuthoringSession::unfinished)
}

/// The most recent conversation that produced `plan_dir` (relative to the
/// project root), if one was recorded.
pub fn for_plan(project_root: &Path, plan_dir: &str) -> Option<AuthoringSession> {
    load_all(project_root)
        .into_iter()
        .find(|session| session.plans.iter().any(|plan| plan == plan_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ExchangeContent, StreamRole};
    use tempfile::TempDir;

    fn session(id: &str, updated_at: &str, plans: &[&str]) -> AuthoringSession {
        AuthoringSession {
            id: id.to_owned(),
            created_at: "2026-01-01T00:00:00+00:00".to_owned(),
            updated_at: updated_at.to_owned(),
            plans: plans.iter().map(|plan| (*plan).to_owned()).collect(),
            draft: String::new(),
            entries: Vec::new(),
        }
    }

    /// The transcript itself round-trips: what is read back is what was said.
    #[test]
    fn a_saved_conversation_reads_back_verbatim() {
        let project = TempDir::new().expect("temp project");
        let mut original = session("20260131T101500000", "2026-01-31T10:15:00+00:00", &[]);
        original.draft = "half a thought".into();
        original.entries.push(ExchangeEntry {
            role: StreamRole::Operator,
            content: ExchangeContent::Prompt {
                text: "an fsm cli".into(),
            },
        });
        original.entries.push(ExchangeEntry {
            role: StreamRole::Planner,
            content: ExchangeContent::Response {
                text: "YAML or TOML?".into(),
                complete: true,
            },
        });
        save(project.path(), &original).expect("save");

        let loaded = load_all(project.path());
        assert_eq!(loaded.len(), 1, "exactly the one session was written");
        let loaded = &loaded[0];
        assert_eq!(loaded.id, original.id);
        assert_eq!(loaded.draft, "half a thought");
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.entries[0].role, StreamRole::Operator);
        assert_eq!(loaded.entries[0].text(), "an fsm cli");
        assert_eq!(loaded.entries[1].role, StreamRole::Planner);
        assert_eq!(loaded.entries[1].text(), "YAML or TOML?");
    }

    /// Saving twice replaces the document rather than accumulating files.
    #[test]
    fn re_saving_a_conversation_rewrites_its_document() {
        let project = TempDir::new().expect("temp project");
        let mut live = session("20260131T101500000", "2026-01-31T10:15:00+00:00", &[]);
        save(project.path(), &live).expect("first save");
        live.updated_at = "2026-01-31T10:20:00+00:00".into();
        live.plans.push("docs/plans/0001-fsm".into());
        save(project.path(), &live).expect("second save");

        let loaded = load_all(project.path());
        assert_eq!(loaded.len(), 1, "one conversation, one document");
        assert_eq!(loaded[0].plans, vec!["docs/plans/0001-fsm".to_owned()]);
    }

    /// The directory keeps itself out of the project's history.
    #[test]
    fn the_sessions_directory_ignores_itself() {
        let project = TempDir::new().expect("temp project");
        save(
            project.path(),
            &session("20260131T101500000", "2026-01-31T10:15:00+00:00", &[]),
        )
        .expect("save");

        let ignore = sessions_dir(project.path()).join(".gitignore");
        assert_eq!(
            std::fs::read_to_string(ignore).expect("the ignore rule is written"),
            "*\n",
        );
    }

    /// Resuming picks the newest draft — and never a conversation that already
    /// landed a plan, which is finished work reachable through that plan.
    #[test]
    fn resuming_prefers_the_newest_conversation_without_a_plan() {
        let project = TempDir::new().expect("temp project");
        save(
            project.path(),
            &session("20260131T090000000", "2026-01-31T09:00:00+00:00", &[]),
        )
        .expect("save older draft");
        save(
            project.path(),
            &session("20260131T100000000", "2026-01-31T10:00:00+00:00", &[]),
        )
        .expect("save newer draft");
        save(
            project.path(),
            &session(
                "20260131T110000000",
                "2026-01-31T11:00:00+00:00",
                &["docs/plans/0001-fsm"],
            ),
        )
        .expect("save finished conversation");

        assert_eq!(
            latest_unfinished(project.path()).map(|s| s.id),
            Some("20260131T100000000".to_owned()),
            "the newest *unfinished* draft is the one to resume",
        );
    }

    /// A finished conversation is found by the plan it wrote.
    #[test]
    fn a_plans_conversation_is_found_by_its_plan_directory() {
        let project = TempDir::new().expect("temp project");
        save(
            project.path(),
            &session(
                "20260131T110000000",
                "2026-01-31T11:00:00+00:00",
                &["docs/plans/0001-fsm"],
            ),
        )
        .expect("save");

        assert_eq!(
            for_plan(project.path(), "docs/plans/0001-fsm").map(|s| s.id),
            Some("20260131T110000000".to_owned()),
        );
        assert!(
            for_plan(project.path(), "docs/plans/0002-other").is_none(),
            "a plan nobody discussed has no conversation",
        );
    }

    /// A project that has never authored anything lists nothing, rather than
    /// failing on the missing directory.
    #[test]
    fn a_project_with_no_conversations_lists_none() {
        let project = TempDir::new().expect("temp project");
        assert!(load_all(project.path()).is_empty());
        assert!(latest_unfinished(project.path()).is_none());
    }

    /// One unreadable document must not cost the operator the others.
    #[test]
    fn an_unparseable_document_is_skipped_not_fatal() {
        let project = TempDir::new().expect("temp project");
        save(
            project.path(),
            &session("20260131T100000000", "2026-01-31T10:00:00+00:00", &[]),
        )
        .expect("save");
        std::fs::write(
            sessions_dir(project.path()).join("broken.json"),
            "{ not json",
        )
        .expect("write the corrupt document");

        let loaded = load_all(project.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "20260131T100000000");
    }
}
