//! Fair in-process and cross-process repository execution leases.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryLeaseOperation {
    Run,
    Finalize,
    ReprepareFinalization,
    RegisterPlan,
    SetTaskDisposition,
    ResetRun,
    PurgeWorktrees,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepositoryLeaseOwner {
    pub plan_dir: PathBuf,
    pub run_uid: String,
    pub operation: RepositoryLeaseOperation,
}

#[derive(Debug, Error)]
pub enum RepositoryLeaseError {
    #[error("repository lease acquisition was cancelled")]
    Cancelled,
    #[error("cannot resolve Git common directory: {0}")]
    Git(String),
    #[error("repository lease I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Default)]
pub struct RepositoryLeaseRegistry {
    entries: Mutex<BTreeMap<PathBuf, Arc<Entry>>>,
    tickets: AtomicU64,
}
#[derive(Default)]
struct Entry {
    state: Mutex<EntryState>,
    changed: Notify,
}
#[derive(Default)]
struct EntryState {
    owner: Option<RepositoryLeaseOwner>,
    waiters: BTreeSet<u64>,
}

pub struct RepositoryLeaseGuard {
    common_dir: PathBuf,
    entry: Arc<Entry>,
    file: File,
    owner: RepositoryLeaseOwner,
}
/// Releases an in-process claim unless ownership is transferred to a guard.
/// Keeping this rollback in RAII form covers every fallible lock setup edge.
struct LocalClaim {
    entry: Arc<Entry>,
    armed: bool,
}
impl LocalClaim {
    fn new(entry: Arc<Entry>) -> Self {
        Self { entry, armed: true }
    }
    fn transfer(mut self) -> Arc<Entry> {
        self.armed = false;
        self.entry.clone()
    }
}
impl Drop for LocalClaim {
    fn drop(&mut self) {
        if self.armed {
            release_local(&self.entry);
        }
    }
}
#[derive(Debug)]
pub struct RepositoryChildToken(File);
impl RepositoryChildToken {
    /// Make the duplicate survive exec in one coordinator-owned mutation
    /// child. Descriptors remain close-on-exec for every ordinary child.
    #[cfg(unix)]
    pub fn inherit_into(&self, command: &mut tokio::process::Command) {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let fd = self.0.as_raw_fd();
        // SAFETY: this post-fork hook only changes FD_CLOEXEC on the valid
        // duplicated descriptor before exec.
        unsafe {
            command.as_std_mut().pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }

    /// Make the duplicate survive exec in a synchronous coordinator child.
    #[cfg(unix)]
    pub fn inherit_into_std(&self, command: &mut std::process::Command) {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let fd = self.0.as_raw_fd();
        // SAFETY: the hook runs after fork and only clears CLOEXEC on this
        // valid duplicate before exec.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
}

impl RepositoryLeaseRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn owner_for(
        &self,
        repo: &Path,
    ) -> Result<Option<RepositoryLeaseOwner>, RepositoryLeaseError> {
        let common = git_common_dir(repo)?;
        Ok(self
            .entries
            .lock()
            .expect("lease map poisoned")
            .get(&common)
            .and_then(|entry| {
                entry
                    .state
                    .lock()
                    .expect("lease state poisoned")
                    .owner
                    .clone()
            }))
    }
    pub async fn acquire(
        &self,
        repo: &Path,
        owner: RepositoryLeaseOwner,
        cancel: &CancellationToken,
    ) -> Result<RepositoryLeaseGuard, RepositoryLeaseError> {
        let common_dir = git_common_dir(repo)?;
        let entry = self
            .entries
            .lock()
            .expect("lease map poisoned")
            .entry(common_dir.clone())
            .or_default()
            .clone();
        let ticket = self.tickets.fetch_add(1, Ordering::Relaxed);
        entry
            .state
            .lock()
            .expect("lease state poisoned")
            .waiters
            .insert(ticket);
        loop {
            let local = {
                let mut state = entry.state.lock().expect("lease state poisoned");
                if state.owner.is_none() && state.waiters.first() == Some(&ticket) {
                    state.waiters.remove(&ticket);
                    state.owner = Some(owner.clone());
                    true
                } else {
                    false
                }
            };
            if local {
                break;
            }
            tokio::select! { _ = entry.changed.notified() => {}, _ = cancel.cancelled() => {
            entry.state.lock().expect("lease state poisoned").waiters.remove(&ticket); entry.changed.notify_waiters(); return Err(RepositoryLeaseError::Cancelled); } }
        }
        let claim = LocalClaim::new(entry);
        let file = open_lock_file(&common_dir)?;
        loop {
            match try_flock(&file) {
                Ok(true) => break,
                Ok(false) => tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(25)) => {}, _ = cancel.cancelled() => { return Err(RepositoryLeaseError::Cancelled); } },
                Err(error) => return Err(error.into()),
            }
        }
        Ok(RepositoryLeaseGuard {
            common_dir,
            entry: claim.transfer(),
            file,
            owner,
        })
    }
    pub fn try_acquire(
        &self,
        repo: &Path,
        owner: RepositoryLeaseOwner,
    ) -> Result<Option<RepositoryLeaseGuard>, RepositoryLeaseError> {
        let common_dir = git_common_dir(repo)?;
        let entry = self
            .entries
            .lock()
            .expect("lease map poisoned")
            .entry(common_dir.clone())
            .or_default()
            .clone();
        {
            let mut state = entry.state.lock().expect("lease state poisoned");
            if state.owner.is_some() || !state.waiters.is_empty() {
                return Ok(None);
            }
            state.owner = Some(owner.clone());
        }
        let claim = LocalClaim::new(entry);
        let file = open_lock_file(&common_dir)?;
        match try_flock(&file) {
            Ok(true) => Ok(Some(RepositoryLeaseGuard {
                common_dir,
                entry: claim.transfer(),
                file,
                owner,
            })),
            Ok(false) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}
impl RepositoryLeaseGuard {
    pub fn owner(&self) -> &RepositoryLeaseOwner {
        &self.owner
    }
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }
    pub fn child_token(&self) -> Result<RepositoryChildToken, RepositoryLeaseError> {
        Ok(RepositoryChildToken(self.file.try_clone()?))
    }
}
impl Drop for RepositoryLeaseGuard {
    fn drop(&mut self) {
        release_local(&self.entry);
    }
}
fn release_local(entry: &Entry) {
    entry.state.lock().expect("lease state poisoned").owner = None;
    entry.changed.notify_waiters();
}

fn open_lock_file(common_dir: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(common_dir.join("makina.repository.lock"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

pub fn git_common_dir(repo: &Path) -> Result<PathBuf, RepositoryLeaseError> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(repo)
        .output()?;
    if !output.status.success() {
        return Err(RepositoryLeaseError::Git(
            String::from_utf8_lossy(&output.stderr).trim().into(),
        ));
    }
    std::fs::canonicalize(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
    .map_err(Into::into)
}

#[cfg(unix)]
fn try_flock(file: &File) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: flock only observes the valid descriptor owned by `file`.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
    {
        Ok(false)
    } else {
        Err(error)
    }
}
#[cfg(not(unix))]
fn try_flock(_file: &File) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "repository execution lease requires Unix flock",
    ))
}
