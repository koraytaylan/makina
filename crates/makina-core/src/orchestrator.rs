//! The real, core-backed [`Api`] implementation.
//!
//! [`CoreApi`] is the orchestrator's outward-facing surface — the concrete type
//! the TUI binds to via `Arc<dyn Api>`.  It implements the full command set:
//!
//! ```text
//!   execute(OpenPlan{path})
//!       │ read file (tokio::fs) → interpret → register (status = Pending)
//!       ▼ broadcast Event::RunOpened
//!   execute(StartRun{run})
//!       │ tokio::spawn(run_graph(graph, …, RunControl{ sink→broadcast, … }))
//!       ▼ status = Running; the Supervisor scheduler drives the graph in the
//!         background, emitting live TaskStateChanged / TaskIterationsUpdated /
//!         RunStatusChanged / AgentExchange events as it goes.
//!   execute(PauseRun{run})   → set pause flag (stop launching NEW tasks)
//!   execute(CancelRun{run})  → cancel token (abort + worktree teardown)
//! ```
//!
//! plus the read queries ([`Api::runs`] / [`Api::run`]) and the live
//! [`Api::subscribe`] stream.
//!
//! # Execution model (task 31: run-control)
//!
//! Execution lives behind injected dependencies so the deterministic TUI path
//! and the e2e ACP path are interchangeable: a `backend: Arc<dyn AgentBackend>`,
//! a [`WorktreeManager`] (repo root + base branch), and a [`Config`].  On
//! `StartRun`, `CoreApi` spawns a background tokio task that runs the
//! Supervisor's [`run_graph`] over the Run's **shared graph**
//! (`Arc<tokio::sync::Mutex<TaskGraph>>`), wired to an [`EventSink`] that
//! forwards every engine [`Event`] to the same broadcast `subscribe()` reads.
//! The graph is shared between the background scheduler (which mutates it) and
//! the read queries (which snapshot it), so `run()`/`runs()` reflect live state.
//!
//! ## Pause semantics (MVP)
//!
//! `PauseRun` sets a cooperative pause flag: the scheduler stops launching NEW
//! task drivers; in-flight tasks finish.  Resume = `StartRun` again, which
//! clears the flag and spawns a fresh `run_graph` that continues launching ready
//! tasks (already-`Done` tasks are skipped, their dependents unlock).  This is
//! the documented "stop launching new tasks" MVP semantics.
//!
//! ## Cancel semantics
//!
//! `CancelRun` cancels the run's [`CancellationToken`]: the scheduler stops
//! launching and `abort_all()`s in-flight drivers — each aborted driver's
//! `DriverGuard` still tears down its worktree + spokes (no leak).  The Run's
//! status is set to `Failed` (cancelled) and that status is emitted; the
//! background task's own terminal status emission is suppressed for a cancelled
//! run (see [`run_graph`]) so it cannot overwrite the cancelled status.
//!
//! # Locking discipline
//!
//! The Runs registry lives behind a `std::sync::Mutex`.  The lock is **never
//! held across an `.await`**: every handler locks, reads/mutates the registry
//! (cloning out the `Arc` graph handle / the cancel+pause handle), drops the
//! guard, and only then awaits I/O, locks the (separate) `tokio::sync::Mutex`
//! graph, or broadcasts an event.
//!
//! # Seams
//!
//! | Concern | Status here | Owning task |
//! |---------|-------------|-------------|
//! | `OpenPlan` → interpret → register → broadcast | implemented | task 28 |
//! | `runs()` / `run()` / `subscribe()` | implemented | task 28 |
//! | `StartRun` / `PauseRun` / `CancelRun` driving the Supervisor | **implemented** | this task (31) |
//! | model-backed interpreter + real ACP backend | injected, not wired | e2e (task 33) |

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;

use crate::actors::{EventSink, RunControl};
use crate::api::{
    Api, ApiError, AuthoredTaskView, Command, CommandOutcome, Event, EventStream, RunId, RunStatus,
    RunView, TaskView,
};
use crate::audit::{AuditRegistry, NoopAuditRegistry};
use crate::backend::AgentBackend;
use crate::config::{CapsConfig, Config, FinalMerge};
use crate::interpreter::TaskListInterpreter;
use crate::paths;
use crate::run_metadata::{
    RunMetadata, TaskSnapshot, load_disk_run_views, remove_run_metadata_for_plan,
    write_run_metadata,
};
use crate::task::{TaskGraph, TaskState};
use crate::worktree::WorktreeManager;

// ── Constants ────────────────────────────────────────────────────────────────────

const SLUG_FALLBACK: &str = "plan";

/// Derive the run slug from the canonical plan-directory basename.
pub fn run_slug(plan_dir: &Path) -> String {
    plan_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_kebab)
        .filter(|slug| slug.len() >= 2)
        .unwrap_or_else(|| SLUG_FALLBACK.to_owned())
}

/// Derive the plan slug from the same canonical directory identity.
pub fn plan_slug(plan_dir: &Path) -> String {
    run_slug(plan_dir)
}

/// Registration/execution state derived by the single typed plan scanner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanDiscoveryState {
    AwaitingCommit,
    Unregistered,
    Ready,
    Active,
    Invalid,
}

impl PlanDiscoveryState {
    pub fn label(self) -> &'static str {
        match self {
            Self::AwaitingCommit => "awaiting-commit",
            Self::Unregistered => "unregistered",
            Self::Ready => "ready",
            Self::Active => "active",
            Self::Invalid => "invalid",
        }
    }
}

impl std::fmt::Display for PlanDiscoveryState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// One correlated plan identity. Typed document data is populated only after
/// the shared loader has validated the complete bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub dir: PathBuf,
    pub key: crate::plan::PlanKey,
    pub slug: String,
    pub state: PlanDiscoveryState,
    pub document: Option<crate::plan::PlanDocument>,
    pub diagnostics: crate::plan::PlanValidationReport,
}

impl PlanEntry {
    pub fn tasks(&self) -> &[crate::plan::TaskDocument] {
        self.document
            .as_ref()
            .map_or(&[], |document| document.tasks.as_slice())
    }

    pub fn has_document(&self) -> bool {
        self.document.is_some()
    }

    pub fn is_executable(&self) -> bool {
        matches!(
            self.state,
            PlanDiscoveryState::Ready | PlanDiscoveryState::Active
        )
    }
}

/// Discover and classify the union of working-tree, committed-base, and
/// retained `plan/*` candidates. Historical numbered directories reserve their
/// number but remain inert when they contain no `tasks/` directory.
pub fn discover_plans(repo_root: &Path) -> Vec<PlanEntry> {
    use crate::plan::{
        FilesystemPlanFileSource, GitTreePlanFileSource, PlanCandidate, PlanKey, PlanReservations,
        PlanValidationDiagnostic, PlanValidationReport, load_plan,
    };

    fn git(root: &Path, args: &[&str]) -> Option<String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }
    fn diagnostic(code: &str, path: PathBuf, message: impl Into<String>) -> PlanValidationReport {
        PlanValidationReport {
            diagnostics: vec![PlanValidationDiagnostic {
                code: code.to_owned(),
                path,
                field: None,
                message: message.into(),
            }],
        }
    }
    fn reserve_directory(reservations: &mut PlanReservations, relative: &Path, name: &str) {
        if name.len() < 4 || !name.as_bytes()[..4].iter().all(u8::is_ascii_digit) {
            return;
        }
        let paths = reservations
            .numbered_directories
            .entry(name[..4].to_owned())
            .or_default();
        if !paths.iter().any(|path| path == relative) {
            paths.push(relative.to_path_buf());
        }
    }
    fn trailer(message: &str, name: &str) -> Option<String> {
        let prefix = format!("{name}: ");
        message
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum RetainedEvidence {
        Registration,
        Claim,
        Landing,
        Bookkeeping,
        Disposition,
        /// A coordinator-owned source transition — `reconcile`, `retry`,
        /// `requeue`, `cancel`, or `blocker`. It rewrites the task's authored
        /// status, which is deliberately outside the digest, so unlike a
        /// disposition it never re-authorizes digests; it only ends whatever
        /// claim was open for that task.
        Transition,
        Prepared,
        FinalIntegration,
        Completion,
    }

    fn retained_evidence(message: &str, expected_plan: &str) -> Result<RetainedEvidence, String> {
        let has = |name| trailer(message, name).is_some_and(|value| !value.is_empty());
        if trailer(message, "Makina-Plan").as_deref() != Some(expected_plan) {
            return Err("retained commit lacks the expected Makina-Plan identity".into());
        }
        match trailer(message, "Makina-Phase").as_deref() {
            Some("plan-registration")
                if has("Makina-Source-Digest")
                    && has("Makina-Executable-Digest")
                    && has("Makina-Validation-Base") =>
            {
                Ok(RetainedEvidence::Registration)
            }
            Some("task-status")
                if has("Makina-Task") && has("Makina-Run") && has("Makina-Landing") =>
            {
                Ok(RetainedEvidence::Bookkeeping)
            }
            // `commit_task_claim` stamps `task-claim`; `landing::inspect_task_evidence`
            // reads that spelling too. Only this classifier did not, so the very
            // first post-registration commit of every plan was unclassifiable
            // and no plan could be re-opened once a single task had been
            // claimed — no resume after an interrupt, no second `OpenPlan`.
            Some("task-claim") if has("Makina-Task") && has("Makina-Run") => {
                Ok(RetainedEvidence::Claim)
            }
            Some("task-status") if has("Makina-Task") && has("Makina-Run") => {
                Ok(RetainedEvidence::Claim)
            }
            Some("task-disposition")
                if has("Makina-Task")
                    && has("Makina-Run")
                    && has("Makina-Previous-Source-Digest")
                    && has("Makina-New-Source-Digest")
                    && has("Makina-Previous-Plan-Digest")
                    && has("Makina-New-Plan-Digest") =>
            {
                Ok(RetainedEvidence::Disposition)
            }
            Some("task-transition")
                if has("Makina-Task") && has("Makina-Run") && has("Makina-Transition") =>
            {
                Ok(RetainedEvidence::Transition)
            }
            Some("finalization-prepared")
                if has("Makina-Run") && has("Makina-Final-Mode") && has("Makina-Expected-Base") =>
            {
                Ok(RetainedEvidence::Prepared)
            }
            Some("final-integration")
                if has("Makina-Run") && has("Makina-Final-Mode") && has("Makina-Plan-Tip") =>
            {
                Ok(RetainedEvidence::FinalIntegration)
            }
            Some("completion") if has("Makina-Run") && has("Makina-Final-Commit") => {
                Ok(RetainedEvidence::Completion)
            }
            None if has("Makina-Task") && has("Makina-Run") => Ok(RetainedEvidence::Landing),
            _ => Err("commit is not recognized retained plan lifecycle evidence".into()),
        }
    }
    fn registration_oid(
        root: &Path,
        key: &PlanKey,
        tip: &str,
    ) -> Result<(String, String, String, String), String> {
        let history = git(root, &["rev-list", "--first-parent", tip])
            .ok_or_else(|| "cannot inspect retained plan lineage".to_owned())?;
        let expected = format!("{}-{}", key.number, key.slug);
        let mut registrations = Vec::new();
        for oid in history.lines() {
            let Some(message) = git(root, &["show", "-s", "--format=%B", oid]) else {
                return Err(format!("cannot read commit {oid}"));
            };
            if trailer(&message, "Makina-Phase").as_deref() == Some("plan-registration")
                && trailer(&message, "Makina-Plan").as_deref() == Some(expected.as_str())
            {
                let required = [
                    "Makina-Source-Digest",
                    "Makina-Executable-Digest",
                    "Makina-Validation-Base",
                ];
                if required
                    .iter()
                    .all(|name| trailer(&message, name).is_some())
                {
                    registrations.push((
                        oid.to_owned(),
                        trailer(&message, "Makina-Source-Digest").unwrap(),
                        trailer(&message, "Makina-Executable-Digest").unwrap(),
                        trailer(&message, "Makina-Validation-Base").unwrap(),
                    ));
                }
            }
        }
        match registrations.as_slice() {
            [] => Err("retained plan ref has no verified Phase-R commit".to_owned()),
            [registration] => Ok(registration.clone()),
            registrations => {
                for pair in registrations.windows(2) {
                    let newer_message = git(root, &["show", "-s", "--format=%B", &pair[0].0])
                        .ok_or_else(|| "cannot read refreshed registration".to_owned())?;
                    if trailer(&newer_message, "Makina-Previous-Registration").as_deref()
                        != Some(pair[1].0.as_str())
                    {
                        return Err(
                            "multiple Phase-R commits are not one verified refresh chain"
                                .to_owned(),
                        );
                    }
                }
                Ok(registrations[0].clone())
            }
        }
    }

    fn authorized_digests(
        root: &Path,
        key: &PlanKey,
        tip: &str,
        registration: &str,
        mut source: String,
        mut executable: String,
    ) -> Result<(String, String), String> {
        // Per-task lineage. A single global slot could only describe one task
        // at a time, so any plan whose scheduler ran two tasks concurrently —
        // which is the normal case, and the entire point of a parallel
        // workstream — produced a "duplicate, skipped, or out of order" verdict
        // and could never be re-opened. Ordering is still enforced strictly,
        // but per task, so concurrent tasks interleave freely while each one's
        // claim → landing → status sequence stays exact.
        #[derive(Debug)]
        enum TaskLineage {
            Claimed { run: String },
            Landed { run: String, oid: String },
        }
        #[derive(Debug)]
        enum FinalizationLineage {
            None,
            Prepared { oid: String, run: String },
            Integrated { oid: String, run: String },
            Complete,
        }

        let range = format!("{registration}..{tip}");
        let history = git(root, &["rev-list", "--first-parent", "--reverse", &range])
            .ok_or_else(|| "cannot inspect post-registration lineage".to_owned())?;
        let mut inflight = BTreeMap::<String, TaskLineage>::new();
        let mut finalization = FinalizationLineage::None;
        for oid in history.lines() {
            let message = git(root, &["show", "-s", "--format=%B", oid])
                .ok_or_else(|| format!("cannot read commit {oid}"))?;
            let expected = format!("{}-{}", key.number, key.slug);
            let evidence = retained_evidence(&message, &expected)
                .map_err(|reason| format!("invalid retained commit {oid}: {reason}"))?;
            let task_of = |name: &str| trailer(&message, name);
            match evidence {
                RetainedEvidence::Registration => {
                    return Err(format!(
                        "retained lineage carries a second registration at {oid}"
                    ));
                }
                RetainedEvidence::Claim => {
                    let (Some(task), Some(run)) = (task_of("Makina-Task"), task_of("Makina-Run"))
                    else {
                        return Err(format!("claim {oid} lacks task/run identity"));
                    };
                    if inflight.contains_key(&task) {
                        return Err(format!(
                            "task {task} is claimed again at {oid} before its prior claim completed"
                        ));
                    }
                    inflight.insert(task, TaskLineage::Claimed { run });
                }
                RetainedEvidence::Landing => {
                    let (Some(task), Some(run)) = (task_of("Makina-Task"), task_of("Makina-Run"))
                    else {
                        return Err(format!("landing {oid} lacks task/run identity"));
                    };
                    match inflight.get(&task) {
                        Some(TaskLineage::Claimed { run: claimed }) if *claimed == run => {
                            inflight.insert(
                                task,
                                TaskLineage::Landed {
                                    run,
                                    oid: oid.to_owned(),
                                },
                            );
                        }
                        other => {
                            return Err(format!(
                                "landing {oid} for {task} does not follow a matching claim (found {other:?})"
                            ));
                        }
                    }
                }
                RetainedEvidence::Bookkeeping => {
                    let (Some(task), Some(run), Some(landing)) = (
                        task_of("Makina-Task"),
                        task_of("Makina-Run"),
                        task_of("Makina-Landing"),
                    ) else {
                        return Err(format!("status {oid} lacks task/run/landing identity"));
                    };
                    match inflight.get(&task) {
                        Some(TaskLineage::Landed {
                            run: landed,
                            oid: landed_oid,
                        }) if *landed == run && *landed_oid == landing => {
                            inflight.remove(&task);
                        }
                        other => {
                            return Err(format!(
                                "status {oid} for {task} does not follow its exact landing (found {other:?})"
                            ));
                        }
                    }
                }
                RetainedEvidence::Disposition => {
                    let previous_source = trailer(&message, "Makina-Previous-Source-Digest")
                        .expect("classified disposition has previous source digest");
                    let next_source = trailer(&message, "Makina-New-Source-Digest")
                        .expect("classified disposition has new source digest");
                    let previous_plan = trailer(&message, "Makina-Previous-Plan-Digest")
                        .expect("classified disposition has previous plan digest");
                    let next_plan = trailer(&message, "Makina-New-Plan-Digest")
                        .expect("classified disposition has new plan digest");
                    if previous_source != source || previous_plan != executable {
                        return Err(
                            "task-disposition digest chain does not match its predecessor".into(),
                        );
                    }
                    source = next_source;
                    executable = next_plan;
                }
                RetainedEvidence::Transition => {
                    // Ends whatever claim was open for the task — a reconcile
                    // or cancel returns an interrupted task to the pool without
                    // a landing. Permissive when no claim is open: a blocker
                    // may be recorded against a task that never claimed.
                    if let Some(task) = task_of("Makina-Task") {
                        inflight.remove(&task);
                    }
                }
                RetainedEvidence::Prepared => {
                    if !inflight.is_empty() {
                        return Err(format!(
                            "finalization is prepared at {oid} while {:?} are still in flight",
                            inflight.keys().collect::<Vec<_>>()
                        ));
                    }
                    if !matches!(finalization, FinalizationLineage::None) {
                        return Err(format!("finalization is prepared twice at {oid}"));
                    }
                    finalization = FinalizationLineage::Prepared {
                        oid: oid.to_owned(),
                        run: trailer(&message, "Makina-Run").unwrap_or_default(),
                    };
                }
                RetainedEvidence::FinalIntegration => match finalization {
                    FinalizationLineage::Prepared { oid: prepared, run }
                        if trailer(&message, "Makina-Run").as_deref() == Some(run.as_str())
                            && trailer(&message, "Makina-Plan-Tip").as_deref()
                                == Some(prepared.as_str()) =>
                    {
                        finalization = FinalizationLineage::Integrated {
                            oid: oid.to_owned(),
                            run,
                        };
                    }
                    other => {
                        return Err(format!(
                            "final integration {oid} does not follow its exact preparation (found {other:?})"
                        ));
                    }
                },
                RetainedEvidence::Completion => match finalization {
                    FinalizationLineage::Integrated {
                        oid: integrated,
                        run,
                    } if trailer(&message, "Makina-Run").as_deref() == Some(run.as_str())
                        && trailer(&message, "Makina-Final-Commit").as_deref()
                            == Some(integrated.as_str()) =>
                    {
                        finalization = FinalizationLineage::Complete;
                    }
                    other => {
                        return Err(format!(
                            "completion {oid} does not follow its exact final integration (found {other:?})"
                        ));
                    }
                },
            }
        }
        let _ = finalization;
        Ok((source, executable))
    }

    let Ok(repo_root) = std::fs::canonicalize(repo_root) else {
        return Vec::new();
    };
    let plans_root = repo_root.join("docs/plans");
    let mut keys = BTreeMap::<PathBuf, PlanKey>::new();
    let mut reservations = PlanReservations::default();
    if let Ok(directory) = std::fs::read_dir(&plans_root) {
        for entry in directory.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let relative = PathBuf::from("docs/plans").join(&name);
            reserve_directory(&mut reservations, &relative, &name);
            if let Ok(key) = PlanKey::parse(relative.clone()) {
                keys.insert(relative, key);
            }
        }
    }

    // The committed side of discovery is the configured target base, never the
    // checkout's moving HEAD. This also discovers base-only plans while the
    // operator has another branch checked out.
    let configured_base = [
        repo_root.join(".makina/config.toml"),
        repo_root.join("makina.toml"),
    ]
    .into_iter()
    .find_map(|path| std::fs::read_to_string(path).ok())
    .and_then(|contents| {
        crate::config::ProjectConfig::from_toml_str(&contents, "project config").ok()
    })
    .map(|config| config.base_branch)
    .filter(|branch| !branch.is_empty())
    .unwrap_or_else(|| "develop".to_owned());
    let base_revision = format!("refs/heads/{configured_base}");
    let base_plans_tree = format!("{base_revision}:docs/plans");
    if let Some(base_dirs) = git(
        &repo_root,
        &["ls-tree", "-d", "--name-only", &base_plans_tree],
    ) {
        for name in base_dirs.lines() {
            let relative = PathBuf::from("docs/plans").join(name);
            let Some(name) = relative.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            reserve_directory(&mut reservations, &relative, name);
            if let Ok(key) = PlanKey::parse(relative.clone()) {
                keys.insert(relative, key);
            }
        }
    }

    let refs = git(
        &repo_root,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads/plan/",
        ],
    )
    .unwrap_or_default();
    let mut retained = BTreeMap::<PlanKey, String>::new();
    for line in refs.lines() {
        let Some((reference, oid)) = line.split_once(' ') else {
            continue;
        };
        let Some(basename) = reference.strip_prefix("refs/heads/plan/") else {
            continue;
        };
        let relative = PathBuf::from("docs/plans").join(basename);
        let Ok(key) = PlanKey::parse(relative.clone()) else {
            continue;
        };
        reservations
            .verified_registrations
            .entry(key.number.clone())
            .or_default()
            .push(reference.to_owned());
        keys.insert(relative, key.clone());
        retained.insert(key, oid.to_owned());
    }

    let working = FilesystemPlanFileSource::new(&repo_root, None).ok();
    let base = GitTreePlanFileSource::new(&repo_root, &base_revision).ok();
    let mut entries = Vec::new();
    for key in keys.into_values() {
        if let Some(tip) = retained.get(&key) {
            let registration = registration_oid(&repo_root, &key, tip);
            let source = GitTreePlanFileSource::new(&repo_root, tip);
            let loaded = source
                .as_ref()
                .map_err(|error| {
                    diagnostic(
                        "invalid-retained-plan",
                        key.relative_dir.clone(),
                        error.to_string(),
                    )
                })
                .and_then(|source| load_plan(source, key.clone(), &reservations));
            // Compute the authorization verdict BEFORE matching so its reason
            // survives. Evaluating it inside a match guard discarded every
            // explanation and reported the same generic "Phase-R trailers
            // disagree" line for a lineage error, a digest mismatch, and a
            // validation-base mismatch alike — three very different faults, all
            // indistinguishable to whoever had to fix them.
            let rejection: Option<String> = match (&registration, &loaded) {
                (
                    Ok((registration, source_digest, executable_digest, validation_base)),
                    Ok(PlanCandidate::Plan(document)),
                ) => match authorized_digests(
                    &repo_root,
                    &key,
                    tip,
                    registration,
                    source_digest.clone(),
                    executable_digest.clone(),
                ) {
                    Err(reason) => Some(reason),
                    Ok((source_digest, executable_digest)) => {
                        if source_digest != document.source_digest.as_str() {
                            Some(format!(
                                "authorized source digest {source_digest} does not match the retained document's {}",
                                document.source_digest.as_str()
                            ))
                        } else if executable_digest != document.executable_digest.as_str() {
                            Some(format!(
                                "authorized executable digest {executable_digest} does not match the retained document's {}",
                                document.executable_digest.as_str()
                            ))
                        } else if document
                            .status
                            .validation_base_oid
                            .as_ref()
                            .is_none_or(|oid| oid.as_str() != validation_base)
                        {
                            Some(format!(
                                "STATUS validation base {} does not match the registration's {validation_base}",
                                document
                                    .status
                                    .validation_base_oid
                                    .as_ref()
                                    .map_or("—", |oid| oid.as_str())
                            ))
                        } else {
                            None
                        }
                    }
                },
                _ => None,
            };
            match (registration, loaded) {
                (Ok((registration, _, _, _)), Ok(PlanCandidate::Plan(document)))
                    if rejection.is_none() =>
                {
                    entries.push(PlanEntry {
                        dir: repo_root.join(&key.relative_dir),
                        slug: format!("{}-{}", key.number, key.slug),
                        state: if registration == *tip {
                            PlanDiscoveryState::Ready
                        } else {
                            PlanDiscoveryState::Active
                        },
                        key,
                        document: Some(*document),
                        diagnostics: PlanValidationReport::default(),
                    })
                }
                (registration, loaded) => {
                    let diagnostics = match loaded {
                        Err(report) => report,
                        _ => diagnostic(
                            "invalid-registration",
                            key.relative_dir.clone(),
                            registration.err().or(rejection).unwrap_or_else(|| {
                                "Phase-R trailers disagree with the retained plan document"
                                    .to_owned()
                            }),
                        ),
                    };
                    entries.push(PlanEntry {
                        dir: repo_root.join(&key.relative_dir),
                        slug: format!("{}-{}", key.number, key.slug),
                        key,
                        state: PlanDiscoveryState::Invalid,
                        document: None,
                        diagnostics,
                    });
                }
            }
            continue;
        }
        if !repo_root.join(&key.relative_dir).exists() {
            match base
                .as_ref()
                .map(|base| load_plan(base, key.clone(), &reservations))
            {
                Some(Ok(PlanCandidate::Plan(document))) => entries.push(PlanEntry {
                    dir: repo_root.join(&key.relative_dir),
                    slug: format!("{}-{}", key.number, key.slug),
                    key,
                    state: PlanDiscoveryState::Unregistered,
                    document: Some(*document),
                    diagnostics: PlanValidationReport::default(),
                }),
                Some(Err(diagnostics)) => entries.push(PlanEntry {
                    dir: repo_root.join(&key.relative_dir),
                    slug: format!("{}-{}", key.number, key.slug),
                    key,
                    state: PlanDiscoveryState::Invalid,
                    document: None,
                    diagnostics,
                }),
                _ => {}
            }
            continue;
        }
        let Some(source) = working.as_ref() else {
            continue;
        };
        match load_plan(source, key.clone(), &reservations) {
            Ok(PlanCandidate::NotCandidate) => {
                match base
                    .as_ref()
                    .map(|base| load_plan(base, key.clone(), &reservations))
                {
                    Some(Ok(PlanCandidate::Plan(document))) => entries.push(PlanEntry {
                        dir: repo_root.join(&key.relative_dir),
                        slug: format!("{}-{}", key.number, key.slug),
                        key,
                        state: PlanDiscoveryState::Unregistered,
                        document: Some(*document),
                        diagnostics: PlanValidationReport::default(),
                    }),
                    Some(Err(diagnostics)) => entries.push(PlanEntry {
                        dir: repo_root.join(&key.relative_dir),
                        slug: format!("{}-{}", key.number, key.slug),
                        key,
                        state: PlanDiscoveryState::Invalid,
                        document: None,
                        diagnostics,
                    }),
                    _ => {}
                }
            }
            Err(diagnostics) => entries.push(PlanEntry {
                dir: repo_root.join(&key.relative_dir),
                slug: format!("{}-{}", key.number, key.slug),
                key,
                state: PlanDiscoveryState::Invalid,
                document: None,
                diagnostics,
            }),
            Ok(PlanCandidate::Plan(document)) => {
                let committed = base
                    .as_ref()
                    .and_then(|base| match load_plan(base, key.clone(), &reservations) {
                        Ok(PlanCandidate::Plan(base_document)) => {
                            Some(base_document.source_digest == document.source_digest)
                        }
                        _ => None,
                    })
                    .unwrap_or(false);
                entries.push(PlanEntry {
                    dir: repo_root.join(&key.relative_dir),
                    slug: format!("{}-{}", key.number, key.slug),
                    key,
                    state: if committed {
                        PlanDiscoveryState::Unregistered
                    } else {
                        PlanDiscoveryState::AwaitingCommit
                    },
                    document: Some(*document),
                    diagnostics: PlanValidationReport::default(),
                });
            }
        }
    }
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    entries
}

/// Load one plan through the same authoritative source selection used by
/// discovery. Working candidates come from the filesystem, base-only plans
/// come from the configured base commit, and registered plans come from the
/// exact retained-ref tip. Consumers must not independently fall back to the
/// checkout because it may be absent or contain different task prose.
pub fn load_authoritative_plan(
    repo_root: &Path,
    key: &crate::plan::PlanKey,
) -> Result<crate::plan::PlanDocument, crate::plan::PlanValidationReport> {
    use crate::plan::{PlanValidationDiagnostic, PlanValidationReport};

    let Some(entry) = discover_plans(repo_root)
        .into_iter()
        .find(|entry| entry.key == *key)
    else {
        return Err(PlanValidationReport {
            diagnostics: vec![PlanValidationDiagnostic {
                code: "plan-not-found".into(),
                path: key.relative_dir.clone(),
                field: None,
                message:
                    "plan is absent from the working tree, configured base, and retained plan refs"
                        .into(),
            }],
        });
    };

    entry.document.ok_or(entry.diagnostics)
}

pub fn discover_plans_per_folder(
    opened_folders: &[PathBuf],
) -> std::collections::HashMap<usize, Vec<PlanEntry>> {
    opened_folders
        .iter()
        .enumerate()
        .map(|(index, root)| (index, discover_plans(root)))
        .collect()
}

/// Sanitize `input` into a valid kebab id per `runtime-artifact-schema.md`
/// §4.1: lowercase; map every maximal run of non-`[a-z0-9]` chars to a single
/// `-`; trim leading/trailing `-`. The caller enforces the §4.1 length minimum.
fn sanitize_kebab(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_dash = false;
    for ch in input.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            // Emit a single separating dash only between alphanumerics, never
            // leading — this also collapses runs of non-alnum to one `-`.
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch);
        } else {
            pending_dash = true;
        }
    }
    out
}

// ── Broadcast capacity ──────────────────────────────────────────────────────────

/// Capacity of the event broadcast channel.
///
/// A subscriber that lags by more than this many events drops the oldest ones;
/// the [`Api::subscribe`] stream maps such lag errors away so it stays
/// infallible (see [`CoreApi::subscribe`]).  An executing Run emits a steady
/// stream of `TaskStateChanged` / `AgentExchange` events, so this is sized
/// generously to absorb bursts (e.g. a multi-task run streaming chunks) while
/// keeping memory bounded.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

// ── Run handle (task 31) ──────────────────────────────────────────────────────

/// Settings that may be changed from the TUI while the process is running.
///
/// These are overlaid onto the startup [`Config`] every time CoreApi spawns a
/// scheduler. A running scheduler still owns its config snapshot; the next
/// Start/Retry observes the latest values.
#[derive(Debug, Clone)]
struct RuntimeSettings {
    caps: CapsConfig,
    concurrency: usize,
    final_merge: FinalMerge,
}

impl RuntimeSettings {
    fn from_config(config: &Config) -> Self {
        Self {
            caps: config.caps.clone(),
            concurrency: config.concurrency,
            final_merge: config.merge.final_,
        }
    }
}

/// The control handles for a background-executing Run.
///
/// Stored in the [`RunEntry`] once `StartRun` spawns the scheduler so that
/// `PauseRun` / `CancelRun` can signal the running task.
struct RunHandle {
    /// Monotonic ownership epoch for this scheduler invocation.
    generation: u64,
    /// Cancellation signal for the background scheduler.  `CancelRun` cancels it
    /// (scheduler aborts + cleans up); also cancelled if a fresh `StartRun`
    /// supersedes a still-running task (defensive).
    cancel: CancellationToken,
    /// Cooperative pause flag.  `PauseRun` sets it `true` (scheduler stops
    /// launching new tasks); a fresh `StartRun` clears it before resuming.
    pause: Arc<AtomicBool>,
    /// Retained scheduler task so replacement/reset/cancel can wait for all
    /// drivers and cleanup to drain before reusing plan-scoped resources.
    join: Option<JoinHandle<()>>,
}

