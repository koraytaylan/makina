//! Typed, validated documents used by the per-task plan format.
//!
//! This module is deliberately storage-agnostic.  A task is parsed through a
//! [`PlanFileSource`], so a later Git-tree implementation can use precisely the
//! same parser as the contained filesystem implementation provided here.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
const MAX_SCALAR_BYTES: usize = 64 * 1024;
const MAX_COLLECTION_ITEMS: usize = 1024;
const MAX_YAML_DEPTH: usize = 16;

/// Repository object identifier format reported by Git.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitObjectFormat {
    Sha1,
    Sha256,
}

impl GitObjectFormat {
    fn hex_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }
}

macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_newtype!(TaskId);
string_newtype!(WorkstreamId);
string_newtype!(TaskSequence);
string_newtype!(GitObjectId);

impl TaskId {
    pub fn parse(value: impl Into<String>) -> Result<Self, PlanDocumentError> {
        let value = value.into();
        if !is_kebab(&value) {
            return Err(field_error("id", "must be lowercase kebab-case"));
        }
        Ok(Self(value))
    }
}

impl WorkstreamId {
    pub fn parse(value: impl Into<String>) -> Result<Self, PlanDocumentError> {
        let value = value.into();
        let valid = value.len() == 4
            && value.bytes().all(|b| b.is_ascii_digit())
            && matches!(value.parse::<u16>(), Ok(1..=99));
        if !valid {
            return Err(field_error(
                "workstream",
                "must be a quoted value from 0001 through 0099",
            ));
        }
        Ok(Self(value))
    }
}

impl TaskSequence {
    pub fn parse(value: impl Into<String>) -> Result<Self, PlanDocumentError> {
        let value = value.into();
        let valid = value.len() == 2
            && value.bytes().all(|b| b.is_ascii_digit())
            && matches!(value.parse::<u8>(), Ok(1..=99));
        if !valid {
            return Err(field_error(
                "filename",
                "task sequence must be 01 through 99",
            ));
        }
        Ok(Self(value))
    }
}

impl GitObjectId {
    pub fn parse(
        value: impl Into<String>,
        format: GitObjectFormat,
    ) -> Result<Self, PlanDocumentError> {
        let value = value.into();
        if value.len() != format.hex_len() || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(field_error(
                "merged_as",
                &format!("must be a full {}-hex Git object ID", format.hex_len()),
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskKind {
    Task,
    Spike,
    Chore,
}

impl fmt::Display for TaskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Task => "task",
            Self::Spike => "spike",
            Self::Chore => "chore",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthoredTaskStatus {
    Planned,
    InProgress,
    Done,
    Blocked,
    Dropped,
}

impl fmt::Display for AuthoredTaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Planned => "planned",
            Self::InProgress => "in-progress",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Dropped => "dropped",
        })
    }
}

