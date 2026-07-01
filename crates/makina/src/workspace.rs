//! Workspace persistence layer for managing opened folders.
//!
//! The `Workspace` struct holds a persistent set of folders opened by the user,
//! stored at `$HOME/.makina/workspace.toml`. The workspace survives restarts,
//! allowing users to resume their multi-folder workspace context automatically.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

/// Path to the workspace persistence file: `$HOME/.makina/workspace.toml`.
fn workspace_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "HOME not set".to_string())?;
    Ok(home.join(".makina").join("workspace.toml"))
}

/// Workspace holds a persistent set of opened folders.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Workspace {
    /// Set of directory paths that are currently opened in the workspace.
    pub opened_folders: HashSet<PathBuf>,
}

impl Workspace {
    /// Create a new empty workspace.
    pub fn new() -> Self {
        Workspace {
            opened_folders: HashSet::new(),
        }
    }

    /// Load a workspace from `$HOME/.makina/workspace.toml`.
    ///
    /// If the file does not exist, returns a new empty `Workspace` (graceful
    /// handling for first-time users). If the file exists but parsing fails,
    /// returns an error.
    ///
    /// Prefer this in production entry points. In tests, use
    /// [`Workspace::load_from`] with an explicit path so tests don't depend
    /// on the operator's home directory.
    pub fn load() -> Result<Self, String> {
        Self::load_from(&workspace_path()?)
    }

    /// Load a workspace from an explicit TOML file path.
    ///
    /// If `path` does not exist, returns a new empty `Workspace` (graceful
    /// handling for first-time users / a nonexistent file). If the file
    /// exists but parsing fails, returns an error.
    pub fn load_from(path: &std::path::Path) -> Result<Self, String> {
        // If the file doesn't exist, return an empty workspace.
        if !path.exists() {
            return Ok(Workspace::new());
        }

        // Read and parse the TOML file.
        let content =
            fs::read_to_string(path).map_err(|e| format!("failed to read workspace file: {e}"))?;

        toml::from_str(&content).map_err(|e| format!("failed to parse workspace TOML: {e}"))
    }

    /// Save the workspace to `$HOME/.makina/workspace.toml` atomically.
    ///
    /// Uses a temporary file + rename pattern to ensure an interrupted write
    /// never corrupts the workspace file.
    ///
    /// Prefer this in production entry points. In tests, use
    /// [`Workspace::save_to`] with an explicit path so tests don't touch the
    /// operator's home directory.
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&workspace_path()?)
    }

    /// Save the workspace to an explicit TOML file path, atomically.
    ///
    /// Ensures the parent directory exists, then writes via a temp file +
    /// rename in the same directory so an interrupted write never leaves a
    /// corrupt or partial file at `path`.
    pub fn save_to(&self, path: &std::path::Path) -> Result<(), String> {
        // Ensure the parent directory exists.
        let parent = path
            .parent()
            .ok_or_else(|| "cannot determine parent directory".to_string())?;
        fs::create_dir_all(parent).map_err(|e| format!("failed to create directory: {e}"))?;

        // Serialize the workspace to TOML.
        let content = toml::to_string_pretty(&self)
            .map_err(|e| format!("failed to serialize workspace: {e}"))?;

        // Write atomically: temp file + rename (same directory, so rename is
        // same-filesystem and therefore atomic on POSIX).
        let temp_path = parent.join(".workspace.toml.tmp");
        fs::write(&temp_path, content)
            .map_err(|e| format!("failed to write temporary workspace file: {e}"))?;

        fs::rename(&temp_path, path).map_err(|e| {
            format!("failed to move temporary workspace file to final location: {e}")
        })?;

        Ok(())
    }

    /// Add a folder to the workspace if not already present.
    pub fn add_folder(&mut self, folder: PathBuf) {
        self.opened_folders.insert(folder);
    }

    /// Remove a folder from the workspace, if present.
    pub fn remove_folder(&mut self, folder: &PathBuf) {
        self.opened_folders.remove(folder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_load_nonexistent_file_returns_empty() {
        // Loading from a path that doesn't exist on disk must return an empty
        // workspace, not an error (graceful handling for first-time users).
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let path = temp_dir
            .path()
            .join("does-not-exist")
            .join("workspace.toml");
        assert!(!path.exists());

        let ws = Workspace::load_from(&path).expect("load_from a missing file must not error");
        assert!(ws.opened_folders.is_empty());
    }

    #[test]
    fn test_save_creates_file_atomically() {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let path = temp_dir.path().join(".makina").join("workspace.toml");
        assert!(!path.exists());

        let mut ws = Workspace::new();
        ws.add_folder(PathBuf::from("/home/user/project1"));

        ws.save_to(&path).expect("save_to should succeed");

        // The file now exists, and no leftover temp file remains (rename
        // consumed it), which is the atomic temp-file + rename contract.
        assert!(path.exists());
        let temp_path = path.parent().unwrap().join(".workspace.toml.tmp");
        assert!(!temp_path.exists());

        let content = fs::read_to_string(&path).expect("failed to read saved file");
        assert!(content.contains("project1"));
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let path = temp_dir.path().join("workspace.toml");

        // Create a workspace with some folders.
        let mut ws = Workspace::new();
        ws.add_folder(PathBuf::from("/home/user/project1"));
        ws.add_folder(PathBuf::from("/home/user/project2"));

        ws.save_to(&path).expect("save_to should succeed");
        let loaded = Workspace::load_from(&path).expect("load_from should succeed");

        assert_eq!(ws.opened_folders, loaded.opened_folders);
    }

    #[test]
    fn test_load_invalid_toml_errors() {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let path = temp_dir.path().join("workspace.toml");
        fs::write(&path, "not valid toml {{{").expect("failed to write invalid toml");

        let result = Workspace::load_from(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_add_and_remove_folder() {
        let mut ws = Workspace::new();
        let folder = PathBuf::from("/home/user/project");

        // Add a folder.
        ws.add_folder(folder.clone());
        assert!(ws.opened_folders.contains(&folder));

        // Remove the folder.
        ws.remove_folder(&folder);
        assert!(!ws.opened_folders.contains(&folder));
    }
}