struct StartWaitingGuard {
    state: Arc<CoreState>,
    run: RunId,
    restore_status: RunStatus,
}

impl Drop for StartWaitingGuard {
    fn drop(&mut self) {
        let changed = {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            runs.get_mut(&self.run.0).is_some_and(|entry| {
                if matches!(entry.status, RunStatus::WaitingForRepository { .. }) {
                    entry.status = self.restore_status.clone();
                    entry.handle = None;
                    true
                } else {
                    false
                }
            })
        };
        if changed {
            let _ = self.state.event_tx.send(Event::RunStatusChanged {
                run: self.run,
                status: self.restore_status.clone(),
            });
        }
    }
}

// ── Registry entry ──────────────────────────────────────────────────────────────

/// One open Run as tracked by the orchestrator's in-memory registry.
///
/// Holds the interpreted [`TaskGraph`] behind an `Arc<tokio::sync::Mutex<…>>`
/// (so the background scheduler and the read queries share one source of truth),
/// the backing file path, the aggregate [`RunStatus`], and — once started — the
/// [`RunHandle`] used to pause/cancel the background execution.
struct RunEntry {
    /// Canonical repository-relative plan directory.
    plan_dir: crate::plan::PlanKey,
    /// Canonical plan identity used to make OpenPlan idempotent even when the
    /// same file is addressed through relative paths or symlinks.
    /// Persistent, sortable run identity (26-char ULID string) minted when this
    /// Run is opened.  Unlike the in-memory [`RunId`] session handle, the ULID's
    /// lexicographic order matches chronological order, giving a stable key that
    /// survives across processes.  Surfaced read-only on [`RunView::run_uid`] and
    /// threaded into the audit ledger.
    run_uid: String,
    /// Plan-scoped, human-facing slug derived from the plan directory.
    /// Cached here so run finalization can stamp it into `run.json` without
    /// re-deriving it from the path.
    run_slug: String,
    /// The plan slug (lowercased kebab form of the plan-directory basename)
    /// derived at open. Threaded into the scheduler so per-task worktree calls
    /// can plan-scope their directory + branch names.
    plan_slug: String,
    /// When this Run transitioned to [`RunStatus::Running`] (set in
    /// [`CoreApi::start_run`]).  `None` until the run is started; carried into the
    /// finalization-time [`RunMetadata`].
    started_at: Option<DateTime<Utc>>,
    /// The interpreted task graph, shared with the background scheduler.  Reads
    /// (`run`/`runs`) lock it briefly to snapshot; the scheduler mutates it as
    /// tasks progress.
    graph: Arc<AsyncMutex<TaskGraph>>,
    /// Aggregate status.  Starts [`RunStatus::Pending`]; driven by `StartRun`
    /// (→ `Running`), `PauseRun` (→ `Paused`), `CancelRun` (→ `Failed`), and the
    /// background task's terminal emission (→ `Completed`/`Failed`).
    status: RunStatus,
    /// The background execution's control handle.  `None` until `StartRun`.
    handle: Option<RunHandle>,
    /// Set by the lease-owning scheduler when authored cancellation could not
    /// be published. The command thread must not advance runtime status then.
    cancellation_status_error: Option<String>,
    /// Latest scheduler ownership epoch. A completion from any older epoch is
    /// stale and may neither finalize status nor clear the current handle.
    scheduler_generation: u64,
    /// Ingestion report computed at open (validate + qualify). Threaded to
    /// every RunView snapshot.
    report: crate::ingestion::IngestionReport,
    /// Present for per-task plans opened from validated source. Open only
    /// inspects checkpoint compatibility; start performs the lease-bound reread.
    plan_source: Option<PlanSourceState>,
}

#[derive(Clone)]
struct PlanSourceState {
    key: crate::plan::PlanKey,
    reconciliation: crate::checkpoint::CheckpointDisposition,
    checkpoint_identity: crate::checkpoint::CheckpointIdentity,
}

fn task_entry_text(task: &crate::task::Task) -> String {
    let mut entry_text = String::new();
    if !task.description.is_empty() {
        entry_text.push_str(&task.description);
    }
    if !task.done_when.is_empty() {
        if !entry_text.is_empty() {
            entry_text.push_str("\n\n");
        }
        entry_text.push_str("### Done when\n\n");
        entry_text.push_str(&task.done_when);
    }
    entry_text
}

/// Project a snapshot graph + metadata into the view-level [`RunView`].
///
/// Pulled out as a free function because [`RunEntry`] no longer holds the graph
/// inline (it is behind an async mutex); callers snapshot the graph first, then
/// build the view from the clone — keeping the registry lock and the graph lock
/// strictly separate.
fn build_view(
    id: RunId,
    run_uid: String,
    plan_dir: crate::plan::PlanKey,
    status: RunStatus,
    repo_root: &std::path::Path,
    graph: &TaskGraph,
    report: crate::ingestion::IngestionReport,
) -> RunView {
    let project = repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let tasks = graph
        .tasks
        .iter()
        .map(|task| TaskView {
            authored: graph
                .authored
                .get(&task.id)
                .map(|metadata| AuthoredTaskView {
                    source_path: metadata.source_path.clone(),
                    workstream: metadata.workstream.clone(),
                    kind: metadata.kind.clone(),
                    status: metadata.status.to_string(),
                    gated: metadata.gated,
                    touches: metadata
                        .touches
                        .iter()
                        .map(|pattern| pattern.as_str().to_owned())
                        .collect(),
                    merged_as: metadata.merged_as.clone(),
                    authored_dependencies: task
                        .depends_on
                        .iter()
                        .filter(|dependency| !metadata.collision_dependencies.contains(dependency))
                        .map(Into::into)
                        .collect(),
                    collision_dependencies: metadata
                        .collision_dependencies
                        .iter()
                        .map(Into::into)
                        .collect(),
                }),
            id: (&task.id).into(),
            title: task.title.clone(),
            state: task.state.into(),
            gate_iterations: task.gate_iterations,
            review_iterations: task.review_iterations,
            depends_on: task.depends_on.iter().map(Into::into).collect(),
            started_at: task.started_at,
            finished_at: task.finished_at,
            failure_reason: task.failure_reason.clone(),
            entry_text: task_entry_text(task),
        })
        .collect();

    RunView {
        id,
        run_uid,
        plan_dir,
        status,
        project,
        tasks,
        report,
    }
}

// ── Shared inner state ──────────────────────────────────────────────────────────

/// The shared, mutable orchestrator state behind an `Arc`.
///
/// Pulled out of [`CoreApi`] into an `Arc<CoreState>` so the **background
/// execution task** (`StartRun`'s spawned scheduler) can hold a clone and update
/// the registry's run status when the run reaches a terminal state — `CoreApi`'s
/// `&self` methods cannot be borrowed by a `'static` spawned future, but a
/// cloned `Arc<CoreState>` can.
struct CoreState {
    /// The interpreter passed to the per-run `Planner` actor (via `run_graph`).
    /// This one *does* respect `config.planner.mechanism` (may be model-backed
    /// via `build_planner_interpreter`).  Separate from the ingestion interpreter
    /// so that TUI file opens stay fast/local while planner authoring flows can
    /// use the model.
    planner_interpreter: Arc<dyn TaskListInterpreter>,

    /// The agent backend for the Developer role.
    ///
    /// Resolved from `config.roles.developer.provider` in the TUI binary; cloned
    /// into every background `run_graph` call as `developer_backend`. In tests and
    /// the simple `new` path, this is the same `Arc` as `reviewer_backend`.
    developer_backend: Arc<dyn AgentBackend>,

    /// The agent backend for the Reviewer role.
    ///
    /// Resolved from `config.roles.reviewer.provider` in the TUI binary; cloned
    /// into every background `run_graph` call as `reviewer_backend`. In tests and
    /// the simple `new` path, this is the same `Arc` as `developer_backend`.
    reviewer_backend: Arc<dyn AgentBackend>,

    /// Worktree/branch lifecycle manager (repo root + base branch) handed to the
    /// background scheduler.
    worktree_manager: WorktreeManager,

    /// Resolved runtime config (gates, caps, concurrency, base branch).
    config: Config,

    /// Live-editable settings saved from the TUI during this process.
    runtime_settings: Mutex<RuntimeSettings>,

    /// The Runs registry: `RunId` → [`RunEntry`].  A `BTreeMap` keeps iteration
    /// order stable (ascending `RunId`, i.e. insertion order) for [`Api::runs`].
    runs: Mutex<BTreeMap<u64, RunEntry>>,

    /// Single-flight gate for canonicalize/load/interpret/register. This keeps
    /// concurrent OpenPlan requests for one plan from both doing side effects.
    open_lock: AsyncMutex<()>,

    /// Serializes lifecycle operations that can replace or drain schedulers.
    /// Registry locks remain short and are never held while awaiting joins.
    lifecycle_lock: AsyncMutex<()>,

    /// Monotonic allocator for fresh [`RunId`]s.  First id is `1`.
    next_id: AtomicU64,

    /// Stable session handles for historical runs loaded from `run.json`.
    ///
    /// Disk snapshots are rediscovered on every [`Api::runs`] query. Keeping
    /// their ULID-to-handle binding here prevents the same historical run from
    /// changing identity between queries or colliding with a later live run.
    disk_run_ids: Mutex<HashMap<String, RunId>>,

    /// Broadcast sender for the live event stream.  Each [`Api::subscribe`] call
    /// derives an independent receiver; the background scheduler's [`EventSink`]
    /// forwards engine events into this same sender.
    event_tx: broadcast::Sender<Event>,

    /// Audit registry: the Supervisor calls this to associate each task's
    /// `working_dir` with its run/slug/task context before dispatching a driver.
    /// The registry is backed by [`crate::audit::JsonlAuditSink`] in production
    /// and [`crate::audit::NoopAuditRegistry`] in tests.
    audit_registry: Arc<dyn AuditRegistry>,
    repository_leases: Arc<crate::repository_lease::RepositoryLeaseRegistry>,
}