/// A validated expected-write pattern. Candidate variants are inert until a
/// coordinator supplies and validates an immutable base commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoPattern {
    Path(String),
    Glob(String),
    TrackedMakinaConfigCandidate,
    TrackedMakinaDeletionCandidate(String),
    TrackedMakinaConfig {
        validation_base: String,
    },
    TrackedMakinaDeletion {
        path: String,
        validation_base: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoChange {
    ModifiedOrdinaryFile,
    Deleted,
    Added,
    RenamedOrCopied,
    TypeChanged,
    Unmerged,
    Submodule,
}

impl RepoPattern {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Path(v) | Self::Glob(v) | Self::TrackedMakinaDeletionCandidate(v) => v,
            Self::TrackedMakinaConfigCandidate | Self::TrackedMakinaConfig { .. } => {
                ".makina/config.toml"
            }
            Self::TrackedMakinaDeletion { path, .. } => path,
        }
    }
    pub fn is_executable(&self) -> bool {
        !matches!(
            self,
            Self::TrackedMakinaConfigCandidate | Self::TrackedMakinaDeletionCandidate(_)
        )
    }

    pub fn permits_change(&self, change: RepoChange) -> bool {
        match self {
            Self::TrackedMakinaConfigCandidate | Self::TrackedMakinaDeletionCandidate(_) => false,
            Self::TrackedMakinaConfig { .. } => change == RepoChange::ModifiedOrdinaryFile,
            Self::TrackedMakinaDeletion { .. } => change == RepoChange::Deleted,
            Self::Path(_) | Self::Glob(_) => {
                !matches!(change, RepoChange::Unmerged | RepoChange::Submodule)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskFrontmatter {
    pub id: TaskId,
    pub title: String,
    pub workstream: WorkstreamId,
    pub kind: TaskKind,
    pub depends_on: Vec<TaskId>,
    pub gated: bool,
    pub touches: Vec<RepoPattern>,
    pub status: AuthoredTaskStatus,
    pub merged_as: Option<GitObjectId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskDocument {
    pub source_path: PathBuf,
    pub sequence: TaskSequence,
    pub frontmatter: TaskFrontmatter,
    /// Includes every byte after the closing frontmatter delimiter.
    pub body: String,
}

/// Narrow coordinator-owned update; immutable authored fields cannot be
/// changed through this API.
impl TaskDocument {
    pub fn update_bookkeeping(
        &mut self,
        status: AuthoredTaskStatus,
        merged_as: Option<GitObjectId>,
    ) -> Result<(), PlanDocumentError> {
        validate_status(status, merged_as.as_ref())?;
        self.frontmatter.status = status;
        self.frontmatter.merged_as = merged_as;
        Ok(())
    }

    pub fn render(&self) -> String {
        let fm = &self.frontmatter;
        let mut out = String::from("---\n");
        out.push_str(&format!("id: {}\n", yaml_scalar(fm.id.as_str())));
        out.push_str(&format!("title: {}\n", yaml_scalar(&fm.title)));
        out.push_str(&format!("workstream: \"{}\"\n", fm.workstream));
        out.push_str(&format!("kind: {}\n", fm.kind));
        render_string_list(
            &mut out,
            "depends_on",
            fm.depends_on.iter().map(TaskId::as_str),
        );
        out.push_str(&format!("gated: {}\n", fm.gated));
        render_string_list(
            &mut out,
            "touches",
            fm.touches.iter().map(RepoPattern::as_str),
        );
        out.push_str(&format!("status: {}\n", fm.status));
        out.push_str(&format!(
            "merged_as: {}\n",
            yaml_scalar(fm.merged_as.as_ref().map_or("", GitObjectId::as_str))
        ));
        out.push_str("---");
        out.push_str(&self.body);
        out
    }
}

/// Storage and immutable-base evidence consumed by the parser.
pub trait PlanFileSource {
    fn read_file(&self, relative_path: &Path) -> Result<Vec<u8>, PlanDocumentError>;
    fn object_format(&self) -> GitObjectFormat;
    fn validation_base_oid(&self) -> Option<&str>;
    fn is_tracked_ordinary_file(&self, relative_path: &Path) -> Result<bool, PlanDocumentError>;
    fn is_tracked_ordinary_file_at(
        &self,
        validation_base: &str,
        relative_path: &Path,
    ) -> Result<bool, PlanDocumentError> {
        if self.validation_base_oid() != Some(validation_base) {
            return Err(field_error(
                "validation_base",
                "source cannot resolve the STATUS validation base",
            ));
        }
        self.is_tracked_ordinary_file(relative_path)
    }
    /// A caller-supplied registration base that must agree with STATUS.
    fn expected_validation_base_oid(&self) -> Option<&str> {
        self.validation_base_oid()
    }
    fn entry_kind(
        &self,
        relative_path: &Path,
    ) -> Result<Option<PlanSourceEntryKind>, PlanDocumentError> {
        let _ = relative_path;
        Err(PlanDocumentError::Io(
            "source does not support bundle entry inspection".into(),
        ))
    }
    fn list_directory(&self, relative_path: &Path) -> Result<Vec<PathBuf>, PlanDocumentError> {
        let _ = relative_path;
        Err(PlanDocumentError::Io(
            "source does not support directory listing".into(),
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanSourceEntryKind {
    File,
    Directory,
    Other,
}

struct StatusValidationBaseSource<'a> {
    source: &'a dyn PlanFileSource,
    validation_base: &'a str,
}

impl PlanFileSource for StatusValidationBaseSource<'_> {
    fn read_file(&self, path: &Path) -> Result<Vec<u8>, PlanDocumentError> {
        self.source.read_file(path)
    }
    fn object_format(&self) -> GitObjectFormat {
        self.source.object_format()
    }
    fn validation_base_oid(&self) -> Option<&str> {
        Some(self.validation_base)
    }
    fn is_tracked_ordinary_file(&self, path: &Path) -> Result<bool, PlanDocumentError> {
        self.source
            .is_tracked_ordinary_file_at(self.validation_base, path)
    }
    fn is_tracked_ordinary_file_at(
        &self,
        validation_base: &str,
        path: &Path,
    ) -> Result<bool, PlanDocumentError> {
        self.source
            .is_tracked_ordinary_file_at(validation_base, path)
    }
    fn expected_validation_base_oid(&self) -> Option<&str> {
        Some(self.validation_base)
    }
    fn entry_kind(&self, path: &Path) -> Result<Option<PlanSourceEntryKind>, PlanDocumentError> {
        self.source.entry_kind(path)
    }
    fn list_directory(&self, path: &Path) -> Result<Vec<PathBuf>, PlanDocumentError> {
        self.source.list_directory(path)
    }
}

/// A plan source backed by one immutable commit in a repository's object
/// database. It never consults the index or working tree.
pub struct GitTreePlanFileSource {
    root: PathBuf,
    commit_oid: String,
    object_format: GitObjectFormat,
}

impl GitTreePlanFileSource {
    pub fn new(
        root: impl AsRef<Path>,
        treeish: impl AsRef<str>,
    ) -> Result<Self, PlanDocumentError> {
        let root = std::fs::canonicalize(root).map_err(|e| PlanDocumentError::Io(e.to_string()))?;
        let format_output = git_output(&root, &["rev-parse", "--show-object-format"])?;
        let object_format = match String::from_utf8_lossy(&format_output).trim() {
            "sha1" => GitObjectFormat::Sha1,
            "sha256" => GitObjectFormat::Sha256,
            other => {
                return Err(PlanDocumentError::Git(format!(
                    "unsupported Git object format `{other}`"
                )));
            }
        };
        let commit = format!("{}^{{commit}}", treeish.as_ref());
        let commit_oid = String::from_utf8(git_output(&root, &["rev-parse", "--verify", &commit])?)
            .map_err(|_| PlanDocumentError::Git("commit object ID was not UTF-8".into()))?
            .trim()
            .to_owned();
        GitObjectId::parse(&commit_oid, object_format).map_err(|_| {
            field_error(
                "validation_base",
                "Git returned an invalid commit object ID for the validation base",
            )
        })?;
        Ok(Self {
            root,
            commit_oid,
            object_format,
        })
    }

    fn lookup(&self, relative_path: &Path) -> Result<Option<GitTreeEntry>, PlanDocumentError> {
        self.lookup_at(&self.commit_oid, relative_path)
    }

    fn lookup_at(
        &self,
        treeish: &str,
        relative_path: &Path,
    ) -> Result<Option<GitTreeEntry>, PlanDocumentError> {
        validate_relative_path(relative_path)?;
        let relative = relative_path
            .to_str()
            .ok_or_else(|| field_error("path", "must be UTF-8"))?;
        let literal = format!(":(literal){relative}");
        let output = git_output(&self.root, &["ls-tree", "-z", treeish, "--", &literal])?;
        parse_single_tree_entry(&output, Some(relative.as_bytes()))
    }
}

impl PlanFileSource for GitTreePlanFileSource {
    fn read_file(&self, relative_path: &Path) -> Result<Vec<u8>, PlanDocumentError> {
        let entry = self
            .lookup(relative_path)?
            .ok_or_else(|| field_error("path", "does not exist in the immutable Git tree"))?;
        if entry.kind != PlanSourceEntryKind::File {
            return Err(field_error(
                "path",
                "expected an ordinary file in the immutable Git tree",
            ));
        }
        git_output(&self.root, &["cat-file", "blob", &entry.oid])
    }

    fn object_format(&self) -> GitObjectFormat {
        self.object_format
    }

    fn validation_base_oid(&self) -> Option<&str> {
        Some(&self.commit_oid)
    }

    fn is_tracked_ordinary_file(&self, relative_path: &Path) -> Result<bool, PlanDocumentError> {
        Ok(self
            .lookup(relative_path)?
            .is_some_and(|entry| entry.kind == PlanSourceEntryKind::File))
    }

    fn is_tracked_ordinary_file_at(
        &self,
        validation_base: &str,
        relative_path: &Path,
    ) -> Result<bool, PlanDocumentError> {
        Ok(self
            .lookup_at(validation_base, relative_path)?
            .is_some_and(|entry| entry.kind == PlanSourceEntryKind::File))
    }

    fn expected_validation_base_oid(&self) -> Option<&str> {
        None
    }

    fn entry_kind(
        &self,
        relative_path: &Path,
    ) -> Result<Option<PlanSourceEntryKind>, PlanDocumentError> {
        Ok(self.lookup(relative_path)?.map(|entry| entry.kind))
    }

    fn list_directory(&self, relative_path: &Path) -> Result<Vec<PathBuf>, PlanDocumentError> {
        if self.entry_kind(relative_path)? != Some(PlanSourceEntryKind::Directory) {
            return Err(field_error(
                "path",
                "expected an ordinary directory in the immutable Git tree",
            ));
        }
        let relative = relative_path
            .to_str()
            .ok_or_else(|| field_error("path", "must be UTF-8"))?;
        let prefix = format!("{relative}/");
        let literal = format!(":(literal){prefix}");
        let output = git_output(
            &self.root,
            &["ls-tree", "-z", &self.commit_oid, "--", &literal],
        )?;
        let mut result = Vec::new();
        for record in output
            .split(|byte| *byte == b'\0')
            .filter(|record| !record.is_empty())
        {
            let entry = parse_tree_entry(record)?;
            let path = std::str::from_utf8(&entry.path)
                .map_err(|_| PlanDocumentError::Git("Git tree path was not UTF-8".into()))?;
            if !path.starts_with(&prefix) || path[prefix.len()..].contains('/') {
                return Err(PlanDocumentError::Git(
                    "directory tree lookup returned an unexpected path".into(),
                ));
            }
            result.push(PathBuf::from(path));
        }
        result.sort_by(|left, right| {
            left.as_os_str()
                .as_encoded_bytes()
                .cmp(right.as_os_str().as_encoded_bytes())
        });
        Ok(result)
    }
}

struct GitTreeEntry {
    kind: PlanSourceEntryKind,
    oid: String,
    path: Vec<u8>,
}

fn git_output(root: &Path, args: &[&str]) -> Result<Vec<u8>, PlanDocumentError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|error| PlanDocumentError::Git(error.to_string()))?;
    if !output.status.success() {
        return Err(PlanDocumentError::Git(
            String::from_utf8_lossy(&output.stderr).trim().into(),
        ));
    }
    Ok(output.stdout)
}

fn parse_single_tree_entry(
    output: &[u8],
    expected_path: Option<&[u8]>,
) -> Result<Option<GitTreeEntry>, PlanDocumentError> {
    let mut records = output
        .split(|byte| *byte == b'\0')
        .filter(|record| !record.is_empty());
    let Some(record) = records.next() else {
        return Ok(None);
    };
    if records.next().is_some() {
        return Err(PlanDocumentError::Git(
            "literal tree lookup returned multiple entries".into(),
        ));
    }
    let entry = parse_tree_entry(record)?;
    if expected_path.is_some_and(|expected| entry.path != expected) {
        return Err(PlanDocumentError::Git(
            "literal tree lookup returned a different path".into(),
        ));
    }
    Ok(Some(entry))
}

fn parse_tree_entry(record: &[u8]) -> Result<GitTreeEntry, PlanDocumentError> {
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| PlanDocumentError::Git("malformed ls-tree output".into()))?;
    let fields = record[..tab]
        .split(|byte| *byte == b' ')
        .collect::<Vec<_>>();
    if fields.len() != 3 {
        return Err(PlanDocumentError::Git("malformed ls-tree header".into()));
    }
    let oid = std::str::from_utf8(fields[2])
        .map_err(|_| PlanDocumentError::Git("Git tree object ID was not UTF-8".into()))?
        .to_owned();
    let kind = match (fields[0], fields[1]) {
        (b"100644" | b"100755", b"blob") => PlanSourceEntryKind::File,
        (b"040000", b"tree") => PlanSourceEntryKind::Directory,
        _ => PlanSourceEntryKind::Other,
    };
    Ok(GitTreeEntry {
        kind,
        oid,
        path: record[tab + 1..].to_vec(),
    })
}

/// Symlink-safe filesystem source rooted at a repository.
pub struct FilesystemPlanFileSource {
    root: PathBuf,
    root_dir: cap_std::fs::Dir,
    validation_base: Option<String>,
    object_format: GitObjectFormat,
}

impl FilesystemPlanFileSource {
    /// Open a symlink-safe non-repository authoring root for structural
    /// validation before immutable-base provenance is bound.
    pub fn new_unbound(
        root: impl AsRef<Path>,
        object_format: GitObjectFormat,
    ) -> Result<Self, PlanDocumentError> {
        let root = std::fs::canonicalize(root).map_err(|e| PlanDocumentError::Io(e.to_string()))?;
        let root_dir = open_root_directory(&root)?;
        Ok(Self {
            root,
            root_dir,
            validation_base: None,
            object_format,
        })
    }

    pub fn new(
        root: impl AsRef<Path>,
        validation_base: Option<String>,
    ) -> Result<Self, PlanDocumentError> {
        let root = std::fs::canonicalize(root).map_err(|e| PlanDocumentError::Io(e.to_string()))?;
        let root_dir = open_root_directory(&root)?;
        let output = Command::new("git")
            .args(["rev-parse", "--show-object-format"])
            .current_dir(&root)
            .output()
            .map_err(|e| PlanDocumentError::Git(e.to_string()))?;
        if !output.status.success() {
            return Err(PlanDocumentError::Git(
                String::from_utf8_lossy(&output.stderr).trim().into(),
            ));
        }
        let object_format = match String::from_utf8_lossy(&output.stdout).trim() {
            "sha1" => GitObjectFormat::Sha1,
            "sha256" => GitObjectFormat::Sha256,
            other => {
                return Err(PlanDocumentError::Git(format!(
                    "unsupported Git object format `{other}`"
                )));
            }
        };
        if let Some(base) = validation_base.as_deref() {
            GitObjectId::parse(base, object_format).map_err(|_| {
                field_error(
                    "validation_base",
                    "must be a full object ID in the repository object format",
                )
            })?;
            let commit = format!("{base}^{{commit}}");
            let valid = Command::new("git")
                .args(["cat-file", "-e", &commit])
                .current_dir(&root)
                .status()
                .map_err(|e| PlanDocumentError::Git(e.to_string()))?;
            if !valid.success() {
                return Err(field_error(
                    "validation_base",
                    "must identify an existing commit",
                ));
            }
        }
        Ok(Self {
            root,
            root_dir,
            validation_base,
            object_format,
        })
    }
}

impl PlanFileSource for FilesystemPlanFileSource {
    fn read_file(&self, relative_path: &Path) -> Result<Vec<u8>, PlanDocumentError> {
        read_contained_file(&self.root_dir, relative_path)
    }
    fn object_format(&self) -> GitObjectFormat {
        self.object_format
    }
    fn validation_base_oid(&self) -> Option<&str> {
        self.validation_base.as_deref()
    }
    fn is_tracked_ordinary_file(&self, relative_path: &Path) -> Result<bool, PlanDocumentError> {
        let Some(base) = self.validation_base_oid() else {
            return Ok(false);
        };
        self.is_tracked_ordinary_file_at(base, relative_path)
    }
    fn is_tracked_ordinary_file_at(
        &self,
        base: &str,
        relative_path: &Path,
    ) -> Result<bool, PlanDocumentError> {
        validate_relative_path(relative_path)?;
        let relative = relative_path
            .to_str()
            .ok_or_else(|| field_error("path", "must be UTF-8"))?;
        let literal_pathspec = format!(":(literal){relative}");
        let output = Command::new("git")
            .args(["ls-tree", "-z", base, "--", &literal_pathspec])
            .current_dir(&self.root)
            .output()
            .map_err(|e| PlanDocumentError::Git(e.to_string()))?;
        if !output.status.success() {
            return Ok(false);
        }
        let mut record = output.stdout.split(|byte| *byte == b'\0');
        let Some(entry) = record.next().filter(|entry| !entry.is_empty()) else {
            return Ok(false);
        };
        if record.next().is_some_and(|entry| !entry.is_empty()) {
            return Err(PlanDocumentError::Git(
                "literal tree lookup returned multiple entries".into(),
            ));
        }
        let Some(tab) = entry.iter().position(|byte| *byte == b'\t') else {
            return Err(PlanDocumentError::Git("malformed ls-tree output".into()));
        };
        let header = &entry[..tab];
        if &entry[tab + 1..] != relative.as_bytes() {
            return Err(PlanDocumentError::Git(
                "literal tree lookup returned a different path".into(),
            ));
        }
        let mut fields = header.split(|byte| *byte == b' ');
        let mode = fields.next().unwrap_or_default();
        let object_type = fields.next().unwrap_or_default();
        Ok(matches!(mode, b"100644" | b"100755") && object_type == b"blob")
    }
    fn entry_kind(
        &self,
        relative_path: &Path,
    ) -> Result<Option<PlanSourceEntryKind>, PlanDocumentError> {
        validate_relative_path(relative_path)?;
        let parent = relative_path.parent().unwrap_or_else(|| Path::new(""));
        let directory = open_contained_directory(&self.root_dir, parent)?;
        let name = relative_path
            .file_name()
            .ok_or_else(|| field_error("path", "missing entry name"))?;
        let metadata = match directory.symlink_metadata(name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(PlanDocumentError::Io(error.to_string())),
        };
        Ok(Some(if metadata.file_type().is_symlink() {
            PlanSourceEntryKind::Other
        } else if metadata.is_file() {
            PlanSourceEntryKind::File
        } else if metadata.is_dir() {
            PlanSourceEntryKind::Directory
        } else {
            PlanSourceEntryKind::Other
        }))
    }
    fn list_directory(&self, relative_path: &Path) -> Result<Vec<PathBuf>, PlanDocumentError> {
        if self.entry_kind(relative_path)? != Some(PlanSourceEntryKind::Directory) {
            return Err(field_error("path", "expected an ordinary directory"));
        }
        let directory = open_contained_directory(&self.root_dir, relative_path)?;
        let mut entries = directory
            .entries()
            .map_err(|error| PlanDocumentError::Io(error.to_string()))?
            .map(|entry| {
                entry
                    .map(|entry| relative_path.join(entry.file_name()))
                    .map_err(|error| PlanDocumentError::Io(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort_by(|left, right| {
            left.as_os_str()
                .as_encoded_bytes()
                .cmp(right.as_os_str().as_encoded_bytes())
        });
        Ok(entries)
    }
}

fn open_root_directory(root: &Path) -> Result<cap_std::fs::Dir, PlanDocumentError> {
    cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())
        .map_err(|error| PlanDocumentError::Io(error.to_string()))
}

fn open_contained_directory(
    root: &cap_std::fs::Dir,
    relative: &Path,
) -> Result<cap_std::fs::Dir, PlanDocumentError> {
    use cap_fs_ext::DirExt;
    let mut directory = root
        .try_clone()
        .map_err(|error| PlanDocumentError::Io(error.to_string()))?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(field_error("path", "must be normalized"));
        };
        directory = directory.open_dir_nofollow(name).map_err(|_| {
            field_error(
                "path",
                "cannot open capability-relative directory without following symlinks",
            )
        })?;
    }
    Ok(directory)
}

fn read_contained_file(
    root: &cap_std::fs::Dir,
    relative: &Path,
) -> Result<Vec<u8>, PlanDocumentError> {
    use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
    use cap_std::fs::OpenOptions;
    use std::io::Read;

    validate_relative_path(relative)?;
    let mut directory = root
        .try_clone()
        .map_err(|e| PlanDocumentError::Io(e.to_string()))?;
    let components: Vec<_> = relative.components().collect();
    for component in &components[..components.len().saturating_sub(1)] {
        let Component::Normal(name) = component else {
            return Err(field_error("path", "must be normalized"));
        };
        directory = directory.open_dir_nofollow(name).map_err(|_| {
            field_error(
                "path",
                "cannot open capability-relative directory without following symlinks",
            )
        })?;
    }
    let final_name = components
        .last()
        .and_then(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .ok_or_else(|| field_error("path", "missing filename"))?;
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(final_name, &options).map_err(|_| {
        field_error(
            "path",
            "cannot open ordinary file without following symlinks",
        )
    })?;
    let metadata = file
        .metadata()
        .map_err(|e| PlanDocumentError::Io(e.to_string()))?;
    if !metadata.is_file() {
        return Err(field_error(
            "path",
            "task document must be an ordinary file",
        ));
    }
    if metadata.len() > MAX_DOCUMENT_BYTES as u64 {
        return Err(PlanDocumentError::ResourceLimit(
            "document is too large".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_DOCUMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| PlanDocumentError::Io(e.to_string()))?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(PlanDocumentError::ResourceLimit(
            "document grew beyond the size limit while being read".into(),
        ));
    }
    Ok(bytes)
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PlanDocumentError {
    #[error("{field}: {message}")]
    Field { field: String, message: String },
    #[error("invalid frontmatter: {0}")]
    Frontmatter(String),
    #[error("resource limit exceeded: {0}")]
    ResourceLimit(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("Git error: {0}")]
    Git(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFrontmatter {
    id: String,
    title: String,
    workstream: String,
    kind: TaskKind,
    depends_on: Vec<String>,
    gated: bool,
    touches: Vec<String>,
    status: AuthoredTaskStatus,
    merged_as: String,
}

/// Parse and fully validate one task document.
pub fn parse_task_document(
    source: &dyn PlanFileSource,
    relative_path: impl AsRef<Path>,
) -> Result<TaskDocument, PlanDocumentError> {
    let relative_path = relative_path.as_ref();
    let bytes = source.read_file(relative_path)?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(PlanDocumentError::ResourceLimit(
            "document is too large".into(),
        ));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| PlanDocumentError::Frontmatter("document is not UTF-8".into()))?;
    let (yaml, body) = split_frontmatter(text)?;
    validate_yaml_surface(yaml)?;
    let raw: RawFrontmatter =
        serde_saphyr::from_str(yaml).map_err(|e| PlanDocumentError::Frontmatter(e.to_string()))?;
    validate_parsed_yaml_limits(&raw)?;
    let (sequence, filename_id, ws_prefix) = parse_filename(relative_path)?;
    let id = TaskId::parse(raw.id)?;
    if id.as_str() != filename_id {
        return Err(field_error("id", "must match the task filename suffix"));
    }
    let workstream = WorkstreamId::parse(raw.workstream)?;
    if &workstream.as_str()[2..] != ws_prefix {
        return Err(field_error(
            "workstream",
            "must match the task filename workstream prefix",
        ));
    }
    if raw.title.trim().is_empty() {
        return Err(field_error("title", "must not be empty"));
    }
    let mut seen = HashSet::new();
    let depends_on = raw
        .depends_on
        .into_iter()
        .map(TaskId::parse)
        .collect::<Result<Vec<_>, _>>()?;
    for dep in &depends_on {
        if !seen.insert(dep.as_str()) {
            return Err(field_error("depends_on", "entries must be unique"));
        }
    }
    let merged_as = if raw.merged_as.is_empty() {
        None
    } else {
        Some(GitObjectId::parse(raw.merged_as, source.object_format())?)
    };
    validate_status(raw.status, merged_as.as_ref())?;
    let touches = raw
        .touches
        .into_iter()
        .map(|p| parse_repo_pattern(&p, raw.kind, source, relative_path))
        .collect::<Result<Vec<_>, _>>()?;
    validate_body(body, &raw.title)?;
    Ok(TaskDocument {
        source_path: relative_path.to_path_buf(),
        sequence,
        frontmatter: TaskFrontmatter {
            id,
            title: raw.title,
            workstream,
            kind: raw.kind,
            depends_on,
            gated: raw.gated,
            touches,
            status: raw.status,
            merged_as,
        },
        body: body.to_owned(),
    })
}

fn split_frontmatter(text: &str) -> Result<(&str, &str), PlanDocumentError> {
    let Some(rest) = text.strip_prefix("---\n") else {
        return Err(PlanDocumentError::Frontmatter(
            "frontmatter must begin at byte zero with `---`".into(),
        ));
    };
    let Some(end) = rest.match_indices("\n---").find_map(|(index, _)| {
        matches!(
            rest.as_bytes().get(index + 4),
            None | Some(b'\n') | Some(b'\r')
        )
        .then_some(index)
    }) else {
        return Err(PlanDocumentError::Frontmatter(
            "missing closing `---` delimiter".into(),
        ));
    };
    let yaml = &rest[..end];
    let body = &rest[end + 4..];
    if body.starts_with("\n---") {
        return Err(PlanDocumentError::Frontmatter(
            "multiple YAML documents are forbidden".into(),
        ));
    }
    Ok((yaml, body))
}

fn validate_yaml_surface(yaml: &str) -> Result<(), PlanDocumentError> {
    if yaml.len() > MAX_DOCUMENT_BYTES {
        return Err(PlanDocumentError::ResourceLimit(
            "frontmatter is too large".into(),
        ));
    }
    let mut keys = HashSet::new();
    let mut depth = 0usize;
    let mut items = 0usize;
    for line in yaml.lines() {
        if line.contains('\t')
            || line
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\r')
        {
            return Err(PlanDocumentError::Frontmatter(
                "tabs and control characters are forbidden".into(),
            ));
        }
        if line.len() > MAX_SCALAR_BYTES {
            return Err(PlanDocumentError::ResourceLimit(
                "scalar is too large".into(),
            ));
        }
        let trimmed = line.trim_start();
        let syntax = yaml_syntax_view(trimmed);
        let value = syntax
            .strip_prefix("- ")
            .unwrap_or(&syntax)
            .split_once(':')
            .map_or(syntax.as_str(), |(_, value)| value.trim_start());
        if trimmed.starts_with('%')
            || syntax == "---"
            || syntax == "..."
            || syntax.contains("<<:")
            || matches!(value, "|" | ">" | "|-" | ">-" | "|+" | ">+")
            || value.contains('{')
            || value.contains('}')
            || contains_yaml_token(&syntax, '&')
            || contains_yaml_token(&syntax, '*')
            || contains_yaml_token(&syntax, '!')
        {
            return Err(PlanDocumentError::Frontmatter("directives, aliases, anchors, tags, merge keys, and multiple documents are forbidden".into()));
        }
        if !line.starts_with(' ')
            && !line.starts_with('-')
            && let Some((key, _)) = line.split_once(':')
            && !keys.insert(key)
        {
            return Err(PlanDocumentError::Frontmatter(format!(
                "duplicate key `{key}`"
            )));
        }
        depth = depth.max(line.len() - trimmed.len());
        items += usize::from(trimmed.starts_with("- "));
    }
    if depth / 2 > MAX_YAML_DEPTH {
        return Err(PlanDocumentError::ResourceLimit(
            "YAML nesting is too deep".into(),
        ));
    }
    if items > MAX_COLLECTION_ITEMS {
        return Err(PlanDocumentError::ResourceLimit(
            "collection has too many items".into(),
        ));
    }
    Ok(())
}

fn yaml_syntax_view(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut quote = None;
    let mut escaped = false;
    for character in line.chars() {
        match quote {
            Some('"') if escaped => {
                escaped = false;
                result.push(' ');
            }
            Some('"') if character == '\\' => {
                escaped = true;
                result.push(' ');
            }
            Some(active) if character == active => {
                quote = None;
                result.push(' ');
            }
            Some(_) => result.push(' '),
            None if matches!(character, '\'' | '"') => {
                quote = Some(character);
                result.push(' ');
            }
            None => result.push(character),
        }
    }
    result
}

fn validate_parsed_yaml_limits(raw: &RawFrontmatter) -> Result<(), PlanDocumentError> {
    if raw.depends_on.len() > MAX_COLLECTION_ITEMS || raw.touches.len() > MAX_COLLECTION_ITEMS {
        return Err(PlanDocumentError::ResourceLimit(
            "collection has too many items".into(),
        ));
    }
    for (field, value) in [
        ("id", raw.id.as_str()),
        ("title", raw.title.as_str()),
        ("workstream", raw.workstream.as_str()),
        ("merged_as", raw.merged_as.as_str()),
    ]
    .into_iter()
    .chain(
        raw.depends_on
            .iter()
            .map(|value| ("depends_on", value.as_str())),
    )
    .chain(raw.touches.iter().map(|value| ("touches", value.as_str())))
    {
        if value.len() > MAX_SCALAR_BYTES {
            return Err(PlanDocumentError::ResourceLimit(format!(
                "{field} scalar is too large"
            )));
        }
        if value.chars().any(char::is_control) {
            return Err(field_error(field, "control characters are forbidden"));
        }
    }
    Ok(())
}

fn contains_yaml_token(s: &str, token: char) -> bool {
    s.char_indices().any(|(i, c)| {
        c == token
            && (i == 0
                || s.as_bytes()[i - 1].is_ascii_whitespace()
                || matches!(s.as_bytes()[i - 1], b'[' | b',' | b'{'))
    })
}

fn parse_filename(path: &Path) -> Result<(TaskSequence, &str, &str), PlanDocumentError> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| field_error("filename", "must be UTF-8"))?;
    let stem = name
        .strip_suffix(".md")
        .ok_or_else(|| field_error("filename", "must end in .md"))?;
    if stem.len() < 6
        || !stem.as_bytes()[..4].iter().all(u8::is_ascii_digit)
        || stem.as_bytes().get(4) != Some(&b'-')
    {
        return Err(field_error(
            "filename",
            "must be `{workstream-prefix}{sequence}-{id}.md`",
        ));
    }
    let ws = &stem[..2];
    if matches!(ws.parse::<u8>(), Err(_) | Ok(0)) {
        return Err(field_error(
            "filename",
            "workstream prefix must be 01 through 99",
        ));
    }
    let sequence = TaskSequence::parse(&stem[2..4])?;
    let id = &stem[5..];
    if !is_kebab(id) {
        return Err(field_error(
            "filename",
            "ID suffix must be lowercase kebab-case",
        ));
    }
    Ok((sequence, id, ws))
}

fn parse_repo_pattern(
    value: &str,
    kind: TaskKind,
    source: &dyn PlanFileSource,
    task_source_path: &Path,
) -> Result<RepoPattern, PlanDocumentError> {
    if value.is_empty() || value.contains('\0') || value.contains('\\') {
        return Err(field_error("touches", "path is empty or unsafe"));
    }
    let path = Path::new(value);
    validate_relative_path(path)?;
    let segments: Vec<&str> = value.split('/').collect();
    let mut glob = false;
    for (i, segment) in segments.iter().enumerate() {
        if segment.contains('*') {
            let valid = *segment == "*" || (*segment == "**" && i + 1 == segments.len());
            if !valid {
                return Err(field_error(
                    "touches",
                    "only single-segment `*` and terminal `/**` are permitted",
                ));
            }
            glob = true;
        }
    }
    if segments.first() == Some(&".git") {
        return Err(field_error("touches", "`.git` paths are forbidden"));
    }
    if segments.first() == Some(&".makina") {
        if glob {
            return Err(field_error(
                "touches",
                "tracked `.makina` exceptions must be exact paths",
            ));
        }
        let tracked = source.is_tracked_ordinary_file(path)?;
        if value == ".makina/config.toml" {
            return match source.validation_base_oid() {
                None => Ok(RepoPattern::TrackedMakinaConfigCandidate),
                Some(base) if tracked => Ok(RepoPattern::TrackedMakinaConfig {
                    validation_base: base.to_owned(),
                }),
                Some(_) => Err(field_error(
                    "touches",
                    "config is not an ordinary tracked blob in the immutable validation base",
                )),
            };
        }
        if kind != TaskKind::Chore {
            return Err(field_error(
                "touches",
                "only a chore may delete another tracked `.makina` artifact",
            ));
        }
        return match source.validation_base_oid() {
            None => Ok(RepoPattern::TrackedMakinaDeletionCandidate(
                value.to_owned(),
            )),
            Some(base) if tracked => Ok(RepoPattern::TrackedMakinaDeletion {
                path: value.to_owned(),
                validation_base: base.to_owned(),
            }),
            Some(_) => Err(field_error(
                "touches",
                "artifact is not an ordinary tracked blob in the immutable validation base",
            )),
        };
    }
    let active_tasks_dir = task_source_path.parent();
    let active_plan_dir = active_tasks_dir.and_then(Path::parent);
    let coordinator_owned = value == "docs/plans/STATUS.md"
        || active_plan_dir.is_some_and(|dir| {
            path == dir.join("STATUS.md")
                || active_tasks_dir.is_some_and(|tasks| path.starts_with(tasks))
        });
    if coordinator_owned {
        return Err(field_error(
            "touches",
            "active plan and root status paths are coordinator-owned",
        ));
    }
    Ok(if glob {
        RepoPattern::Glob(value.to_owned())
    } else {
        RepoPattern::Path(value.to_owned())
    })
}

/// Parse an authoring-time footprint without binding tracked-file provenance.
/// Immutable-base validation later upgrades candidate `.makina` exceptions.
pub fn parse_generated_repo_pattern(
    value: &str,
    kind: TaskKind,
    task_source_path: &Path,
) -> Result<RepoPattern, PlanDocumentError> {
    struct UnboundSource;
    impl PlanFileSource for UnboundSource {
        fn read_file(&self, _relative_path: &Path) -> Result<Vec<u8>, PlanDocumentError> {
            Err(PlanDocumentError::Io(
                "unbound generated source has no files".into(),
            ))
        }
        fn is_tracked_ordinary_file(
            &self,
            _relative_path: &Path,
        ) -> Result<bool, PlanDocumentError> {
            Ok(false)
        }
        fn object_format(&self) -> GitObjectFormat {
            GitObjectFormat::Sha1
        }
        fn validation_base_oid(&self) -> Option<&str> {
            None
        }
    }
    parse_repo_pattern(value, kind, &UnboundSource, task_source_path)
}

fn validate_relative_path(path: &Path) -> Result<(), PlanDocumentError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(field_error(
            "path",
            "must be a normalized repository-relative path without traversal",
        ));
    }
    Ok(())
}

fn validate_status(
    status: AuthoredTaskStatus,
    merged: Option<&GitObjectId>,
) -> Result<(), PlanDocumentError> {
    if (status == AuthoredTaskStatus::Done) != merged.is_some() {
        return Err(field_error(
            "merged_as",
            "done requires merge evidence and every other status requires an empty value",
        ));
    }
    Ok(())
}

fn validate_body(body: &str, title: &str) -> Result<(), PlanDocumentError> {
    let normalized = body.strip_prefix('\n').unwrap_or(body);
    let first = normalized.lines().next().unwrap_or_default();
    if first != format!("# {title}") {
        return Err(field_error(
            "body",
            "H1 must exactly equal the frontmatter title",
        ));
    }
    let lines: Vec<&str> = normalized.lines().collect();
    let Some(steps) = lines.iter().position(|line| line.trim() == "**Steps:**") else {
        return Err(field_error(
            "body",
            "an ordered `**Steps:**` section is required",
        ));
    };
    let has_ordered = lines[steps + 1..].iter().any(|line| {
        let t = line.trim_start();
        t.split_once('.')
            .is_some_and(|(n, rest)| !rest.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    });
    if !has_ordered {
        return Err(field_error(
            "body",
            "the Steps section must contain an ordered list",
        ));
    }
    let last_nonempty = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .unwrap_or(0);
    let block_start = lines[..=last_nonempty]
        .iter()
        .rposition(|line| line.trim().is_empty())
        .map_or(0, |index| index + 1);
    let done = lines[block_start]
        .trim()
        .strip_prefix("- **Done when:**")
        .map(str::trim)
        .unwrap_or_default();
    if done.is_empty() {
        return Err(field_error(
            "body",
            "final semantic block must be a non-empty `Done when` criterion",
        ));
    }
    Ok(())
}

fn is_kebab(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !value.contains("--")
}

fn field_error(field: &str, message: &str) -> PlanDocumentError {
    PlanDocumentError::Field {
        field: field.into(),
        message: message.into(),
    }
}

fn yaml_scalar(value: &str) -> String {
    let starts_safely = value
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'.');
    let plain = starts_safely
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'/' | b'.'))
        && !matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "yes" | "no" | "on" | "off"
        );
    if plain {
        value.to_owned()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn render_string_list<'a>(out: &mut String, key: &str, values: impl Iterator<Item = &'a str>) {
    let values: Vec<_> = values.collect();
    if values.is_empty() {
        out.push_str(&format!("{key}: []\n"));
    } else {
        out.push_str(&format!("{key}:\n"));
        for value in values {
            out.push_str(&format!("  - {}\n", yaml_scalar(value)));
        }
    }
}

// ── Generated plan authoring ───────────────────────────────────────────────────────────

/// Structured, non-executable authoring data for one generated plan.
///
/// The renderer, rather than a model response, owns paths, frontmatter order,
/// bookkeeping fields, and document ordering. The resulting map is still only
/// a candidate bundle: callers must validate it with [`load_plan`] before
/// publishing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedPlanBundle {
    pub key: PlanKey,
    pub title: String,
    /// Authored Markdown beginning with the first section below the plan H1.
    pub scope: String,
    /// Authored Markdown beginning with the first section below the plan H1.
    pub architecture: String,
    pub initial_status: GeneratedInitialStatus,
    pub workstreams: Vec<GeneratedWorkstream>,
    pub tasks: Vec<GeneratedTaskDocument>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedInitialStatus {
    pub goal: String,
    pub root_cause: String,
    pub approach: String,
    pub outcome: String,
    pub base_name: String,
    pub base_oid: GitObjectId,
    /// ISO calendar date (`YYYY-MM-DD`) supplied by the generation coordinator.
    pub last_updated: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedWorkstream {
    pub id: WorkstreamId,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedTaskDocument {
    pub sequence: TaskSequence,
    pub frontmatter: TaskFrontmatter,
    /// Markdown bytes after the frontmatter delimiter, normally beginning with
    /// a newline and the task H1.
    pub body: String,
}

impl GeneratedPlanBundle {
    /// Render the closed plan subtree in canonical path and byte form.
    pub fn render_files(&self) -> Result<BTreeMap<PathBuf, Vec<u8>>, PlanDocumentError> {
        self.validate_authoring_data()?;
        let heading = format!("# Plan {} — {}\n", self.key.number, self.title.trim());
        let mut files = BTreeMap::new();
        files.insert(
            PathBuf::from("SCOPE.md"),
            render_authored_markdown(&heading, &self.scope).into_bytes(),
        );
        files.insert(
            PathBuf::from("ARCHITECTURE.md"),
            render_authored_markdown(&heading, &self.architecture).into_bytes(),
        );
        files.insert(
            PathBuf::from("STATUS.md"),
            self.render_initial_status(&heading).into_bytes(),
        );

        let mut tasks = self.tasks.clone();
        tasks.sort_by_key(task_filename);
        for mut task in tasks {
            task.frontmatter.depends_on.sort();
            task.frontmatter
                .touches
                .sort_by(|left, right| left.as_str().cmp(right.as_str()));
            task.frontmatter.status = AuthoredTaskStatus::Planned;
            task.frontmatter.merged_as = None;
            let path = PathBuf::from("tasks").join(task_filename(&task));
            let document = TaskDocument {
                source_path: self.key.relative_dir.join(&path),
                sequence: task.sequence,
                frontmatter: task.frontmatter,
                body: normalize_task_body(&task.body),
            };
            files.insert(path, document.render().into_bytes());
        }
        Ok(files)
    }

    fn validate_authoring_data(&self) -> Result<(), PlanDocumentError> {
        for (field, value) in [
            ("title", self.title.as_str()),
            ("scope", self.scope.as_str()),
            ("architecture", self.architecture.as_str()),
            ("goal", self.initial_status.goal.as_str()),
            ("root_cause", self.initial_status.root_cause.as_str()),
            ("approach", self.initial_status.approach.as_str()),
            ("outcome", self.initial_status.outcome.as_str()),
            ("base_name", self.initial_status.base_name.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(field_error(field, "must not be empty"));
            }
            if value.contains('\0') || value.contains('\r') {
                return Err(field_error(field, "contains forbidden control characters"));
            }
        }
        let date = self.initial_status.last_updated.as_bytes();
        if date.len() != 10
            || date[4] != b'-'
            || date[7] != b'-'
            || date
                .iter()
                .enumerate()
                .any(|(index, byte)| !matches!(index, 4 | 7) && !byte.is_ascii_digit())
        {
            return Err(field_error("last_updated", "must be YYYY-MM-DD"));
        }
        let workstreams = self
            .workstreams
            .iter()
            .map(|workstream| workstream.id.as_str())
            .collect::<BTreeSet<_>>();
        if workstreams.len() != self.workstreams.len() {
            return Err(field_error("workstreams", "IDs must be unique"));
        }
        if self
            .workstreams
            .iter()
            .any(|workstream| workstream.title.trim().is_empty())
        {
            return Err(field_error("workstreams", "titles must not be empty"));
        }
        let expected_workstreams = self
            .workstreams
            .iter()
            .map(|workstream| {
                (
                    workstream.id.as_str().to_owned(),
                    workstream.title.trim().to_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let (scope_workstreams, scope_duplicates) = scope_workstreams(&self.scope);
        let (architecture_workstreams, architecture_duplicates) =
            architecture_workstreams(&self.architecture);
        if !scope_duplicates.is_empty()
            || !architecture_duplicates.is_empty()
            || scope_workstreams != expected_workstreams
            || architecture_workstreams != expected_workstreams
        {
            return Err(field_error(
                "workstreams",
                "scope and architecture must each declare the typed workstreams exactly once",
            ));
        }
        if self.tasks.is_empty() {
            return Err(field_error("tasks", "must contain at least one task"));
        }
        for task in &self.tasks {
            if !workstreams.contains(task.frontmatter.workstream.as_str()) {
                return Err(field_error(
                    "workstream",
                    "task references an undeclared workstream",
                ));
            }
        }
        let paths = self
            .tasks
            .iter()
            .map(task_filename)
            .collect::<BTreeSet<_>>();
        if paths.len() != self.tasks.len() {
            return Err(field_error(
                "tasks",
                "canonical task filenames must be unique",
            ));
        }
        Ok(())
    }

    fn render_initial_status(&self, heading: &str) -> String {
        let status = &self.initial_status;
        let short_oid = &status.base_oid.as_str()[..7];
        format!(
            "{} — 📋 Planned\n\n- **Status:** 📋 Planned.\n- **Goal:** {}\n- **Root cause:** {}\n- **Approach:** {}\n- **Progress:** 0/{} tasks done; 0 blocked; 0 dropped.\n- **Integration:** `planned`; run —; base `{}` @ `{}`; validation base —; mode —; final integration —.\n- **Exceptions:** —.\n- **Outcome:** {}\n\n_Last updated: {}, against `{}` @ `{}`._\n",
            heading.trim_end(),
            status.goal.trim(),
            status.root_cause.trim(),
            status.approach.trim(),
            self.tasks.len(),
            status.base_name,
            status.base_oid,
            status.outcome.trim(),
            status.last_updated,
            status.base_name,
            short_oid,
        )
    }
}

fn render_authored_markdown(heading: &str, authored: &str) -> String {
    format!("{}\n{}\n", heading.trim_end(), authored.trim())
}

fn normalize_task_body(body: &str) -> String {
    format!("\n{}\n", body.trim())
}

fn task_filename(task: &GeneratedTaskDocument) -> String {
    format!(
        "{}{}-{}.md",
        &task.frontmatter.workstream.as_str()[2..],
        task.sequence,
        task.frontmatter.id
    )
}

// ── Complete plan bundle model ─────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PlanKey {
    pub relative_dir: PathBuf,
    pub number: String,
    pub slug: String,
}

impl PlanKey {
    pub fn parse(path: impl Into<PathBuf>) -> Result<Self, PlanDocumentError> {
        let relative_dir = path.into();
        validate_relative_path(&relative_dir)?;
        let basename = relative_dir
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| field_error("plan", "basename must be UTF-8"))?;
        let Some((number, slug)) = basename.split_once('-') else {
            return Err(field_error("plan", "basename must be `NNNN-<slug>`"));
        };
        let valid_slug = !slug.is_empty()
            && slug.split('-').all(|token| {
                !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_alphanumeric())
            });
        if basename.len() > 200
            || number.len() != 4
            || !number.bytes().all(|byte| byte.is_ascii_digit())
            || !valid_slug
        {
            return Err(field_error(
                "plan",
                "basename must be a <=200-byte `NNNN-<ASCII-slug>`",
            ));
        }
        let reference = format!("refs/heads/plan/{basename}");
        let status = Command::new("git")
            .args(["check-ref-format", &reference])
            .status()
            .map_err(|error| PlanDocumentError::Git(error.to_string()))?;
        if !status.success() {
            return Err(field_error(
                "plan",
                "basename is not Git-ref-safe without sanitization",
            ));
        }
        let number = number.to_owned();
        let slug = slug.to_owned();
        Ok(Self {
            relative_dir,
            number,
            slug,
        })
    }
    pub fn ref_name(&self) -> String {
        format!("refs/heads/plan/{}-{}", self.number, self.slug)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownDocument {
    pub source_path: PathBuf,
    pub body: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanIntegrationState {
    Planned,
    Assembling,
    AwaitingIntegration,
    FinalizationPending,
    IntegrationBlocked,
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanStatusDocument {
    pub source: MarkdownDocument,
    pub display_status: String,
    pub goal: String,
    pub root_cause: String,
    pub approach: String,
    pub done: usize,
    pub blocked: usize,
    pub dropped: usize,
    pub total: usize,
    pub integration_state: PlanIntegrationState,
    pub run: Option<String>,
    pub base_name: String,
    pub base_oid: GitObjectId,
    pub validation_base_oid: Option<GitObjectId>,
    pub mode: Option<String>,
    pub final_oid: Option<GitObjectId>,
    pub exceptions: String,
    pub outcome: String,
    pub last_updated: String,
}

string_newtype!(SourceDigest);
string_newtype!(PlanDigest);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanDocument {
    pub key: PlanKey,
    pub title: String,
    pub scope: MarkdownDocument,
    pub architecture: MarkdownDocument,
    pub status: PlanStatusDocument,
    pub workstreams: BTreeSet<String>,
    pub tasks: Vec<TaskDocument>,
    pub source_digest: SourceDigest,
    pub executable_digest: PlanDigest,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlanValidationDiagnostic {
    pub code: String,
    pub path: PathBuf,
    pub field: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanValidationReport {
    pub diagnostics: Vec<PlanValidationDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyDiagnostic {
    pub code: &'static str,
    pub task_id: Option<String>,
    pub message: String,
}

pub fn validate_dependency_records(records: &[(String, Vec<String>)]) -> Vec<DependencyDiagnostic> {
    let mut result = Vec::new();
    let mut ids = BTreeSet::new();
    for (id, _) in records {
        if !ids.insert(id.as_str()) {
            result.push(DependencyDiagnostic {
                code: "duplicate-task-id",
                task_id: Some(id.clone()),
                message: format!("duplicate task id: {id}"),
            });
        }
    }
    for (id, dependencies) in records {
        let mut local = BTreeSet::new();
        for dependency in dependencies {
            if !local.insert(dependency) {
                result.push(DependencyDiagnostic {
                    code: "duplicate-dependency",
                    task_id: Some(id.clone()),
                    message: format!("task `{id}` repeats dependency `{dependency}`"),
                });
            } else if dependency == id {
                result.push(DependencyDiagnostic {
                    code: "self-dependency",
                    task_id: Some(id.clone()),
                    message: format!("task `{id}` depends on itself"),
                });
            } else if !ids.contains(dependency.as_str()) {
                result.push(DependencyDiagnostic {
                    code: "unknown-dependency",
                    task_id: Some(id.clone()),
                    message: format!("task `{id}` depends on unknown task `{dependency}`"),
                });
            }
        }
    }
    let map = records
        .iter()
        .map(|(id, dependencies)| (id.as_str(), dependencies.as_slice()))
        .collect::<BTreeMap<_, _>>();
    let mut states = BTreeMap::new();
    let mut stack = Vec::new();
    fn visit<'a>(
        id: &'a str,
        map: &BTreeMap<&'a str, &'a [String]>,
        states: &mut BTreeMap<&'a str, u8>,
        stack: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        states.insert(id, 1);
        stack.push(id);
        for dependency in map[id] {
            let dependency = dependency.as_str();
            if !map.contains_key(dependency) {
                continue;
            }
            if states.get(dependency) == Some(&1) {
                let start = stack
                    .iter()
                    .position(|entry| *entry == dependency)
                    .unwrap_or(0);
                let mut cycle = stack[start..]
                    .iter()
                    .map(|entry| (*entry).to_owned())
                    .collect::<Vec<_>>();
                cycle.push(dependency.to_owned());
                return Some(cycle);
            }
            if states.get(dependency).copied().unwrap_or(0) == 0
                && let Some(cycle) = visit(dependency, map, states, stack)
            {
                return Some(cycle);
            }
        }
        stack.pop();
        states.insert(id, 2);
        None
    }
    for id in map.keys().copied() {
        if states.get(id).copied().unwrap_or(0) == 0
            && let Some(cycle) = visit(id, &map, &mut states, &mut stack)
        {
            result.push(DependencyDiagnostic {
                code: "dependency-cycle",
                task_id: None,
                message: cycle.join(" -> "),
            });
            break;
        }
    }
    result
}

impl PlanValidationReport {
    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }
    fn add(
        &mut self,
        code: &str,
        path: impl Into<PathBuf>,
        field: Option<&str>,
        message: impl Into<String>,
    ) {
        self.diagnostics.push(PlanValidationDiagnostic {
            code: code.into(),
            path: path.into(),
            field: field.map(str::to_owned),
            message: message.into(),
        });
    }
    fn sorted(mut self) -> Self {
        self.diagnostics.sort();
        self.diagnostics.dedup();
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanCandidate {
    NotCandidate,
    Plan(Box<PlanDocument>),
}

#[derive(Clone, Debug, Default)]
pub struct PlanReservations {
    pub numbered_directories: BTreeMap<String, Vec<PathBuf>>,
    pub verified_registrations: BTreeMap<String, Vec<String>>,
}

pub fn digest_records_v1(domain: &str, records: &[(&str, Vec<u8>)]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    for (tag, value) in records {
        hasher.update((tag.len() as u32).to_be_bytes());
        hasher.update(tag.as_bytes());
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }
    crate::json::hex_encode(&hasher.finalize())
}

pub fn digest_list_v1(domain: &str, tag: &str, items: &[Vec<u8>]) -> String {
    let mut value = Vec::new();
    value.extend_from_slice(&(items.len() as u64).to_be_bytes());
    for item in items {
        value.extend_from_slice(&(4u32).to_be_bytes());
        value.extend_from_slice(b"item");
        value.extend_from_slice(&(item.len() as u64).to_be_bytes());
        value.extend_from_slice(item);
    }
    digest_records_v1(domain, &[(tag, value)])
}

pub fn load_plan_path(
    source: &dyn PlanFileSource,
    relative_dir: impl Into<PathBuf>,
    reservations: &PlanReservations,
) -> Result<PlanCandidate, PlanValidationReport> {
    let relative_dir = relative_dir.into();
    let tasks = relative_dir.join("tasks");
    match source.entry_kind(&tasks) {
        Ok(None) => return Ok(PlanCandidate::NotCandidate),
        Ok(Some(PlanSourceEntryKind::Directory)) => {}
        Ok(Some(_)) => {
            let mut report = PlanValidationReport::default();
            report.add(
                "malformed-tasks-entry",
                tasks,
                None,
                "tasks exists but is not an ordinary directory",
            );
            return Err(report.sorted());
        }
        Err(error) => {
            let mut report = PlanValidationReport::default();
            report.add("malformed-tasks-entry", tasks, None, error.to_string());
            return Err(report.sorted());
        }
    }
    let key = PlanKey::parse(relative_dir.clone()).map_err(|error| {
        let mut report = PlanValidationReport::default();
        report.add(
            "invalid-plan-key",
            relative_dir,
            Some("plan"),
            error.to_string(),
        );
        report.sorted()
    })?;
    load_plan(source, key, reservations)
}

pub fn load_plan(
    source: &dyn PlanFileSource,
    key: PlanKey,
    reservations: &PlanReservations,
) -> Result<PlanCandidate, PlanValidationReport> {
    let tasks_dir = key.relative_dir.join("tasks");
    match source.entry_kind(&tasks_dir) {
        Ok(None) => return Ok(PlanCandidate::NotCandidate),
        Ok(Some(PlanSourceEntryKind::Directory)) => {}
        Ok(Some(_)) => {
            let mut report = PlanValidationReport::default();
            report.add(
                "malformed-tasks-entry",
                tasks_dir,
                None,
                "tasks exists but is not an ordinary directory",
            );
            return Err(report.sorted());
        }
        Err(error) => {
            let mut report = PlanValidationReport::default();
            report.add("malformed-tasks-entry", tasks_dir, None, error.to_string());
            return Err(report.sorted());
        }
    }
    let mut report = PlanValidationReport::default();
    for name in ["SCOPE.md", "ARCHITECTURE.md", "STATUS.md"] {
        let path = key.relative_dir.join(name);
        if !matches!(
            source.entry_kind(&path),
            Ok(Some(PlanSourceEntryKind::File))
        ) {
            report.add(
                "missing-plan-file",
                path,
                None,
                format!("new-format plan requires ordinary {name}"),
            );
        }
    }
    // Deliberately assembled here so the obsolete filename cannot be copied as
    // a live producer constant; it is used only to reject mixed bundles.
    let legacy = key.relative_dir.join(["TASKS", ".md"].concat());
    if source.entry_kind(&legacy).ok().flatten().is_some() {
        report.add(
            "mixed-plan-format",
            legacy,
            None,
            "typed tasks/ cannot coexist with the inert pre-cutover task-list file",
        );
    }
    let entries = source.list_directory(&tasks_dir).unwrap_or_else(|error| {
        report.add(
            "invalid-tasks-directory",
            &tasks_dir,
            None,
            error.to_string(),
        );
        Vec::new()
    });
    let mut task_paths = Vec::new();
    for path in entries {
        match source.entry_kind(&path) {
            Ok(Some(PlanSourceEntryKind::File))
                if path.extension().and_then(|value| value.to_str()) == Some("md") =>
            {
                task_paths.push(path)
            }
            Ok(Some(PlanSourceEntryKind::Directory)) => {
                report.add("nested-task-directory", path, None, "tasks/ must be flat")
            }
            _ => report.add(
                "non-task-entry",
                path,
                None,
                "tasks/ accepts ordinary .md files only",
            ),
        }
    }
    if task_paths.is_empty() {
        report.add(
            "empty-task-directory",
            &tasks_dir,
            None,
            "tasks/ must not be empty",
        );
    }
    let scope = load_markdown(source, key.relative_dir.join("SCOPE.md"), &mut report);
    let architecture = load_markdown(
        source,
        key.relative_dir.join("ARCHITECTURE.md"),
        &mut report,
    );
    let status_markdown = load_markdown(source, key.relative_dir.join("STATUS.md"), &mut report);
    let status = status_markdown.clone().and_then(|document| {
        let surface_errors = status_surface_errors(&document.body);
        for error in &surface_errors {
            report.add("invalid-plan-status", &document.source_path, None, error);
        }
        if !surface_errors.is_empty() {
            return None;
        }
        match parse_status_document(document, source.object_format()) {
            Ok(status) => Some(status),
            Err(error) => {
                report.add(
                    "invalid-plan-status",
                    key.relative_dir.join("STATUS.md"),
                    None,
                    error,
                );
                None
            }
        }
    });
    if let (Some(expected), Some(recorded)) = (
        source.expected_validation_base_oid(),
        status
            .as_ref()
            .and_then(|status| status.validation_base_oid.as_ref())
            .map(GitObjectId::as_str),
    ) && expected != recorded
    {
        report.add(
            "validation-base-mismatch",
            key.relative_dir.join("STATUS.md"),
            Some("Integration"),
            "registered source validation base disagrees with STATUS",
        );
    }
    let provenance_source = status
        .as_ref()
        .and_then(|status| status.validation_base_oid.as_ref())
        .map(|base| StatusValidationBaseSource {
            source,
            validation_base: base.as_str(),
        });
    let task_source: &dyn PlanFileSource = provenance_source
        .as_ref()
        .map_or(source, |source| source as &dyn PlanFileSource);
    let mut tasks = Vec::new();
    let mut malformed_stubs = Vec::new();
    for path in task_paths {
        match parse_task_document(task_source, &path) {
            Ok(task) => tasks.push(task),
            Err(error) => {
                report.add("invalid-task-document", &path, None, error.to_string());
                if let Some(stub) = parse_task_stub(task_source, &path) {
                    malformed_stubs.push(stub);
                }
            }
        }
    }
    tasks.sort_by(|left, right| {
        left.source_path
            .as_os_str()
            .as_encoded_bytes()
            .cmp(right.source_path.as_os_str().as_encoded_bytes())
    });

    let prefix = format!("# Plan {} — ", key.number);
    let scope_title = scope
        .as_ref()
        .and_then(|document| document.body.lines().next()?.strip_prefix(&prefix))
        .map(str::to_owned);
    let architecture_title = architecture
        .as_ref()
        .and_then(|document| document.body.lines().next()?.strip_prefix(&prefix))
        .map(str::to_owned);
    let status_title = status_markdown
        .as_ref()
        .and_then(|document| document.body.lines().next()?.strip_prefix(&prefix))
        .map(|tail| {
            tail.rsplit_once(" — ")
                .map_or(tail, |(title, _)| title)
                .to_owned()
        });
    let title = status_title
        .clone()
        .or(scope_title.clone())
        .unwrap_or_default();
    let scope_identity = scope_title.as_ref() == Some(&title)
        || scope.as_ref().is_some_and(|document| {
            document.body.lines().next() == Some(format!("# Scope — Plan {}", key.number).as_str())
        });
    let architecture_identity = architecture_title.as_ref() == Some(&title)
        || architecture.as_ref().is_some_and(|document| {
            document.body.lines().next().is_some_and(|line| {
                line == format!("# Architecture — Plan {}", key.number)
                    || line == format!("# Architecture — Plan {} (deltas)", key.number)
            })
        });
    if !scope_identity || !architecture_identity || status_title.is_none() {
        report.add(
            "plan-identity-mismatch",
            &key.relative_dir,
            Some("title"),
            "all plan H1s must match folder number and title",
        );
    }
    let (scope_workstreams, scope_duplicates) = scope
        .as_ref()
        .map(|document| scope_workstreams(&document.body))
        .unwrap_or_default();
    let (architecture_workstreams, architecture_duplicates) = architecture
        .as_ref()
        .map(|document| architecture_workstreams(&document.body))
        .unwrap_or_default();
    if let Some(document) = scope.as_ref() {
        for error in validate_scope_workstream_grammar(&document.body) {
            report.add(
                "invalid-scope-workstream",
                &document.source_path,
                Some("workstreams"),
                error,
            );
        }
    }
    if let Some(document) = architecture.as_ref() {
        for error in validate_architecture_workstream_grammar(&document.body) {
            report.add(
                "invalid-architecture-workstream",
                &document.source_path,
                Some("workstreams"),
                error,
            );
        }
    }
    for duplicate in scope_duplicates {
        report.add(
            "duplicate-scope-workstream",
            key.relative_dir.join("SCOPE.md"),
            Some("workstreams"),
            format!("duplicate workstream {duplicate}"),
        );
    }
    for duplicate in architecture_duplicates {
        report.add(
            "duplicate-architecture-workstream",
            key.relative_dir.join("ARCHITECTURE.md"),
            Some("workstreams"),
            format!("duplicate workstream {duplicate}"),
        );
    }
    if scope_workstreams != architecture_workstreams {
        report.add(
            "workstream-mismatch",
            &key.relative_dir,
            Some("workstreams"),
            "Scope and Architecture workstreams must match one-to-one",
        );
    }
    for task in &tasks {
        if !scope_workstreams.contains_key(task.frontmatter.workstream.as_str()) {
            report.add(
                "undeclared-workstream",
                &task.source_path,
                Some("workstream"),
                "task workstream is not declared by the plan",
            );
        }
    }
    for stub in &malformed_stubs {
        if !scope_workstreams.contains_key(&stub.workstream) {
            report.add(
                "undeclared-workstream",
                &stub.path,
                Some("workstream"),
                "malformed task selects no declared workstream",
            );
        }
    }
    validate_bundle_dag(&tasks, &malformed_stubs, &mut report);
    validate_plan_reservations(&key, reservations, &mut report);
    if let Some(status) = status.as_ref() {
        validate_plan_status(status, &tasks, &malformed_stubs, &mut report);
        validate_registration_provenance(&key, reservations, status, &mut report);
    }
    if !report.is_empty() {
        return Err(report.sorted());
    }
    let scope = scope.expect("required source exists after validation");
    let architecture = architecture.expect("required source exists after validation");
    let status = status.expect("status parsed after validation");
    let (source_digest, executable_digest) =
        plan_digests(&key, &title, &scope, &architecture, &status, &tasks);
    Ok(PlanCandidate::Plan(Box::new(PlanDocument {
        key,
        title,
        scope,
        architecture,
        status,
        workstreams: scope_workstreams.into_keys().collect(),
        tasks,
        source_digest,
        executable_digest,
    })))
}

fn status_surface_errors(markdown: &str) -> Vec<String> {
    let expected = [
        "Status",
        "Goal",
        "Root cause",
        "Approach",
        "Progress",
        "Integration",
        "Exceptions",
        "Outcome",
    ];
    let mut counts = BTreeMap::new();
    let mut errors = Vec::new();
    for line in markdown.lines() {
        let line = line.strip_prefix("- ").unwrap_or(line);
        if let Some(rest) = line.strip_prefix("**")
            && let Some((name, value)) = rest.split_once(":** ")
        {
            if !expected.contains(&name) {
                errors.push(format!("unknown status anchor {name}"));
            } else {
                *counts.entry(name).or_insert(0usize) += 1;
                if value.trim().is_empty() {
                    errors.push(format!("empty {name} anchor"));
                }
            }
        }
    }
    for name in expected {
        match counts.get(name).copied().unwrap_or(0) {
            0 => errors.push(format!("missing {name} anchor")),
            1 => {}
            count => errors.push(format!("{name} anchor occurs {count} times")),
        }
    }
    if let Some(line) = markdown
        .lines()
        .find_map(|line| line.strip_prefix("- **Integration:** "))
        && line.split(';').count() != 6
    {
        errors.push("Integration must contain exactly six semicolon-delimited fields".into());
    }
    let last_updated = markdown
        .lines()
        .filter(|line| line.starts_with("_Last updated:") && line.ends_with("._"))
        .count();
    if last_updated != 1 {
        errors.push(format!("last-updated anchor occurs {last_updated} times"));
    }
    errors.sort();
    errors
}

fn load_markdown(
    source: &dyn PlanFileSource,
    path: PathBuf,
    report: &mut PlanValidationReport,
) -> Option<MarkdownDocument> {
    match source.read_file(&path).and_then(|bytes| {
        String::from_utf8(bytes).map_err(|_| PlanDocumentError::Io("Markdown must be UTF-8".into()))
    }) {
        Ok(body) => Some(MarkdownDocument {
            source_path: path,
            body,
        }),
        Err(error) => {
            report.add("unreadable-plan-file", path, None, error.to_string());
            None
        }
    }
}

fn scope_workstreams(markdown: &str) -> (BTreeMap<String, String>, BTreeSet<String>) {
    let mut active = false;
    let mut result = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    for line in markdown.lines() {
        if line == "## In scope" {
            active = true;
            continue;
        }
        if active && line.starts_with("## ") {
            break;
        }
        if active
            && let Some(value) = line.strip_prefix("- **")
            && let Some((number, name)) = value.split_once(" — ")
            && number.len() == 4
        {
            let name = name
                .split(".**")
                .next()
                .unwrap_or(name)
                .trim_end_matches("**")
                .trim_end_matches('.');
            if result.insert(number.into(), name.into()).is_some() {
                duplicates.insert(number.into());
            }
        }
    }
    (result, duplicates)
}

fn architecture_workstreams(markdown: &str) -> (BTreeMap<String, String>, BTreeSet<String>) {
    let mut result = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    for (number, name) in markdown
        .lines()
        .filter_map(|line| line.strip_prefix("## ")?.split_once(" — "))
        .filter(|(number, _)| number.len() == 4 && number.bytes().all(|byte| byte.is_ascii_digit()))
    {
        if result.insert(number.into(), name.trim().into()).is_some() {
            duplicates.insert(number.into());
        }
    }
    (result, duplicates)
}

fn valid_workstream_number(number: &str) -> bool {
    number.len() == 4
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && matches!(number.parse::<u16>(), Ok(1..=99))
}

fn validate_scope_workstream_grammar(markdown: &str) -> Vec<String> {
    let mut active = false;
    let mut errors = Vec::new();
    for line in markdown.lines() {
        if line == "## In scope" {
            active = true;
            continue;
        }
        if active && line.starts_with("## ") {
            break;
        }
        if active && line.starts_with("- **") {
            let valid = line
                .strip_prefix("- **")
                .and_then(|value| value.split_once(" — "))
                .is_some_and(|(number, rest)| {
                    valid_workstream_number(number)
                        && rest.split_once(".**").is_some_and(|(name, suffix)| {
                            !name.trim().is_empty()
                                && (suffix.is_empty() || suffix.starts_with(' '))
                        })
                });
            if !valid {
                errors.push(format!("invalid Scope workstream declaration `{line}`"));
            }
        }
    }
    errors
}

fn validate_architecture_workstream_grammar(markdown: &str) -> Vec<String> {
    markdown
        .lines()
        .filter(|line| {
            line.starts_with("## ") && line.as_bytes().get(3).is_some_and(u8::is_ascii_digit)
        })
        .filter_map(|line| {
            let valid = line
                .strip_prefix("## ")
                .and_then(|value| value.split_once(" — "))
                .is_some_and(|(number, name)| {
                    valid_workstream_number(number) && !name.trim().is_empty()
                });
            (!valid).then(|| format!("invalid Architecture workstream heading `{line}`"))
        })
        .collect()
}

fn validate_plan_reservations(
    key: &PlanKey,
    reservations: &PlanReservations,
    report: &mut PlanValidationReport,
) {
    let directory = reservations
        .numbered_directories
        .get(&key.number)
        .into_iter()
        .flatten()
        .any(|path| path != &key.relative_dir);
    let registration = reservations
        .verified_registrations
        .get(&key.number)
        .is_some_and(|values| {
            values.iter().any(|identity| {
                identity != &key.ref_name() && identity != &key.relative_dir.to_string_lossy()
            })
        });
    if directory || registration {
        report.add(
            "reserved-plan-number",
            &key.relative_dir,
            Some("number"),
            format!("{} is already reserved", key.number),
        );
    }
}

fn validate_registration_provenance(
    key: &PlanKey,
    reservations: &PlanReservations,
    status: &PlanStatusDocument,
    report: &mut PlanValidationReport,
) {
    let own_registration = reservations
        .verified_registrations
        .get(&key.number)
        .is_some_and(|values| {
            values.iter().any(|identity| {
                identity == &key.ref_name() || identity == &key.relative_dir.to_string_lossy()
            })
        });
    if own_registration && status.validation_base_oid.is_none() {
        report.add(
            "missing-registration-validation-base",
            &status.source.source_path,
            Some("Integration"),
            "a verified Phase-R identity requires an immutable validation-base OID",
        );
    }
}

#[derive(Clone, Debug)]
struct BundleTaskStub {
    path: PathBuf,
    id: String,
    workstream: String,
    sequence: String,
    dependencies: Vec<String>,
    status: Option<AuthoredTaskStatus>,
}

fn parse_task_stub(source: &dyn PlanFileSource, path: &Path) -> Option<BundleTaskStub> {
    let bytes = source.read_file(path).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let (yaml, _) = split_frontmatter(text).ok()?;
    let raw: RawFrontmatter = serde_saphyr::from_str(yaml).ok()?;
    let sequence = path.file_name()?.to_str()?.get(2..4)?.to_owned();
    Some(BundleTaskStub {
        path: path.to_owned(),
        id: raw.id,
        workstream: raw.workstream,
        sequence,
        dependencies: raw.depends_on,
        status: Some(raw.status),
    })
}

fn validate_bundle_dag(
    tasks: &[TaskDocument],
    stubs: &[BundleTaskStub],
    report: &mut PlanValidationReport,
) {
    let mut prefixes = BTreeSet::new();
    for task in tasks {
        let prefix = format!(
            "{}{}",
            &task.frontmatter.workstream.as_str()[2..],
            task.sequence
        );
        if !prefixes.insert(prefix) {
            report.add(
                "duplicate-task-prefix",
                &task.source_path,
                Some("filename"),
                "numeric prefixes must be unique",
            );
        }
    }
    for stub in stubs {
        let prefix = format!(
            "{}{}",
            stub.workstream.get(2..).unwrap_or(""),
            stub.sequence
        );
        if !prefixes.insert(prefix) {
            report.add(
                "duplicate-task-prefix",
                &stub.path,
                Some("filename"),
                "numeric prefixes must be unique",
            );
        }
    }
    let mut records = tasks
        .iter()
        .map(|task| {
            (
                task.frontmatter.id.as_str().to_owned(),
                task.frontmatter
                    .depends_on
                    .iter()
                    .map(|dependency| dependency.as_str().to_owned())
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    records.extend(
        stubs
            .iter()
            .map(|stub| (stub.id.clone(), stub.dependencies.clone())),
    );
    for diagnostic in validate_dependency_records(&records) {
        let path = diagnostic
            .task_id
            .as_deref()
            .and_then(|id| tasks.iter().find(|task| task.frontmatter.id.as_str() == id))
            .map(|task| task.source_path.clone())
            .or_else(|| {
                diagnostic
                    .task_id
                    .as_deref()
                    .and_then(|id| stubs.iter().find(|stub| stub.id == id))
                    .map(|stub| stub.path.clone())
            })
            .unwrap_or_else(|| {
                tasks.first().map_or_else(
                    || {
                        stubs
                            .first()
                            .map_or_else(PathBuf::new, |stub| stub.path.clone())
                    },
                    |task| task.source_path.clone(),
                )
            });
        report.add(
            diagnostic.code,
            path,
            Some("depends_on"),
            diagnostic.message,
        );
    }
}

fn parse_status_document(
    source: MarkdownDocument,
    format: GitObjectFormat,
) -> Result<PlanStatusDocument, String> {
    let mut anchors = BTreeMap::new();
    let expected = [
        "Status",
        "Goal",
        "Root cause",
        "Approach",
        "Progress",
        "Integration",
        "Exceptions",
        "Outcome",
    ];
    for line in source.body.lines() {
        let line = line.strip_prefix("- ").unwrap_or(line);
        if let Some(rest) = line.strip_prefix("**")
            && let Some((name, value)) = rest.split_once(":** ")
        {
            if !expected.contains(&name) {
                return Err(format!("unknown status anchor {name}"));
            }
            if anchors.insert(name, value.to_owned()).is_some() {
                return Err(format!("duplicate {name} anchor"));
            }
        }
    }
    for name in expected {
        if !anchors.contains_key(name) {
            return Err(format!("missing {name} anchor"));
        }
    }
    let get = |name: &str| {
        anchors
            .get(name)
            .cloned()
            .ok_or_else(|| format!("missing {name} anchor"))
    };
    let display_status = source
        .body
        .lines()
        .next()
        .and_then(|line| line.rsplit_once(" — "))
        .map(|(_, status)| status.trim().to_owned())
        .ok_or("invalid status H1")?;
    let authored_status = get("Status")?;
    if authored_status.trim_end_matches('.').trim() != display_status {
        return Err("Status anchor and H1 disagree".into());
    }
    let goal = get("Goal")?;
    let root_cause = get("Root cause")?;
    let approach = get("Approach")?;
    let outcome = get("Outcome")?;
    if [&goal, &root_cause, &approach, &outcome]
        .iter()
        .any(|value| value.trim().is_empty())
    {
        return Err("narrative status anchors must be non-empty".into());
    }
    let progress = get("Progress")?;
    let progress_parts = progress.split(';').map(str::trim).collect::<Vec<_>>();
    let (done, total) = progress_parts
        .first()
        .and_then(|part| part.split_whitespace().next())
        .and_then(|part| part.split_once('/'))
        .ok_or("invalid Progress done/total")?;
    let done = done.parse().map_err(|_| "invalid done count")?;
    let total = total.parse().map_err(|_| "invalid total count")?;
    let blocked = progress_parts
        .get(1)
        .and_then(|part| part.split_whitespace().next())
        .ok_or("missing blocked count")?
        .parse()
        .map_err(|_| "invalid blocked count")?;
    let dropped = progress_parts
        .get(2)
        .and_then(|part| part.split_whitespace().next())
        .ok_or("missing dropped count")?
        .parse()
        .map_err(|_| "invalid dropped count")?;
    let integration = get("Integration")?;
    let parts = integration.split(';').map(str::trim).collect::<Vec<_>>();
    if parts.len() != 6 {
        return Err("Integration must contain exactly six fields".into());
    }
    let integration_state = match parts.first().map(|part| part.trim_matches('`')) {
        Some("planned") => PlanIntegrationState::Planned,
        Some("assembling") => PlanIntegrationState::Assembling,
        Some("awaiting-integration") => PlanIntegrationState::AwaitingIntegration,
        Some("finalization-pending") => PlanIntegrationState::FinalizationPending,
        Some("integration-blocked") => PlanIntegrationState::IntegrationBlocked,
        Some("complete") => PlanIntegrationState::Complete,
        _ => return Err("invalid Integration state".into()),
    };
    let optional = |value: &str| {
        let value = value.trim().trim_end_matches('.').trim_matches('`');
        (value != "—").then(|| value.to_owned())
    };
    let run = parts
        .get(1)
        .and_then(|part| part.strip_prefix("run "))
        .and_then(optional);
    if run.as_deref().is_some_and(|run| {
        run.len() != 26
            || !run
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    }) {
        return Err("run must be a canonical 26-character ULID".into());
    }
    let base = parts
        .get(2)
        .and_then(|part| part.strip_prefix("base "))
        .ok_or("missing integration base")?;
    let (base_name, base_oid) = base.split_once(" @ ").ok_or("invalid integration base")?;
    if base_name.trim_matches('`').trim().is_empty() {
        return Err("base name must be non-empty".into());
    }
    let base_oid = GitObjectId::parse(base_oid.trim_matches('`'), format)
        .map_err(|error| error.to_string())?;
    let validation_base_oid = parts
        .get(3)
        .and_then(|part| part.strip_prefix("validation base "))
        .and_then(optional)
        .map(|oid| GitObjectId::parse(oid, format).map_err(|error| error.to_string()))
        .transpose()?;
    let mode = parts
        .get(4)
        .and_then(|part| part.strip_prefix("mode "))
        .and_then(optional);
    if mode
        .as_deref()
        .is_some_and(|mode| !matches!(mode, "Squash" | "MergeCommit" | "Stage" | "Manual"))
    {
        return Err("invalid integration mode".into());
    }
    let final_oid = parts
        .get(5)
        .and_then(|part| part.strip_prefix("final integration "))
        .and_then(optional)
        .map(|oid| GitObjectId::parse(oid, format).map_err(|error| error.to_string()))
        .transpose()?;
    let coherent = match integration_state {
        PlanIntegrationState::Planned => run.is_none() && mode.is_none() && final_oid.is_none(),
        PlanIntegrationState::Assembling => {
            run.is_some() && validation_base_oid.is_some() && final_oid.is_none()
        }
        PlanIntegrationState::AwaitingIntegration => {
            run.is_some() && validation_base_oid.is_some() && mode.is_none() && final_oid.is_none()
        }
        PlanIntegrationState::FinalizationPending | PlanIntegrationState::IntegrationBlocked => {
            run.is_some() && validation_base_oid.is_some() && mode.is_some() && final_oid.is_none()
        }
        PlanIntegrationState::Complete => {
            run.is_some() && validation_base_oid.is_some() && mode.is_some() && final_oid.is_some()
        }
    };
    if !coherent {
        return Err("Integration state/evidence fields are incoherent".into());
    }
    let exceptions = get("Exceptions")?;
    let last_updated = source
        .body
        .lines()
        .find(|line| line.starts_with("_Last updated:") && line.ends_with("._"))
        .ok_or("missing last-updated anchor")?
        .to_owned();
    Ok(PlanStatusDocument {
        source,
        display_status,
        goal,
        root_cause,
        approach,
        done,
        blocked,
        dropped,
        total,
        integration_state,
        run,
        base_name: base_name.trim_matches('`').into(),
        base_oid,
        validation_base_oid,
        mode,
        final_oid,
        exceptions,
        outcome,
        last_updated,
    })
}

fn validate_plan_status(
    status: &PlanStatusDocument,
    tasks: &[TaskDocument],
    stubs: &[BundleTaskStub],
    report: &mut PlanValidationReport,
) {
    let done = tasks
        .iter()
        .filter(|task| task.frontmatter.status == AuthoredTaskStatus::Done)
        .count()
        + stubs
            .iter()
            .filter(|stub| stub.status == Some(AuthoredTaskStatus::Done))
            .count();
    let blocked = tasks
        .iter()
        .filter(|task| task.frontmatter.status == AuthoredTaskStatus::Blocked)
        .count()
        + stubs
            .iter()
            .filter(|stub| stub.status == Some(AuthoredTaskStatus::Blocked))
            .count();
    let dropped = tasks
        .iter()
        .filter(|task| task.frontmatter.status == AuthoredTaskStatus::Dropped)
        .count()
        + stubs
            .iter()
            .filter(|stub| stub.status == Some(AuthoredTaskStatus::Dropped))
            .count();
    if (status.done, status.blocked, status.dropped, status.total)
        != (done, blocked, dropped, tasks.len() + stubs.len())
    {
        report.add(
            "status-count-drift",
            &status.source.source_path,
            Some("Progress"),
            "Progress must equal task frontmatter counts",
        );
    }
    let display = status.display_status.to_ascii_lowercase();
    let lifecycle_ok = match status.integration_state {
        PlanIntegrationState::Planned => {
            display.contains("planned")
                && tasks
                    .iter()
                    .all(|task| task.frontmatter.status == AuthoredTaskStatus::Planned)
        }
        PlanIntegrationState::Assembling => {
            display.contains("progress")
                && tasks.iter().any(|task| {
                    matches!(
                        task.frontmatter.status,
                        AuthoredTaskStatus::Planned | AuthoredTaskStatus::InProgress
                    )
                })
        }
        PlanIntegrationState::AwaitingIntegration | PlanIntegrationState::FinalizationPending => {
            display.contains("progress")
                && !tasks
                    .iter()
                    .any(|task| task.frontmatter.status == AuthoredTaskStatus::InProgress)
        }
        PlanIntegrationState::IntegrationBlocked => display.contains("blocked"),
        PlanIntegrationState::Complete => {
            display.contains("complete")
                && tasks.iter().all(|task| {
                    matches!(
                        task.frontmatter.status,
                        AuthoredTaskStatus::Done | AuthoredTaskStatus::Dropped
                    )
                })
        }
    };
    if !lifecycle_ok {
        report.add(
            "status-lifecycle-drift",
            &status.source.source_path,
            Some("Status"),
            "display status, task states, and Integration state are incoherent",
        );
    }
    let expected = tasks
        .iter()
        .filter(|task| {
            matches!(
                task.frontmatter.status,
                AuthoredTaskStatus::Blocked | AuthoredTaskStatus::Dropped
            )
        })
        .map(|task| task.frontmatter.id.as_str())
        .map(str::to_owned)
        .chain(
            stubs
                .iter()
                .filter(|stub| {
                    matches!(
                        stub.status,
                        Some(AuthoredTaskStatus::Blocked | AuthoredTaskStatus::Dropped)
                    )
                })
                .map(|stub| stub.id.clone()),
        )
        .collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    let mut malformed = false;
    if !status.exceptions.trim_start().starts_with('—') {
        for entry in status
            .exceptions
            .split(';')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            match entry.split_once(" — ") {
                Some((id, reason)) if is_kebab(id) => {
                    let reason = reason.trim_end_matches('.').trim();
                    if let Some((original, resolution)) = reason.rsplit_once(" [resolved: ") {
                        if !original.trim().is_empty()
                            && resolution.ends_with(']')
                            && !resolution.trim_end_matches(']').trim().is_empty()
                        {
                            continue;
                        }
                        malformed = true;
                    } else if !reason.is_empty() && actual.insert(id.to_owned()) {
                        continue;
                    } else {
                        malformed = true;
                    }
                }
                _ => malformed = true,
            }
        }
    }
    if malformed || actual != expected {
        report.add("exception-coverage", &status.source.source_path, Some("Exceptions"), "Exceptions must contain exactly one `task-id — reason` entry for each blocked/dropped task");
    }
}

fn plan_digests(
    key: &PlanKey,
    title: &str,
    scope: &MarkdownDocument,
    architecture: &MarkdownDocument,
    status: &PlanStatusDocument,
    tasks: &[TaskDocument],
) -> (SourceDigest, PlanDigest) {
    fn frame(tag: &str, value: &[u8]) -> Vec<u8> {
        let mut result = Vec::new();
        result.extend_from_slice(&(tag.len() as u32).to_be_bytes());
        result.extend_from_slice(tag.as_bytes());
        result.extend_from_slice(&(value.len() as u64).to_be_bytes());
        result.extend_from_slice(value);
        result
    }
    fn list(items: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
        let items = items.into_iter().collect::<Vec<_>>();
        let mut result = Vec::new();
        result.extend_from_slice(&(items.len() as u64).to_be_bytes());
        for item in items {
            result.extend(frame("item", &item));
        }
        result
    }
    let mut executable = vec![
        (
            "plan-key".to_owned(),
            key.relative_dir.to_string_lossy().as_bytes().to_vec(),
        ),
        ("title".to_owned(), title.as_bytes().to_vec()),
    ];
    for task in tasks {
        let mut record = Vec::new();
        record.extend(frame("path", task.source_path.to_string_lossy().as_bytes()));
        for (field, value) in [
            ("id", task.frontmatter.id.as_str().to_owned()),
            ("title", task.frontmatter.title.clone()),
            (
                "workstream",
                task.frontmatter.workstream.as_str().to_owned(),
            ),
            ("kind", task.frontmatter.kind.to_string()),
            ("gated", task.frontmatter.gated.to_string()),
            ("body", task.body.replace("\r\n", "\n")),
        ] {
            record.extend(frame(field, value.as_bytes()));
        }
        record.extend(frame(
            "dependencies",
            &list(
                task.frontmatter
                    .depends_on
                    .iter()
                    .map(|dependency| dependency.as_str().as_bytes().to_vec()),
            ),
        ));
        let mut touches = task
            .frontmatter
            .touches
            .iter()
            .map(|touch| touch.as_str().as_bytes().to_vec())
            .collect::<Vec<_>>();
        touches.sort();
        record.extend(frame("touches", &list(touches)));
        executable.push(("task".into(), record));
    }
    let records = executable
        .iter()
        .map(|(tag, value)| (tag.as_str(), value.clone()))
        .collect::<Vec<_>>();
    let plan_digest = digest_records_v1("makina.executable-digest.v1", &records);
    executable.extend([
        (
            "scope".into(),
            scope.body.replace("\r\n", "\n").into_bytes(),
        ),
        (
            "architecture".into(),
            architecture.body.replace("\r\n", "\n").into_bytes(),
        ),
        ("status:goal".into(), status.goal.as_bytes().to_vec()),
        (
            "status:root-cause".into(),
            status.root_cause.as_bytes().to_vec(),
        ),
        (
            "status:approach".into(),
            status.approach.as_bytes().to_vec(),
        ),
        ("status:outcome".into(), status.outcome.as_bytes().to_vec()),
    ]);
    let records = executable
        .iter()
        .map(|(tag, value)| (tag.as_str(), value.clone()))
        .collect::<Vec<_>>();
    (
        SourceDigest(digest_records_v1("makina.source-digest.v1", &records)),
        PlanDigest(plan_digest),
    )
}

pub fn validate_root_rollup(plan: &PlanDocument, root_status: &str) -> PlanValidationReport {
    validate_root_rollup_state(plan, root_status, RootRollupState::Registered)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootRollupState {
    Unregistered,
    Proposed,
    Registered,
}

pub fn validate_root_rollup_state(
    plan: &PlanDocument,
    root_status: &str,
    state: RootRollupState,
) -> PlanValidationReport {
    let mut report = PlanValidationReport::default();
    let expected_progress = if plan.status.dropped == 0 {
        format!("{}/{}", plan.status.done, plan.status.total)
    } else {
        format!(
            "{} + {} / {}",
            plan.status.done, plan.status.dropped, plan.status.total
        )
    };
    let rows = root_status
        .lines()
        .filter(|line| line.starts_with(&format!("| {} |", plan.key.number)))
        .collect::<Vec<_>>();
    if state == RootRollupState::Unregistered && rows.is_empty() {
        return report;
    }
    let expected_link = format!(
        "[status]({}/STATUS.md)",
        plan.key.relative_dir.file_name().unwrap().to_string_lossy()
    );
    let valid = rows.len() == 1
        && rows.first().is_some_and(|row| {
            let columns = row.split('|').map(str::trim).collect::<Vec<_>>();
            columns.get(2) == Some(&plan.title.as_str())
                && columns.get(3) == Some(&plan.status.display_status.as_str())
                && columns.get(4) == Some(&expected_progress.as_str())
                && columns.get(5) == Some(&plan.status.outcome.as_str())
                && columns.get(6) == Some(&expected_link.as_str())
        });
    if !valid {
        report.add(
            "root-rollup-mismatch",
            "docs/plans/STATUS.md",
            None,
            "registered/proposed row must exactly reflect the typed plan",
        );
    }
    report.sorted()
}

#[cfg(test)]
mod git_tree_source_tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestRepo(PathBuf);

    impl TestRepo {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "makina-git-tree-source-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(root.join("plans/0001-example/tasks")).unwrap();
            run(&root, &["init", "-q"]);
            run(&root, &["config", "user.name", "Makina Test"]);
            run(&root, &["config", "user.email", "makina@example.invalid"]);
            fs::write(root.join("plans/0001-example/STATUS.md"), b"committed\n").unwrap();
            fs::write(root.join("plans/0001-example/tasks/01-one.md"), b"task\n").unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink("STATUS.md", root.join("plans/0001-example/link.md"))
                .unwrap();
            run(&root, &["add", "."]);
            run(&root, &["commit", "-qm", "fixture"]);
            let commit = String::from_utf8(run(&root, &["rev-parse", "HEAD"]))
                .unwrap()
                .trim()
                .to_owned();
            let gitlink = format!("160000,{commit},plans/0001-example/vendor");
            run(&root, &["update-index", "--add", "--cacheinfo", &gitlink]);
            run(&root, &["commit", "-qm", "add gitlink"]);
            Self(root)
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run(root: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test]
    fn git_tree_source_reads_and_lists_the_exact_commit() {
        let repo = TestRepo::new();
        let source = GitTreePlanFileSource::new(&repo.0, "HEAD").unwrap();
        let oid = source.validation_base_oid().unwrap().to_owned();
        assert_eq!(oid.len(), source.object_format().hex_len());

        fs::write(
            repo.0.join("plans/0001-example/STATUS.md"),
            b"working tree\n",
        )
        .unwrap();
        fs::write(
            repo.0.join("plans/0001-example/untracked.md"),
            b"untracked\n",
        )
        .unwrap();

        assert_eq!(
            source
                .read_file(Path::new("plans/0001-example/STATUS.md"))
                .unwrap(),
            b"committed\n"
        );
        assert_eq!(
            source
                .entry_kind(Path::new("plans/0001-example/tasks"))
                .unwrap(),
            Some(PlanSourceEntryKind::Directory)
        );
        assert_eq!(
            source
                .list_directory(Path::new("plans/0001-example"))
                .unwrap(),
            vec![
                PathBuf::from("plans/0001-example/STATUS.md"),
                #[cfg(unix)]
                PathBuf::from("plans/0001-example/link.md"),
                PathBuf::from("plans/0001-example/tasks"),
                PathBuf::from("plans/0001-example/vendor"),
            ]
        );
        assert!(
            !source
                .is_tracked_ordinary_file(Path::new("plans/0001-example/untracked.md"))
                .unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_tree_source_rejects_symlinks_and_invalid_bases_as_files() {
        let repo = TestRepo::new();
        let source = GitTreePlanFileSource::new(&repo.0, "HEAD").unwrap();
        let link = Path::new("plans/0001-example/link.md");
        assert_eq!(
            source.entry_kind(link).unwrap(),
            Some(PlanSourceEntryKind::Other)
        );
        assert!(!source.is_tracked_ordinary_file(link).unwrap());
        assert!(source.read_file(link).is_err());
        let gitlink = Path::new("plans/0001-example/vendor");
        assert_eq!(
            source.entry_kind(gitlink).unwrap(),
            Some(PlanSourceEntryKind::Other)
        );
        assert!(!source.is_tracked_ordinary_file(gitlink).unwrap());
        assert!(source.read_file(gitlink).is_err());
        assert!(GitTreePlanFileSource::new(&repo.0, "does-not-exist").is_err());
    }
}
