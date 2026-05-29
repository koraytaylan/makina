//! File-browser state and pure navigation logic.
//!
//! [`FileBrowser`] is the view state for picking a task-list file to open.  It
//! holds **no IO**: the actual directory reads happen in the IO layer
//! ([`crate::event`]), which feeds fresh listings in via
//! [`crate::app::AppEvent::BrowserOpened`].  This module only owns the in-memory
//! listing and the pure selection/clamp logic, so every transition is unit
//! testable without touching the filesystem.
//!
//! # Why state-only (no `std::fs` here)
//!
//! Keeping `FileBrowser` free of IO mirrors the [`crate::app::App`] design: pure
//! state + pure `update`, with all reads/writes pushed to the async event loop.
//! The event loop reads a directory, builds the [`Vec<DirEntry>`], and hands it
//! to `App::update`; selecting a directory triggers another read; selecting a
//! file triggers `api.execute(OpenRun{..})`.

use std::path::{Path, PathBuf};

// ── Directory entry ─────────────────────────────────────────────────────────────

/// One row in the file browser: a directory or a file under the current dir.
///
/// The IO layer constructs these from `std::fs::read_dir`; the browser renders
/// and navigates them but never reads the disk itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Display name (the final path component, e.g. `my-feature.md` or `src`).
    pub name: String,
    /// Absolute path to this entry.
    pub path: PathBuf,
    /// `true` if this entry is a directory (navigable with Enter), `false` if a
    /// file (selectable to open a Run).
    pub is_dir: bool,
}

// ── FileBrowser ─────────────────────────────────────────────────────────────────

/// In-memory state of the file browser overlay.
///
/// Construct via [`FileBrowser::new`] once the IO layer has read the initial
/// directory.  Selection is an index into [`FileBrowser::entries`]; it is kept
/// in `[0, entries.len())` (or `0` when empty) by the navigation helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBrowser {
    /// The directory currently being listed.
    pub cwd: PathBuf,
    /// The entries under `cwd`, in display order.
    pub entries: Vec<DirEntry>,
    /// Index of the highlighted row in `entries`.  Always valid when `entries`
    /// is non-empty; `0` when empty.
    pub selected: usize,
}

impl FileBrowser {
    /// Build a new browser positioned at `cwd` with the given `entries`.
    ///
    /// Selection starts at the first row.
    pub fn new(cwd: PathBuf, entries: Vec<DirEntry>) -> Self {
        Self {
            cwd,
            entries,
            selected: 0,
        }
    }

    /// Move the selection one row up, clamped at the first entry.
    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the selection one row down, clamped at the last entry.
    ///
    /// No-op when the listing is empty.
    pub fn select_down(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        self.selected = (self.selected + 1).min(last);
    }

    /// Return the currently highlighted entry, or `None` when the listing is
    /// empty.
    pub fn selected_entry(&self) -> Option<&DirEntry> {
        self.entries.get(self.selected)
    }

    /// The parent of [`FileBrowser::cwd`], if any (used by "go up" navigation).
    ///
    /// Returns `None` at the filesystem root.
    pub fn parent(&self) -> Option<&Path> {
        self.cwd.parent()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            path: PathBuf::from("/root").join(name),
            is_dir: true,
        }
    }

    fn file(name: &str) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            path: PathBuf::from("/root").join(name),
            is_dir: false,
        }
    }

    fn sample() -> FileBrowser {
        FileBrowser::new(
            PathBuf::from("/root"),
            vec![dir("src"), file("a.md"), file("b.md")],
        )
    }

    #[test]
    fn new_starts_selection_at_zero() {
        let b = sample();
        assert_eq!(b.selected, 0);
        assert_eq!(b.selected_entry().unwrap().name, "src");
    }

    #[test]
    fn select_down_advances_and_clamps() {
        let mut b = sample();
        b.select_down();
        assert_eq!(b.selected, 1);
        b.select_down();
        assert_eq!(b.selected, 2);
        // Clamp at last.
        b.select_down();
        assert_eq!(b.selected, 2, "select_down must clamp at the last entry");
    }

    #[test]
    fn select_up_retreats_and_clamps() {
        let mut b = sample();
        b.select_down();
        b.select_down();
        assert_eq!(b.selected, 2);
        b.select_up();
        assert_eq!(b.selected, 1);
        b.select_up();
        assert_eq!(b.selected, 0);
        // Clamp at zero.
        b.select_up();
        assert_eq!(b.selected, 0, "select_up must clamp at zero");
    }

    #[test]
    fn select_down_on_empty_is_noop() {
        let mut b = FileBrowser::new(PathBuf::from("/root"), vec![]);
        b.select_down();
        assert_eq!(b.selected, 0);
        assert!(b.selected_entry().is_none());
    }

    #[test]
    fn selected_entry_tracks_index() {
        let mut b = sample();
        b.select_down();
        assert_eq!(b.selected_entry().unwrap().name, "a.md");
        assert!(!b.selected_entry().unwrap().is_dir);
    }

    #[test]
    fn parent_returns_parent_dir() {
        let b = sample();
        assert_eq!(b.parent(), Some(Path::new("/")));
    }

    #[test]
    fn parent_at_root_is_none() {
        let b = FileBrowser::new(PathBuf::from("/"), vec![]);
        assert!(b.parent().is_none());
    }
}