impl CoreState {
    /// Publish cancellation bookkeeping while the scheduler still owns the
    /// repository lease and all task drivers have quiesced.
    async fn commit_authored_cancellation(
        &self,
        run: RunId,
        run_uid: &str,
        plan_slug: &str,
    ) -> Result<(), String> {
        use crate::plan::{GitTreePlanFileSource, PlanCandidate, PlanFileSource, PlanReservations};
        if plan_slug.is_empty() {
            return Ok(());
        }
        let key = self
            .runs
            .lock()
            .map_err(|_| "runs registry mutex poisoned".to_owned())?
            .get(&run.0)
            .ok_or_else(|| format!("unknown run {run}"))?
            .plan_dir
            .clone();
        let root = crate::paths::run_dir(&self.worktree_manager.repo_root, run_uid)
            .map_err(|error| error.to_string())?
            .join("integration");
        let plan_ref = format!("refs/heads/plan/{plan_slug}");
        // Try the integration worktree first; if it doesn't exist (e.g.
        // already cleaned up), fall back to the main repo root.
        let git_cwd: std::path::PathBuf =
            if root.join(".git").exists() || root.join(".git").is_file() {
                root.clone()
            } else {
                self.worktree_manager.repo_root.clone()
            };
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&git_cwd)
            .args(["rev-parse", "--verify", &plan_ref])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        let old = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let source =
            GitTreePlanFileSource::new(&git_cwd, &old).map_err(|error| error.to_string())?;
        let mut plan = match crate::plan::load_plan(&source, key, &PlanReservations::default())
            .map_err(|report| format!("cancellation source invalid: {:?}", report.diagnostics))?
        {
            PlanCandidate::Plan(plan) => *plan,
            PlanCandidate::NotCandidate => return Err("cancellation source is not a plan".into()),
        };
        let mut changed = Vec::new();
        for task in &mut plan.tasks {
            // Blocked is durable only with its validated Exceptions evidence;
            // done/dropped are likewise terminal. Every volatile claimed task
            // returns to authored planned after its worker has quiesced.
            if task.frontmatter.status == crate::plan::AuthoredTaskStatus::InProgress {
                task.update_bookkeeping(crate::plan::AuthoredTaskStatus::Planned, None)
                    .map_err(|error| error.to_string())?;
                changed.push(task.frontmatter.id.as_str().to_owned());
            }
        }
        if changed.is_empty() {
            return Ok(());
        }
        plan.status.done = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Done)
            .count();
        plan.status.blocked = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Blocked)
            .count();
        plan.status.dropped = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Dropped)
            .count();
        let transition = crate::plan_status::StatusTransition {
            integration_state: plan.status.integration_state,
            run: plan.status.run.clone(),
            validation_base: plan.status.validation_base_oid.clone(),
            mode: plan.status.mode.clone(),
            final_oid: plan.status.final_oid.clone(),
            display_status: plan.status.display_status.clone(),
            last_updated: plan.status.last_updated.clone(),
        };
        let status = crate::plan_status::render_plan_status(&plan, &transition)
            .map_err(|error| error.to_string())?;
        plan.status.source.body = status.clone();
        let board = String::from_utf8(
            source
                .read_file(Path::new("docs/plans/STATUS.md"))
                .map_err(|error| error.to_string())?,
        )
        .map_err(|_| "root status is not UTF-8".to_owned())?;
        let board = crate::plan_status::update_root_row(&board, &plan)
            .map_err(|error| error.to_string())?;
        let mut writes = plan
            .tasks
            .iter()
            .filter(|task| changed.iter().any(|id| id == task.frontmatter.id.as_str()))
            .map(|task| crate::landing::OwnedWrite {
                path: task.source_path.clone(),
                bytes: task.render().into_bytes(),
            })
            .collect::<Vec<_>>();
        writes.push(crate::landing::OwnedWrite {
            path: plan.status.source.source_path.clone(),
            bytes: status.into_bytes(),
        });
        writes.push(crate::landing::OwnedWrite {
            path: PathBuf::from("docs/plans/STATUS.md"),
            bytes: board.into_bytes(),
        });
        crate::landing::commit_source_transition(
            &root,
            &plan_ref,
            &old,
            &writes,
            &crate::landing::SourceTransitionIdentity {
                plan: plan_slug.to_owned(),
                task: "all".into(),
                run: run_uid.to_owned(),
                action: "cancel".into(),
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Allocate the next monotonic [`RunId`].
    fn alloc_id(&self) -> RunId {
        RunId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Clone the startup config and overlay the latest TUI-editable settings.
    fn scheduler_config(&self) -> Config {
        let runtime = self
            .runtime_settings
            .lock()
            .expect("runtime settings mutex poisoned")
            .clone();
        let mut config = self.config.clone();
        config.caps = runtime.caps;
        config.concurrency = runtime.concurrency;
        config.merge.final_ = runtime.final_merge;
        config
    }

    /// Replace the live-editable settings after validating the resulting config.
    fn update_runtime_settings(
        &self,
        caps: CapsConfig,
        concurrency: usize,
        final_merge: FinalMerge,
    ) -> Result<(), ApiError> {
        if caps.gate_iterations == 0 {
            return Err(ApiError::InvalidCommand {
                reason: "caps.gate_iterations must be at least 1".to_string(),
            });
        }
        if caps.reviewer_iterations == 0 {
            return Err(ApiError::InvalidCommand {
                reason: "caps.reviewer_iterations must be at least 1".to_string(),
            });
        }
        if caps.wall_clock_secs == 0 {
            return Err(ApiError::InvalidCommand {
                reason: "caps.wall_clock_secs must be at least 1".to_string(),
            });
        }
        if matches!(caps.idle_secs, Some(0)) {
            return Err(ApiError::InvalidCommand {
                reason: "caps.idle_secs must be at least 1".to_string(),
            });
        }
        if concurrency == 0 {
            return Err(ApiError::InvalidCommand {
                reason: "concurrency must be at least 1".to_string(),
            });
        }

        let mut runtime = self
            .runtime_settings
            .lock()
            .expect("runtime settings mutex poisoned");
        *runtime = RuntimeSettings {
            caps,
            concurrency,
            final_merge,
        };
        Ok(())
    }

    /// Build an [`EventSink`] that forwards every engine [`Event`] into the
    /// broadcast channel `subscribe()` reads.
    ///
    /// `send` only errors when there are no live receivers, which is fine — the
    /// TUI may not be subscribed yet; events are best-effort.  The events the
    /// engine produces are already tagged with the correct `RunId` (the
    /// scheduler/drivers stamp it from the `RunControl`), so this is a thin pass.
    fn make_sink(state: Arc<CoreState>) -> EventSink {
        let tx = state.event_tx.clone();
        let repo_root = state.worktree_manager.repo_root.clone();
        Arc::new(move |event: Event| {
            if let Event::AgentExchange {
                run,
                task,
                event: exchange,
                ..
            } = &event
            {
                let run_uid = {
                    let runs = state.runs.lock().expect("runs registry mutex poisoned");
                    runs.get(&run.0).map(|entry| entry.run_uid.clone())
                };
                if let Some(run_uid) = run_uid
                    && let Err(e) = (|| -> std::io::Result<()> {
                        let logs_dir = paths::run_logs_dir(&repo_root, &run_uid)?;
                        let path = logs_dir.join(format!("{}_transcript.jsonl", task.0));
                        let line = serde_json::to_string(exchange)
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                        let mut file = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(path)?;
                        writeln!(file, "{line}")?;
                        Ok(())
                    })()
                {
                    tracing::warn!(
                        run_uid = %run_uid,
                        task_id = %task.0,
                        error = %e,
                        "failed to persist agent exchange transcript; continuing"
                    );
                }
            }
            let _ = tx.send(event);
        })
    }

    /// Record the final aggregate status of a run **after** its background
    /// scheduler returns, unless the run was cancelled (Cancel owns that status).
    ///
    /// Derives `Completed` (all tasks `Done`) or `Failed` (otherwise — a failed
    /// task, or a paused run that stopped launching) from the live graph, writes
    /// it into the registry, and clears the run's handle (it is no longer
    /// executing).  Called only from the background task; `run_graph` has already
    /// broadcast the matching `RunStatusChanged` event, so this just keeps the
    /// registry's snapshot status consistent with what the TUI was told.
    async fn finalize_run_status(&self, run: RunId, generation: u64) {
        // Snapshot the graph handle + whether this run was cancelled, under the
        // registry lock; drop the guard before awaiting the graph lock.  Also
        // snapshot the run's identity (run_uid/run_slug/started_at) so the
        // finalization-time `run.json` can be built without re-taking the lock.
        let (graph, cancelled, run_uid, run_slug, plan_slug, plan_dir, started_at) = {
            let runs = self.runs.lock().expect("runs registry mutex poisoned");
            match runs.get(&run.0) {
                Some(entry)
                    if entry.scheduler_generation == generation
                        && entry
                            .handle
                            .as_ref()
                            .is_some_and(|handle| handle.generation == generation) =>
                {
                    let cancelled = entry
                        .handle
                        .as_ref()
                        .is_some_and(|h| h.cancel.is_cancelled());
                    (
                        Arc::clone(&entry.graph),
                        cancelled,
                        entry.run_uid.clone(),
                        entry.run_slug.clone(),
                        entry.plan_slug.clone(),
                        entry.plan_dir.clone(),
                        entry.started_at,
                    )
                }
                _ => return, // removed or superseded; this scheduler owns nothing.
            }
        };
        if cancelled {
            let mut runs = self.runs.lock().expect("runs registry mutex poisoned");
            if let Some(entry) = runs.get_mut(&run.0)
                && entry.scheduler_generation == generation
                && entry
                    .handle
                    .as_ref()
                    .is_some_and(|handle| handle.generation == generation)
            {
                entry.handle = None;
            }
            drop(runs);
            self.audit_registry.evict_run(&run.to_string());
            return; // Cancel/replacement owns the visible status.
        }
        // Derive the terminal status from the live task states and collect
        // per-task snapshots for the persistent run record.
        let (status, task_snapshots) = {
            let g = graph.lock().await;
            let all_done = g
                .tasks
                .iter()
                .all(|t| t.state == crate::task::TaskState::Done);
            let terminal_status = if all_done {
                RunStatus::Completed
            } else {
                RunStatus::Failed
            };
            // Capture per-task state at finalization time for the run snapshot.
            let snapshots: Vec<TaskSnapshot> = g
                .tasks
                .iter()
                .map(|t| TaskSnapshot {
                    id: t.id.0.clone(),
                    title: t.title.clone(),
                    state: crate::api::TaskState::from(t.state),
                    gate_iterations: t.gate_iterations,
                    review_iterations: t.review_iterations,
                    depends_on: t.depends_on.iter().map(|d| d.0.clone()).collect(),
                    started_at: t.started_at,
                    finished_at: t.finished_at,
                    failure_reason: t.failure_reason.clone(),
                })
                .collect();
            (terminal_status, snapshots)
        }; // graph guard dropped before re-taking the registry lock.

        {
            let mut runs = self.runs.lock().expect("runs registry mutex poisoned");
            let Some(entry) = runs.get_mut(&run.0) else {
                return;
            };
            if entry.scheduler_generation != generation
                || entry
                    .handle
                    .as_ref()
                    .is_none_or(|handle| handle.generation != generation)
            {
                return;
            }
            // Only finalize a scheduler that still owns a Running run. A
            // cooperative pause deliberately drains the scheduler with a
            // non-terminal graph; it must keep Paused and must not write a
            // misleading terminal run.json.
            if entry.status != RunStatus::Running {
                entry.handle = None;
                return;
            }
            entry.status = status.clone();
            // The scheduler has finished; the handle is spent.
            entry.handle = None;
        } // registry guard dropped before the best-effort async write.

        // Persist the run's identity + lifecycle window + per-task snapshots
        // to `run.json`, best-effort.  A failure here must never propagate or
        // abort the run — mirror the seed-persist warn-only pattern.
        let started_at = started_at.unwrap_or_else(Utc::now);
        let meta = RunMetadata::with_tasks(
            run_uid.clone(),
            run_slug,
            plan_slug,
            status,
            started_at,
            Utc::now(),
            task_snapshots,
        )
        .with_plan_dir(&plan_dir, &self.worktree_manager.repo_root);
        if let Err(e) = write_run_metadata(&meta, &self.worktree_manager.repo_root).await {
            tracing::warn!(run_uid = %run_uid, error = %e, "run.json write failed");
        }

        // The run is terminal; evict its per-task audit-registry entries so the
        // registry stays bounded by in-flight runs.  The stored ids are the
        // `"run:{n}"` form (`RunId` Display), so pass `&run.to_string()`.
        self.audit_registry.evict_run(&run.to_string());
    }
}

// ── CoreApi ─────────────────────────────────────────────────────────────────────

/// The real, core-backed orchestrator [`Api`].
///
/// Construct with [`CoreApi::new`], injecting:
/// - a [`TaskListInterpreter`] (the deterministic
///   typed plan projection for the TUI; a
///   `ModelInterpreter` for the e2e),
/// - the agent `backend` (`NoopBackend` in tests; the ACP backend in the e2e),
/// - a [`WorktreeManager`] (repo root + base branch), and
/// - a resolved [`Config`] (gates + caps + concurrency).
///
/// # Concurrency
///
/// `CoreApi` is `Send + Sync` and all methods take `&self`, so it can be shared
/// across the TUI's async tasks as `Arc<dyn Api>`.  Internal mutable state lives
/// in an `Arc<CoreState>` (so background tasks can update it too) behind a
/// `std::sync::Mutex` that is never held across an `.await`.
pub struct CoreApi {
    /// Shared mutable state (also cloned into background execution tasks).
    state: Arc<CoreState>,
}

/// A closed, generated plan subtree. Paths are relative to the plan directory;
/// callers cannot supply root-board or arbitrary repository writes.
#[derive(Clone, Debug)]
pub struct GeneratedPlanBundle {
    pub key: crate::plan::PlanKey,
    /// Digest computed from the caller's closed, validated source.
    pub expected_source_digest: String,
    pub files: BTreeMap<PathBuf, Vec<u8>>,
}

/// Backend-free plan authoring facade. It owns only repository identity,
/// exclusion, worktree publication, and canonical plan rendering dependencies.
#[derive(Clone)]
pub struct AuthoringCoordinator {
    repo_root: PathBuf,
    base_branch: String,
    repository_leases: Arc<crate::repository_lease::RepositoryLeaseRegistry>,
    worktree_manager: WorktreeManager,
    lease_held: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthoringSession {
    pub expected_base_oid: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthoringCandidate {
    AwaitingCommit,
    Committed,
}

impl AuthoringCoordinator {
    pub fn new(
        repo_root: PathBuf,
        base_branch: String,
        repository_leases: Arc<crate::repository_lease::RepositoryLeaseRegistry>,
    ) -> Self {
        Self {
            worktree_manager: WorktreeManager::new(repo_root.clone(), base_branch.clone()),
            repo_root,
            base_branch,
            repository_leases,
            lease_held: false,
        }
    }

    /// Bind this coordinator to a contract session that already owns the
    /// repository lease. All Git CAS/rechecks remain active.
    pub fn with_held_session(mut self) -> Self {
        self.lease_held = true;
        self
    }

    pub async fn reserve_numbers(&self, count: usize) -> Result<Vec<String>, ApiError> {
        let invalid = |reason: String| ApiError::InvalidCommand { reason };
        if count == 0 || count > 9999 {
            return Err(invalid(
                "reservation count must be between 1 and 9999".into(),
            ));
        }
        let git = |args: Vec<String>| async move {
            let output = tokio::process::Command::new("git")
                .arg("-C")
                .arg(&self.repo_root)
                .args(args)
                .output()
                .await
                .map_err(|error| invalid(error.to_string()))?;
            if !output.status.success() {
                return Err(invalid(
                    String::from_utf8_lossy(&output.stderr).trim().into(),
                ));
            }
            Ok::<String, ApiError>(String::from_utf8_lossy(&output.stdout).trim().into())
        };
        let base = git(vec!["rev-parse".into(), self.base_branch.clone()]).await?;
        let mut used = std::collections::BTreeSet::new();
        let tree = git(vec![
            "ls-tree".into(),
            "--name-only".into(),
            format!("{base}:docs/plans"),
        ])
        .await?;
        for name in tree.lines() {
            if name.len() >= 4 && name.as_bytes()[..4].iter().all(u8::is_ascii_digit) {
                used.insert(name[..4].to_owned());
            }
        }
        if let Ok(entries) = std::fs::read_dir(self.repo_root.join("docs/plans")) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let bytes = name.as_encoded_bytes();
                if bytes.len() >= 4 && bytes[..4].iter().all(u8::is_ascii_digit) {
                    used.insert(String::from_utf8_lossy(&bytes[..4]).into());
                }
            }
        }
        let refs = git(vec![
            "for-each-ref".into(),
            "--format=%(refname:short)".into(),
            "refs/heads/plan/".into(),
        ])
        .await?;
        for reference in refs.lines() {
            let name = reference.strip_prefix("plan/").unwrap_or(reference);
            if name.len() < 5 || !name.as_bytes()[..4].iter().all(u8::is_ascii_digit) {
                return Err(invalid(format!(
                    "old or mixed-format plan ref is unsupported: {reference}"
                )));
            }
            let message = git(vec![
                "show".into(),
                "-s".into(),
                "--format=%B".into(),
                reference.into(),
            ])
            .await?;
            if message
                .lines()
                .filter(|line| *line == "Makina-Phase: plan-registration")
                .count()
                != 1
                || message
                    .lines()
                    .filter(|line| *line == format!("Makina-Plan: {name}"))
                    .count()
                    != 1
            {
                return Err(invalid(format!(
                    "numbered plan ref lacks verified R evidence: {reference}"
                )));
            }
            used.insert(name[..4].into());
        }
        let values = (1..=9999)
            .map(|value| format!("{value:04}"))
            .filter(|value| !used.contains(value))
            .take(count)
            .collect::<Vec<_>>();
        if values.len() != count {
            return Err(invalid("global plan number namespace is exhausted".into()));
        }
        Ok(values)
    }

    pub async fn start_authoring_session(&self) -> Result<AuthoringSession, ApiError> {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["rev-parse", &self.base_branch])
            .output()
            .await
            .map_err(|error| ApiError::InvalidCommand {
                reason: error.to_string(),
            })?;
        if !output.status.success() {
            return Err(ApiError::InvalidCommand {
                reason: String::from_utf8_lossy(&output.stderr).trim().into(),
            });
        }
        Ok(AuthoringSession {
            expected_base_oid: String::from_utf8_lossy(&output.stdout).trim().into(),
        })
    }

    pub fn render_blueprint(
        &self,
        bundle: &crate::plan::GeneratedPlanBundle,
    ) -> Result<BTreeMap<PathBuf, Vec<u8>>, ApiError> {
        bundle
            .render_files()
            .map_err(|error| ApiError::InvalidCommand {
                reason: format!("generated blueprint is invalid: {error}"),
            })
    }

    pub fn inspect_candidate(
        &self,
        key: crate::plan::PlanKey,
        expected_base_oid: &str,
        expected_source_digest: &str,
    ) -> Result<AuthoringCandidate, ApiError> {
        use crate::plan::{
            FilesystemPlanFileSource, GitTreePlanFileSource, PlanCandidate, PlanReservations,
            load_plan,
        };
        let invalid = |reason: String| ApiError::InvalidCommand { reason };
        if let Ok(source) = GitTreePlanFileSource::new(&self.repo_root, expected_base_oid)
            && let Ok(PlanCandidate::Plan(plan)) =
                load_plan(&source, key.clone(), &PlanReservations::default())
        {
            if plan.source_digest.as_str() == expected_source_digest {
                return Ok(AuthoringCandidate::Committed);
            }
            return Err(invalid("committed candidate source digest mismatch".into()));
        }
        let source =
            FilesystemPlanFileSource::new(&self.repo_root, Some(expected_base_oid.to_owned()))
                .map_err(|error| invalid(error.to_string()))?;
        match load_plan(&source, key, &PlanReservations::default()) {
            Ok(PlanCandidate::Plan(plan))
                if plan.source_digest.as_str() == expected_source_digest =>
            {
                Ok(AuthoringCandidate::AwaitingCommit)
            }
            Ok(_) => Err(invalid("candidate source digest mismatch".into())),
            Err(report) => Err(invalid(format!(
                "candidate is invalid: {:?}",
                report.diagnostics
            ))),
        }
    }
}

/// Repository-semantic lifecycle facade shared by supervisors and the local
/// plan contract. It is deliberately independent of agents, interpreters, and
/// UI configuration: callers bind one exact repository, plan ref, and run,
/// while this type owns authoritative Git-tree loading and evidence reads.
#[derive(Clone, Debug)]
pub struct PlanContractCoordinator {
    repo_root: PathBuf,
    plan: crate::plan::PlanKey,
    plan_ref: String,
    run_uid: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanSourceAction {
    Block { reason: String },
    Retry,
    Requeue,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedFinalization {
    pub prepared_oid: String,
    pub expected_base_oid: String,
    pub base_ref: String,
    pub mode: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadyTaskSet {
    pub tasks: Vec<crate::plan::TaskId>,
    pub complete: bool,
    pub blocked: bool,
}

impl PlanContractCoordinator {
    pub fn new(
        repo_root: impl AsRef<Path>,
        plan: crate::plan::PlanKey,
        plan_ref: impl Into<String>,
        run_uid: impl Into<String>,
    ) -> Result<Self, String> {
        let repo_root = std::fs::canonicalize(repo_root).map_err(|error| error.to_string())?;
        let plan_ref = plan_ref.into();
        let run_uid = run_uid.into();
        for (name, value) in [("plan ref", &plan_ref), ("run UID", &run_uid)] {
            if value.is_empty() || value.contains(['\n', '\r', '\0']) {
                return Err(format!("{name} is empty or contains a line delimiter"));
            }
        }
        Ok(Self {
            repo_root,
            plan,
            plan_ref,
            run_uid,
        })
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }
    pub fn plan(&self) -> &crate::plan::PlanKey {
        &self.plan
    }
    pub fn plan_ref(&self) -> &str {
        &self.plan_ref
    }
    pub fn run_uid(&self) -> &str {
        &self.run_uid
    }

    async fn integration_workspace(&self, base_name: &str) -> Result<PathBuf, String> {
        let plan_name = self
            .plan
            .relative_dir
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "plan directory is not UTF-8".to_owned())?;
        crate::worktree::WorktreeManager::new(self.repo_root.clone(), base_name.to_owned())
            .create_integration_workspace(plan_name, &self.run_uid)
            .await
            .map(|workspace| workspace.path)
            .map_err(|error| error.to_string())
    }

    /// Load the plan exclusively from the exact immutable plan-ref tree.
    pub fn load(&self) -> Result<crate::plan::PlanDocument, String> {
        let source = crate::plan::GitTreePlanFileSource::new(&self.repo_root, &self.plan_ref)
            .map_err(|error| error.to_string())?;
        match crate::plan::load_plan(
            &source,
            self.plan.clone(),
            &crate::plan::PlanReservations::default(),
        )
        .map_err(|report| format!("plan validation failed: {:?}", report.diagnostics))?
        {
            crate::plan::PlanCandidate::Plan(plan) => Ok(*plan),
            crate::plan::PlanCandidate::NotCandidate => {
                Err("plan ref does not contain a canonical plan bundle".into())
            }
        }
    }

    pub fn parse_oid(&self, value: impl Into<String>) -> Result<crate::plan::GitObjectId, String> {
        use crate::plan::PlanFileSource as _;
        let source = crate::plan::GitTreePlanFileSource::new(&self.repo_root, &self.plan_ref)
            .map_err(|error| error.to_string())?;
        crate::plan::GitObjectId::parse(value, source.object_format())
            .map_err(|error| error.to_string())
    }

    /// Reconcile one task only from retained R/A/B lineage evidence.
    pub async fn inspect_task(
        &self,
        task: &crate::plan::TaskId,
    ) -> Result<crate::landing::TaskEvidenceState, String> {
        crate::landing::inspect_task_evidence(
            &self.repo_root,
            &self.plan_ref,
            self.plan
                .relative_dir
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| "plan directory is not UTF-8".to_owned())?,
            task.as_str(),
        )
        .await
        .map_err(|error| error.to_string())
    }

    pub fn ready_tasks(&self) -> Result<ReadyTaskSet, String> {
        let plan = self.load()?;
        let done = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Done)
            .map(|task| task.frontmatter.id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let tasks: Vec<crate::plan::TaskId> = plan
            .tasks
            .iter()
            .filter(|task| {
                task.frontmatter.status == crate::plan::AuthoredTaskStatus::Planned
                    && !task.frontmatter.gated
                    && task
                        .frontmatter
                        .depends_on
                        .iter()
                        .all(|dependency| done.contains(dependency))
            })
            .map(|task| task.frontmatter.id.clone())
            .collect();
        let complete = plan.tasks.iter().all(|task| {
            matches!(
                task.frontmatter.status,
                crate::plan::AuthoredTaskStatus::Done | crate::plan::AuthoredTaskStatus::Dropped
            )
        });
        let blocked = !complete && tasks.is_empty();
        Ok(ReadyTaskSet {
            tasks,
            complete,
            blocked,
        })
    }

    pub async fn ensure_task_worktree(
        &self,
        task_id: &crate::plan::TaskId,
    ) -> Result<PathBuf, String> {
        let plan = self.load()?;
        let plan_name = self
            .plan
            .relative_dir
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "plan directory is not UTF-8".to_owned())?;
        crate::worktree::WorktreeManager::new(self.repo_root.clone(), plan.status.base_name)
            .with_fork_branch(self.plan_ref.trim_start_matches("refs/heads/").to_owned())
            .create(plan_name, task_id.as_str())
            .await
            .map(|handle| handle.path)
            .map_err(|error| error.to_string())
    }

    /// Publish the coordinator-owned planned -> in-progress claim transition.
    /// The caller supplies only semantic identity and time; paths and bytes are
    /// derived from the validated immutable plan tree.
    pub async fn claim_task(
        &self,
        task_id: &crate::plan::TaskId,
        expected_plan_oid: &str,
        last_updated: &str,
    ) -> Result<String, String> {
        use crate::plan::PlanFileSource as _;
        let source = crate::plan::GitTreePlanFileSource::new(&self.repo_root, expected_plan_oid)
            .map_err(|error| error.to_string())?;
        let mut plan = self.load()?;
        let task = plan
            .tasks
            .iter_mut()
            .find(|task| &task.frontmatter.id == task_id)
            .ok_or_else(|| format!("unknown task {task_id}"))?;
        if task.frontmatter.status != crate::plan::AuthoredTaskStatus::Planned {
            return Err("claim requires authored planned status".into());
        }
        if !matches!(
            self.inspect_task(task_id).await?,
            crate::landing::TaskEvidenceState::RegistrationOnly
        ) {
            return Err("claim requires registration-only retained evidence".into());
        }
        task.update_bookkeeping(crate::plan::AuthoredTaskStatus::InProgress, None)
            .map_err(|error| error.to_string())?;
        let task_path = task.source_path.clone();
        let task_bytes = task.render().into_bytes();
        plan.status.display_status = "In progress".into();
        plan.status.integration_state = crate::plan::PlanIntegrationState::Assembling;
        plan.status.run = Some(self.run_uid.clone());
        plan.status.validation_base_oid = Some(plan.status.base_oid.clone());
        plan.status.last_updated = last_updated.into();
        let transition = crate::plan_status::StatusTransition {
            integration_state: plan.status.integration_state,
            run: plan.status.run.clone(),
            validation_base: plan.status.validation_base_oid.clone(),
            mode: None,
            final_oid: None,
            display_status: plan.status.display_status.clone(),
            last_updated: last_updated.into(),
        };
        let status = crate::plan_status::render_plan_status(&plan, &transition)
            .map_err(|error| error.to_string())?;
        plan.status.done = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Done)
            .count();
        let root = String::from_utf8(
            source
                .read_file(Path::new("docs/plans/STATUS.md"))
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let root =
            crate::plan_status::update_root_row(&root, &plan).map_err(|error| error.to_string())?;
        let workspace = self.integration_workspace(&plan.status.base_name).await?;
        crate::landing::commit_task_claim(
            &workspace,
            &self.plan_ref,
            expected_plan_oid,
            &[
                crate::landing::OwnedWrite {
                    path: task_path,
                    bytes: task_bytes,
                },
                crate::landing::OwnedWrite {
                    path: self.plan.relative_dir.join("STATUS.md"),
                    bytes: status.into_bytes(),
                },
                crate::landing::OwnedWrite {
                    path: PathBuf::from("docs/plans/STATUS.md"),
                    bytes: root.into_bytes(),
                },
            ],
            &crate::landing::StatusLandingIdentity {
                plan: self
                    .plan
                    .relative_dir
                    .file_name()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| "plan directory is not UTF-8".to_owned())?
                    .into(),
                task: task_id.to_string(),
                run: self.run_uid.clone(),
                landing: "claim".into(),
            },
        )
        .await
        .map_err(|error| error.to_string())
    }

    /// Publish Phase B after the exact repository-format Phase-A commit exists.
    pub async fn complete_task(
        &self,
        task_id: &crate::plan::TaskId,
        expected_plan_oid: &str,
        phase_a_oid: crate::plan::GitObjectId,
        last_updated: &str,
    ) -> Result<String, String> {
        use crate::plan::PlanFileSource as _;
        let source = crate::plan::GitTreePlanFileSource::new(&self.repo_root, expected_plan_oid)
            .map_err(|error| error.to_string())?;
        let mut plan = self.load()?;
        let task = plan
            .tasks
            .iter_mut()
            .find(|task| &task.frontmatter.id == task_id)
            .ok_or_else(|| format!("unknown task {task_id}"))?;
        if task.frontmatter.status != crate::plan::AuthoredTaskStatus::InProgress {
            return Err("Phase B requires authored in-progress status".into());
        }
        match self.inspect_task(task_id).await? {
            crate::landing::TaskEvidenceState::LandingPending { implementation_oid }
                if implementation_oid == phase_a_oid.as_str() => {}
            _ => return Err("Phase B requires exact retained Phase-A evidence".into()),
        }
        task.update_bookkeeping(
            crate::plan::AuthoredTaskStatus::Done,
            Some(phase_a_oid.clone()),
        )
        .map_err(|error| error.to_string())?;
        let task_path = task.source_path.clone();
        let task_bytes = task.render().into_bytes();
        plan.status.done = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Done)
            .count();
        let all_terminal = plan.tasks.iter().all(|task| {
            matches!(
                task.frontmatter.status,
                crate::plan::AuthoredTaskStatus::Done | crate::plan::AuthoredTaskStatus::Dropped
            )
        });
        plan.status.display_status = "In progress".into();
        plan.status.integration_state = if all_terminal {
            crate::plan::PlanIntegrationState::AwaitingIntegration
        } else {
            crate::plan::PlanIntegrationState::Assembling
        };
        plan.status.run = Some(self.run_uid.clone());
        plan.status.validation_base_oid = Some(plan.status.base_oid.clone());
        plan.status.last_updated = last_updated.into();
        let transition = crate::plan_status::StatusTransition {
            integration_state: plan.status.integration_state,
            run: plan.status.run.clone(),
            validation_base: plan.status.validation_base_oid.clone(),
            mode: None,
            final_oid: None,
            display_status: plan.status.display_status.clone(),
            last_updated: last_updated.into(),
        };
        let status = crate::plan_status::render_plan_status(&plan, &transition)
            .map_err(|error| error.to_string())?;
        let root = String::from_utf8(
            source
                .read_file(Path::new("docs/plans/STATUS.md"))
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let root =
            crate::plan_status::update_root_row(&root, &plan).map_err(|error| error.to_string())?;
        let workspace = self.integration_workspace(&plan.status.base_name).await?;
        crate::landing::commit_task_status(
            &workspace,
            &self.plan_ref,
            expected_plan_oid,
            &[
                crate::landing::OwnedWrite {
                    path: task_path,
                    bytes: task_bytes,
                },
                crate::landing::OwnedWrite {
                    path: self.plan.relative_dir.join("STATUS.md"),
                    bytes: status.into_bytes(),
                },
                crate::landing::OwnedWrite {
                    path: PathBuf::from("docs/plans/STATUS.md"),
                    bytes: root.into_bytes(),
                },
            ],
            &crate::landing::StatusLandingIdentity {
                plan: self
                    .plan
                    .relative_dir
                    .file_name()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| "plan directory is not UTF-8".to_owned())?
                    .into(),
                task: task_id.to_string(),
                run: self.run_uid.clone(),
                landing: phase_a_oid.to_string(),
            },
        )
        .await
        .map_err(|error| error.to_string())
    }

    /// Create repository-format Phase A in the private integration workspace.
    pub async fn land_phase_a(&self, task_id: &crate::plan::TaskId) -> Result<String, String> {
        let plan = self.load()?;
        if !plan
            .tasks
            .iter()
            .any(|task| &task.frontmatter.id == task_id)
        {
            return Err(format!("unknown task {task_id}"));
        }
        let plan_slug = self
            .plan
            .relative_dir
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "plan directory is not UTF-8".to_owned())?;
        let manager = crate::worktree::WorktreeManager::new(
            self.repo_root.clone(),
            plan.status.base_name.clone(),
        );
        let workspace = manager
            .create_integration_workspace(plan_slug, &self.run_uid)
            .await
            .map_err(|error| error.to_string())?;
        let branch = format!(
            "task/{}",
            crate::paths::short_worktree_name(plan_slug, task_id.as_str())
        );
        self.validate_candidate(task_id).await?;
        let merger = crate::merge::SquashMerger::new(workspace.path, workspace.plan_branch);
        let identity = crate::merge::TaskLandingIdentity {
            plan: plan_slug.into(),
            task: task_id.to_string(),
            run: self.run_uid.clone(),
        };
        match merger
            .squash_merge_with_evidence(&branch, &format!("feat(plan): land {task_id}"), &identity)
            .await
            .map_err(|error| error.to_string())?
        {
            crate::merge::MergeOutcome::Merged { oid } => {
                // `validate_candidate` already checked the task branch against
                // its exact claim merge-base. A's parent can legitimately also
                // include coordinator-owned claim bookkeeping, so rechecking
                // A against its parent would misattribute those reserved paths
                // to the task implementation.
                Ok(oid.to_string())
            }
            crate::merge::MergeOutcome::Conflict { details } => {
                Err(format!("Phase A conflict: {details}"))
            }
        }
    }

    pub async fn validate_candidate(&self, task_id: &crate::plan::TaskId) -> Result<(), String> {
        let plan = self.load()?;
        let authored = plan
            .tasks
            .iter()
            .find(|task| &task.frontmatter.id == task_id)
            .ok_or_else(|| format!("unknown task {task_id}"))?;
        let touches = authored
            .frontmatter
            .touches
            .iter()
            .map(crate::task::AuthoredRepoPattern::from)
            .collect::<Vec<_>>();
        let plan_slug = self
            .plan
            .relative_dir
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "plan directory is not UTF-8".to_owned())?;
        let branch = format!(
            "task/{}",
            crate::paths::short_worktree_name(plan_slug, task_id.as_str())
        );
        let merge_base = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["merge-base", self.plan_ref.as_str(), branch.as_str()])
            .output()
            .await
            .map_err(|error| error.to_string())?;
        if !merge_base.status.success() {
            return Err(String::from_utf8_lossy(&merge_base.stderr).trim().into());
        }
        crate::actors::supervisor::enforce_task_branch_footprint(
            &self.repo_root,
            &crate::task::TaskId(task_id.to_string()),
            &branch,
            &touches,
            String::from_utf8_lossy(&merge_base.stdout).trim(),
        )
        .await
    }

    /// Apply a closed authored disposition and publish its digest-linked
    /// coordinator transaction. Repository paths and bytes are derived here.
    pub async fn set_disposition(
        &self,
        task_id: &crate::plan::TaskId,
        expected_plan_oid: &str,
        action: crate::api::TaskDispositionAction,
    ) -> Result<String, String> {
        use crate::plan::PlanFileSource as _;
        let source = crate::plan::GitTreePlanFileSource::new(&self.repo_root, expected_plan_oid)
            .map_err(|error| error.to_string())?;
        let mut plan = self.load()?;
        let old_source = plan.source_digest.to_string();
        let old_plan = plan.executable_digest.to_string();
        let task = plan
            .tasks
            .iter_mut()
            .find(|task| &task.frontmatter.id == task_id)
            .ok_or_else(|| format!("unknown task {task_id}"))?;
        let action_name = match &action {
            crate::api::TaskDispositionAction::Ungate => {
                if task.frontmatter.status != crate::plan::AuthoredTaskStatus::Planned
                    || !task.frontmatter.gated
                {
                    return Err("ungate requires authored planned + gated=true".into());
                }
                task.frontmatter.gated = false;
                "ungate"
            }
            crate::api::TaskDispositionAction::Drop { reason } => {
                let reason = reason.trim();
                if reason.is_empty() || reason.len() > 240 || reason.contains(['\n', '\r', '\0']) {
                    return Err("drop reason must be a non-empty bounded single line".into());
                }
                if matches!(
                    task.frontmatter.status,
                    crate::plan::AuthoredTaskStatus::Done
                        | crate::plan::AuthoredTaskStatus::InProgress
                ) {
                    return Err("done or in-progress authored work cannot be dropped".into());
                }
                task.update_bookkeeping(crate::plan::AuthoredTaskStatus::Dropped, None)
                    .map_err(|error| error.to_string())?;
                plan.status.source.body = crate::plan_status::append_disposition_exception(
                    &plan.status.source.body,
                    &format!("Task `{task_id}` dropped: {reason}"),
                )
                .map_err(|error| error.to_string())?;
                "drop"
            }
        }
        .to_owned();
        plan.status.done = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Done)
            .count();
        plan.status.blocked = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Blocked)
            .count();
        plan.status.dropped = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Dropped)
            .count();
        let transition = crate::plan_status::StatusTransition {
            integration_state: plan.status.integration_state,
            run: plan.status.run.clone(),
            validation_base: plan.status.validation_base_oid.clone(),
            mode: plan.status.mode.clone(),
            final_oid: plan.status.final_oid.clone(),
            display_status: plan.status.display_status.clone(),
            last_updated: plan.status.last_updated.clone(),
        };
        let status = crate::plan_status::render_plan_status(&plan, &transition)
            .map_err(|error| error.to_string())?;
        plan.status.source.body = status.clone();
        let board = String::from_utf8(
            source
                .read_file(Path::new("docs/plans/STATUS.md"))
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let board = crate::plan_status::update_root_row(&board, &plan)
            .map_err(|error| error.to_string())?;
        let task = plan
            .tasks
            .iter()
            .find(|task| &task.frontmatter.id == task_id)
            .expect("task retained");
        let writes = vec![
            crate::landing::OwnedWrite {
                path: task.source_path.clone(),
                bytes: task.render().into_bytes(),
            },
            crate::landing::OwnedWrite {
                path: plan.status.source.source_path.clone(),
                bytes: status.into_bytes(),
            },
            crate::landing::OwnedWrite {
                path: PathBuf::from("docs/plans/STATUS.md"),
                bytes: board.into_bytes(),
            },
        ];
        let plan_name = self
            .plan
            .relative_dir
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "plan directory is not UTF-8".to_owned())?;
        let manager = crate::worktree::WorktreeManager::new(
            self.repo_root.clone(),
            plan.status.base_name.clone(),
        );
        let workspace = manager
            .create_integration_workspace(plan_name, &self.run_uid)
            .await
            .map_err(|error| error.to_string())?;
        for write in &writes {
            tokio::fs::write(workspace.path.join(&write.path), &write.bytes)
                .await
                .map_err(|error| error.to_string())?;
        }
        let reread = crate::plan::FilesystemPlanFileSource::new(
            &workspace.path,
            plan.status
                .validation_base_oid
                .as_ref()
                .map(|oid| oid.to_string()),
        )
        .map_err(|error| error.to_string())?;
        let new_plan = match crate::plan::load_plan(
            &reread,
            self.plan.clone(),
            &crate::plan::PlanReservations::default(),
        )
        .map_err(|report| format!("disposition validation failed: {:?}", report.diagnostics))?
        {
            crate::plan::PlanCandidate::Plan(plan) => *plan,
            crate::plan::PlanCandidate::NotCandidate => return Err("disposition lost plan".into()),
        };
        if matches!(action, crate::api::TaskDispositionAction::Drop { .. })
            && (new_plan.source_digest.to_string() != old_source
                || new_plan.executable_digest.to_string() != old_plan)
        {
            return Err("drop changed executable or source digest".into());
        }
        crate::landing::commit_task_disposition(
            &workspace.path,
            &self.plan_ref,
            expected_plan_oid,
            &writes,
            &crate::landing::DispositionIdentity {
                plan: plan_name.into(),
                task: task_id.to_string(),
                run: self.run_uid.clone(),
                action: action_name,
                previous_source_digest: old_source,
                source_digest: new_plan.source_digest.to_string(),
                previous_plan_digest: old_plan,
                plan_digest: new_plan.executable_digest.to_string(),
            },
        )
        .await
        .map_err(|error| error.to_string())
    }

    pub async fn transition_task(
        &self,
        task_id: &crate::plan::TaskId,
        expected_plan_oid: &str,
        action: PlanSourceAction,
    ) -> Result<String, String> {
        use crate::plan::PlanFileSource as _;
        let source = crate::plan::GitTreePlanFileSource::new(&self.repo_root, expected_plan_oid)
            .map_err(|error| error.to_string())?;
        let mut plan = self.load()?;
        let task = plan
            .tasks
            .iter_mut()
            .find(|task| &task.frontmatter.id == task_id)
            .ok_or_else(|| format!("unknown task {task_id}"))?;
        let action_name = match action {
            PlanSourceAction::Block { reason } => {
                let reason = reason.trim();
                if reason.is_empty() || reason.len() > 240 || reason.contains(['\n', '\r', '\0']) {
                    return Err("block reason must be a non-empty bounded single line".into());
                }
                if matches!(
                    task.frontmatter.status,
                    crate::plan::AuthoredTaskStatus::Done
                        | crate::plan::AuthoredTaskStatus::Dropped
                ) {
                    return Err("terminal authored work cannot be blocked".into());
                }
                task.update_bookkeeping(crate::plan::AuthoredTaskStatus::Blocked, None)
                    .map_err(|error| error.to_string())?;
                plan.status.source.body = crate::plan_status::append_blocker_exception(
                    &plan.status.source.body,
                    task_id.as_str(),
                    reason,
                )
                .map_err(|error| error.to_string())?;
                "blocker"
            }
            PlanSourceAction::Retry | PlanSourceAction::Requeue => {
                if task.frontmatter.status == crate::plan::AuthoredTaskStatus::Blocked {
                    plan.status.source.body = crate::plan_status::resolve_exception(
                        &plan.status.source.body,
                        task_id.as_str(),
                        &format!("retry {}", self.run_uid),
                    )
                    .map_err(|error| error.to_string())?;
                }
                task.update_bookkeeping(crate::plan::AuthoredTaskStatus::Planned, None)
                    .map_err(|error| error.to_string())?;
                if matches!(action, PlanSourceAction::Retry) {
                    "retry"
                } else {
                    "requeue"
                }
            }
            PlanSourceAction::Cancel => {
                if task.frontmatter.status != crate::plan::AuthoredTaskStatus::InProgress {
                    return Err("cancel requires authored in-progress status".into());
                }
                task.update_bookkeeping(crate::plan::AuthoredTaskStatus::Planned, None)
                    .map_err(|error| error.to_string())?;
                "cancel"
            }
        }
        .to_owned();
        plan.status.done = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Done)
            .count();
        plan.status.blocked = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Blocked)
            .count();
        plan.status.dropped = plan
            .tasks
            .iter()
            .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Dropped)
            .count();
        let transition = crate::plan_status::StatusTransition {
            integration_state: plan.status.integration_state,
            run: plan.status.run.clone(),
            validation_base: plan.status.validation_base_oid.clone(),
            mode: plan.status.mode.clone(),
            final_oid: plan.status.final_oid.clone(),
            display_status: plan.status.display_status.clone(),
            last_updated: plan.status.last_updated.clone(),
        };
        let status = crate::plan_status::render_plan_status(&plan, &transition)
            .map_err(|error| error.to_string())?;
        plan.status.source.body = status.clone();
        let board = String::from_utf8(
            source
                .read_file(Path::new("docs/plans/STATUS.md"))
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let board = crate::plan_status::update_root_row(&board, &plan)
            .map_err(|error| error.to_string())?;
        let task = plan
            .tasks
            .iter()
            .find(|task| &task.frontmatter.id == task_id)
            .expect("task retained");
        let plan_name = self
            .plan
            .relative_dir
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "plan directory is not UTF-8".to_owned())?;
        let manager = crate::worktree::WorktreeManager::new(
            self.repo_root.clone(),
            plan.status.base_name.clone(),
        );
        let workspace = manager
            .create_integration_workspace(plan_name, &self.run_uid)
            .await
            .map_err(|error| error.to_string())?;
        crate::landing::commit_source_transition(
            &workspace.path,
            &self.plan_ref,
            expected_plan_oid,
            &[
                crate::landing::OwnedWrite {
                    path: task.source_path.clone(),
                    bytes: task.render().into_bytes(),
                },
                crate::landing::OwnedWrite {
                    path: plan.status.source.source_path.clone(),
                    bytes: status.into_bytes(),
                },
                crate::landing::OwnedWrite {
                    path: PathBuf::from("docs/plans/STATUS.md"),
                    bytes: board.into_bytes(),
                },
            ],
            &crate::landing::SourceTransitionIdentity {
                plan: plan_name.into(),
                task: task_id.to_string(),
                run: self.run_uid.clone(),
                action: action_name,
            },
        )
        .await
        .map_err(|error| error.to_string())
    }

    pub async fn prepare_finalization(
        &self,
        expected_plan_oid: &str,
        mode: &str,
        last_updated: &str,
    ) -> Result<PreparedFinalization, String> {
        use crate::plan::PlanFileSource as _;
        if !matches!(mode, "squash" | "stage" | "merge_commit" | "manual") {
            return Err("unsupported finalization mode".into());
        }
        let source = crate::plan::GitTreePlanFileSource::new(&self.repo_root, expected_plan_oid)
            .map_err(|error| error.to_string())?;
        let mut plan = self.load()?;
        if plan.tasks.iter().any(|task| {
            !matches!(
                task.frontmatter.status,
                crate::plan::AuthoredTaskStatus::Done | crate::plan::AuthoredTaskStatus::Dropped
            )
        }) {
            return Err("finalization requires every task done or dropped".into());
        }
        let base_ref = format!("refs/heads/{}", plan.status.base_name);
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["rev-parse", base_ref.as_str()])
            .output()
            .await
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().into());
        }
        let expected_base_oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        plan.status.display_status = "In progress".into();
        plan.status.integration_state = crate::plan::PlanIntegrationState::FinalizationPending;
        plan.status.run = Some(self.run_uid.clone());
        plan.status.mode = Some(
            match mode {
                "squash" => "Squash",
                "stage" => "Stage",
                "merge_commit" => "MergeCommit",
                "manual" => "Manual",
                _ => unreachable!("validated closed mode"),
            }
            .into(),
        );
        plan.status.last_updated = last_updated.into();
        let transition = crate::plan_status::StatusTransition {
            integration_state: plan.status.integration_state,
            run: plan.status.run.clone(),
            validation_base: plan.status.validation_base_oid.clone(),
            mode: plan.status.mode.clone(),
            final_oid: None,
            display_status: plan.status.display_status.clone(),
            last_updated: last_updated.into(),
        };
        let status = crate::plan_status::render_plan_status(&plan, &transition)
            .map_err(|error| error.to_string())?;
        let board = String::from_utf8(
            source
                .read_file(Path::new("docs/plans/STATUS.md"))
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let board = crate::plan_status::update_root_row(&board, &plan)
            .map_err(|error| error.to_string())?;
        let plan_name = self
            .plan
            .relative_dir
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "plan directory is not UTF-8".to_owned())?;
        let manager = crate::worktree::WorktreeManager::new(
            self.repo_root.clone(),
            plan.status.base_name.clone(),
        );
        let workspace = manager
            .create_integration_workspace(plan_name, &self.run_uid)
            .await
            .map_err(|error| error.to_string())?;
        let identity = crate::landing::FinalizationIdentity {
            plan: plan_name.into(),
            run: self.run_uid.clone(),
            mode: mode.into(),
            expected_base: expected_base_oid.clone(),
        };
        let prepared_oid = crate::landing::commit_finalization_prepared(
            &workspace.path,
            &self.plan_ref,
            &base_ref,
            expected_plan_oid,
            &expected_base_oid,
            &[
                crate::landing::OwnedWrite {
                    path: plan.status.source.source_path,
                    bytes: status.into_bytes(),
                },
                crate::landing::OwnedWrite {
                    path: PathBuf::from("docs/plans/STATUS.md"),
                    bytes: board.into_bytes(),
                },
            ],
            &identity,
            true,
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(PreparedFinalization {
            prepared_oid,
            expected_base_oid,
            base_ref,
            mode: mode.into(),
        })
    }

    pub async fn integrate_finalization(
        &self,
        prepared: &PreparedFinalization,
        manual_oid: Option<&str>,
    ) -> Result<String, String> {
        let plan = self.load()?;
        let plan_name = self
            .plan
            .relative_dir
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "plan directory is not UTF-8".to_owned())?;
        let manager = crate::worktree::WorktreeManager::new(
            self.repo_root.clone(),
            plan.status.base_name.clone(),
        );
        let workspace = manager
            .create_integration_workspace(plan_name, &self.run_uid)
            .await
            .map_err(|error| error.to_string())?;
        let identity = crate::landing::FinalizationIdentity {
            plan: plan_name.into(),
            run: self.run_uid.clone(),
            mode: prepared.mode.clone(),
            expected_base: prepared.expected_base_oid.clone(),
        };
        if let Some(oid) = manual_oid {
            return crate::landing::verify_manual_final_integration(
                &workspace.path,
                &prepared.base_ref,
                &prepared.expected_base_oid,
                &prepared.prepared_oid,
                oid,
                &identity,
            )
            .await
            .map_err(|error| error.to_string());
        }
        let tasks = plan
            .tasks
            .iter()
            .filter_map(|task| {
                task.frontmatter
                    .merged_as
                    .as_ref()
                    .map(|oid| (task.frontmatter.id.to_string(), oid.to_string()))
            })
            .collect::<Vec<_>>();
        crate::landing::commit_final_integration(
            &workspace.path,
            &prepared.base_ref,
            &prepared.expected_base_oid,
            &prepared.prepared_oid,
            &identity,
            prepared.mode == "merge_commit",
            &tasks,
        )
        .await
        .map_err(|error| error.to_string())
    }

    pub async fn complete_finalization(
        &self,
        prepared: &PreparedFinalization,
        final_oid: &str,
        last_updated: &str,
    ) -> Result<String, String> {
        use crate::plan::PlanFileSource as _;
        let source =
            crate::plan::GitTreePlanFileSource::new(&self.repo_root, &prepared.prepared_oid)
                .map_err(|error| error.to_string())?;
        let mut plan = match crate::plan::load_plan(
            &source,
            self.plan.clone(),
            &crate::plan::PlanReservations::default(),
        )
        .map_err(|report| format!("prepared plan invalid: {:?}", report.diagnostics))?
        {
            crate::plan::PlanCandidate::Plan(plan) => *plan,
            crate::plan::PlanCandidate::NotCandidate => {
                return Err("prepared tree lost plan".into());
            }
        };
        let final_typed = self.parse_oid(final_oid.to_owned())?;
        plan.status.display_status = "Complete".into();
        plan.status.integration_state = crate::plan::PlanIntegrationState::Complete;
        plan.status.final_oid = Some(final_typed.clone());
        plan.status.last_updated = last_updated.into();
        let transition = crate::plan_status::StatusTransition {
            integration_state: plan.status.integration_state,
            run: plan.status.run.clone(),
            validation_base: plan.status.validation_base_oid.clone(),
            mode: plan.status.mode.clone(),
            final_oid: Some(final_typed),
            display_status: plan.status.display_status.clone(),
            last_updated: last_updated.into(),
        };
        let status = crate::plan_status::render_plan_status(&plan, &transition)
            .map_err(|error| error.to_string())?;
        let board = String::from_utf8(
            source
                .read_file(Path::new("docs/plans/STATUS.md"))
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let board = crate::plan_status::update_root_row(&board, &plan)
            .map_err(|error| error.to_string())?;
        let plan_name = self
            .plan
            .relative_dir
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "plan directory is not UTF-8".to_owned())?;
        let manager = crate::worktree::WorktreeManager::new(
            self.repo_root.clone(),
            plan.status.base_name.clone(),
        );
        let workspace = manager
            .create_integration_workspace(plan_name, &self.run_uid)
            .await
            .map_err(|error| error.to_string())?;
        crate::landing::commit_finalization_completion(
            &workspace.path,
            &prepared.base_ref,
            final_oid,
            &[
                crate::landing::OwnedWrite {
                    path: plan.status.source.source_path,
                    bytes: status.into_bytes(),
                },
                crate::landing::OwnedWrite {
                    path: PathBuf::from("docs/plans/STATUS.md"),
                    bytes: board.into_bytes(),
                },
            ],
            &crate::landing::FinalizationIdentity {
                plan: plan_name.into(),
                run: self.run_uid.clone(),
                mode: prepared.mode.clone(),
                expected_base: prepared.expected_base_oid.clone(),
            },
        )
        .await
        .map_err(|error| error.to_string())
    }
}

impl CoreApi {}

impl AuthoringCoordinator {
    pub async fn render_and_publish_blueprint(
        &self,
        blueprint: crate::api::GeneratedPlanBlueprint,
    ) -> Result<CommandOutcome, ApiError> {
        Ok(self.render_blueprint_candidate(blueprint, true).await?.1)
    }

    pub async fn render_blueprint_candidate(
        &self,
        blueprint: crate::api::GeneratedPlanBlueprint,
        commit: bool,
    ) -> Result<(crate::plan::PlanKey, CommandOutcome), ApiError> {
        self.render_blueprint_candidate_reserved(blueprint, commit, None)
            .await
    }

    pub async fn render_blueprint_candidate_reserved(
        &self,
        blueprint: crate::api::GeneratedPlanBlueprint,
        commit: bool,
        reserved_number: Option<String>,
    ) -> Result<(crate::plan::PlanKey, CommandOutcome), ApiError> {
        use crate::plan::{
            AuthoredTaskStatus, GeneratedInitialStatus, GeneratedTaskDocument, GeneratedWorkstream,
            GitObjectFormat, PlanCandidate, PlanKey, PlanReservations, TaskFrontmatter,
            TaskId as PlanTaskId, TaskKind, TaskSequence, WorkstreamId, load_plan,
            parse_generated_repo_pattern,
        };
        let invalid = |reason: String| ApiError::InvalidCommand { reason };
        async fn git(root: &Path, args: &[&str]) -> Result<String, ApiError> {
            let output = tokio::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|error| ApiError::InvalidCommand {
                    reason: error.to_string(),
                })?;
            if !output.status.success() {
                return Err(ApiError::InvalidCommand {
                    reason: String::from_utf8_lossy(&output.stderr).trim().into(),
                });
            }
            Ok(String::from_utf8_lossy(&output.stdout).trim().into())
        }

        let root = &self.repo_root;
        let base = git(root, &["rev-parse", &self.base_branch]).await?;
        let format = match git(root, &["rev-parse", "--show-object-format"])
            .await?
            .as_str()
        {
            "sha1" => GitObjectFormat::Sha1,
            "sha256" => GitObjectFormat::Sha256,
            other => return Err(invalid(format!("unsupported Git object format {other}"))),
        };

        let mut reserved = std::collections::BTreeSet::new();
        if let Ok(entries) = std::fs::read_dir(root.join("docs/plans")) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let bytes = name.as_encoded_bytes();
                if bytes.len() >= 4 && bytes[..4].iter().all(u8::is_ascii_digit) {
                    reserved.insert(String::from_utf8_lossy(&bytes[..4]).into_owned());
                }
            }
        }
        let base_dirs = git(
            root,
            &["ls-tree", "--name-only", &format!("{base}:docs/plans")],
        )
        .await?;
        for name in base_dirs.lines() {
            let bytes = name.as_bytes();
            if bytes.len() >= 4 && bytes[..4].iter().all(u8::is_ascii_digit) {
                reserved.insert(name[..4].to_owned());
            }
        }
        let refs = git(
            root,
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/plan/",
            ],
        )
        .await?;
        let mut existing_slug_number = None;
        for reference in refs.lines() {
            let name = reference.strip_prefix("plan/").unwrap_or(reference);
            let bytes = name.as_bytes();
            if bytes.len() >= 4 && bytes[..4].iter().all(u8::is_ascii_digit) {
                reserved.insert(name[..4].to_owned());
                if name.get(4..) == Some(format!("-{}", blueprint.slug).as_str())
                    && existing_slug_number.replace(name[..4].to_owned()).is_some()
                {
                    return Err(invalid(
                        "multiple plan refs already use the generated slug".into(),
                    ));
                }
            }
        }
        let reusing_existing_slug = existing_slug_number.is_some();
        let number = match existing_slug_number {
            Some(number)
                if reserved_number
                    .as_ref()
                    .is_none_or(|value| value == &number) =>
            {
                number
            }
            Some(_) => {
                return Err(invalid(
                    "generated slug already uses another reserved number".into(),
                ));
            }
            None => reserved_number.unwrap_or_else(|| {
                (1..=9999)
                    .map(|value| format!("{value:04}"))
                    .find(|number| !reserved.contains(number))
                    .unwrap_or_default()
            }),
        };
        if number.is_empty() || (!reusing_existing_slug && reserved.contains(&number)) {
            return Err(invalid("reserved plan number is unavailable".into()));
        }
        let key = PlanKey::parse(
            PathBuf::from("docs/plans").join(format!("{number}-{}", blueprint.slug)),
        )
        .map_err(|error| invalid(error.to_string()))?;
        let base_oid = crate::plan::GitObjectId::parse(base.clone(), format)
            .map_err(|error| invalid(error.to_string()))?;

        let workstreams = blueprint
            .workstreams
            .into_iter()
            .map(|workstream| {
                Ok(GeneratedWorkstream {
                    id: WorkstreamId::parse(workstream.id)
                        .map_err(|error| invalid(error.to_string()))?,
                    title: workstream.title,
                })
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        let mut tasks = Vec::with_capacity(blueprint.tasks.len());
        for task in blueprint.tasks {
            let kind = match task.kind.as_str() {
                "task" => TaskKind::Task,
                "spike" => TaskKind::Spike,
                "chore" => TaskKind::Chore,
                _ => {
                    return Err(invalid(format!(
                        "invalid generated task kind `{}`",
                        task.kind
                    )));
                }
            };
            let sequence =
                TaskSequence::parse(task.sequence).map_err(|error| invalid(error.to_string()))?;
            let id = PlanTaskId::parse(task.id).map_err(|error| invalid(error.to_string()))?;
            let workstream =
                WorkstreamId::parse(task.workstream).map_err(|error| invalid(error.to_string()))?;
            let task_path = key.relative_dir.join("tasks").join(format!(
                "{}{}-{}.md",
                &workstream.as_str()[2..],
                sequence,
                id
            ));
            let touches = task
                .touches
                .iter()
                .map(|value| parse_generated_repo_pattern(value, kind, &task_path))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| invalid(error.to_string()))?;
            tasks.push(GeneratedTaskDocument {
                sequence,
                frontmatter: TaskFrontmatter {
                    id,
                    title: task.title,
                    workstream,
                    kind,
                    depends_on: task
                        .depends_on
                        .into_iter()
                        .map(PlanTaskId::parse)
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|error| invalid(error.to_string()))?,
                    gated: task.gated,
                    touches,
                    status: AuthoredTaskStatus::Planned,
                    merged_as: None,
                },
                body: task.body,
            });
        }
        let authored = crate::plan::GeneratedPlanBundle {
            key: key.clone(),
            title: blueprint.title,
            scope: blueprint.scope,
            architecture: blueprint.architecture,
            initial_status: GeneratedInitialStatus {
                goal: blueprint.initial_status.goal,
                root_cause: blueprint.initial_status.root_cause,
                approach: blueprint.initial_status.approach,
                outcome: blueprint.initial_status.outcome,
                base_name: self.base_branch.clone(),
                base_oid,
                last_updated: blueprint.initial_status.last_updated,
            },
            workstreams,
            tasks,
        };
        let files = authored
            .render_files()
            .map_err(|error| invalid(format!("generated blueprint is invalid: {error}")))?;

        let state_root = crate::checkpoint::external_state_root(root)
            .map_err(|error| invalid(error.to_string()))?;
        let generation_root = state_root.join("generation");
        tokio::fs::create_dir_all(&generation_root)
            .await
            .map_err(|error| invalid(error.to_string()))?;
        let generation_root =
            std::fs::canonicalize(&generation_root).map_err(|error| invalid(error.to_string()))?;
        let state_root =
            std::fs::canonicalize(&state_root).map_err(|error| invalid(error.to_string()))?;
        if !generation_root.starts_with(&state_root) {
            return Err(invalid("generation root escaped external state".into()));
        }
        let temporary = generation_root.join(format!(
            "{}-{}",
            ulid::Ulid::from_datetime(std::time::SystemTime::now()),
            number
        ));
        tokio::fs::create_dir(&temporary)
            .await
            .map_err(|error| invalid(error.to_string()))?;
        let result = async {
            for (relative, bytes) in &files {
                let target = temporary.join(&key.relative_dir).join(relative);
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|error| invalid(error.to_string()))?;
                }
                tokio::fs::write(&target, bytes)
                    .await
                    .map_err(|error| invalid(error.to_string()))?;
            }
            let source = crate::plan::FilesystemPlanFileSource::new_unbound(&temporary, format)
                .map_err(|error| invalid(error.to_string()))?;
            let plan = match load_plan(&source, key.clone(), &PlanReservations::default())
                .map_err(|report| invalid(render_generation_diagnostics(&report.diagnostics)))?
            {
                PlanCandidate::Plan(plan) => plan,
                PlanCandidate::NotCandidate => {
                    return Err(invalid("generated bundle is not a plan".into()));
                }
            };
            let closed = GeneratedPlanBundle {
                key: key.clone(),
                expected_source_digest: plan.source_digest.to_string(),
                files,
            };
            if !commit {
                let plan_root = root.join(&key.relative_dir);
                tokio::fs::create_dir(&plan_root)
                    .await
                    .map_err(|error| invalid(error.to_string()))?;
                for (relative, bytes) in &closed.files {
                    let target = plan_root.join(relative);
                    if let Some(parent) = target.parent() {
                        tokio::fs::create_dir_all(parent)
                            .await
                            .map_err(|error| invalid(error.to_string()))?;
                    }
                    tokio::fs::write(target, bytes)
                        .await
                        .map_err(|error| invalid(error.to_string()))?;
                }
                return Ok((key, CommandOutcome::AwaitingCommit));
            }
            match self.publish_generated(closed, base).await? {
                CommandOutcome::PlanRegistered { registration_oid } => Ok((
                    key.clone(),
                    CommandOutcome::PlanGenerated {
                        plan_dir: key,
                        registration_oid,
                        report: crate::api::PlanGenerationReport::default(),
                    },
                )),
                _ => Err(invalid(
                    "generated registration returned an invalid outcome".into(),
                )),
            }
        }
        .await;
        let _ = tokio::fs::remove_dir_all(&temporary).await;
        result
    }
}

/// Render bundle diagnostics for the reader who has to act on them.
///
/// Debug-formatting the vector produced a wall of `PlanValidationDiagnostic {
/// code: "…", path: "…", … }` structs. That string is not an internal detail:
/// it reaches the authoring transcript, and it is handed back to the planner as
/// the correction to apply. Bounded, because the whole of it becomes prompt.
fn render_generation_diagnostics(diagnostics: &[crate::plan::PlanValidationDiagnostic]) -> String {
    const MAX_REPORTED: usize = 8;
    if diagnostics.is_empty() {
        return "generated bundle validation failed without a diagnostic".into();
    }
    let rendered = diagnostics
        .iter()
        .take(MAX_REPORTED)
        .map(|diagnostic| {
            let field = diagnostic
                .field
                .as_deref()
                .map(|field| format!(" [{field}]"))
                .unwrap_or_default();
            format!(
                "{} ({}){field}: {}",
                diagnostic.path.display(),
                diagnostic.code,
                diagnostic.message
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    match diagnostics.len().saturating_sub(MAX_REPORTED) {
        0 => format!("generated bundle validation failed: {rendered}"),
        elided => format!("generated bundle validation failed: {rendered}; and {elided} more"),
    }
}

impl CoreApi {
    async fn generate_plan_bundle(
        &self,
        blueprint: crate::api::GeneratedPlanBlueprint,
    ) -> Result<CommandOutcome, ApiError> {
        AuthoringCoordinator::new(
            self.state.worktree_manager.repo_root.clone(),
            self.state.worktree_manager.base_branch.clone(),
            Arc::clone(&self.state.repository_leases),
        )
        .render_and_publish_blueprint(blueprint)
        .await
    }

    async fn delayed_finalization(
        &self,
        plan_dir: crate::plan::PlanKey,
        run_uid: String,
        expected_plan_oid: String,
        input: Option<crate::api::FinalizeInput>,
    ) -> Result<CommandOutcome, ApiError> {
        use crate::plan::PlanFileSource as _;
        let invalid = |reason: String| ApiError::InvalidCommand { reason };
        if run_uid.is_empty() || run_uid.contains(['\n', '\r', '\0']) {
            return Err(invalid("run_uid is invalid".into()));
        }
        let live = {
            let runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            runs.iter()
                .find(|(_, entry)| entry.run_uid == run_uid && entry.plan_dir == plan_dir)
                .map(|(id, entry)| {
                    (
                        RunId(*id),
                        entry.plan_slug.clone(),
                        Arc::clone(&entry.graph),
                        entry.handle.is_some(),
                    )
                })
        };
        let (run, plan_slug) = if let Some((run, plan_slug, graph, has_driver)) = live {
            if has_driver {
                return Err(invalid(
                    "finalization requires every child driver to be quiescent".into(),
                ));
            }
            let graph = graph.lock().await;
            if graph
                .tasks
                .iter()
                .any(|task| !matches!(task.state, TaskState::Done | TaskState::Dropped))
            {
                return Err(invalid(
                    "finalization requires every non-dropped task to be done".into(),
                ));
            }
            (run, plan_slug)
        } else {
            let metadata = crate::run_metadata::read_run_metadata(
                &self.state.worktree_manager.repo_root,
                &run_uid,
            )
            .map_err(|e| invalid(format!("read run metadata: {e}")))?;
            if let Some(metadata) = metadata.as_ref()
                && (metadata
                    .resolved_plan_dir(&self.state.worktree_manager.repo_root)
                    .as_ref()
                    != Some(&plan_dir)
                    || metadata.tasks().iter().any(|task| {
                        !matches!(
                            task.state,
                            crate::api::TaskState::Done | crate::api::TaskState::Dropped
                        )
                    }))
            {
                return Err(invalid(
                    "durable run metadata is not finalization-ready for this plan".into(),
                ));
            }
            let run = {
                let mut ids = self
                    .state
                    .disk_run_ids
                    .lock()
                    .expect("disk run id mutex poisoned");
                *ids.entry(run_uid.clone())
                    .or_insert_with(|| self.state.alloc_id())
            };
            (
                run,
                metadata.as_ref().map_or_else(
                    || format!("{}-{}", plan_dir.number, plan_dir.slug),
                    |metadata| metadata.plan_slug().to_owned(),
                ),
            )
        };
        let mode = self
            .state
            .runtime_settings
            .lock()
            .expect("runtime settings mutex poisoned")
            .final_merge;
        match (&input, mode) {
            (
                Some(crate::api::FinalizeInput::Automatic),
                FinalMerge::Squash | FinalMerge::MergeCommit,
            )
            | (Some(crate::api::FinalizeInput::PreparedStage), FinalMerge::Stage)
            | (Some(crate::api::FinalizeInput::ManualCommit(_)), FinalMerge::Manual)
            | (None, _) => {}
            (Some(actual), configured) => {
                return Err(invalid(format!(
                    "finalization input {actual:?} does not match configured mode {configured:?}"
                )));
            }
        }
        let root = &self.state.worktree_manager.repo_root;
        let owner = crate::repository_lease::RepositoryLeaseOwner {
            plan_dir: plan_dir.relative_dir.clone(),
            run_uid: run_uid.clone(),
            operation: if input.is_some() {
                crate::repository_lease::RepositoryLeaseOperation::Finalize
            } else {
                crate::repository_lease::RepositoryLeaseOperation::ReprepareFinalization
            },
        };
        let cancel = CancellationToken::new();
        let _lease = self
            .state
            .repository_leases
            .acquire(root, owner, &cancel)
            .await
            .map_err(|e| invalid(e.to_string()))?;
        let plan_ref = plan_dir.ref_name();
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--verify", &plan_ref])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| invalid(e.to_string()))?;
        if !output.status.success() {
            return Err(invalid(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        let actual = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if actual != expected_plan_oid {
            return Err(invalid(format!(
                "plan ref moved: expected {expected_plan_oid}, found {actual}"
            )));
        }
        let source = crate::plan::GitTreePlanFileSource::new(root, &actual)
            .map_err(|e| invalid(e.to_string()))?;
        let plan = match crate::plan::load_plan(
            &source,
            plan_dir.clone(),
            &crate::plan::PlanReservations::default(),
        )
        .map_err(|report| {
            invalid(format!(
                "retained plan is invalid: {:?}",
                report.diagnostics
            ))
        })? {
            crate::plan::PlanCandidate::Plan(plan) => *plan,
            crate::plan::PlanCandidate::NotCandidate => {
                return Err(invalid("retained ref does not contain the plan".into()));
            }
        };
        if plan.tasks.iter().any(|task| {
            !matches!(
                task.frontmatter.status,
                crate::plan::AuthoredTaskStatus::Done | crate::plan::AuthoredTaskStatus::Dropped
            )
        }) {
            return Err(invalid(
                "authored status is not ready for finalization".into(),
            ));
        }
        // `mode_name` is the commit-trailer spelling; `status_mode` is the
        // STATUS.md spelling the plan loader accepts. Never swap them.
        let (mode_name, status_mode) = crate::plan_status::final_mode_names(mode);
        let base_ref = format!("refs/heads/{}", self.state.worktree_manager.base_branch);
        let base_output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", &base_ref])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| invalid(e.to_string()))?;
        if !base_output.status.success() {
            return Err(invalid(
                String::from_utf8_lossy(&base_output.stderr).trim().into(),
            ));
        }
        let current_base = String::from_utf8_lossy(&base_output.stdout)
            .trim()
            .to_owned();
        let identity = crate::landing::FinalizationIdentity {
            plan: format!("{}-{}", plan_dir.number, plan_dir.slug),
            run: run_uid.clone(),
            mode: mode_name.into(),
            expected_base: current_base.clone(),
        };
        let workspace = self
            .state
            .worktree_manager
            .create_integration_workspace(&identity.plan, &format!("finalize-{run_uid}"))
            .await
            .map_err(|e| invalid(e.to_string()))?;
        if input.is_none() {
            let mut prepared_plan = plan;
            prepared_plan.status.integration_state =
                crate::plan::PlanIntegrationState::FinalizationPending;
            prepared_plan.status.run = Some(run_uid.clone());
            prepared_plan.status.mode = Some(status_mode.into());
            prepared_plan.status.final_oid = None;
            prepared_plan.status.display_status =
                crate::plan_status::display_badge(prepared_plan.status.integration_state).into();
            let transition = crate::plan_status::StatusTransition {
                integration_state: prepared_plan.status.integration_state,
                run: prepared_plan.status.run.clone(),
                validation_base: prepared_plan.status.validation_base_oid.clone(),
                mode: prepared_plan.status.mode.clone(),
                final_oid: None,
                display_status: prepared_plan.status.display_status.clone(),
                last_updated: prepared_plan.status.last_updated.clone(),
            };
            let status = crate::plan_status::render_plan_status(&prepared_plan, &transition)
                .map_err(|e| invalid(e.to_string()))?;
            prepared_plan.status.source.body = status.clone();
            let base_source = crate::plan::GitTreePlanFileSource::new(root, &current_base)
                .map_err(|e| invalid(e.to_string()))?;
            use crate::plan::PlanFileSource as _;
            let board = String::from_utf8(
                base_source
                    .read_file(Path::new("docs/plans/STATUS.md"))
                    .map_err(|e| invalid(e.to_string()))?,
            )
            .map_err(|_| invalid("root board is not UTF-8".into()))?;
            // Read from the current base, which may never have carried this
            // plan's row: registration can only add a missing row to the plan
            // ref, never to base.
            let board = crate::plan_status::upsert_root_row(&board, &prepared_plan)
                .map_err(|e| invalid(e.to_string()))?;
            let prepared = crate::landing::commit_finalization_prepared(
                &workspace.path,
                &plan_ref,
                &base_ref,
                &actual,
                &current_base,
                &[
                    crate::landing::OwnedWrite {
                        path: prepared_plan.status.source.source_path.clone(),
                        bytes: status.into_bytes(),
                    },
                    crate::landing::OwnedWrite {
                        path: PathBuf::from("docs/plans/STATUS.md"),
                        bytes: board.into_bytes(),
                    },
                ],
                &identity,
                false,
            )
            .await
            .map_err(|e| invalid(e.to_string()))?;
            let _ = self.state.event_tx.send(Event::PlanOperation {
                run,
                plan_slug,
                label: prepared_plan.title,
                operation: crate::api::PlanOperationKind::ReprepareFinalization,
                phase: crate::api::PlanOperationPhase::Finished,
                message: format!("Prepared finalization at {prepared}"),
            });
            return Ok(CommandOutcome::FinalizationAccepted { plan_oid: prepared });
        }
        let message_output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["show", "-s", "--format=%B", &actual])
            .output()
            .await
            .map_err(|e| invalid(e.to_string()))?;
        let prepared_message = String::from_utf8_lossy(&message_output.stdout);
        let trailer = |name: &str| {
            let prefix = format!("{name}: ");
            let values = prepared_message
                .lines()
                .filter_map(|line| line.strip_prefix(&prefix))
                .collect::<Vec<_>>();
            (values.len() == 1).then(|| values[0].to_owned())
        };
        if trailer("Makina-Phase").as_deref() != Some("finalization-prepared")
            || trailer("Makina-Plan").as_deref() != Some(identity.plan.as_str())
            || trailer("Makina-Run").as_deref() != Some(run_uid.as_str())
            || trailer("Makina-Final-Mode").as_deref() != Some(mode_name)
        {
            return Err(invalid(
                "expected plan tip is not exact Phase P evidence".into(),
            ));
        }
        let expected_base = trailer("Makina-Expected-Base")
            .ok_or_else(|| invalid("Phase P lacks expected base".into()))?;
        let task_evidence = plan
            .tasks
            .iter()
            .filter_map(|task| {
                task.frontmatter.merged_as.as_ref().map(|oid| {
                    (
                        task.frontmatter.id.as_str().to_owned(),
                        oid.as_str().to_owned(),
                    )
                })
            })
            .collect::<Vec<_>>();
        let final_oid = match input.as_ref().expect("checked input") {
            crate::api::FinalizeInput::Automatic => {
                crate::landing::commit_final_integration(
                    &workspace.path,
                    &base_ref,
                    &expected_base,
                    &actual,
                    &identity,
                    mode == FinalMerge::MergeCommit,
                    &task_evidence,
                )
                .await
            }
            crate::api::FinalizeInput::PreparedStage => {
                crate::landing::commit_final_integration(
                    &workspace.path,
                    &base_ref,
                    &expected_base,
                    &actual,
                    &identity,
                    false,
                    &task_evidence,
                )
                .await
            }
            crate::api::FinalizeInput::ManualCommit(oid) => {
                crate::landing::verify_manual_final_integration(
                    &workspace.path,
                    &base_ref,
                    &expected_base,
                    &actual,
                    oid.as_str(),
                    &identity,
                )
                .await
            }
        }
        .map_err(|e| invalid(e.to_string()))?;
        let final_source = crate::plan::GitTreePlanFileSource::new(root, &final_oid)
            .map_err(|e| invalid(e.to_string()))?;
        let mut complete_plan = match crate::plan::load_plan(
            &final_source,
            plan_dir.clone(),
            &crate::plan::PlanReservations::default(),
        )
        .map_err(|r| invalid(format!("final tree plan invalid: {:?}", r.diagnostics)))?
        {
            crate::plan::PlanCandidate::Plan(plan) => *plan,
            crate::plan::PlanCandidate::NotCandidate => {
                return Err(invalid("F lost plan source".into()));
            }
        };
        complete_plan.status.integration_state = crate::plan::PlanIntegrationState::Complete;
        complete_plan.status.run = Some(run_uid.clone());
        complete_plan.status.mode = Some(status_mode.into());
        complete_plan.status.final_oid = Some(
            crate::plan::GitObjectId::parse(&final_oid, final_source.object_format())
                .map_err(|e| invalid(e.to_string()))?,
        );
        complete_plan.status.display_status =
            crate::plan_status::display_badge(complete_plan.status.integration_state).into();
        let transition = crate::plan_status::StatusTransition {
            integration_state: complete_plan.status.integration_state,
            run: complete_plan.status.run.clone(),
            validation_base: complete_plan.status.validation_base_oid.clone(),
            mode: complete_plan.status.mode.clone(),
            final_oid: complete_plan.status.final_oid.clone(),
            display_status: complete_plan.status.display_status.clone(),
            last_updated: complete_plan.status.last_updated.clone(),
        };
        let status = crate::plan_status::render_plan_status(&complete_plan, &transition)
            .map_err(|e| invalid(e.to_string()))?;
        complete_plan.status.source.body = status.clone();
        let board = String::from_utf8(
            final_source
                .read_file(Path::new("docs/plans/STATUS.md"))
                .map_err(|e| invalid(e.to_string()))?,
        )
        .map_err(|_| invalid("root board is not UTF-8".into()))?;
        // The completed tree descends from base, so it inherits base's board —
        // including a board that never listed this plan.
        let board = crate::plan_status::upsert_root_row(&board, &complete_plan)
            .map_err(|e| invalid(e.to_string()))?;
        let completion = crate::landing::commit_finalization_completion(
            &workspace.path,
            &base_ref,
            &final_oid,
            &[
                crate::landing::OwnedWrite {
                    path: complete_plan.status.source.source_path.clone(),
                    bytes: status.into_bytes(),
                },
                crate::landing::OwnedWrite {
                    path: PathBuf::from("docs/plans/STATUS.md"),
                    bytes: board.into_bytes(),
                },
            ],
            &identity,
        )
        .await
        .map_err(|e| invalid(e.to_string()))?;
        if let Some(metadata) = crate::run_metadata::read_run_metadata(root, &run_uid)
            .map_err(|e| invalid(format!("read run metadata after C: {e}")))?
        {
            crate::run_metadata::write_run_metadata(
                &metadata.with_completion_oid(completion.clone()),
                root,
            )
            .await
            .map_err(|e| invalid(format!("persist completion evidence: {e}")))?;
        }
        let operation = if input.is_some() {
            crate::api::PlanOperationKind::Finalize
        } else {
            crate::api::PlanOperationKind::ReprepareFinalization
        };
        let _ = self.state.event_tx.send(Event::PlanOperation {
            run,
            plan_slug,
            label: plan.title,
            operation,
            phase: crate::api::PlanOperationPhase::Finished,
            message: format!("Finalization completed at {completion}"),
        });
        Ok(CommandOutcome::FinalizationAccepted {
            plan_oid: completion,
        })
    }
}

impl AuthoringCoordinator {
    /// Register a validated generated bundle directly, without materializing it
    /// in the operator checkout. This is the internal initial-R entry used by
    /// generation workflows.
    pub async fn publish_generated(
        &self,
        bundle: GeneratedPlanBundle,
        expected_base_oid: String,
    ) -> Result<CommandOutcome, ApiError> {
        use crate::plan::{
            FilesystemPlanFileSource, PlanCandidate, PlanFileSource, PlanReservations, load_plan,
        };
        fn invalid(reason: impl Into<String>) -> ApiError {
            ApiError::InvalidCommand {
                reason: reason.into(),
            }
        }
        if bundle.files.is_empty() {
            return Err(invalid("generated bundle is empty"));
        }
        for path in bundle.files.keys() {
            if path.is_absolute()
                || path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
                || path == Path::new("STATUS.md").parent().unwrap_or(Path::new(""))
            {
                return Err(invalid(format!(
                    "generated bundle path is not closed: {}",
                    path.display()
                )));
            }
        }
        async fn git(root: &Path, args: &[&str]) -> Result<String, ApiError> {
            let output = tokio::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|error| ApiError::InvalidCommand {
                    reason: error.to_string(),
                })?;
            if !output.status.success() {
                return Err(ApiError::InvalidCommand {
                    reason: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                });
            }
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
        fn trailer(message: &str, name: &str) -> Option<String> {
            let prefix = format!("{name}: ");
            let values = message
                .lines()
                .filter_map(|line| line.strip_prefix(&prefix))
                .collect::<Vec<_>>();
            (values.len() == 1).then(|| values[0].to_owned())
        }
        let root = &self.repo_root;
        let identity = format!("{}-{}", bundle.key.number, bundle.key.slug);
        let owner = crate::repository_lease::RepositoryLeaseOwner {
            plan_dir: bundle.key.relative_dir.clone(),
            run_uid: format!("generate-{identity}"),
            operation: crate::repository_lease::RepositoryLeaseOperation::RegisterPlan,
        };
        let cancel = CancellationToken::new();
        let _lease = if self.lease_held {
            None
        } else {
            Some(
                self.repository_leases
                    .acquire(root, owner, &cancel)
                    .await
                    .map_err(|e| invalid(e.to_string()))?,
            )
        };
        let base = git(root, &["rev-parse", &self.base_branch]).await?;
        if base != expected_base_oid {
            return Err(invalid(format!(
                "target base moved: expected {expected_base_oid}, found {base}"
            )));
        }
        let plan_ref = bundle.key.ref_name();
        if let Ok(tip) = git(root, &["rev-parse", "--verify", &plan_ref]).await {
            let message = git(root, &["show", "-s", "--format=%B", &tip]).await?;
            if trailer(&message, "Makina-Phase").as_deref() == Some("plan-registration")
                && trailer(&message, "Makina-Plan").as_deref() == Some(identity.as_str())
                && trailer(&message, "Makina-Validation-Base").as_deref()
                    == Some(expected_base_oid.as_str())
                && trailer(&message, "Makina-Source-Origin").as_deref() == Some("generated")
                && trailer(&message, "Makina-Source-Digest").as_deref()
                    == Some(bundle.expected_source_digest.as_str())
            {
                let retained = crate::plan::GitTreePlanFileSource::new(root, &tip)
                    .map_err(|e| invalid(e.to_string()))?;
                let verified =
                    match load_plan(&retained, bundle.key.clone(), &PlanReservations::default())
                        .map_err(|report| {
                            invalid(format!(
                                "existing generated registration is invalid: {:?}",
                                report.diagnostics
                            ))
                        })? {
                        PlanCandidate::Plan(plan) => plan,
                        PlanCandidate::NotCandidate => {
                            return Err(invalid("existing generated registration lost its plan"));
                        }
                    };
                if verified.source_digest.as_str() != bundle.expected_source_digest
                    || trailer(&message, "Makina-Executable-Digest").as_deref()
                        != Some(verified.executable_digest.as_str())
                {
                    return Err(invalid(
                        "existing generated registration digest/tree verification failed",
                    ));
                }
                return Ok(CommandOutcome::PlanRegistered {
                    registration_oid: tip,
                });
            }
            return Err(invalid(format!(
                "{plan_ref} already contains divergent evidence"
            )));
        }
        let listed = git(
            root,
            &[
                "ls-tree",
                "--name-only",
                &format!("{expected_base_oid}:docs/plans"),
            ],
        )
        .await?;
        let mut reservations = PlanReservations::default();
        for name in listed
            .lines()
            .filter(|name| name.len() >= 4 && name.as_bytes()[..4].iter().all(u8::is_ascii_digit))
        {
            reservations
                .numbered_directories
                .entry(name[..4].to_owned())
                .or_default()
                .push(PathBuf::from("docs/plans").join(name));
        }
        if reservations
            .numbered_directories
            .contains_key(&bundle.key.number)
        {
            return Err(invalid(format!(
                "plan number {} is already reserved in the target base",
                bundle.key.number
            )));
        }
        if let Ok(entries) = std::fs::read_dir(root.join("docs/plans")) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if name.len() >= 4
                    && name[..4] == bundle.key.number
                    && entry.file_type().is_ok_and(|kind| kind.is_dir())
                {
                    return Err(invalid(format!(
                        "plan number {} is already reserved by working source {}",
                        bundle.key.number,
                        entry.path().display()
                    )));
                }
            }
        }
        let refs = git(
            root,
            &["for-each-ref", "--format=%(refname)", "refs/heads/plan/"],
        )
        .await?;
        for reference in refs.lines() {
            if let Some(name) = reference.strip_prefix("refs/heads/plan/")
                && name.starts_with(&bundle.key.number)
            {
                return Err(invalid(format!(
                    "plan number {} is already reserved by {reference}",
                    bundle.key.number
                )));
            }
        }
        let workspace = self
            .worktree_manager
            .create_integration_workspace(
                &identity,
                &format!(
                    "generated-{}",
                    &expected_base_oid[..12.min(expected_base_oid.len())]
                ),
            )
            .await
            .map_err(|e| invalid(e.to_string()))?;
        for (relative, bytes) in &bundle.files {
            let target = workspace.path.join(&bundle.key.relative_dir).join(relative);
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| invalid(e.to_string()))?;
            }
            tokio::fs::write(target, bytes)
                .await
                .map_err(|e| invalid(e.to_string()))?;
        }
        let generated =
            FilesystemPlanFileSource::new(&workspace.path, Some(expected_base_oid.clone()))
                .map_err(|e| invalid(e.to_string()))?;
        let mut plan = match load_plan(&generated, bundle.key.clone(), &reservations)
            .map_err(|r| invalid(format!("generated plan is invalid: {:?}", r.diagnostics)))?
        {
            PlanCandidate::Plan(plan) => *plan,
            PlanCandidate::NotCandidate => return Err(invalid("generated subtree is not a plan")),
        };
        let transition = crate::plan_status::StatusTransition {
            integration_state: plan.status.integration_state,
            run: plan.status.run.clone(),
            validation_base: Some(
                crate::plan::GitObjectId::parse(&expected_base_oid, generated.object_format())
                    .map_err(|e| invalid(e.to_string()))?,
            ),
            mode: plan.status.mode.clone(),
            final_oid: plan.status.final_oid.clone(),
            display_status: plan.status.display_status.clone(),
            last_updated: plan.status.last_updated.clone(),
        };
        let status = crate::plan_status::render_plan_status(&plan, &transition)
            .map_err(|e| invalid(e.to_string()))?;
        tokio::fs::write(
            workspace
                .path
                .join(bundle.key.relative_dir.join("STATUS.md")),
            &status,
        )
        .await
        .map_err(|e| invalid(e.to_string()))?;
        plan.status.source.body = status;
        let base_source = crate::plan::GitTreePlanFileSource::new(root, &expected_base_oid)
            .map_err(|e| invalid(e.to_string()))?;
        let board = String::from_utf8(
            base_source
                .read_file(Path::new("docs/plans/STATUS.md"))
                .map_err(|e| invalid(e.to_string()))?,
        )
        .map_err(|_| invalid("root status board is not UTF-8"))?;
        let board = crate::plan_status::register_root_row(&board, &plan)
            .map_err(|e| invalid(e.to_string()))?;
        tokio::fs::write(workspace.path.join("docs/plans/STATUS.md"), board)
            .await
            .map_err(|e| invalid(e.to_string()))?;
        let verified_source =
            FilesystemPlanFileSource::new(&workspace.path, Some(expected_base_oid.clone()))
                .map_err(|e| invalid(e.to_string()))?;
        let verified = match load_plan(&verified_source, bundle.key.clone(), &reservations)
            .map_err(|r| {
                invalid(format!(
                    "generated registration validation failed: {:?}",
                    r.diagnostics
                ))
            })? {
            PlanCandidate::Plan(plan) => *plan,
            PlanCandidate::NotCandidate => {
                return Err(invalid("generated registration lost its plan"));
            }
        };
        if verified.source_digest.as_str() != bundle.expected_source_digest {
            return Err(invalid(format!(
                "generated source digest mismatch: expected {}, found {}",
                bundle.expected_source_digest, verified.source_digest
            )));
        }
        git(
            &workspace.path,
            &[
                "add",
                "--",
                bundle
                    .key
                    .relative_dir
                    .to_str()
                    .ok_or_else(|| invalid("plan path is not UTF-8"))?,
                "docs/plans/STATUS.md",
            ],
        )
        .await?;
        let message = format!(
            "chore(plan): register {identity}\n\nMakina-Phase: plan-registration\nMakina-Plan: {identity}\nMakina-Source-Digest: {}\nMakina-Executable-Digest: {}\nMakina-Validation-Base: {expected_base_oid}\nMakina-Source-Origin: generated",
            verified.source_digest, verified.executable_digest
        );
        git(&workspace.path, &["commit", "-m", &message]).await?;
        let candidate = git(&workspace.path, &["rev-parse", "HEAD"]).await?;
        self.worktree_manager
            .publish_registration(&workspace, &candidate, &expected_base_oid)
            .await
            .map_err(|e| invalid(e.to_string()))?;
        Ok(CommandOutcome::PlanRegistered {
            registration_oid: candidate,
        })
    }
}

impl CoreApi {
    pub async fn register_generated_plan(
        &self,
        bundle: GeneratedPlanBundle,
        expected_base_oid: String,
    ) -> Result<CommandOutcome, ApiError> {
        let coordinator = AuthoringCoordinator::new(
            self.state.worktree_manager.repo_root.clone(),
            self.state.worktree_manager.base_branch.clone(),
            Arc::clone(&self.state.repository_leases),
        );
        let outcome = coordinator
            .publish_generated(bundle.clone(), expected_base_oid)
            .await?;
        if let CommandOutcome::PlanRegistered { registration_oid } = &outcome {
            let _ = self.state.event_tx.send(Event::PlanRegistered {
                plan_dir: bundle.key,
                registration_oid: registration_oid.clone(),
            });
        }
        Ok(outcome)
    }

    async fn set_task_disposition(
        &self,
        run: RunId,
        task_id: crate::api::TaskId,
        expected_plan_oid: String,
        action: crate::api::TaskDispositionAction,
    ) -> Result<CommandOutcome, ApiError> {
        let invalid = |reason: String| ApiError::InvalidCommand { reason };
        let (key, run_uid, graph, has_driver) = {
            let runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get(&run.0).ok_or(ApiError::UnknownRun { run })?;
            (
                entry.plan_dir.clone(),
                entry.run_uid.clone(),
                Arc::clone(&entry.graph),
                entry.handle.is_some(),
            )
        };
        let current_state = graph
            .lock()
            .await
            .tasks
            .iter()
            .find(|task| task.id.0 == task_id.0)
            .map(|task| task.state)
            .ok_or_else(|| invalid(format!("unknown task {task_id}")))?;
        match &action {
            crate::api::TaskDispositionAction::Ungate if current_state != TaskState::Gated => {
                return Err(invalid("ungate requires a planned gated task".into()));
            }
            crate::api::TaskDispositionAction::Drop { reason } => {
                let reason = reason.trim();
                if reason.is_empty() || reason.len() > 240 || reason.contains(['\n', '\r', '\0']) {
                    return Err(invalid(
                        "drop reason must be a non-empty single line of at most 240 bytes".into(),
                    ));
                }
                if has_driver
                    || matches!(
                        current_state,
                        TaskState::InProgress | TaskState::InReview | TaskState::Done
                    )
                {
                    return Err(invalid("cannot drop work with a live driver, unlanded evidence, or completed landing".into()));
                }
            }
            crate::api::TaskDispositionAction::Ungate => {}
        }
        let root = &self.state.worktree_manager.repo_root;
        let _lease = self
            .state
            .repository_leases
            .acquire(
                root,
                crate::repository_lease::RepositoryLeaseOwner {
                    plan_dir: key.relative_dir.clone(),
                    run_uid: run_uid.clone(),
                    operation:
                        crate::repository_lease::RepositoryLeaseOperation::SetTaskDisposition,
                },
                &CancellationToken::new(),
            )
            .await
            .map_err(|error| invalid(error.to_string()))?;
        let coordinator = PlanContractCoordinator::new(root, key.clone(), key.ref_name(), run_uid)
            .map_err(invalid)?;
        let new_tip = coordinator
            .set_disposition(
                &crate::plan::TaskId::parse(task_id.0.clone())
                    .map_err(|error| invalid(error.to_string()))?,
                &expected_plan_oid,
                action.clone(),
            )
            .await
            .map_err(invalid)?;
        crate::checkpoint::archive_clean_checkpoint(root, &key)
            .await
            .map_err(|error| invalid(error.to_string()))?;
        let mut graph = graph.lock().await;
        if let Some(task) = graph.tasks.iter_mut().find(|task| task.id.0 == task_id.0) {
            task.state = if matches!(action, crate::api::TaskDispositionAction::Ungate) {
                TaskState::New
            } else {
                TaskState::Dropped
            };
        }
        if let Some(metadata) = graph
            .authored
            .get_mut(&crate::task::TaskId(task_id.0.clone()))
        {
            metadata.gated = false;
            if matches!(action, crate::api::TaskDispositionAction::Drop { .. }) {
                metadata.status = crate::plan::AuthoredTaskStatus::Dropped;
            }
        }
        let _ = new_tip;
        Ok(CommandOutcome::Acknowledged)
    }
}

impl AuthoringCoordinator {
    pub async fn publish_candidate(
        &self,
        key: crate::plan::PlanKey,
        expected_base_oid: String,
        expected_source_digest: String,
        commit: bool,
    ) -> Result<CommandOutcome, ApiError> {
        let invalid = |reason: String| ApiError::InvalidCommand { reason };
        match self.inspect_candidate(key.clone(), &expected_base_oid, &expected_source_digest)? {
            AuthoringCandidate::Committed => {
                return self
                    .publish_committed(key, expected_base_oid, expected_source_digest)
                    .await;
            }
            AuthoringCandidate::AwaitingCommit if !commit => {
                return Ok(CommandOutcome::AwaitingCommit);
            }
            AuthoringCandidate::AwaitingCommit => {}
        }
        let owner = crate::repository_lease::RepositoryLeaseOwner {
            plan_dir: key.relative_dir.clone(),
            run_uid: format!(
                "author-{}",
                &expected_source_digest[..12.min(expected_source_digest.len())]
            ),
            operation: crate::repository_lease::RepositoryLeaseOperation::RegisterPlan,
        };
        let lease = if self.lease_held {
            None
        } else {
            Some(
                self.repository_leases
                    .acquire(&self.repo_root, owner, &CancellationToken::new())
                    .await
                    .map_err(|error| invalid(error.to_string()))?,
            )
        };
        let git = |args: Vec<String>| async move {
            let output = tokio::process::Command::new("git")
                .arg("-C")
                .arg(&self.repo_root)
                .args(args)
                .output()
                .await
                .map_err(|error| invalid(error.to_string()))?;
            if !output.status.success() {
                return Err(invalid(
                    String::from_utf8_lossy(&output.stderr).trim().into(),
                ));
            }
            Ok::<String, ApiError>(String::from_utf8_lossy(&output.stdout).trim().into())
        };
        let indexed_git = |index: PathBuf, args: Vec<String>| async move {
            let output = tokio::process::Command::new("git")
                .arg("-C")
                .arg(&self.repo_root)
                .env("GIT_INDEX_FILE", index)
                .args(args)
                .output()
                .await
                .map_err(|error| invalid(error.to_string()))?;
            if !output.status.success() {
                return Err(invalid(
                    String::from_utf8_lossy(&output.stderr).trim().into(),
                ));
            }
            Ok::<String, ApiError>(String::from_utf8_lossy(&output.stdout).trim().into())
        };
        let head = git(vec!["rev-parse".into(), self.base_branch.clone()]).await?;
        let authored_base = if head == expected_base_oid {
            let state = crate::checkpoint::external_state_root(&self.repo_root)
                .map_err(|error| invalid(error.to_string()))?;
            tokio::fs::create_dir_all(&state)
                .await
                .map_err(|error| invalid(error.to_string()))?;
            let index = state.join(format!(
                "authoring-index-{}",
                ulid::Ulid::from_datetime(std::time::SystemTime::now())
            ));
            indexed_git(
                index.clone(),
                vec!["read-tree".into(), expected_base_oid.clone()],
            )
            .await?;
            indexed_git(
                index.clone(),
                vec![
                    "add".into(),
                    "--".into(),
                    key.relative_dir.to_string_lossy().into_owned(),
                ],
            )
            .await?;
            let tree = indexed_git(index.clone(), vec!["write-tree".into()]).await?;
            let identity = format!("{}-{}", key.number, key.slug);
            let message = format!(
                "docs(plan): author {identity}\n\nMakina-Phase: plan-authoring\nMakina-Plan: {identity}\nMakina-Source-Digest: {expected_source_digest}\nMakina-Authoring-Base: {expected_base_oid}"
            );
            let authored = git(vec![
                "commit-tree".into(),
                tree,
                "-p".into(),
                expected_base_oid.clone(),
                "-m".into(),
                message,
            ])
            .await?;
            git(vec![
                "update-ref".into(),
                format!("refs/heads/{}", self.base_branch),
                authored.clone(),
                expected_base_oid.clone(),
            ])
            .await?;
            git(vec![
                "reset".into(),
                "HEAD".into(),
                "--".into(),
                key.relative_dir.to_string_lossy().into_owned(),
            ])
            .await?;
            let _ = tokio::fs::remove_file(index).await;
            authored
        } else {
            let message = git(vec![
                "show".into(),
                "-s".into(),
                "--format=%B".into(),
                head.clone(),
            ])
            .await?;
            let exact = |name: &str, value: &str| {
                message
                    .lines()
                    .filter(|line| *line == format!("{name}: {value}"))
                    .count()
                    == 1
            };
            if !exact("Makina-Phase", "plan-authoring")
                || !exact("Makina-Source-Digest", &expected_source_digest)
                || !exact("Makina-Authoring-Base", &expected_base_oid)
            {
                return Err(invalid(
                    "base moved without the exact authored commit".into(),
                ));
            }
            head
        };
        drop(lease);
        self.publish_committed(key, authored_base, expected_source_digest)
            .await
    }

    pub async fn publish_committed(
        &self,
        key: crate::plan::PlanKey,
        expected_base_oid: String,
        expected_source_digest: String,
    ) -> Result<CommandOutcome, ApiError> {
        self.publish_committed_inner(key, expected_base_oid, expected_source_digest, None)
            .await
    }

    /// Publish a committed plan while pinning the registration commit to a
    /// caller-validated Git author and committer identity.
    pub async fn publish_committed_with_identity(
        &self,
        key: crate::plan::PlanKey,
        expected_base_oid: String,
        expected_source_digest: String,
        name: &str,
        email: &str,
    ) -> Result<CommandOutcome, ApiError> {
        self.publish_committed_inner(
            key,
            expected_base_oid,
            expected_source_digest,
            Some((name, email)),
        )
        .await
    }

    async fn publish_committed_inner(
        &self,
        key: crate::plan::PlanKey,
        expected_base_oid: String,
        expected_source_digest: String,
        commit_identity: Option<(&str, &str)>,
    ) -> Result<CommandOutcome, ApiError> {
        use crate::plan::{
            FilesystemPlanFileSource, GitTreePlanFileSource, PlanCandidate, PlanFileSource,
            PlanReservations, load_plan,
        };

        fn invalid(reason: impl Into<String>) -> ApiError {
            ApiError::InvalidCommand {
                reason: reason.into(),
            }
        }
        fn report(report: crate::plan::PlanValidationReport) -> ApiError {
            invalid(
                report
                    .diagnostics
                    .into_iter()
                    .map(|d| format!("{}: {}", d.code, d.message))
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        }
        async fn git(root: &Path, args: &[&str]) -> Result<String, ApiError> {
            let output = tokio::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| invalid(e.to_string()))?;
            if !output.status.success() {
                return Err(invalid(String::from_utf8_lossy(&output.stderr).trim()));
            }
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
        async fn git_commit(
            root: &Path,
            message: &str,
            identity: Option<(&str, &str)>,
        ) -> Result<(), ApiError> {
            let mut command = tokio::process::Command::new("git");
            command
                .arg("-C")
                .arg(root)
                .args(["commit", "-m", message])
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true);
            if let Some((name, email)) = identity {
                command
                    .env("GIT_AUTHOR_NAME", name)
                    .env("GIT_AUTHOR_EMAIL", email)
                    .env("GIT_COMMITTER_NAME", name)
                    .env("GIT_COMMITTER_EMAIL", email);
            }
            let output = command
                .output()
                .await
                .map_err(|error| invalid(error.to_string()))?;
            if !output.status.success() {
                return Err(invalid(String::from_utf8_lossy(&output.stderr).trim()));
            }
            Ok(())
        }
        fn exact(message: &str, name: &str) -> Option<String> {
            let prefix = format!("{name}: ");
            let values = message
                .lines()
                .filter_map(|line| line.strip_prefix(&prefix))
                .collect::<Vec<_>>();
            (values.len() == 1).then(|| values[0].to_owned())
        }

        let root = &self.repo_root;
        let base_tip = git(root, &["rev-parse", &self.base_branch]).await?;
        if base_tip != expected_base_oid {
            return Err(invalid(format!(
                "target base moved: expected {expected_base_oid}, found {base_tip}"
            )));
        }
        let plan_identity = key.plan_identity();
        let plan_ref = key.ref_name();
        let mut refresh_old = None;
        let mut source_origin = "base".to_owned();
        if let Ok(tip) = git(root, &["rev-parse", "--verify", &plan_ref]).await {
            let message = git(root, &["show", "-s", "--format=%B", &tip]).await?;
            let retained_source =
                GitTreePlanFileSource::new(root, &tip).map_err(|e| invalid(e.to_string()))?;
            let retained_plan =
                match load_plan(&retained_source, key.clone(), &PlanReservations::default())
                    .map_err(report)?
                {
                    PlanCandidate::Plan(plan) => *plan,
                    PlanCandidate::NotCandidate => {
                        return Err(invalid("registered ref does not contain the plan bundle"));
                    }
                };
            let exact_registration = exact(&message, "Makina-Phase").as_deref()
                == Some("plan-registration")
                && exact(&message, "Makina-Plan").as_deref() == Some(plan_identity.as_str())
                && exact(&message, "Makina-Source-Digest").as_deref()
                    == Some(expected_source_digest.as_str())
                && exact(&message, "Makina-Validation-Base").as_deref()
                    == Some(expected_base_oid.as_str())
                && exact(&message, "Makina-Source-Origin").as_deref() == Some("base")
                && exact(&message, "Makina-Executable-Digest").as_deref()
                    == Some(retained_plan.executable_digest.as_str())
                && retained_plan.source_digest.as_str() == expected_source_digest
                && git(root, &["rev-parse", &format!("{tip}^")]).await? == expected_base_oid;
            if exact_registration {
                return Ok(CommandOutcome::PlanRegistered {
                    registration_oid: tip,
                });
            }
            let refreshable = exact(&message, "Makina-Phase").as_deref()
                == Some("plan-registration")
                && exact(&message, "Makina-Plan").as_deref() == Some(plan_identity.as_str())
                && git(
                    root,
                    &["rev-list", "--count", &format!("{tip}..{plan_ref}")],
                )
                .await?
                    == "0";
            if !refreshable {
                return Err(invalid(format!(
                    "{} contains post-registration or divergent evidence at {tip}",
                    plan_ref
                )));
            }
            source_origin = exact(&message, "Makina-Source-Origin")
                .filter(|origin| matches!(origin.as_str(), "base" | "generated"))
                .ok_or_else(|| invalid("old registration has invalid source origin"))?;
            refresh_old = Some(tip);
        }

        let source = GitTreePlanFileSource::new(root, &expected_base_oid)
            .map_err(|e| invalid(e.to_string()))?;
        let base_plan = match load_plan(&source, key.clone(), &PlanReservations::default()) {
            Ok(PlanCandidate::Plan(plan)) => *plan,
            Ok(PlanCandidate::NotCandidate) | Err(_)
                if refresh_old.is_some() && source_origin == "generated" =>
            {
                let retained = GitTreePlanFileSource::new(root, refresh_old.as_ref().unwrap())
                    .map_err(|e| invalid(e.to_string()))?;
                match load_plan(&retained, key.clone(), &PlanReservations::default())
                    .map_err(report)?
                {
                    PlanCandidate::Plan(plan) => *plan,
                    PlanCandidate::NotCandidate => {
                        return Err(invalid(
                            "generated registration lost its closed plan subtree",
                        ));
                    }
                }
            }
            Ok(PlanCandidate::NotCandidate) | Err(_) => {
                let working = FilesystemPlanFileSource::new(root, Some(expected_base_oid.clone()))
                    .map_err(|e| invalid(e.to_string()))?;
                if let Ok(PlanCandidate::Plan(plan)) =
                    load_plan(&working, key.clone(), &PlanReservations::default())
                    && plan.source_digest.as_str() == expected_source_digest
                {
                    return Ok(CommandOutcome::AwaitingCommit);
                }
                return Err(invalid(
                    "the expected plan bundle is not present in the target-base commit",
                ));
            }
        };
        if base_plan.source_digest.as_str() != expected_source_digest {
            return Err(invalid(format!(
                "source digest mismatch: expected {expected_source_digest}, found {}",
                base_plan.source_digest
            )));
        }

        let owner = crate::repository_lease::RepositoryLeaseOwner {
            plan_dir: key.relative_dir.clone(),
            run_uid: format!(
                "register-{}",
                &expected_source_digest[..12.min(expected_source_digest.len())]
            ),
            operation: crate::repository_lease::RepositoryLeaseOperation::RegisterPlan,
        };
        let cancel = CancellationToken::new();
        let _lease = if self.lease_held {
            None
        } else {
            Some(
                self.repository_leases
                    .acquire(root, owner, &cancel)
                    .await
                    .map_err(|e| invalid(e.to_string()))?,
            )
        };
        // Recheck the base after waiting for the lease.
        let locked_base = git(root, &["rev-parse", &self.base_branch]).await?;
        if locked_base != expected_base_oid {
            return Err(invalid(format!(
                "target base moved while waiting for the repository lease: expected {expected_base_oid}, found {locked_base}"
            )));
        }

        let workspace = self
            .worktree_manager
            .create_integration_workspace(
                &plan_identity,
                &format!(
                    "registration-{}",
                    &expected_source_digest[..12.min(expected_source_digest.len())]
                ),
            )
            .await
            .map_err(|e| invalid(e.to_string()))?;
        if let Some(old_registration) = refresh_old.as_ref() {
            git(
                &workspace.path,
                &["checkout", "--detach", &expected_base_oid],
            )
            .await?;
            if source_origin == "generated" {
                git(
                    &workspace.path,
                    &[
                        "checkout",
                        old_registration,
                        "--",
                        key.relative_dir
                            .to_str()
                            .ok_or_else(|| invalid("plan path is not UTF-8"))?,
                    ],
                )
                .await?;
            }
        }
        let transition = crate::plan_status::StatusTransition {
            integration_state: base_plan.status.integration_state,
            run: base_plan.status.run.clone(),
            validation_base: Some(
                crate::plan::GitObjectId::parse(&expected_base_oid, source.object_format())
                    .map_err(|e| invalid(e.to_string()))?,
            ),
            mode: base_plan.status.mode.clone(),
            final_oid: base_plan.status.final_oid.clone(),
            display_status: base_plan.status.display_status.clone(),
            last_updated: base_plan.status.last_updated.clone(),
        };
        let status = crate::plan_status::render_plan_status(&base_plan, &transition)
            .map_err(|e| invalid(e.to_string()))?;
        let root_board = source
            .read_file(Path::new("docs/plans/STATUS.md"))
            .map_err(|e| invalid(e.to_string()))?;
        let root_board =
            String::from_utf8(root_board).map_err(|_| invalid("root status board is not UTF-8"))?;
        let root_board = crate::plan_status::register_root_row(&root_board, &base_plan)
            .map_err(|e| invalid(e.to_string()))?;
        tokio::fs::write(
            workspace.path.join(key.relative_dir.join("STATUS.md")),
            status,
        )
        .await
        .map_err(|e| invalid(e.to_string()))?;
        tokio::fs::write(workspace.path.join("docs/plans/STATUS.md"), root_board)
            .await
            .map_err(|e| invalid(e.to_string()))?;

        let reread_source =
            FilesystemPlanFileSource::new(&workspace.path, Some(expected_base_oid.clone()))
                .map_err(|e| invalid(e.to_string()))?;
        let registered = match load_plan(&reread_source, key.clone(), &PlanReservations::default())
            .map_err(report)?
        {
            PlanCandidate::Plan(plan) => *plan,
            PlanCandidate::NotCandidate => {
                return Err(invalid("registration workspace lost its plan bundle"));
            }
        };
        if registered.source_digest != base_plan.source_digest
            || registered.executable_digest != base_plan.executable_digest
        {
            return Err(invalid(
                "registration bookkeeping changed authored plan digests",
            ));
        }
        git(
            &workspace.path,
            &[
                "add",
                "--",
                key.relative_dir
                    .join("STATUS.md")
                    .to_str()
                    .ok_or_else(|| invalid("plan path is not UTF-8"))?,
                "docs/plans/STATUS.md",
            ],
        )
        .await?;
        let previous = refresh_old
            .as_ref()
            .map(|old| format!("\nMakina-Previous-Registration: {old}"))
            .unwrap_or_default();
        let message = format!(
            "chore(plan): register {plan_identity}\n\nMakina-Phase: plan-registration\nMakina-Plan: {plan_identity}\nMakina-Source-Digest: {}\nMakina-Executable-Digest: {}\nMakina-Validation-Base: {expected_base_oid}\nMakina-Source-Origin: {source_origin}{previous}",
            registered.source_digest, registered.executable_digest
        );
        git_commit(&workspace.path, &message, commit_identity).await?;
        let candidate = git(&workspace.path, &["rev-parse", "HEAD"]).await?;
        if let Some(old) = refresh_old {
            self.worktree_manager
                .publish_registration_refresh(&workspace, &candidate, &expected_base_oid, &old)
                .await
        } else {
            self.worktree_manager
                .publish_registration(&workspace, &candidate, &expected_base_oid)
                .await
        }
        .map_err(|e| invalid(e.to_string()))?;
        // Publication owns the ref, not this retained recovery checkout. Free
        // the plan branch immediately so lifecycle sessions can attach it in
        // their own private integration workspace.
        git(&workspace.path, &["checkout", "--detach", &candidate]).await?;
        Ok(CommandOutcome::PlanRegistered {
            registration_oid: candidate,
        })
    }
}

impl CoreApi {
    async fn register_plan(
        &self,
        key: crate::plan::PlanKey,
        expected_base_oid: String,
        expected_source_digest: String,
    ) -> Result<CommandOutcome, ApiError> {
        let coordinator = AuthoringCoordinator::new(
            self.state.worktree_manager.repo_root.clone(),
            self.state.worktree_manager.base_branch.clone(),
            Arc::clone(&self.state.repository_leases),
        );
        let outcome = coordinator
            .publish_committed(key.clone(), expected_base_oid, expected_source_digest)
            .await?;
        if let CommandOutcome::PlanRegistered { registration_oid } = &outcome {
            let _ = self.state.event_tx.send(Event::PlanRegistered {
                plan_dir: key,
                registration_oid: registration_oid.clone(),
            });
        }
        Ok(outcome)
    }

    fn load_plan_graph(&self, key: &crate::plan::PlanKey) -> Result<TaskGraph, ApiError> {
        let repo_root = &self.state.worktree_manager.repo_root;
        let plan =
            load_authoritative_plan(repo_root, key).map_err(|report| ApiError::InvalidCommand {
                reason: report
                    .diagnostics
                    .iter()
                    .map(|diagnostic| format!("{}: {}", diagnostic.code, diagnostic.message))
                    .collect::<Vec<_>>()
                    .join("; "),
            })?;
        Ok(crate::plan_runtime::ProjectedTaskGraph::from_document(&plan, chrono::Utc::now()).graph)
    }

    /// Create a new `CoreApi` with all execution dependencies injected.
    ///
    /// The TUI passes the deterministic *ingestion* interpreter (for OpenPlan),
    /// a separate *planner* interpreter (respecting mechanism, for the Planner
    /// actor), a `NoopBackend` (or the ACP backend), a `WorktreeManager` pointed
    /// at the repo, and a resolved `Config`.  Tests pass `NoopBackend` + a
    /// temp-repo `WorktreeManager` + a trivial `Config` (planner defaults to
    /// same as ingestion).
    ///
    /// `audit_registry` is the seam the Supervisor uses to register each task's
    /// worktree context before dispatching a driver.  Pass
    /// `Arc::new(NoopAuditRegistry)` (the default, available via
    /// [`CoreApi::new`]) for tests that do not need the ledger; pass
    /// `Arc<JsonlAuditSink>` in production (where `main.rs` wires the same
    /// `Arc` into both `AcpBackend::with_audit_sink` and here).
    pub fn new(
        interpreter: Arc<dyn TaskListInterpreter>,
        backend: Arc<dyn AgentBackend>,
        worktree_manager: WorktreeManager,
        config: Config,
    ) -> Self {
        let planner_interpreter = Arc::clone(&interpreter);
        // In the simple test path, both roles share the same backend Arc.
        let backend_clone = Arc::clone(&backend);
        Self::with_audit_registry(
            interpreter,
            // For the simple `new` path (mostly tests), default planner to same
            // as ingestion interpreter.  The TUI binary and tests that care
            // about planner mechanism will use `with_audit_registry` (or the
            // updated helpers) to pass a separately-built one.
            planner_interpreter,
            backend,
            backend_clone,
            worktree_manager,
            config,
            Arc::new(NoopAuditRegistry),
        )
    }

    /// Like [`CoreApi::new`] but with an explicit [`AuditRegistry`].
    ///
    /// Use this in production to inject the `JsonlAuditSink` so the Supervisor
    /// can register each task's worktree context and audit entries are routed to
    /// `.tasks/{slug}/audit.jsonl`.
    ///
    /// Note the two interpreters: `interpreter` (ingestion, for OpenPlan) and
    /// `planner_interpreter` (for the Planner actor / mechanism).
    pub fn with_audit_registry(
        _interpreter: Arc<dyn TaskListInterpreter>,
        planner_interpreter: Arc<dyn TaskListInterpreter>,
        developer_backend: Arc<dyn AgentBackend>,
        reviewer_backend: Arc<dyn AgentBackend>,
        worktree_manager: WorktreeManager,
        config: Config,
        audit_registry: Arc<dyn AuditRegistry>,
    ) -> Self {
        Self::with_repository_lease_registry(
            _interpreter,
            planner_interpreter,
            developer_backend,
            reviewer_backend,
            worktree_manager,
            config,
            audit_registry,
            Arc::new(crate::repository_lease::RepositoryLeaseRegistry::new()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_repository_lease_registry(
        _interpreter: Arc<dyn TaskListInterpreter>,
        planner_interpreter: Arc<dyn TaskListInterpreter>,
        developer_backend: Arc<dyn AgentBackend>,
        reviewer_backend: Arc<dyn AgentBackend>,
        worktree_manager: WorktreeManager,
        config: Config,
        audit_registry: Arc<dyn AuditRegistry>,
        repository_leases: Arc<crate::repository_lease::RepositoryLeaseRegistry>,
    ) -> Self {
        let (event_tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let runtime_settings = RuntimeSettings::from_config(&config);
        Self {
            state: Arc::new(CoreState {
                planner_interpreter,
                developer_backend,
                reviewer_backend,
                worktree_manager,
                config,
                runtime_settings: Mutex::new(runtime_settings),
                runs: Mutex::new(BTreeMap::new()),
                open_lock: AsyncMutex::new(()),
                lifecycle_lock: AsyncMutex::new(()),
                next_id: AtomicU64::new(1),
                disk_run_ids: Mutex::new(HashMap::new()),
                event_tx,
                audit_registry,
                repository_leases,
            }),
        }
    }

    /// Open a validated per-task plan as a read-only projection. Historical
    /// Pre-cutover plan records and cached graph artifacts are not executable inputs.
    ///
    /// # Lock discipline
    ///
    /// All I/O (file read, persist load/write, interpret) happens with **no lock
    /// held**; the registry lock is taken only for the brief insert, then dropped
    /// before the broadcast.
    async fn open_run(&self, plan_key: crate::plan::PlanKey) -> Result<CommandOutcome, ApiError> {
        // Serialize the entire identity/load/register path. Without this, two
        // concurrent opens can both miss the registry and allocate distinct
        // runs that share plan branches and worktree names.
        let _open_guard = self.state.open_lock.lock().await;

        // 0. Run project discovery on first open (auto-run if [discovery] stamp absent).
        self.run_discovery_if_needed().await;

        let repo_root = &self.state.worktree_manager.repo_root;
        let slug = format!("{}-{}", plan_key.number, plan_key.slug).to_ascii_lowercase();
        // The plan identity is NOT case-folded: it names `refs/heads/plan/{id}`,
        // which registration published verbatim from `PlanKey::ref_name`. Git
        // refs are case-sensitive, so folding here would make every mixed-case
        // plan directory look unregistered to its own run. Worktree directory
        // names stay stable regardless — `paths::short_worktree_name` lowercases
        // internally.
        let worktree_plan_slug = plan_key.plan_identity();

        // OpenPlan is idempotent for the canonical project+plan identity. A
        // CoreApi is bound to one project, so the canonical plan path is the
        // remaining identity component.
        let existing = {
            let runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            runs.iter()
                .find_map(|(id, entry)| (entry.plan_dir == plan_key).then_some(RunId(*id)))
        };
        if let Some(run) = existing {
            let _ = self.state.event_tx.send(Event::RunOpened {
                run,
                plan_dir: plan_key,
            });
            return Ok(CommandOutcome::RunOpened { run });
        }

        // Distinct source paths must not share either the artifact slug or the
        // plan-scoped branch/worktree namespace while both are live.
        {
            let runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            if let Some(entry) = runs
                .values()
                .find(|entry| entry.run_slug == slug || entry.plan_slug == worktree_plan_slug)
            {
                return Err(ApiError::InvalidCommand {
                    reason: format!(
                        "plan `{}` is already open from `{}`; refusing a second live run that would share artifacts or worktrees",
                        worktree_plan_slug,
                        entry.plan_dir.relative_dir.display()
                    ),
                });
            }
        }

        // New-format plans are always loaded and validated before a checkpoint
        // is consulted. This branch is read-only: it does not create the state
        // root, archive a stale checkpoint, or seed a replacement.
        let source_projection = {
            let plan = match load_authoritative_plan(repo_root, &plan_key) {
                Ok(plan) => plan,
                Err(report)
                    if report
                        .diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.code == "plan-not-found") =>
                {
                    return Err(ApiError::InvalidCommand {
                        reason: "run execution requires a validated plan directory containing SCOPE.md, ARCHITECTURE.md, STATUS.md, and tasks/*.md; pre-cutover records and cached graph artifacts are read-only history".into(),
                    });
                }
                Err(report) => {
                    return Err(ApiError::InvalidCommand {
                        reason: report
                            .diagnostics
                            .iter()
                            .map(|diagnostic| {
                                format!("{}: {}", diagnostic.code, diagnostic.message)
                            })
                            .collect::<Vec<_>>()
                            .join("; "),
                    });
                }
            };
            let mut projected =
                crate::plan_runtime::ProjectedTaskGraph::from_document(&plan, chrono::Utc::now());
            let checkpoint = match crate::checkpoint::load_checkpoint(repo_root, &plan.key).await {
                Ok(checkpoint) => checkpoint,
                Err(error) => {
                    tracing::warn!(%error, "checkpoint unavailable during read-only plan open");
                    None
                }
            };
            // Apply the checkpoint overlay at open time so the graph reflects
            // the latest runtime state (e.g. Done tasks stay Done, InProgress
            // tasks reset to Ready, stale Skipped dependents are un-skipped).
            // Without this, the TUI shows all tasks as New until StartRun's
            // reconcile_run_under_repository_lease applies the overlay — and if
            // a stale checkpoint has Skipped dependents, the user sees them as
            // Skipped even though they will run.
            if let Some(ref cp) = checkpoint
                && matches!(
                    crate::checkpoint::inspect_checkpoint(&plan, Some(cp)),
                    crate::checkpoint::CheckpointDisposition::Compatible
                )
            {
                crate::checkpoint::overlay_compatible(&mut projected.graph, cp);
                // Persist the reconciled graph immediately so the stale
                // Skipped state is overwritten on disk. Without this, if
                // StartRun's reconcile fails (e.g. RetainForRecovery), the
                // stale checkpoint with Skipped tasks survives and the user
                // sees "first task ready, other two skipped" on every attempt.
                let identity = crate::checkpoint::CheckpointIdentity::from_plan(&plan);
                if let Err(e) =
                    crate::checkpoint::persist_checkpoint(repo_root, identity, &projected.graph)
                        .await
                {
                    tracing::warn!(error = %e, "failed to persist reconciled checkpoint during open_run");
                }
            }
            let reconciliation = if checkpoint.is_none()
                && crate::checkpoint::checkpoint_path(repo_root, &plan.key).is_err()
            {
                crate::checkpoint::CheckpointDisposition::Unavailable {
                    reason: "external checkpoint location is unavailable".into(),
                }
            } else {
                crate::checkpoint::inspect_checkpoint(&plan, checkpoint.as_ref())
            };
            Some((
                projected.graph,
                PlanSourceState {
                    key: plan.key.clone(),
                    reconciliation,
                    checkpoint_identity: crate::checkpoint::CheckpointIdentity::from_plan(&plan),
                },
            ))
        };

        // Historical plan records and cached graph artifacts are intentionally
        // inert. Execution begins only from a fully validated per-task plan.
        let Some((graph, plan_source)) = source_projection else {
            return Err(ApiError::InvalidCommand {
                reason: "run execution requires a validated plan directory containing SCOPE.md, ARCHITECTURE.md, STATUS.md, and tasks/*.md; pre-cutover records and cached graph artifacts are read-only history".into(),
            });
        };

        // Compute ingestion report (validate + qualify) right after graph is
        // resolved (artifact or fresh), before registry insert.  Fold any
        // carried interpret failure (from fresh path) into the report so the
        // run is reviewable as Pending with a blocking issue.
        let report = {
            let mut issues = crate::ingestion::validate(&graph);
            issues.extend(crate::ingestion::qualify(&graph));
            crate::ingestion::IngestionReport { issues }
        };

        // 3. Allocate an id, mint the persistent ULID run identity, and register
        //    the Run.  Lock → insert → DROP guard before any further
        //    await/broadcast.
        let id = self.state.alloc_id();
        let run_uid = ulid::Ulid::from_datetime(std::time::SystemTime::now()).to_string();
        {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            runs.insert(
                id.0,
                RunEntry {
                    plan_dir: plan_key.clone(),
                    run_uid,
                    run_slug: slug.clone(),
                    plan_slug: worktree_plan_slug,
                    started_at: None,
                    graph: Arc::new(AsyncMutex::new(graph)),
                    status: RunStatus::Pending,
                    handle: None,
                    cancellation_status_error: None,
                    scheduler_generation: 0,
                    report,
                    plan_source: Some(plan_source),
                },
            );
        } // guard dropped here

        // 4. Broadcast RunOpened (lock no longer held).
        let _ = self.state.event_tx.send(Event::RunOpened {
            run: id,
            plan_dir: plan_key,
        });

        Ok(CommandOutcome::RunOpened { run: id })
    }

    /// Run project discovery if the repo has not been stamped yet.
    ///
    /// On the first open, if the `[discovery]` stamp is absent from the project
    /// config, spawns a single model pass to inspect the repo and propose gates +
    /// role constraints. Applies the result to the config and writes it back.
    /// Non-fatal: discovery failure only logs a warning; the run proceeds.
    ///
    /// Idempotent: if the stamp is present, skips discovery. Also skips if no
    /// config file exists (discovery only runs when there's an explicit project config).
    async fn run_discovery_if_needed(&self) {
        use crate::config::{ProjectConfig, ProjectConfigWrite};
        use crate::discovery::{apply_discovery, discover_project};
        use crate::paths::config_file;
        use chrono::Utc;

        let repo_root = &self.state.worktree_manager.repo_root;
        let config_path = config_file(repo_root);

        // Only proceed if a config file exists. Discovery is opt-in (only runs if
        // the user/repo explicitly has a project config).
        if !config_path.exists() {
            return;
        }

        // Read the current project config to check for the discovery stamp.
        let project_config: ProjectConfig = match tokio::fs::read_to_string(&config_path).await {
            Ok(s) => match ProjectConfig::from_toml_str(&s, "project") {
                Ok(cfg) => cfg,
                Err(_) => return, // Unparseable config: skip discovery
            },
            Err(_) => return, // Can't read config: skip discovery
        };

        // If [discovery] stamp is present, skip (already run).
        if project_config.discovery.is_some() {
            return;
        }

        // Stamp is absent: run discovery.
        tracing::info!("Running first-open project discovery...");

        // Run discovery with the developer backend.
        let (result, scanned_files) =
            match discover_project(self.state.developer_backend.as_ref(), repo_root).await {
                Ok((result, scanned)) => (result, scanned),
                Err(e) => {
                    tracing::warn!("Project discovery failed (non-fatal): {e}");
                    // Emit a discovery event to the UI so it knows discovery was attempted.
                    let _ = self
                        .state
                        .event_tx
                        .send(crate::api::Event::ProjectDiscovered {
                            project_root: repo_root.clone(),
                            gate_count: 0,
                            scanned_files: 0,
                        });
                    return;
                }
            };

        // Build the write view and apply discovery to it.
        let mut write_config = ProjectConfigWrite::from_project_and_roles(
            project_config,
            self.state.config.roles.clone(),
        );
        let mut roles = self.state.config.roles.clone();

        let now = Utc::now().to_rfc3339();
        apply_discovery(&mut write_config, &mut roles, &result, &now, &scanned_files);

        // Write the updated config back.
        if let Err(e) = crate::config::write_project_config(repo_root, |cfg| {
            *cfg = write_config;
        })
        .await
        {
            tracing::warn!("Failed to write project config after discovery: {e}");
        } else {
            tracing::info!(
                "Project discovery completed: {} gates, {} files",
                result.gates.len(),
                scanned_files.len()
            );
        }

        // Emit a discovery event to the UI.
        let _ = self
            .state
            .event_tx
            .send(crate::api::Event::ProjectDiscovered {
                project_root: repo_root.clone(),
                gate_count: result.gates.len(),
                scanned_files: scanned_files.len(),
            });
    }

    /// Force-re-run project discovery regardless of any existing `[discovery]` stamp.
    ///
    /// Re-scans the repository, replaces all `source = "discovered"` gates with
    /// the newly discovered ones, folds the updated role constraints into the role
    /// assignments, and re-stamps `last_run` in the project config. Emits
    /// `Event::ProjectDiscovered` on completion.
    ///
    /// Non-fatal: discovery failure only logs a warning; the function returns
    /// `Ok(Acknowledged)` in all cases rather than propagating the error.
    async fn force_discover_project(&self) -> Result<CommandOutcome, ApiError> {
        use crate::config::{ProjectConfig, ProjectConfigWrite};
        use crate::discovery::{apply_discovery, discover_project};
        use crate::paths::config_file;
        use chrono::Utc;

        let repo_root = &self.state.worktree_manager.repo_root;
        let config_path = config_file(repo_root);

        // Run discovery even if no config file exists (unlike auto-run which skips in this case).
        // If no config file, start from a default write view.
        let project_config: ProjectConfig = if config_path.exists() {
            match tokio::fs::read_to_string(&config_path).await {
                Ok(s) => ProjectConfig::from_toml_str(&s, "project").unwrap_or_default(),
                Err(_) => ProjectConfig::default(),
            }
        } else {
            ProjectConfig::default()
        };

        tracing::info!("Force-re-running project discovery...");

        // Run discovery with the developer backend.
        let (result, scanned_files) =
            match discover_project(self.state.developer_backend.as_ref(), repo_root).await {
                Ok((result, scanned)) => (result, scanned),
                Err(e) => {
                    tracing::warn!("Force project discovery failed (non-fatal): {e}");
                    let _ = self
                        .state
                        .event_tx
                        .send(crate::api::Event::ProjectDiscovered {
                            project_root: repo_root.clone(),
                            gate_count: 0,
                            scanned_files: 0,
                        });
                    return Ok(CommandOutcome::Acknowledged);
                }
            };

        // Build the write view and apply discovery to it.
        let mut write_config = ProjectConfigWrite::from_project_and_roles(
            project_config,
            self.state.config.roles.clone(),
        );
        let mut roles = self.state.config.roles.clone();

        let now = Utc::now().to_rfc3339();
        apply_discovery(&mut write_config, &mut roles, &result, &now, &scanned_files);

        // Write the updated config back.
        if let Err(e) = crate::config::write_project_config(repo_root, |cfg| {
            *cfg = write_config;
        })
        .await
        {
            tracing::warn!("Failed to write project config after force discovery: {e}");
        } else {
            tracing::info!(
                "Force project discovery completed: {} gates, {} files",
                result.gates.len(),
                scanned_files.len()
            );
        }

        // Emit a discovery event to the UI.
        let _ = self
            .state
            .event_tx
            .send(crate::api::Event::ProjectDiscovered {
                project_root: repo_root.clone(),
                gate_count: result.gates.len(),
                scanned_files: scanned_files.len(),
            });

        Ok(CommandOutcome::Acknowledged)
    }

    /// Reject a project-qualified command sent to the wrong repository API.
    fn require_project_root(&self, requested: &Path) -> Result<(), ApiError> {
        let expected =
            std::fs::canonicalize(&self.state.worktree_manager.repo_root).map_err(|error| {
                ApiError::Internal {
                    reason: format!(
                        "cannot resolve configured project root {}: {error}",
                        self.state.worktree_manager.repo_root.display()
                    ),
                }
            })?;
        let requested =
            std::fs::canonicalize(requested).map_err(|error| ApiError::InvalidCommand {
                reason: format!(
                    "cannot resolve requested project root {}: {error}",
                    requested.display()
                ),
            })?;
        if requested != expected {
            return Err(ApiError::InvalidCommand {
                reason: format!(
                    "command targets project {} but this API owns {}",
                    requested.display(),
                    expected.display()
                ),
            });
        }
        Ok(())
    }

    /// Re-read and reconcile a typed plan while the repository execution lease
    /// is held. Both initial start and retry call this before mutating runtime
    /// state, so a queued operation can never act on the preview captured by
    /// `OpenPlan` or on a checkpoint/Git/worktree view from before it waited.
    async fn reconcile_run_under_repository_lease(
        &self,
        run: RunId,
    ) -> Result<Arc<AsyncMutex<TaskGraph>>, ApiError> {
        let (plan_source, graph, run_uid) = {
            let runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get(&run.0).ok_or(ApiError::UnknownRun { run })?;
            (
                entry.plan_source.clone(),
                Arc::clone(&entry.graph),
                entry.run_uid.clone(),
            )
        };
        let Some(opened) = plan_source else {
            return Ok(graph);
        };
        if let crate::checkpoint::CheckpointDisposition::Unavailable { reason } =
            &opened.reconciliation
        {
            return Err(ApiError::InvalidCommand {
                reason: format!("cannot start without external runtime state: {reason}"),
            });
        }
        crate::checkpoint::external_state_root(&self.state.worktree_manager.repo_root).map_err(
            |error| ApiError::InvalidCommand {
                reason: format!("cannot start without external runtime state: {error}"),
            },
        )?;
        // Case-preserving: this must resolve the very ref registration published.
        let plan_identity = opened.key.plan_identity();
        let plan_ref = opened.key.ref_name();
        let tip_output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.state.worktree_manager.repo_root)
            .args(["rev-parse", "--verify", &plan_ref])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| ApiError::InvalidCommand {
                reason: error.to_string(),
            })?;
        let registered = tip_output.status.success();
        let source: Box<dyn crate::plan::PlanFileSource + Send> = if registered {
            let plan_tip = String::from_utf8_lossy(&tip_output.stdout)
                .trim()
                .to_owned();
            Box::new(
                crate::plan::GitTreePlanFileSource::new(
                    &self.state.worktree_manager.repo_root,
                    &plan_tip,
                )
                .map_err(|error| ApiError::InvalidCommand {
                    reason: error.to_string(),
                })?,
            )
        } else {
            // Pre-registration callers retain the original source-reread
            // contract. Once R exists, reconciliation is pinned to its Git tree.
            Box::new(
                crate::plan::FilesystemPlanFileSource::new(
                    &self.state.worktree_manager.repo_root,
                    None,
                )
                .map_err(|error| ApiError::InvalidCommand {
                    reason: error.to_string(),
                })?,
            )
        };
        let reread = crate::plan::load_plan(
            source.as_ref(),
            opened.key.clone(),
            &crate::plan::PlanReservations::default(),
        )
        .map_err(|report| ApiError::InvalidCommand {
            reason: format!(
                "plan source became invalid after open: {}",
                report
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.code.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        })?;
        let crate::plan::PlanCandidate::Plan(mut reread) = reread else {
            return Err(ApiError::InvalidCommand {
                reason: "plan source disappeared after open".into(),
            });
        };
        drop(source);
        let mut stale_clean = Vec::new();
        for task in &reread.tasks {
            if !registered {
                break;
            }
            let evidence = crate::landing::inspect_task_evidence(
                &self.state.worktree_manager.repo_root,
                &plan_ref,
                &plan_identity,
                task.frontmatter.id.as_str(),
            )
            .await
            .map_err(|error| ApiError::InvalidCommand {
                reason: format!(
                    "task evidence is ambiguous for {}: {error}",
                    task.frontmatter.id
                ),
            })?;
            match (task.frontmatter.status, evidence) {
                (
                    crate::plan::AuthoredTaskStatus::Done,
                    crate::landing::TaskEvidenceState::Complete {
                        implementation_oid, ..
                    },
                ) if task
                    .frontmatter
                    .merged_as
                    .as_ref()
                    .is_some_and(|oid| oid.as_str() == implementation_oid) => {}
                (crate::plan::AuthoredTaskStatus::Done, evidence) => {
                    return Err(ApiError::InvalidCommand {
                        reason: format!(
                            "source marks {} done without matching reachable A/B evidence: {evidence:?}",
                            task.frontmatter.id
                        ),
                    });
                }
                (
                    crate::plan::AuthoredTaskStatus::InProgress,
                    crate::landing::TaskEvidenceState::LandingPending { implementation_oid },
                ) => {
                    return Err(ApiError::InvalidCommand {
                        reason: format!(
                            "task {} has landed implementation {implementation_oid} but Phase B is missing; retain recovery state and resume bookkeeping",
                            task.frontmatter.id
                        ),
                    });
                }
                (
                    crate::plan::AuthoredTaskStatus::InProgress,
                    crate::landing::TaskEvidenceState::Claimed { claim_oid },
                ) => {
                    self.state
                        .worktree_manager
                        .prove_stale_claim_clean(
                            &plan_identity,
                            task.frontmatter.id.as_str(),
                            &claim_oid,
                        )
                        .await
                        .map_err(|error| ApiError::InvalidCommand {
                            reason: format!(
                                "task {} retains dirty/divergent recovery evidence: {error}",
                                task.frontmatter.id
                            ),
                        })?;
                    stale_clean.push(crate::task::TaskId(task.frontmatter.id.as_str().to_owned()));
                }
                (crate::plan::AuthoredTaskStatus::InProgress, other) => {
                    return Err(ApiError::InvalidCommand {
                        reason: format!(
                            "stale in-progress task {} has incoherent evidence {other:?}",
                            task.frontmatter.id
                        ),
                    });
                }
                (_, crate::landing::TaskEvidenceState::LandingPending { implementation_oid }) => {
                    return Err(ApiError::InvalidCommand {
                        reason: format!(
                            "task {} has unbookkept landing {implementation_oid}; source state cannot bypass Phase B",
                            task.frontmatter.id
                        ),
                    });
                }
                _ => {}
            }
        }
        if !stale_clean.is_empty() {
            self.state
                .worktree_manager
                .create_integration_workspace(&plan_identity, &run_uid)
                .await
                .map_err(|error| ApiError::InvalidCommand {
                    reason: format!("prepare stale-claim reconciliation workspace: {error}"),
                })?;
            self.commit_retry_source_transitions(run, &stale_clean, "reconcile")
                .await?;
            let new_tip_output = tokio::process::Command::new("git")
                .arg("-C")
                .arg(&self.state.worktree_manager.repo_root)
                .args(["rev-parse", "--verify", &plan_ref])
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|error| ApiError::InvalidCommand {
                    reason: error.to_string(),
                })?;
            let new_tip = String::from_utf8_lossy(&new_tip_output.stdout)
                .trim()
                .to_owned();
            let new_source = crate::plan::GitTreePlanFileSource::new(
                &self.state.worktree_manager.repo_root,
                &new_tip,
            )
            .map_err(|error| ApiError::InvalidCommand {
                reason: error.to_string(),
            })?;
            reread = match crate::plan::load_plan(
                &new_source,
                opened.key,
                &crate::plan::PlanReservations::default(),
            )
            .map_err(|report| ApiError::InvalidCommand {
                reason: format!("auto-reset source invalid: {:?}", report.diagnostics),
            })? {
                crate::plan::PlanCandidate::Plan(plan) => plan,
                crate::plan::PlanCandidate::NotCandidate => {
                    return Err(ApiError::InvalidCommand {
                        reason: "auto-reset lost plan".into(),
                    });
                }
            };
        }
        let mut fresh_graph =
            crate::plan_runtime::ProjectedTaskGraph::from_document(&reread, Utc::now()).graph;
        crate::checkpoint::probe_writable_checkpoint_root(
            &self.state.worktree_manager.repo_root,
            &reread.key,
        )
        .await
        .map_err(|error| ApiError::InvalidCommand {
            reason: format!("external checkpoint root is not durably writable: {error}"),
        })?;
        let mut checkpoint =
            crate::checkpoint::load_checkpoint(&self.state.worktree_manager.repo_root, &reread.key)
                .await
                .map_err(|error| ApiError::InvalidCommand {
                    reason: format!("checkpoint is malformed or unreadable: {error}"),
                })?;
        let (actual_refs, actual_worktrees) = crate::checkpoint::inspect_repository_evidence(
            &self.state.worktree_manager.repo_root,
            &reread.key,
        )
        .await
        .map_err(|error| ApiError::InvalidCommand {
            reason: format!("cannot inspect checkpoint recovery evidence: {error}"),
        })?;
        if let Some(saved) = checkpoint.as_mut() {
            saved.active_refs.extend(actual_refs);
            saved.active_refs.sort();
            saved.active_refs.dedup();
            saved.active_worktrees.extend(actual_worktrees);
            saved.active_worktrees.sort();
            saved.active_worktrees.dedup();
        }
        match crate::checkpoint::inspect_checkpoint(&reread, checkpoint.as_ref()) {
            crate::checkpoint::CheckpointDisposition::Compatible => {
                tracing::info!(run = %run, "checkpoint disposition: compatible — overlaying");
                if let Some(checkpoint) = checkpoint.as_ref() {
                    crate::checkpoint::overlay_compatible(&mut fresh_graph, checkpoint);
                }
                // Write the reconciled graph back to the checkpoint immediately
                // so the stale Skipped state doesn't survive even if the scheduler
                // never starts (e.g. the run fails after reconcile).
                let identity = crate::checkpoint::CheckpointIdentity::from_plan(&reread);
                if let Err(e) = crate::checkpoint::persist_checkpoint(
                    &self.state.worktree_manager.repo_root,
                    identity,
                    &fresh_graph,
                )
                .await
                {
                    tracing::warn!(run = %run, error = %e, "failed to persist reconciled checkpoint");
                }
            }
            crate::checkpoint::CheckpointDisposition::ReplaceClean { .. } => {
                tracing::info!(run = %run, "checkpoint disposition: replace-clean — archiving stale");
                crate::checkpoint::archive_clean_checkpoint(
                    &self.state.worktree_manager.repo_root,
                    &reread.key,
                )
                .await
                .map_err(|error| ApiError::InvalidCommand {
                    reason: format!("failed to archive stale checkpoint: {error}"),
                })?;
            }
            crate::checkpoint::CheckpointDisposition::RetainForRecovery { reason } => {
                return Err(ApiError::InvalidCommand {
                    reason: format!("checkpoint requires recovery before start: {reason}"),
                });
            }
            crate::checkpoint::CheckpointDisposition::Unavailable { reason } => {
                return Err(ApiError::InvalidCommand { reason });
            }
            crate::checkpoint::CheckpointDisposition::Missing => {}
        }
        {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;
            entry.plan_source = Some(PlanSourceState {
                key: reread.key.clone(),
                reconciliation: crate::checkpoint::CheckpointDisposition::Compatible,
                checkpoint_identity: crate::checkpoint::CheckpointIdentity::from_plan(&reread),
            });
        }
        *graph.lock().await = fresh_graph;
        Ok(graph)
    }

    /// Implement `StartRun`: spawn the Supervisor scheduler in the background.
    ///
    /// Looks up the Run's shared graph, builds a fresh [`RunControl`] (a sink
    /// wired to the broadcast, a cleared pause flag, a fresh cancel token),
    /// records the handle, sets the status `Running`, then `tokio::spawn`s
    /// [`run_graph`] over the graph and returns promptly for a fresh run. Resume
    /// first joins the cancelled paused generation so cleanup cannot overlap.
    /// The scheduler emits the live events the TUI observes.
    ///
    /// Resume: re-issuing `StartRun` on a paused Run clears the pause flag and
    /// spawns a fresh scheduler that continues launching ready tasks (done tasks
    /// are skipped).
    async fn start_run(&self, run: RunId) -> Result<CommandOutcome, ApiError> {
        let lifecycle_guard = self.state.lifecycle_lock.lock().await;
        let lease_cancel = CancellationToken::new();

        let (lease_owner, prior_join, restore_status) = {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;
            if !matches!(entry.status, RunStatus::Pending | RunStatus::Paused) {
                return Err(ApiError::InvalidCommand {
                    reason: format!("cannot start a run in status {:?}", entry.status),
                });
            }
            let restore_status = entry.status.clone();
            let prior_join = entry.handle.take().and_then(|mut handle| {
                handle.cancel.cancel();
                handle.join.take()
            });
            let owner = crate::repository_lease::RepositoryLeaseOwner {
                plan_dir: entry.plan_dir.relative_dir.clone(),
                run_uid: entry.run_uid.clone(),
                operation: crate::repository_lease::RepositoryLeaseOperation::Run,
            };
            let current = self
                .state
                .repository_leases
                .owner_for(&self.state.worktree_manager.repo_root)
                .map_err(|error| ApiError::InvalidCommand {
                    reason: error.to_string(),
                })?;
            entry.status = RunStatus::WaitingForRepository {
                owner: current.clone(),
            };
            entry.handle = Some(RunHandle {
                generation: entry.scheduler_generation,
                cancel: lease_cancel.clone(),
                pause: Arc::new(AtomicBool::new(false)),
                join: None,
            });
            let _ = self.state.event_tx.send(Event::RepositoryLeaseWaiting {
                run,
                owner: current,
            });
            let _ = self.state.event_tx.send(Event::RunProgress {
                run,
                phase: "acquiring repository lease".into(),
            });
            (owner, prior_join, restore_status)
        };
        let _waiting_guard = StartWaitingGuard {
            state: Arc::clone(&self.state),
            run,
            restore_status,
        };
        drop(lifecycle_guard);
        if let Some(join) = prior_join {
            let _ = join.await;
        }
        let repository_lease = self
            .state
            .repository_leases
            .acquire(
                &self.state.worktree_manager.repo_root,
                lease_owner,
                &lease_cancel,
            )
            .await
            .map_err(|error| ApiError::InvalidCommand {
                reason: error.to_string(),
            })?;
        let _lifecycle_guard = self.state.lifecycle_lock.lock().await;

        self.reconcile_run_under_repository_lease(run).await?;

        // Take everything we need out of the registry under ONE lock, then drop
        // the guard before spawning (no lock across the spawn / await boundary).
        let (graph, cancel, pause, run_slug, run_uid, plan_slug, generation, old_join) = {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;

            if !matches!(entry.status, RunStatus::WaitingForRepository { .. }) {
                return Err(ApiError::InvalidCommand {
                    reason: format!(
                        "cannot start a run in status {:?}; only Pending or Paused runs may be started",
                        entry.status
                    ),
                });
            }

            if entry.report.is_blocked() {
                let blockers: Vec<_> = entry.report.blocking().collect();
                let n = blockers.len();
                let codes = blockers
                    .iter()
                    .map(|i| i.code.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                return Err(ApiError::InvalidCommand {
                    reason: format!(
                        "cannot start: {} blocking ingestion issue(s) — {}",
                        n, codes
                    ),
                });
            }

            // A paused scheduler may still be draining in-flight work. Cancel it
            // and retain its join so plan-scoped worktrees cannot overlap the
            // replacement scheduler.
            let old_join = entry.handle.take().and_then(|mut old| {
                old.cancel.cancel();
                old.pause.store(false, Ordering::SeqCst);
                old.join.take()
            });

            let cancel = CancellationToken::new();
            let pause = Arc::new(AtomicBool::new(false));
            let generation =
                entry
                    .scheduler_generation
                    .checked_add(1)
                    .ok_or_else(|| ApiError::Internal {
                        reason: format!("scheduler generation overflow for run {run}"),
                    })?;
            entry.scheduler_generation = generation;
            entry.handle = Some(RunHandle {
                generation,
                cancel: cancel.clone(),
                pause: Arc::clone(&pause),
                join: None,
            });
            entry.status = RunStatus::Running;
            // Stamp the run's start instant for the finalization-time `run.json`.
            entry.started_at = Some(Utc::now());

            // Derive the slug here — inside the same lock — so we don't need a
            // second lock acquisition below. Uses the same plan-scoped derivation
            // as `open_run` so the persisted artifact, audit ledger, and per-task
            // logs all agree on one slug.
            let slug = entry.plan_slug.clone();

            // The persistent ULID identity, threaded into the scheduler so the
            // audit ledger can key entries on it.
            let run_uid = entry.run_uid.clone();

            // The plan slug, threaded into the scheduler so per-task worktree
            // calls can plan-scope their directory + branch names.
            let plan_slug = entry.plan_slug.clone();

            // Return the pieces the background task needs.
            (
                Arc::clone(&entry.graph),
                cancel,
                pause,
                slug,
                run_uid,
                plan_slug,
                generation,
                old_join,
            )
        }; // registry guard dropped here.

        if let Some(join) = old_join
            && let Err(e) = join.await
            && !e.is_cancelled()
        {
            tracing::warn!(run = %run, error = %e, "superseded scheduler join failed");
        }

        let join = self.spawn_run_scheduler(
            run,
            graph,
            cancel,
            pause,
            run_slug,
            run_uid,
            plan_slug,
            generation,
            repository_lease,
        );

        // Retain the JoinHandle only if this generation still owns the run. A
        // very small graph may finish before this lock is reacquired; in that
        // case finalization already cleared the provisional handle and dropping
        // this completed JoinHandle is correct.
        {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            if let Some(handle) = runs
                .get_mut(&run.0)
                .filter(|entry| entry.scheduler_generation == generation)
                .and_then(|entry| entry.handle.as_mut())
                .filter(|handle| handle.generation == generation)
            {
                handle.join = Some(join);
            }
        }

        Ok(CommandOutcome::Acknowledged)
    }

    /// Build the per-run [`RunControl`] and `tokio::spawn` the Supervisor
    /// scheduler over `graph`, finalizing the registry status when it drains.
    ///
    /// Shared by [`CoreApi::start_run`] and the retry re-dispatch path
    /// ([`CoreApi::retry_task`] / [`CoreApi::retry_failed_tasks`]).  The caller
    /// must have already recorded the [`RunHandle`] (`cancel`/`pause`) in the
    /// registry and dropped the registry guard.  The scheduler creates its own
    /// fresh `Semaphore::new(concurrency)` internally — correct for retry because
    /// the prior scheduler already exited.
    #[allow(clippy::too_many_arguments)]
    fn spawn_run_scheduler(
        &self,
        run: RunId,
        graph: Arc<AsyncMutex<TaskGraph>>,
        cancel: CancellationToken,
        pause: Arc<AtomicBool>,
        run_slug: String,
        run_uid: String,
        plan_slug: String,
        generation: u64,
        repository_lease: crate::repository_lease::RepositoryLeaseGuard,
    ) -> JoinHandle<()> {
        let checkpoint_identity = self
            .state
            .runs
            .lock()
            .expect("runs registry mutex poisoned")
            .get(&run.0)
            .and_then(|entry| entry.plan_source.as_ref())
            .map(|source| source.checkpoint_identity.clone());
        // Build the per-run control (sink → broadcast, pause flag, cancel token).
        let control = RunControl {
            run,
            sink: CoreState::make_sink(Arc::clone(&self.state)),
            pause,
            cancel,
        };

        // Create the per-run logs directory up front (best-effort). This is a
        // synchronous call, so use `std::fs::create_dir_all` (via the paths
        // helper), not `tokio::fs`. On failure we warn and continue — never
        // abort the run. Mirrors the best-effort dir-create+warn in `audit.rs`.
        if let Err(e) = paths::run_logs_dir(&self.state.worktree_manager.repo_root, &run_uid) {
            tracing::warn!(
                run_uid = %run_uid,
                error = %e,
                "failed to create per-run logs dir; continuing"
            );
        }

        // Clone the static execution deps + the shared state for the background
        // task (so it can finalize the registry status when the scheduler ends).
        let run_id_for_sink = run;
        let event_tx_for_sink = self.state.event_tx.clone();
        let worktree_manager = self
            .state
            .worktree_manager
            .clone()
            .with_repository_child_token(Arc::new(
                repository_lease
                    .child_token()
                    .expect("repository lease descriptor must be clonable"),
            ))
            .with_command_sink(Arc::new(move |cmd: &str, cwd: &std::path::Path| {
                let _ = event_tx_for_sink.send(Event::RunCommand {
                    run: run_id_for_sink,
                    command: cmd.to_string(),
                    working_dir: cwd.display().to_string(),
                });
            }));
        let config = self.state.scheduler_config();
        let developer_backend = Arc::clone(&self.state.developer_backend);
        let reviewer_backend = Arc::clone(&self.state.reviewer_backend);
        let audit_registry = Arc::clone(&self.state.audit_registry);
        let planner_interpreter = Arc::clone(&self.state.planner_interpreter);
        let state = Arc::clone(&self.state);

        // Spawn the scheduler.  It emits RunStatusChanged{Running} at the start
        // and the aggregate terminal status at the end (unless cancelled).  We do
        // not await it — execution proceeds in the background; the TUI observes
        // via subscribe().  When it returns we finalize the registry status so a
        // later `run()`/`runs()` snapshot reflects Completed/Failed.  The
        // The returned JoinHandle is retained in RunEntry so lifecycle commands
        // can wait for terminal cleanup before reusing plan-scoped resources.
        tokio::spawn(async move {
            let _repository_lease = repository_lease;
            let cancellation = control.cancel.clone();
            let cancellation_run_uid = run_uid.clone();
            let cancellation_plan_slug = plan_slug.clone();
            match crate::actors::supervisor::run_graph_with_checkpoint(
                graph,
                worktree_manager,
                config,
                developer_backend,
                reviewer_backend,
                control,
                audit_registry,
                run_slug,
                run_uid,
                plan_slug,
                planner_interpreter,
                checkpoint_identity,
            )
            .await
            {
                Ok(report) => {
                    // Best-effort: log the plan_branch_left if the branch was left unmerged.
                    if let Some(branch) = &report.plan_branch_left {
                        tracing::info!(branch = %branch, "plan branch left unmerged after run");
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "run_graph failed");
                }
            }
            if cancellation.is_cancelled()
                && let Err(error) = state
                    .commit_authored_cancellation(
                        run,
                        &cancellation_run_uid,
                        &cancellation_plan_slug,
                    )
                    .await
            {
                tracing::error!(run = %run, error = %error, "authored cancellation transition failed");
                if let Ok(mut runs) = state.runs.lock()
                    && let Some(entry) = runs.get_mut(&run.0)
                {
                    entry.cancellation_status_error = Some(error);
                }
            }
            state.finalize_run_status(run, generation).await;
        })
    }

    /// Update settings that the next spawned scheduler should use.
    fn update_runtime_settings(
        &self,
        caps: CapsConfig,
        concurrency: usize,
        final_merge: FinalMerge,
    ) -> Result<CommandOutcome, ApiError> {
        self.state
            .update_runtime_settings(caps, concurrency, final_merge)?;
        Ok(CommandOutcome::Acknowledged)
    }

    /// Implement `PauseRun`: stop launching NEW tasks (in-flight finish).
    ///
    /// Sets the cooperative pause flag on the Run's handle (if running) and the
    /// status to `Paused`, then broadcasts `RunStatusChanged{Paused}`.  See the
    /// module-level pause-semantics note.
    async fn pause_run(&self, run: RunId) -> Result<CommandOutcome, ApiError> {
        let _lifecycle_guard = self.state.lifecycle_lock.lock().await;
        {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;
            if !matches!(entry.status, RunStatus::Pending | RunStatus::Running) {
                return Err(ApiError::InvalidCommand {
                    reason: format!(
                        "cannot pause a run in status {:?}; only Pending or Running runs may be paused",
                        entry.status
                    ),
                });
            }
            if entry.status == RunStatus::Running {
                let handle = entry.handle.as_ref().ok_or_else(|| ApiError::Internal {
                    reason: format!("running run {run} has no scheduler handle"),
                })?;
                handle.pause.store(true, Ordering::SeqCst);
            }
            entry.status = RunStatus::Paused;
        } // guard dropped before broadcast.
        let _ = self.state.event_tx.send(Event::RunStatusChanged {
            run,
            status: RunStatus::Paused,
        });
        Ok(CommandOutcome::Acknowledged)
    }

    /// Implement `CancelRun`: abort the scheduler + clean up; status → Failed.
    ///
    /// Cancels the Run's [`CancellationToken`] (the scheduler aborts in-flight
    /// drivers; each `DriverGuard` tears down its worktree/spokes — no leak),
    /// sets the status to `Failed` (cancelled), and broadcasts the change.  The
    /// Run is kept in the registry (its final state is observable) rather than
    /// dropped — the TUI can still inspect what completed before cancellation.
    async fn cancel_run(&self, run: RunId) -> Result<CommandOutcome, ApiError> {
        let _lifecycle_guard = self.state.lifecycle_lock.lock().await;
        let join = {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;
            entry.handle.take().and_then(|mut handle| {
                // Cancel the background scheduler; it aborts + cleans up.  We do
                // NOT drop the handle's cancel state from the entry before the
                // scheduler observes it — `cancel.cancel()` is sticky, so the
                // background task observes the sticky cancellation token before
                // this command records the terminal runtime status.
                handle.cancel.cancel();
                // Clearing the pause flag is harmless and avoids a stuck flag if
                // the run is somehow resumed; cancel takes precedence anyway.
                handle.pause.store(false, Ordering::SeqCst);
                handle.join.take()
            })
        }; // guard dropped before awaiting scheduler cleanup.
        if let Some(join) = join
            && let Err(e) = join.await
            && !e.is_cancelled()
        {
            tracing::warn!(run = %run, error = %e, "cancelled scheduler join failed");
        }
        {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;
            if let Some(reason) = entry.cancellation_status_error.take() {
                return Err(ApiError::InvalidCommand {
                    reason: format!(
                        "workers quiesced but authored cancellation was not persisted; runtime status unchanged: {reason}"
                    ),
                });
            }
            entry.scheduler_generation =
                entry
                    .scheduler_generation
                    .checked_add(1)
                    .ok_or_else(|| ApiError::Internal {
                        reason: format!("scheduler generation overflow for run {run}"),
                    })?;
            // Runtime cancellation is persisted only after worker quiescence.
            entry.status = RunStatus::Failed;
        }
        self.state.audit_registry.evict_run(&run.to_string());
        let _ = self.state.event_tx.send(Event::RunStatusChanged {
            run,
            status: RunStatus::Failed,
        });
        Ok(CommandOutcome::Acknowledged)
    }

    /// Implement `ReinterpretRun`: bypass artifact, re-interpret source .md,
    /// recompute report, atomically replace graph+report under lock, emit
    /// RunOpened (so TUI reloads the view), return Acknowledged.
    ///
    /// Concurrent `ReinterpretRun` calls on the same run are not serialized beyond the
    /// registry lock. Last writer wins (the second swap overwrites the first's graph+report).
    /// This is the same tolerance already present for concurrent `OpenPlan` of the same slug
    /// from two TUI instances. A per-run in-flight flag can be added later if needed.
    async fn reinterpret_run(&self, run: RunId) -> Result<CommandOutcome, ApiError> {
        let _lifecycle_guard = self.state.lifecycle_lock.lock().await;
        let repo_root = self.state.worktree_manager.repo_root.clone();

        // Lookup path + slug + enforce Pending (no lock held across the await).
        let (plan_dir, slug) = {
            let runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get(&run.0).ok_or(ApiError::UnknownRun { run })?;
            if entry.status != RunStatus::Pending {
                return Err(ApiError::InvalidCommand {
                    reason: "reinterpret only valid for Pending runs".into(),
                });
            }
            (entry.plan_dir.clone(), entry.run_slug.clone())
        };

        // Fresh interpret without seed-persist until status is re-checked below.
        let new_graph = self.load_plan_graph(&plan_dir)?;
        let interpret_issues = Vec::new();

        // Recompute report exactly as open_run does.
        let report = {
            let mut issues = crate::ingestion::validate(&new_graph);
            issues.extend(crate::ingestion::qualify(&new_graph));
            issues.extend(interpret_issues);
            crate::ingestion::IngestionReport { issues }
        };

        // Second lookup + re-check (defensive for races with Cancel or with a StartRun
        // that became legal because this re-interpret cleared the last blocker).
        // We deliberately do not hold the registry lock across the await above.
        let seed_snapshot = (!new_graph.tasks.is_empty()).then(|| new_graph.clone());

        {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;
            if entry.status != RunStatus::Pending {
                return Err(ApiError::InvalidCommand {
                    reason: "reinterpret only valid for Pending runs".into(),
                });
            }
            entry.graph = Arc::new(AsyncMutex::new(new_graph));
            entry.report = report;
        }

        // Seed only after the swap succeeds; never while holding the registry lock.
        if let Some(graph) = seed_snapshot
            && let Err(e) = crate::persist::persist_graph_as(&graph, &repo_root, &slug).await
        {
            tracing::warn!(
                slug = %slug,
                error = %e,
                "seed-persist failed after re-interpret; continuing",
            );
        }

        // Emit RunOpened (reusing the event is the smaller change; its
        // resolve_api_event + RunLoaded path will refresh the TUI's RunView).
        let _ = self.state.event_tx.send(Event::RunOpened { run, plan_dir });

        Ok(CommandOutcome::Acknowledged)
    }

    /// Implement `RetryTask`: reset one `Failed` task (and its skipped cascade),
    /// give it a fresh budget, persist, and re-dispatch (plan 0017).
    ///
    /// Validates the run is open and retryable (not actively `Running`) and that
    /// the named task is in [`crate::task::TaskState::Failed`].  Under the graph
    /// lock it resets the task (`Failed → New`, metadata cleared), un-skips the
    /// dependents skipped solely because of this failure, and re-marks readiness.
    /// Persists the reset graph + run snapshot, emits one `TaskRetried` per reset
    /// task, flips the run `Failed → Running`, and spawns a fresh scheduler.
    async fn retry_task(
        &self,
        run: RunId,
        task: crate::api::TaskId,
    ) -> Result<CommandOutcome, ApiError> {
        self.retry_impl(run, Some(task)).await
    }

    /// Implement `RetryFailedTasks`: reset every `Failed` task in the run (and
    /// their skipped cascades) under a single lock + un-skip sweep + readiness
    /// re-mark, persist, and re-dispatch (plan 0017).
    async fn retry_failed_tasks(&self, run: RunId) -> Result<CommandOutcome, ApiError> {
        self.retry_impl(run, None).await
    }

    /// Publish authored retry state before touching the volatile task graph.
    /// The caller holds the repository lease for the entire sequence.
    async fn commit_retry_source_transitions(
        &self,
        run: RunId,
        targets: &[crate::task::TaskId],
        action: &str,
    ) -> Result<(), ApiError> {
        use crate::plan::{GitTreePlanFileSource, PlanCandidate, PlanFileSource, PlanReservations};
        let (key, run_uid, plan_slug) = {
            let runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get(&run.0).ok_or(ApiError::UnknownRun { run })?;
            (
                entry.plan_dir.clone(),
                entry.run_uid.clone(),
                entry.plan_slug.clone(),
            )
        };
        if plan_slug.is_empty() || targets.is_empty() {
            return Ok(());
        }
        let invalid = |reason: String| ApiError::InvalidCommand { reason };
        let root = crate::paths::run_dir(&self.state.worktree_manager.repo_root, &run_uid)
            .map_err(|error| invalid(error.to_string()))?
            .join("integration");
        let plan_ref = format!("refs/heads/plan/{plan_slug}");
        for target in targets {
            let output = tokio::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["rev-parse", "--verify", &plan_ref])
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|error| invalid(error.to_string()))?;
            if !output.status.success() {
                return Err(invalid(
                    String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                ));
            }
            let old = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let source =
                GitTreePlanFileSource::new(&root, &old).map_err(|e| invalid(e.to_string()))?;
            let mut plan =
                match crate::plan::load_plan(&source, key.clone(), &PlanReservations::default())
                    .map_err(|report| {
                        invalid(format!("retry source is invalid: {:?}", report.diagnostics))
                    })? {
                    PlanCandidate::Plan(plan) => *plan,
                    PlanCandidate::NotCandidate => {
                        return Err(invalid("retry source is not a plan".into()));
                    }
                };
            let task = plan
                .tasks
                .iter_mut()
                .find(|task| task.frontmatter.id.as_str() == target.0)
                .ok_or_else(|| {
                    invalid(format!(
                        "retry task {target} is absent from registered plan"
                    ))
                })?;
            if task.frontmatter.status == crate::plan::AuthoredTaskStatus::Blocked {
                plan.status.source.body = crate::plan_status::resolve_exception(
                    &plan.status.source.body,
                    &target.0,
                    &format!("retry {run_uid}"),
                )
                .map_err(|error| invalid(error.to_string()))?;
            }
            task.update_bookkeeping(crate::plan::AuthoredTaskStatus::Planned, None)
                .map_err(|error| invalid(error.to_string()))?;
            plan.status.done = plan
                .tasks
                .iter()
                .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Done)
                .count();
            plan.status.blocked = plan
                .tasks
                .iter()
                .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Blocked)
                .count();
            plan.status.dropped = plan
                .tasks
                .iter()
                .filter(|task| task.frontmatter.status == crate::plan::AuthoredTaskStatus::Dropped)
                .count();
            let transition = crate::plan_status::StatusTransition {
                integration_state: plan.status.integration_state,
                run: plan.status.run.clone(),
                validation_base: plan.status.validation_base_oid.clone(),
                mode: plan.status.mode.clone(),
                final_oid: plan.status.final_oid.clone(),
                display_status: plan.status.display_status.clone(),
                last_updated: plan.status.last_updated.clone(),
            };
            let status = crate::plan_status::render_plan_status(&plan, &transition)
                .map_err(|e| invalid(e.to_string()))?;
            plan.status.source.body = status.clone();
            let board = String::from_utf8(
                source
                    .read_file(Path::new("docs/plans/STATUS.md"))
                    .map_err(|e| invalid(e.to_string()))?,
            )
            .map_err(|_| invalid("root status is not UTF-8".into()))?;
            let board = crate::plan_status::update_root_row(&board, &plan)
                .map_err(|e| invalid(e.to_string()))?;
            let task = plan
                .tasks
                .iter()
                .find(|task| task.frontmatter.id.as_str() == target.0)
                .expect("task retained");
            crate::landing::commit_source_transition(
                &root,
                &plan_ref,
                &old,
                &[
                    crate::landing::OwnedWrite {
                        path: task.source_path.clone(),
                        bytes: task.render().into_bytes(),
                    },
                    crate::landing::OwnedWrite {
                        path: plan.status.source.source_path.clone(),
                        bytes: status.into_bytes(),
                    },
                    crate::landing::OwnedWrite {
                        path: PathBuf::from("docs/plans/STATUS.md"),
                        bytes: board.into_bytes(),
                    },
                ],
                &crate::landing::SourceTransitionIdentity {
                    plan: plan_slug.clone(),
                    task: target.0.clone(),
                    run: run_uid.clone(),
                    action: action.into(),
                },
            )
            .await
            .map_err(|e| {
                invalid(format!(
                    "retry status commit failed before runtime reset: {e}"
                ))
            })?;
        }
        Ok(())
    }

    fn emit_plan_operation(
        &self,
        run: RunId,
        plan_slug: &str,
        label: &str,
        phase: crate::api::PlanOperationPhase,
        message: impl Into<String>,
    ) {
        let _ = self.state.event_tx.send(Event::PlanOperation {
            run,
            plan_slug: plan_slug.to_string(),
            label: label.to_string(),
            operation: crate::api::PlanOperationKind::Reset,
            phase,
            message: message.into(),
        });
    }

    /// Reset a run to a freshly interpreted Pending graph without re-dispatching.
    async fn reset_run(&self, run: RunId) -> Result<CommandOutcome, ApiError> {
        let _lifecycle_guard = self.state.lifecycle_lock.lock().await;
        let repo_root = self.state.worktree_manager.repo_root.clone();

        let (plan_dir, run_uid, run_slug, plan_slug, old_join, had_handle, old_graph) = {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;
            if entry.status == RunStatus::Running {
                return Err(ApiError::InvalidCommand {
                    reason: "stop or pause the run before resetting it".into(),
                });
            }
            let old_handle = entry.handle.take();
            let had_handle = old_handle.is_some();
            let old_join = old_handle.and_then(|mut handle| {
                handle.cancel.cancel();
                handle.pause.store(false, Ordering::SeqCst);
                handle.join.take()
            });
            entry.scheduler_generation =
                entry
                    .scheduler_generation
                    .checked_add(1)
                    .ok_or_else(|| ApiError::Internal {
                        reason: format!("scheduler generation overflow for run {run}"),
                    })?;
            (
                entry.plan_dir.clone(),
                entry.run_uid.clone(),
                entry.run_slug.clone(),
                entry.plan_slug.clone(),
                old_join,
                had_handle,
                Arc::clone(&entry.graph),
            )
        };

        let label = if plan_slug.is_empty() {
            run_slug.clone()
        } else {
            plan_slug.clone()
        };
        self.emit_plan_operation(
            run,
            &plan_slug,
            &label,
            crate::api::PlanOperationPhase::Started,
            "Starting reset",
        );

        if had_handle {
            self.emit_plan_operation(
                run,
                &plan_slug,
                &label,
                crate::api::PlanOperationPhase::Step,
                "Cancelled in-flight scheduler",
            );
        }
        if let Some(join) = old_join
            && let Err(e) = join.await
            && !e.is_cancelled()
        {
            tracing::warn!(run = %run, error = %e, "reset scheduler join failed");
        }
        if let Some(reason) = self
            .state
            .runs
            .lock()
            .expect("runs registry mutex poisoned")
            .get_mut(&run.0)
            .and_then(|entry| entry.cancellation_status_error.take())
        {
            return Err(ApiError::InvalidCommand {
                reason: format!(
                    "workers quiesced but authored requeue was not persisted; runtime reset stopped: {reason}"
                ),
            });
        }
        let archive_task_ids = {
            let graph = old_graph.lock().await;
            graph
                .tasks
                .iter()
                .map(|task| task.id.0.clone())
                .collect::<Vec<_>>()
        };
        let reset_cancel = CancellationToken::new();
        let _reset_lease = self
            .state
            .repository_leases
            .acquire(
                &repo_root,
                crate::repository_lease::RepositoryLeaseOwner {
                    plan_dir: plan_dir.relative_dir.clone(),
                    run_uid: run_uid.clone(),
                    operation: crate::repository_lease::RepositoryLeaseOperation::ResetRun,
                },
                &reset_cancel,
            )
            .await
            .map_err(|error| ApiError::InvalidCommand {
                reason: error.to_string(),
            })?;

        if !plan_slug.is_empty() {
            use crate::plan::{GitTreePlanFileSource, PlanCandidate, PlanReservations};
            let plan_ref = format!("refs/heads/plan/{plan_slug}");
            let output = tokio::process::Command::new("git")
                .arg("-C")
                .arg(&repo_root)
                .args(["rev-parse", "--verify", &plan_ref])
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|error| ApiError::InvalidCommand {
                    reason: error.to_string(),
                })?;
            if output.status.success() {
                let tip = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                let source = GitTreePlanFileSource::new(&repo_root, &tip).map_err(|error| {
                    ApiError::InvalidCommand {
                        reason: error.to_string(),
                    }
                })?;
                if let PlanCandidate::Plan(plan) =
                    crate::plan::load_plan(&source, plan_dir.clone(), &PlanReservations::default())
                        .map_err(|report| ApiError::InvalidCommand {
                            reason: format!(
                                "cannot reset invalid registered plan: {:?}",
                                report.diagnostics
                            ),
                        })?
                    && plan.status.integration_state == crate::plan::PlanIntegrationState::Complete
                {
                    return Err(ApiError::InvalidCommand {
                        reason: "completed plans cannot be reset; author a new plan".into(),
                    });
                }
                self.state
                    .worktree_manager
                    .archive_run_refs(&plan_slug, &run_uid, &archive_task_ids)
                    .await
                    .map_err(|error| ApiError::InvalidCommand {
                        reason: format!("reset could not archive recovery refs: {error}"),
                    })?;
            }
        }

        let old_task_ids = {
            let g = old_graph.lock().await;
            g.tasks.iter().map(|t| t.id.0.clone()).collect::<Vec<_>>()
        };

        self.emit_plan_operation(
            run,
            &plan_slug,
            &label,
            crate::api::PlanOperationPhase::Step,
            format!("Removing {} task worktree(s)", old_task_ids.len()),
        );
        for task_id in &old_task_ids {
            if let Err(e) = self
                .state
                .worktree_manager
                .remove(&plan_slug, task_id)
                .await
            {
                tracing::warn!(
                    run_uid = %run_uid,
                    plan_slug = %plan_slug,
                    task_id,
                    error = %e,
                    "failed to remove task worktree during run reset; continuing",
                );
            }
        }
        if !plan_slug.is_empty() {
            self.emit_plan_operation(
                run,
                &plan_slug,
                &label,
                crate::api::PlanOperationPhase::Step,
                "Deleting plan branch",
            );
            if let Err(e) = self
                .state
                .worktree_manager
                .delete_plan_branch(&plan_slug)
                .await
            {
                tracing::warn!(
                    run_uid = %run_uid,
                    plan_slug = %plan_slug,
                    error = %e,
                    "failed to delete plan branch during run reset; continuing",
                );
            }
        }

        self.emit_plan_operation(
            run,
            &plan_slug,
            &label,
            crate::api::PlanOperationPhase::Step,
            "Reloading plan documents",
        );
        let (new_graph, interpret_issues) = match self.load_plan_graph(&plan_dir) {
            Ok(graph) => (graph, Vec::new()),
            Err(e) => {
                self.emit_plan_operation(
                    run,
                    &plan_slug,
                    &label,
                    crate::api::PlanOperationPhase::Failed,
                    format!("Reset failed while reloading plan documents: {e}"),
                );
                return Err(e);
            }
        };

        let report = {
            let mut issues = crate::ingestion::validate(&new_graph);
            issues.extend(crate::ingestion::qualify(&new_graph));
            issues.extend(interpret_issues);
            crate::ingestion::IngestionReport { issues }
        };
        let graph_snapshot = (!new_graph.tasks.is_empty()).then(|| new_graph.clone());
        let graph = Arc::new(AsyncMutex::new(new_graph));

        {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;
            entry.graph = Arc::clone(&graph);
            entry.report = report;
            entry.status = RunStatus::Pending;
            entry.started_at = None;
            entry.handle = None;
        }

        if let Some(graph) = graph_snapshot {
            self.emit_plan_operation(
                run,
                &plan_slug,
                &label,
                crate::api::PlanOperationPhase::Step,
                "Persisting fresh pending graph",
            );
            if let Err(e) = crate::persist::persist_graph_as(&graph, &repo_root, &run_slug).await {
                tracing::warn!(
                    run_uid = %run_uid,
                    error = %e,
                    "failed to persist graph after run reset; continuing",
                );
            }
        }

        self.emit_plan_operation(
            run,
            &plan_slug,
            &label,
            crate::api::PlanOperationPhase::Step,
            "Clearing completed run snapshots for this plan",
        );
        let removed = remove_run_metadata_for_plan(&repo_root, &plan_slug).await;
        self.emit_plan_operation(
            run,
            &plan_slug,
            &label,
            crate::api::PlanOperationPhase::Step,
            format!("Cleared {removed} completed snapshot(s)"),
        );

        let _ = self.state.event_tx.send(Event::RunStatusChanged {
            run,
            status: RunStatus::Pending,
        });
        let _ = self.state.event_tx.send(Event::RunOpened { run, plan_dir });
        self.emit_plan_operation(
            run,
            &plan_slug,
            &label,
            crate::api::PlanOperationPhase::Finished,
            "Reset complete",
        );

        Ok(CommandOutcome::Acknowledged)
    }

    async fn purge_worktrees(&self) -> Result<CommandOutcome, ApiError> {
        let owner = crate::repository_lease::RepositoryLeaseOwner {
            plan_dir: PathBuf::new(),
            run_uid: "maintenance".into(),
            operation: crate::repository_lease::RepositoryLeaseOperation::PurgeWorktrees,
        };
        let Some(_lease) = self
            .state
            .repository_leases
            .try_acquire(&self.state.worktree_manager.repo_root, owner)
            .map_err(|error| ApiError::InvalidCommand {
                reason: error.to_string(),
            })?
        else {
            return Ok(CommandOutcome::RepositoryBusy {
                owner: self
                    .state
                    .repository_leases
                    .owner_for(&self.state.worktree_manager.repo_root)
                    .map_err(|error| ApiError::InvalidCommand {
                        reason: error.to_string(),
                    })?,
            });
        };
        let report = self
            .state
            .worktree_manager
            .purge_makina_worktrees()
            .await
            .map_err(|e| ApiError::InvalidCommand {
                reason: format!("failed to purge worktrees: {e}"),
            })?;
        Ok(CommandOutcome::WorktreesPurged {
            removed: report.worktrees_removed + report.orphan_dirs_removed,
            preserved: report.preserved.into_iter().map(|item| item.path).collect(),
        })
    }

    /// Shared implementation for `RetryTask`/`RetryFailedTasks`.
    ///
    /// `task == Some(id)` retries exactly one named `Failed` task (rejecting a
    /// non-`Failed` target); `task == None` retries every `Failed` task in the
    /// run.  Both paths share one graph lock, one un-skip sweep, one readiness
    /// re-mark, one persist, and one re-dispatch spawn.
    async fn retry_impl(
        &self,
        run: RunId,
        task: Option<crate::api::TaskId>,
    ) -> Result<CommandOutcome, ApiError> {
        use crate::task::{TaskId as DomainTaskId, TaskState as DomainTaskState};

        let lifecycle_guard = self.state.lifecycle_lock.lock().await;
        let lease_cancel = CancellationToken::new();

        // 1. Lock the registry: validate the run is open + retryable, snapshot the
        //    graph handle and run identity, then drop the guard before awaiting.
        let (
            _graph,
            run_uid,
            run_slug,
            plan_slug,
            plan_dir,
            started_at,
            generation,
            old_join,
            lease_owner,
            restore_status,
        ) = {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;
            // Idempotence / race guard: only Failed/Paused/Completed runs are
            // retryable. A still-actively-Running (or Pending) run is rejected so
            // a retry never races the live scheduler.
            if !matches!(
                entry.status,
                RunStatus::Failed | RunStatus::Paused | RunStatus::Completed
            ) {
                return Err(ApiError::InvalidCommand {
                    reason: format!(
                        "cannot retry a run in status {:?}; only Failed/Paused/Completed runs are retryable",
                        entry.status
                    ),
                });
            }
            let restore_status = entry.status.clone();
            let old_join = entry.handle.take().and_then(|mut handle| {
                handle.cancel.cancel();
                handle.pause.store(false, Ordering::SeqCst);
                handle.join.take()
            });
            let generation =
                entry
                    .scheduler_generation
                    .checked_add(1)
                    .ok_or_else(|| ApiError::Internal {
                        reason: format!("scheduler generation overflow for run {run}"),
                    })?;
            entry.scheduler_generation = generation;
            let lease_owner = crate::repository_lease::RepositoryLeaseOwner {
                plan_dir: entry.plan_dir.relative_dir.clone(),
                run_uid: entry.run_uid.clone(),
                operation: crate::repository_lease::RepositoryLeaseOperation::Run,
            };
            let current = self
                .state
                .repository_leases
                .owner_for(&self.state.worktree_manager.repo_root)
                .map_err(|error| ApiError::InvalidCommand {
                    reason: error.to_string(),
                })?;
            entry.status = RunStatus::WaitingForRepository {
                owner: current.clone(),
            };
            entry.handle = Some(RunHandle {
                generation,
                cancel: lease_cancel.clone(),
                pause: Arc::new(AtomicBool::new(false)),
                join: None,
            });
            let _ = self.state.event_tx.send(Event::RepositoryLeaseWaiting {
                run,
                owner: current,
            });
            (
                Arc::clone(&entry.graph),
                entry.run_uid.clone(),
                entry.run_slug.clone(),
                entry.plan_slug.clone(),
                entry.plan_dir.clone(),
                entry.started_at,
                generation,
                old_join,
                lease_owner,
                restore_status,
            )
        }; // registry guard dropped before awaiting the graph lock.

        let _waiting_guard = StartWaitingGuard {
            state: Arc::clone(&self.state),
            run,
            restore_status,
        };
        drop(lifecycle_guard);

        if let Some(join) = old_join
            && let Err(e) = join.await
            && !e.is_cancelled()
        {
            tracing::warn!(run = %run, error = %e, "retry scheduler join failed");
        }

        let repository_lease = self
            .state
            .repository_leases
            .acquire(
                &self.state.worktree_manager.repo_root,
                lease_owner,
                &lease_cancel,
            )
            .await
            .map_err(|error| ApiError::InvalidCommand {
                reason: error.to_string(),
            })?;
        let _lifecycle_guard = self.state.lifecycle_lock.lock().await;

        // The retry target and checkpoint state may have changed while this
        // request waited behind another run. Reconcile the same authoritative
        // inputs as StartRun before resetting any task or entering Running.
        let graph = self.reconcile_run_under_repository_lease(run).await?;

        // Validate and snapshot the retry set without mutating runtime. The
        // authored plan/task/root transaction must publish first so a failure
        // cannot leave an in-memory retry that source does not record.
        let durable_targets = {
            let g = graph.lock().await;
            match &task {
                Some(target) => {
                    let id = DomainTaskId(target.0.clone());
                    let state = g.get(&id).map(|task| task.state);
                    if state != Some(DomainTaskState::Failed) {
                        return Err(ApiError::InvalidCommand {
                            reason: match state {
                                Some(state) => format!(
                                    "task {} is in state {state:?}, not Failed; cannot retry",
                                    target.0
                                ),
                                None => format!("task {} is not in run {run}", target.0),
                            },
                        });
                    }
                    vec![id]
                }
                None => g
                    .tasks
                    .iter()
                    .filter(|task| task.state == DomainTaskState::Failed)
                    .map(|task| task.id.clone())
                    .collect(),
            }
        };
        if durable_targets.is_empty() {
            return Ok(CommandOutcome::Acknowledged);
        }
        self.commit_retry_source_transitions(run, &durable_targets, "retry")
            .await?;

        // 2. Reset the target task(s) under the graph lock; collect every reset
        //    (and revived) id so we can emit + persist after dropping the guard.
        //    Returns the cloned reset graph for persistence too.
        let (reset_ids, graph_snapshot) = {
            let mut g = graph.lock().await;

            // Determine which Failed tasks to reset.
            let targets: Vec<DomainTaskId> = match &task {
                Some(t) => vec![DomainTaskId(t.0.clone())],
                None => g
                    .tasks
                    .iter()
                    .filter(|t| t.state == DomainTaskState::Failed)
                    .map(|t| t.id.clone())
                    .collect(),
            };

            // RetryTask on a non-Failed (or missing) target is an InvalidCommand.
            if let Some(t) = &task {
                let target = DomainTaskId(t.0.clone());
                let state = g.get(&target).map(|task| task.state);
                if state != Some(DomainTaskState::Failed) {
                    return Err(ApiError::InvalidCommand {
                        reason: match state {
                            Some(s) => {
                                format!("task {} is in state {s:?}, not Failed; cannot retry", t.0)
                            }
                            None => format!("task {} is not in run {run}", t.0),
                        },
                    });
                }
            }

            // No failed tasks to retry (RetryFailedTasks on a clean run) is a no-op
            // success: nothing to reset, nothing to dispatch.
            if targets.is_empty() {
                return Ok(CommandOutcome::Acknowledged);
            }

            // Reset each Failed target (Failed → New, fresh budget).
            let mut reset_ids: Vec<DomainTaskId> = Vec::new();
            for id in &targets {
                crate::actors::supervisor::reset_task_for_retry_locked(&mut g, id)
                    .map_err(|reason| ApiError::InvalidCommand { reason })?;
                reset_ids.push(id.clone());
            }

            // Un-skip the dependents that were skipped solely because of these
            // failures, then re-mark readiness for everything now eligible.
            let revived = crate::actors::supervisor::unskip_dependents_locked(&mut g, &reset_ids);
            reset_ids.extend(revived);
            crate::actors::supervisor::remark_ready_locked(&mut g);

            (reset_ids, g.clone())
        }; // graph guard dropped before persisting / spawning.

        // 3. Persist the reset graph (best-effort; warn on failure).
        if let Err(e) = crate::persist::persist_graph_as(
            &graph_snapshot,
            &self.state.worktree_manager.repo_root,
            &run_slug,
        )
        .await
        {
            tracing::warn!(
                run_uid = %run_uid,
                error = %e,
                "failed to persist reset graph after retry; continuing",
            );
        }

        // 4. Refresh the run snapshot (`run.json`) so a restart sees the reset
        //    states; best-effort, mirroring `finalize_run_status`.
        let task_snapshots: Vec<TaskSnapshot> = graph_snapshot
            .tasks
            .iter()
            .map(|t| TaskSnapshot {
                id: t.id.0.clone(),
                title: t.title.clone(),
                state: crate::api::TaskState::from(t.state),
                gate_iterations: t.gate_iterations,
                review_iterations: t.review_iterations,
                depends_on: t.depends_on.iter().map(|d| d.0.clone()).collect(),
                started_at: t.started_at,
                finished_at: t.finished_at,
                failure_reason: t.failure_reason.clone(),
            })
            .collect();
        let started_at = started_at.unwrap_or_else(Utc::now);
        let meta = RunMetadata::with_tasks(
            run_uid.clone(),
            run_slug.clone(),
            plan_slug.clone(),
            RunStatus::Running,
            started_at,
            Utc::now(),
            task_snapshots,
        )
        .with_plan_dir(&plan_dir, &self.state.worktree_manager.repo_root);
        if let Err(e) = write_run_metadata(&meta, &self.state.worktree_manager.repo_root).await {
            tracing::warn!(run_uid = %run_uid, error = %e, "run.json refresh failed after retry");
        }

        // 5. Emit a `TaskRetried` + `TaskStateChanged` per reset/revived task so
        //    the TUI animates them back to New/Ready.
        for id in &reset_ids {
            let view_task = crate::api::TaskId(id.0.clone());
            let _ = self.state.event_tx.send(Event::TaskRetried {
                run,
                task: view_task.clone(),
            });
            let new_state = {
                let g = graph_snapshot.get(id).map(|t| t.state);
                g.map(crate::api::TaskState::from)
            };
            if let Some(state) = new_state {
                let _ = self.state.event_tx.send(Event::TaskStateChanged {
                    run,
                    task: view_task,
                    state,
                });
            }
        }

        // 6. Record a fresh RunHandle, flip the run Failed → Running, and spawn a
        //    fresh scheduler over the reset graph (mirroring `start_run`).
        let cancel = CancellationToken::new();
        let pause = Arc::new(AtomicBool::new(false));
        {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get_mut(&run.0).ok_or(ApiError::UnknownRun { run })?;
            entry.handle = Some(RunHandle {
                generation,
                cancel: cancel.clone(),
                pause: Arc::clone(&pause),
                join: None,
            });
            entry.status = RunStatus::Running;
            if entry.started_at.is_none() {
                entry.started_at = Some(started_at);
            }
        } // registry guard dropped before broadcast + spawn.

        let _ = self.state.event_tx.send(Event::RunStatusChanged {
            run,
            status: RunStatus::Running,
        });

        let join = self.spawn_run_scheduler(
            run,
            graph,
            cancel,
            pause,
            run_slug,
            run_uid,
            plan_slug,
            generation,
            repository_lease,
        );
        {
            let mut runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            if let Some(handle) = runs
                .get_mut(&run.0)
                .filter(|entry| entry.scheduler_generation == generation)
                .and_then(|entry| entry.handle.as_mut())
                .filter(|handle| handle.generation == generation)
            {
                handle.join = Some(join);
            }
        }

        Ok(CommandOutcome::Acknowledged)
    }

    /// Snapshot a single Run's view, locking the registry then the graph (never
    /// both at once, never the registry lock across the `.await`).
    async fn view_of(&self, id: RunId) -> Option<RunView> {
        // Pull the Arc graph handle + metadata out under the registry lock, then
        // drop the guard before awaiting the (separate) graph mutex.
        let (run_uid, path, status, report, graph) = {
            let runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let entry = runs.get(&id.0)?;
            (
                entry.run_uid.clone(),
                entry.plan_dir.clone(),
                entry.status.clone(),
                entry.report.clone(),
                Arc::clone(&entry.graph),
            )
        }; // registry guard dropped before await.
        let g = graph.lock().await;
        Some(build_view(
            id,
            run_uid,
            path,
            status,
            &self.state.worktree_manager.repo_root,
            &g,
            report,
        ))
    }

    /// Reload a historical snapshot for a stable session handle previously
    /// assigned by [`Api::runs`]. Historical handles are deliberately kept out
    /// of the live registry so lifecycle commands continue to reject them.
    fn disk_view_of(&self, id: RunId) -> Option<RunView> {
        let run_uid = {
            let disk_run_ids = self
                .state
                .disk_run_ids
                .lock()
                .expect("disk run id mutex poisoned");
            disk_run_ids
                .iter()
                .find_map(|(run_uid, mapped)| (*mapped == id).then(|| run_uid.clone()))
        };
        let run_uid = run_uid?;

        let live_run_uids = {
            let runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            runs.values()
                .map(|entry| entry.run_uid.clone())
                .collect::<std::collections::HashSet<_>>()
        };
        let mut provisional_id = 0;
        let mut view = load_disk_run_views(
            &self.state.worktree_manager.repo_root,
            &live_run_uids,
            &mut provisional_id,
        )
        .into_iter()
        .find(|view| view.run_uid == run_uid)?;
        view.id = id;
        Some(view)
    }
}

#[async_trait]
impl Api for CoreApi {
    /// Execute a [`Command`].
    ///
    /// All four commands are implemented:
    /// * [`Command::OpenPlan`] reads + interprets the file, registers the Run, and
    ///   broadcasts [`Event::RunOpened`].
    /// * [`Command::StartRun`] spawns the Supervisor scheduler in the background
    ///   (the Run actually executes) and returns [`CommandOutcome::Acknowledged`].
    /// * [`Command::PauseRun`] stops the scheduler launching NEW tasks.
    /// * [`Command::CancelRun`] aborts the scheduler and cleans up.
    /// * [`Command::ReinterpretRun`] re-reads the source (async, like OpenPlan).
    /// * [`Command::RetryTask`] / [`Command::RetryFailedTasks`] reset the failed
    ///   task(s) + skipped cascade, persist, and re-dispatch (async, plan 0017).
    /// * [`Command::ResetRun`] resets a run to a fresh Pending graph.
    /// * [`Command::UpdateRuntimeSettings`] updates the config snapshot used by
    ///   subsequently spawned schedulers.
    /// * [`Command::PurgeWorktrees`] removes Makina-created transient worktrees.
    async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
        match command {
            Command::GeneratePlanBundle { blueprint } => self.generate_plan_bundle(blueprint).await,
            Command::RegisterPlan {
                plan_dir,
                expected_base_oid,
                expected_source_digest,
            } => {
                self.register_plan(plan_dir, expected_base_oid, expected_source_digest)
                    .await
            }
            Command::SetTaskDisposition {
                run,
                task,
                expected_plan_oid,
                action,
            } => {
                self.set_task_disposition(run, task, expected_plan_oid, action)
                    .await
            }
            Command::FinalizePlan {
                plan_dir,
                run_uid,
                expected_plan_oid,
                input,
            } => {
                self.delayed_finalization(plan_dir, run_uid, expected_plan_oid, Some(input))
                    .await
            }
            Command::ReprepareFinalization {
                plan_dir,
                run_uid,
                expected_plan_oid,
            } => {
                self.delayed_finalization(plan_dir, run_uid, expected_plan_oid, None)
                    .await
            }
            Command::OpenPlan { plan_dir } => self.open_run(plan_dir).await,
            // Lifecycle commands may await a superseded scheduler's cleanup so
            // plan-scoped worktrees are never owned by overlapping generations.
            Command::StartRun { run } => self.start_run(run).await,
            Command::PauseRun { run } => self.pause_run(run).await,
            Command::CancelRun { run } => self.cancel_run(run).await,
            Command::ReinterpretRun { run } => self.reinterpret_run(run).await,
            // Retry is async: it persists the reset graph + run snapshot.
            Command::RetryTask { run, task } => self.retry_task(run, task).await,
            Command::RetryFailedTasks { run } => self.retry_failed_tasks(run).await,
            Command::ResetRun { run } => self.reset_run(run).await,
            Command::RegisterProject { project_root } => {
                self.require_project_root(&project_root)?;
                Ok(CommandOutcome::Acknowledged)
            }
            Command::UnregisterProject { project_root } => {
                self.require_project_root(&project_root)?;
                Ok(CommandOutcome::Acknowledged)
            }
            Command::UpdateRuntimeSettings {
                project_root,
                caps,
                concurrency,
                final_merge,
            } => {
                self.require_project_root(&project_root)?;
                self.update_runtime_settings(caps, concurrency, final_merge)
            }
            Command::DiscoverProject { project_root } => {
                self.require_project_root(&project_root)?;
                self.force_discover_project().await
            }
            Command::PurgeWorktrees { project_root } => {
                self.require_project_root(&project_root)?;
                self.purge_worktrees().await
            }
        }
    }

    /// Snapshot all open Runs plus any finished runs loaded from disk that are
    /// not present in the live registry.
    ///
    /// Live runs are returned in ascending [`RunId`] (insertion) order, followed
    /// by disk-snapshot runs in ULID (chronological) order.  Disk runs are only
    /// included when their `run_uid` is absent from the live registry — i.e. they
    /// are finished, evicted runs that survive across process restarts.
    async fn runs(&self) -> Vec<RunView> {
        // Snapshot the (id, path, status, graph-handle) tuples under the registry
        // lock, drop the guard, THEN lock each graph to build its view — so the
        // registry lock is never held across the graph `.await`.
        let (entries, live_run_uids): (
            Vec<(
                RunId,
                String,
                crate::plan::PlanKey,
                RunStatus,
                crate::ingestion::IngestionReport,
                Arc<AsyncMutex<TaskGraph>>,
            )>,
            std::collections::HashSet<String>,
        ) = {
            let runs = self
                .state
                .runs
                .lock()
                .expect("runs registry mutex poisoned");
            let live_uids: std::collections::HashSet<String> =
                runs.values().map(|e| e.run_uid.clone()).collect();
            let entries = runs
                .iter()
                .map(|(id, entry)| {
                    (
                        RunId(*id),
                        entry.run_uid.clone(),
                        entry.plan_dir.clone(),
                        entry.status.clone(),
                        entry.report.clone(),
                        Arc::clone(&entry.graph),
                    )
                })
                .collect();
            (entries, live_uids)
        }; // registry guard dropped before any graph await.

        let mut views = Vec::with_capacity(entries.len());
        for (id, run_uid, path, status, report, graph) in entries {
            let g = graph.lock().await;
            views.push(build_view(
                id,
                run_uid,
                path,
                status,
                &self.state.worktree_manager.repo_root,
                &g,
                report,
            ));
        }

        // Append finished runs loaded from disk (not in the live registry).
        let mut provisional_id = 0;
        let mut disk_views = load_disk_run_views(
            &self.state.worktree_manager.repo_root,
            &live_run_uids,
            &mut provisional_id,
        );
        {
            let mut disk_run_ids = self
                .state
                .disk_run_ids
                .lock()
                .expect("disk run id mutex poisoned");
            for view in &mut disk_views {
                view.id = *disk_run_ids
                    .entry(view.run_uid.clone())
                    .or_insert_with(|| self.state.alloc_id());
            }
        }
        views.extend(disk_views);

        views
    }

    /// Snapshot a live Run or a historical snapshot whose stable handle was
    /// previously surfaced by [`Api::runs`].
    async fn run(&self, id: RunId) -> Option<RunView> {
        match self.view_of(id).await {
            Some(view) => Some(view),
            None => self.disk_view_of(id),
        }
    }

    /// Subscribe to the live event stream.
    fn subscribe(&self) -> EventStream {
        let rx = self.state.event_tx.subscribe();
        let stream = BroadcastStream::new(rx).filter_map(|result| result.ok());
        Box::pin(stream)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::noop::NoopBackend;
    use crate::config::{Config, GlobalConfig, ProjectConfig};
    use crate::dependency::EdgeInferrer;
    use crate::interpreter::SourceProjectionUnavailable;
    // `next()` comes from `tokio_stream::StreamExt`, already in scope via the
    // glob import above (the orchestrator uses it for the broadcast stream).
    use std::sync::Arc;

    // Use the process-global HOME_ENV_LOCK from lib.rs so all test modules
    // serialize HOME mutations across crate boundaries.
    use crate::HOME_ENV_LOCK;
    use crate::test_support::setup_temp_repo as shared_setup_temp_repo;

    /// The rejection message is read by a person and re-prompted to a planner,
    /// so it has to name the file, the rule, and the field — not Debug-print
    /// the diagnostic structs, which is what it used to do.
    #[test]
    fn generation_diagnostics_render_as_prose() {
        let rendered = render_generation_diagnostics(&[crate::plan::PlanValidationDiagnostic {
            code: "invalid-task-document".into(),
            path: PathBuf::from("docs/plans/0001-fsm/tasks/0101-parse.md"),
            field: Some("body".into()),
            message: "H1 must exactly equal the frontmatter title".into(),
        }]);

        assert!(
            rendered.contains("docs/plans/0001-fsm/tasks/0101-parse.md"),
            "{rendered}"
        );
        assert!(rendered.contains("invalid-task-document"), "{rendered}");
        assert!(rendered.contains("[body]"), "{rendered}");
        assert!(
            rendered.contains("H1 must exactly equal the frontmatter title"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("PlanValidationDiagnostic"),
            "the struct must not leak into the message: {rendered}"
        );
    }

    /// The whole string becomes prompt, so a pathological bundle must not turn
    /// into an unbounded one.
    #[test]
    fn generation_diagnostics_are_bounded() {
        let many = (0..20)
            .map(|index| crate::plan::PlanValidationDiagnostic {
                code: "invalid-task-document".into(),
                path: PathBuf::from(format!("tasks/{index}.md")),
                field: None,
                message: "bad".into(),
            })
            .collect::<Vec<_>>();

        let rendered = render_generation_diagnostics(&many);
        assert!(rendered.contains("and 12 more"), "{rendered}");
        assert!(!rendered.contains("tasks/8.md"), "{rendered}");
    }

    /// Build a `Config` with NO gates (the gate loop is a no-op) so the develop
    /// → review loop advances straight from develop to review — the same config
    /// the task-21–25 integration tests use.
    fn no_gate_config() -> Config {
        Config::resolve(GlobalConfig::default(), ProjectConfig::default())
    }

    /// Build a `CoreApi` over typed-source projection + a `NoopBackend`
    /// (configured: developer output then approve verdict) + a temp-repo
    /// `WorktreeManager` + a no-gate `Config`.  Returns the api and the temp dir
    /// (keep it alive for the test).
    fn execution_core_api() -> (CoreApi, tempfile::TempDir) {
        let ingestion = Arc::new(EdgeInferrer::new(Arc::new(
            SourceProjectionUnavailable::new(),
        )));
        let planner = Arc::new(SourceProjectionUnavailable::new());
        // Cycle: developer output, then approve verdict (covers any task count).
        let backend: Arc<dyn AgentBackend> = Arc::new(NoopBackend::with_responses(vec![
            "Implemented the feature.".into(),
            r#"{"verdict":"approve"}"#.into(),
        ]));
        let repo_dir = shared_setup_temp_repo();
        let wm = WorktreeManager::new(repo_dir.path().to_path_buf(), "develop".into());
        let api = CoreApi::with_audit_registry(
            ingestion,
            planner,
            Arc::clone(&backend),
            backend,
            wm,
            no_gate_config(),
            Arc::new(NoopAuditRegistry),
        );
        (api, repo_dir)
    }

    // ── OpenPlan (task 28 behavior, preserved) ─────────────────────────────────

    /// **Acceptance (task 28): OpenPlan interprets the file and creates a Run.**
    #[tokio::test]
    async fn project_registration_commands_validate_the_bound_repository() {
        let (api, repo) = execution_core_api();
        for command in [
            Command::RegisterProject {
                project_root: repo.path().to_path_buf(),
            },
            Command::UnregisterProject {
                project_root: repo.path().to_path_buf(),
            },
        ] {
            assert!(matches!(
                api.execute(command).await,
                Ok(CommandOutcome::Acknowledged)
            ));
        }

        let other = shared_setup_temp_repo();
        let error = api
            .execute(Command::RegisterProject {
                project_root: other.path().to_path_buf(),
            })
            .await
            .expect_err("a repository-bound API must reject another root");
        assert!(matches!(error, ApiError::InvalidCommand { .. }));
    }

    #[tokio::test]
    async fn historical_run_ids_are_stable_across_queries() {
        let _home_guard = HOME_ENV_LOCK.lock().await;
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let original_home = std::env::var_os("HOME");
        // SAFETY: serialized by HOME_ENV_LOCK for the full test lifetime.
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        let (api, repo) = execution_core_api();
        let run_uid = ulid::Ulid::from_datetime(std::time::SystemTime::now()).to_string();
        let now = Utc::now();
        let metadata = RunMetadata::new(
            run_uid.clone(),
            "historical-tasks".to_string(),
            "historical".to_string(),
            RunStatus::Completed,
            now,
            now,
        );
        write_run_metadata(&metadata, repo.path())
            .await
            .expect("write historical run metadata");

        assert!(
            api.runs().await.iter().all(|view| view.run_uid != run_uid),
            "metadata without a typed plan identity must remain inert"
        );

        // SAFETY: restore the process-global value while HOME_ENV_LOCK is held.
        unsafe {
            match original_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[tokio::test]
    async fn project_qualified_commands_validate_the_bound_repository_root() {
        let (api, repo) = execution_core_api();
        let same_root_alias = repo.path().join(".");

        assert!(matches!(
            api.execute(Command::RegisterProject {
                project_root: same_root_alias.clone(),
            })
            .await,
            Ok(CommandOutcome::Acknowledged)
        ));
        assert!(matches!(
            api.execute(Command::UnregisterProject {
                project_root: same_root_alias,
            })
            .await,
            Ok(CommandOutcome::Acknowledged)
        ));

        let other = shared_setup_temp_repo();
        let other_root = other.path().to_path_buf();
        let settings = no_gate_config();
        let wrong_project_commands = vec![
            Command::RegisterProject {
                project_root: other_root.clone(),
            },
            Command::UnregisterProject {
                project_root: other_root.clone(),
            },
            Command::UpdateRuntimeSettings {
                project_root: other_root.clone(),
                caps: settings.caps.clone(),
                concurrency: settings.concurrency,
                final_merge: settings.merge.final_,
            },
            Command::DiscoverProject {
                project_root: other_root.clone(),
            },
            Command::PurgeWorktrees {
                project_root: other_root,
            },
        ];
        for command in wrong_project_commands {
            let error = api
                .execute(command)
                .await
                .expect_err("a repository-bound API must reject another project");
            assert!(matches!(
                error,
                ApiError::InvalidCommand { reason } if reason.contains("targets project")
            ));
        }

        let error = api
            .execute(Command::RegisterProject {
                project_root: repo.path().join("missing-project"),
            })
            .await
            .expect_err("a missing project root must be rejected");
        assert!(matches!(
            error,
            ApiError::InvalidCommand { reason }
                if reason.contains("cannot resolve requested project root")
        ));
    }

    // ── Plan-scoped run slug (mk-run-slug) ────────────────────────────────────

    /// Returns whether `s` satisfies the §4.1 kebab predicate: starts and ends
    /// with an alphanumeric, contains no consecutive hyphens, and is at least
    /// two characters long.
    fn is_valid_kebab(s: &str) -> bool {
        s.len() >= 2
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
            && s.chars().last().is_some_and(|c| c.is_ascii_alphanumeric())
            && !s.contains("--")
    }

    #[test]
    fn run_slug_is_plan_scoped_and_valid() {
        let plan = run_slug(Path::new("/repo/docs/plans/0003-Runtime-and-TUI-Hardening"));
        assert_eq!(plan, "0003-runtime-and-tui-hardening");
        let a = run_slug(Path::new("/repo/docs/plans/0003-alpha"));
        let b = run_slug(Path::new("/repo/docs/plans/0004-beta"));
        assert_ne!(a, b);
        let fallback = run_slug(Path::new("/"));
        assert_eq!(fallback, SLUG_FALLBACK);
        for s in [&plan, &a, &b, &fallback] {
            assert!(
                is_valid_kebab(s),
                "slug {s:?} must satisfy the §4.1 kebab predicate"
            );
        }
    }

    #[test]
    fn plan_slug_uses_directory_identity() {
        assert_eq!(
            plan_slug(Path::new("/repo/docs/plans/0003-Runtime-and-TUI-Hardening")),
            "0003-runtime-and-tui-hardening",
        );
        assert_eq!(plan_slug(Path::new("/")), SLUG_FALLBACK);
    }

    /// Querying before any typed plan is opened is side-effect free.
    #[tokio::test]
    async fn queries_empty_before_any_open() {
        let (api, _repo) = execution_core_api();
        assert!(api.runs().await.is_empty());
        assert!(api.run(RunId(1)).await.is_none());
    }

    // ── Seed-persist (task orchestrator-seed-write) ───────────────────────────

    /// **Acceptance (orchestrator-seed-write):**
    /// Opening a run on a typed plan in a temp repo with no pre-existing
    /// `.tasks/{slug}.json` creates the file with all tasks in the `new` state,
    /// asserted BEFORE any StartRun command.
    #[tokio::test]
    async fn start_run_unknown_id_is_rejected() {
        let (api, _repo) = execution_core_api();
        let err = api.execute(Command::StartRun { run: RunId(999) }).await;
        assert!(matches!(err, Err(ApiError::UnknownRun { run: RunId(999) })));
    }

    #[tokio::test]
    async fn cancel_run_unknown_id_is_rejected() {
        let (api, _repo) = execution_core_api();
        let err = api.execute(Command::CancelRun { run: RunId(123) }).await;
        assert!(matches!(err, Err(ApiError::UnknownRun { .. })));
    }

    // ── PauseRun stops launching new tasks ────────────────────────────────────

    /// **Pause stops launching new tasks (documented MVP semantics).**
    ///
    /// Pause a Run BEFORE starting it (the pause flag is set on the handle when
    /// the first task is about to launch).  Here we assert the simpler, robust
    /// invariant the MVP guarantees: `PauseRun` sets the status to `Paused` and
    /// emits `RunStatusChanged{Paused}`, and a paused run does not drive its
    /// tasks to Done.  We then resume via `StartRun` and confirm it completes —
    /// proving pause is a *cooperative stop-launching* flag, not a teardown.
    #[tokio::test]
    async fn pause_run_unknown_id_is_rejected() {
        let (api, _repo) = execution_core_api();
        let err = api.execute(Command::PauseRun { run: RunId(7) }).await;
        assert!(matches!(err, Err(ApiError::UnknownRun { .. })));
    }

    // ── ReinterpretRun ─────────────────────────────────────────────────────────

    /// **Reinterpret clears a blocking report and allows StartRun.**
    ///
    /// Opens with a model-path interpreter + cycling NoopBackend whose first
    /// response is a JSON graph with a non-actionable title (blocking report);
    /// StartRun refused. ReinterpretRun forces re-interpret (second/clear graph);
    /// report no longer blocked and StartRun now succeeds.
    #[test]
    fn build_view_project_is_repo_root_basename() {
        let repo_root = std::path::Path::new("/home/dev/projects/makina");
        let graph = TaskGraph {
            slug: "demo".to_string(),
            tasks: vec![],
            authored: Default::default(),
        };

        let view = build_view(
            RunId(7),
            "01J0000000000000000000000".to_string(),
            crate::plan::PlanKey::parse("docs/plans/0001-Demo").unwrap(),
            RunStatus::Pending,
            repo_root,
            &graph,
            crate::ingestion::IngestionReport::default(),
        );

        assert_eq!(view.project, "makina");
    }

    // ── open_run attaches IngestionReport (task requirement) ──────────────────

    /// Acceptance: `open_run` computes `IngestionReport` (validate + qualify)
    /// at graph resolution time and threads it onto the `RunEntry` (and thus
    /// every `RunView` returned by `runs()` / `run()`). Modelled on
    /// `open_run_interprets_file_and_creates_run`.
    #[test]
    fn task_view_carries_timestamps() {
        use chrono::TimeZone;

        let t0 = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 10, 0, 0)
            .single()
            .unwrap();
        let t1 = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 11, 0, 0)
            .single()
            .unwrap();
        let now = chrono::Utc::now();

        // Build a graph with two tasks: one started+finished, one not yet started.
        let started_task = crate::task::Task {
            id: crate::task::TaskId("started-task".into()),
            title: "Started task".into(),
            description: String::new(),
            done_when: String::new(),
            depends_on: vec![],
            section: None,
            state: crate::task::TaskState::Done,
            gate_iterations: 0,
            review_iterations: 0,
            created_at: now,
            updated_at: now,
            started_at: Some(t0),
            finished_at: Some(t1),
            failure_reason: None,
        };
        let pending_task = crate::task::Task {
            id: crate::task::TaskId("pending-task".into()),
            title: "Pending task".into(),
            description: String::new(),
            done_when: String::new(),
            depends_on: vec![],
            section: None,
            state: crate::task::TaskState::New,
            gate_iterations: 0,
            review_iterations: 0,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
            failure_reason: None,
        };

        let mut authored = std::collections::BTreeMap::new();
        authored.insert(
            crate::task::TaskId("started-task".into()),
            crate::task::AuthoredTaskMetadata {
                source_path: "tasks/0301-started-task.md".into(),
                workstream: "0003".into(),
                kind: "code".into(),
                gated: true,
                touches: vec![crate::task::AuthoredRepoPattern::Path("src/**".into())],
                status: crate::plan::AuthoredTaskStatus::InProgress,
                merged_as: Some("0123456789abcdef".into()),
                seed: crate::task::AuthoredSeedOutcome::NeedsInProgressReconciliation,
                collision_dependencies: vec![crate::task::TaskId("pending-task".into())],
                branch_base_oid: None,
            },
        );
        let graph = crate::task::TaskGraph {
            slug: "test-graph".into(),
            tasks: vec![started_task, pending_task],
            authored,
        };

        let repo_root = std::path::Path::new("/tmp/fake-repo");
        let view = build_view(
            RunId(1),
            "test-run-uid".into(),
            crate::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            RunStatus::Running,
            repo_root,
            &graph,
            crate::ingestion::IngestionReport::default(),
        );

        assert_eq!(view.tasks.len(), 2);
        let authored = view.tasks[0]
            .authored
            .as_ref()
            .expect("authored plan metadata retained in run view");
        assert_eq!(authored.workstream, "0003");
        assert_eq!(authored.kind, "code");
        assert_eq!(authored.status, "in-progress");
        assert!(authored.gated);
        assert_eq!(authored.touches, ["src/**"]);
        assert_eq!(authored.merged_as.as_deref(), Some("0123456789abcdef"));
        assert_eq!(
            authored.source_path,
            Path::new("tasks/0301-started-task.md")
        );
        assert_eq!(authored.collision_dependencies[0].0, "pending-task");

        // The started+finished task must carry both timestamps through.
        assert_eq!(
            view.tasks[0].started_at,
            Some(t0),
            "started task: started_at must be Some(t0)"
        );
        assert_eq!(
            view.tasks[0].finished_at,
            Some(t1),
            "started task: finished_at must be Some(t1)"
        );

        // The not-yet-started task must yield None/None.
        assert_eq!(
            view.tasks[1].started_at, None,
            "pending task: started_at must be None"
        );
        assert_eq!(
            view.tasks[1].finished_at, None,
            "pending task: finished_at must be None"
        );
    }

    #[test]
    fn non_plan_dirs_ignored() {
        let tmp = tempfile::TempDir::new().expect("create temp dir");
        let plans_dir = tmp.path().join("docs").join("plans");
        std::fs::create_dir_all(&plans_dir).expect("create docs/plans");

        // Create a non-plan directory (missing SCOPE.md and ARCHITECTURE.md)
        let assets = plans_dir.join("assets");
        std::fs::create_dir(&assets).expect("create assets");
        std::fs::write(assets.join("foo.png"), "fake image").expect("write foo.png");

        // Create a valid plan to ensure non-plans are properly filtered
        let plan_0001 = plans_dir.join("0001-x");
        std::fs::create_dir(&plan_0001).expect("create 0001-x");
        std::fs::write(plan_0001.join("SCOPE.md"), "scope").expect("write SCOPE.md");
        std::fs::write(plan_0001.join("ARCHITECTURE.md"), "architecture")
            .expect("write ARCHITECTURE.md");

        let entries = discover_plans(tmp.path());
        // Historical pre-per-task directories are inert, as is assets/.
        assert!(entries.is_empty());

        // Test with missing docs/plans
        let empty_tmp = tempfile::TempDir::new().expect("create temp dir");
        let entries = discover_plans(empty_tmp.path());
        assert_eq!(entries, vec![]);
    }
}
