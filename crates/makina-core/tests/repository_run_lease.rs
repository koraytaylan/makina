use makina_core::repository_lease::{
    RepositoryLeaseOperation, RepositoryLeaseOwner, RepositoryLeaseRegistry,
};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
#[tokio::test]
#[ignore]
async fn sigkill_parent_helper() {
    use std::io::Write;
    let Some(path) = std::env::var_os("MAKINA_LEASE_HELPER_REPO") else {
        return;
    };
    let registry = RepositoryLeaseRegistry::new();
    let guard = registry
        .acquire(
            std::path::Path::new(&path),
            owner("killed-parent"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let token = guard.child_token().unwrap();
    let mut child = tokio::process::Command::new("sh");
    child.args(["-c", "sleep 1"]);
    token.inherit_into(&mut child);
    child.spawn().unwrap();
    println!("LEASE_HELPER_READY");
    std::io::stdout().flush().unwrap();
    std::future::pending::<()>().await;
}

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-b", "develop"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "user.name", "Test"]);
    std::fs::write(repo.path().join("README"), "x").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "base"]);
    repo
}
fn owner(id: &str) -> RepositoryLeaseOwner {
    RepositoryLeaseOwner {
        plan_dir: format!("docs/plans/{id}").into(),
        run_uid: id.into(),
        operation: RepositoryLeaseOperation::Run,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn lock_open_error_rolls_back_acquire_and_wakes_fifo() {
    use std::os::unix::fs::symlink;
    let repo = repo();
    let common = makina_core::repository_lease::git_common_dir(repo.path()).unwrap();
    let lock = common.join("makina.repository.lock");
    symlink(common.join("missing-target"), &lock).unwrap();
    let registry = Arc::new(RepositoryLeaseRegistry::new());
    assert!(
        registry
            .acquire(repo.path(), owner("broken"), &CancellationToken::new())
            .await
            .is_err()
    );
    std::fs::remove_file(lock).unwrap();
    let guard = tokio::time::timeout(
        Duration::from_secs(1),
        registry.acquire(repo.path(), owner("next"), &CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(guard.owner().run_uid, "next");
}

#[cfg(unix)]
#[tokio::test]
async fn lock_open_error_rolls_back_try_acquire() {
    use std::os::unix::fs::symlink;
    let repo = repo();
    let common = makina_core::repository_lease::git_common_dir(repo.path()).unwrap();
    let lock = common.join("makina.repository.lock");
    symlink(common.join("missing-target"), &lock).unwrap();
    let registry = RepositoryLeaseRegistry::new();
    assert!(registry.try_acquire(repo.path(), owner("broken")).is_err());
    std::fs::remove_file(lock).unwrap();
    assert!(
        registry
            .try_acquire(repo.path(), owner("next"))
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn shared_registry_is_fifo_and_cancellation_removes_waiter() {
    let repo = repo();
    let registry = Arc::new(RepositoryLeaseRegistry::new());
    let first = registry
        .acquire(repo.path(), owner("one"), &CancellationToken::new())
        .await
        .unwrap();
    let cancelled = CancellationToken::new();
    let second = tokio::spawn({
        let registry = registry.clone();
        let path = repo.path().to_owned();
        let cancel = cancelled.clone();
        async move { registry.acquire(&path, owner("two"), &cancel).await }
    });
    let third = tokio::spawn({
        let registry = registry.clone();
        let path = repo.path().to_owned();
        async move {
            registry
                .acquire(&path, owner("three"), &CancellationToken::new())
                .await
        }
    });
    tokio::task::yield_now().await;
    cancelled.cancel();
    assert!(second.await.unwrap().is_err());
    drop(first);
    let third = tokio::time::timeout(Duration::from_secs(2), third)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(third.owner().run_uid, "three");
}

#[tokio::test]
async fn worktree_aliases_serialize_but_different_repositories_do_not() {
    let primary = repo();
    let other = repo();
    let alias_dir = tempfile::tempdir().unwrap();
    let alias = alias_dir.path().join("alias");
    git(
        primary.path(),
        &["worktree", "add", alias.to_str().unwrap(), "-b", "alias"],
    );
    let registry = Arc::new(RepositoryLeaseRegistry::new());
    let first = registry
        .acquire(primary.path(), owner("one"), &CancellationToken::new())
        .await
        .unwrap();
    let alias_wait = tokio::spawn({
        let r = registry.clone();
        let alias = alias.clone();
        async move {
            r.acquire(&alias, owner("alias"), &CancellationToken::new())
                .await
        }
    });
    let different = tokio::time::timeout(
        Duration::from_secs(1),
        registry.acquire(other.path(), owner("other"), &CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), async {
            while !alias_wait.is_finished() {
                tokio::task::yield_now().await
            }
        })
        .await
        .is_err()
    );
    drop(first);
    drop(different);
    assert_eq!(alias_wait.await.unwrap().unwrap().owner().run_uid, "alias");
}

#[tokio::test]
async fn python_holder_and_rust_use_the_same_persistent_flock() {
    use std::io::{BufRead, Write};
    let repo = repo();
    let common = makina_core::repository_lease::git_common_dir(repo.path()).unwrap();
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/repository-lease/inert-flock-holder.py");
    let mut holder = Command::new("python3")
        .arg(script)
        .arg("--git-common-dir")
        .arg(&common)
        .arg("--run")
        .arg("python")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = std::io::BufReader::new(holder.stdout.take().unwrap());
    let mut ready = String::new();
    reader.read_line(&mut ready).unwrap();
    assert!(ready.contains("ready"));
    let registry = Arc::new(RepositoryLeaseRegistry::new());
    let cancel = CancellationToken::new();
    let waiter = tokio::spawn({
        let registry = registry.clone();
        let path = repo.path().to_owned();
        async move {
            registry
                .acquire(&path, owner("rust"), &CancellationToken::new())
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!waiter.is_finished());
    holder
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{\"op\":\"release\"}\n")
        .unwrap();
    holder.wait().unwrap();
    let guard = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(guard);
    let mut crashed = Command::new("python3")
        .arg(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/repository-lease/inert-flock-holder.py"),
        )
        .arg("--git-common-dir")
        .arg(&common)
        .arg("--run")
        .arg("crash")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut crashed_reader = std::io::BufReader::new(crashed.stdout.take().unwrap());
    let mut line = String::new();
    crashed_reader.read_line(&mut line).unwrap();
    assert!(line.contains("ready"));
    crashed.kill().unwrap();
    crashed.wait().unwrap();
    assert!(
        registry
            .try_acquire(repo.path(), owner("after-crash"))
            .unwrap()
            .is_some()
    );
    drop(cancel);
    assert!(common.join("makina.repository.lock").exists());
}

#[tokio::test]
async fn inherited_child_token_keeps_kernel_lock_after_parent_guard_drops() {
    let repo = repo();
    let first_registry = Arc::new(RepositoryLeaseRegistry::new());
    let guard = first_registry
        .acquire(repo.path(), owner("parent"), &CancellationToken::new())
        .await
        .unwrap();
    let token = guard.child_token().unwrap();
    drop(guard);
    let contender_registry = Arc::new(RepositoryLeaseRegistry::new());
    let contender = tokio::spawn({
        let registry = contender_registry;
        let path = repo.path().to_owned();
        async move {
            registry
                .acquire(&path, owner("contender"), &CancellationToken::new())
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!contender.is_finished());
    drop(token);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), contender)
            .await
            .unwrap()
            .unwrap()
            .is_ok()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn mutation_child_inherits_lock_but_ordinary_child_does_not() {
    let repo = repo();
    let registry = Arc::new(RepositoryLeaseRegistry::new());
    let guard = registry
        .acquire(repo.path(), owner("parent"), &CancellationToken::new())
        .await
        .unwrap();
    let token = guard.child_token().unwrap();
    let mut command = tokio::process::Command::new("sh");
    command.args(["-c", "sleep 0.3"]);
    token.inherit_into(&mut command);
    let mut child = command.spawn().unwrap();
    drop(token);
    drop(guard);
    let other = Arc::new(RepositoryLeaseRegistry::new());
    assert!(
        other
            .try_acquire(repo.path(), owner("blocked"))
            .unwrap()
            .is_none()
    );
    child.wait().await.unwrap();
    assert!(
        other
            .try_acquire(repo.path(), owner("free"))
            .unwrap()
            .is_some()
    );

    let guard = registry
        .acquire(repo.path(), owner("ordinary"), &CancellationToken::new())
        .await
        .unwrap();
    let mut ordinary = tokio::process::Command::new("sh")
        .args(["-c", "sleep 0.3"])
        .spawn()
        .unwrap();
    drop(guard);
    assert!(
        other
            .try_acquire(repo.path(), owner("not-inherited"))
            .unwrap()
            .is_some()
    );
    ordinary.kill().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn inherited_child_keeps_lock_after_rust_parent_is_sigkilled() {
    use std::io::BufRead;
    use std::os::unix::process::ExitStatusExt;

    let repo = repo();
    let mut parent = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "sigkill_parent_helper",
            "--nocapture",
        ])
        .env("MAKINA_LEASE_HELPER_REPO", repo.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = std::io::BufReader::new(parent.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(output.read_line(&mut line).unwrap(), 0);
        if line.contains("LEASE_HELPER_READY") {
            break;
        }
    }
    let killed_pid = parent.id() as libc::pid_t;
    // SAFETY: the PID belongs to the child test process just spawned above.
    assert_eq!(unsafe { libc::kill(killed_pid, libc::SIGKILL) }, 0);
    let status = parent.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGKILL));

    let contender = RepositoryLeaseRegistry::new();
    assert!(
        contender
            .try_acquire(repo.path(), owner("while-child-lives"))
            .unwrap()
            .is_none()
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        contender
            .try_acquire(repo.path(), owner("after-child"))
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn scheduler_terminal_matrix_releases_local_and_kernel_ownership() {
    let repo = repo();
    let repo_path = repo.path().to_owned();
    let registry = Arc::new(RepositoryLeaseRegistry::new());

    // Normal scheduler success.
    tokio::spawn({
        let registry = Arc::clone(&registry);
        let path = repo_path.clone();
        async move {
            let _guard = registry
                .acquire(&path, owner("success"), &CancellationToken::new())
                .await
                .unwrap();
        }
    })
    .await
    .unwrap();
    let probe = RepositoryLeaseRegistry::new();
    drop(
        probe
            .try_acquire(&repo_path, owner("after-success"))
            .unwrap()
            .unwrap(),
    );

    // Terminal scheduler error still drops the guard on return.
    let result = tokio::spawn({
        let registry = Arc::clone(&registry);
        let path = repo_path.clone();
        async move {
            let _guard = registry
                .acquire(&path, owner("error"), &CancellationToken::new())
                .await
                .unwrap();
            Err::<(), &'static str>("injected terminal error")
        }
    })
    .await
    .unwrap();
    assert!(result.is_err());
    drop(
        probe
            .try_acquire(&repo_path, owner("after-error"))
            .unwrap()
            .unwrap(),
    );

    // A scheduler panic unwinds through the lease guard.
    let panicked = tokio::spawn({
        let registry = Arc::clone(&registry);
        let path = repo_path.clone();
        async move {
            let _guard = registry
                .acquire(&path, owner("panic"), &CancellationToken::new())
                .await
                .unwrap();
            panic!("injected scheduler panic");
        }
    })
    .await;
    assert!(panicked.unwrap_err().is_panic());
    drop(
        probe
            .try_acquire(&repo_path, owner("after-panic"))
            .unwrap()
            .unwrap(),
    );

    // Cancellation/abort drops all task locals, including the guard.
    let acquired = Arc::new(tokio::sync::Notify::new());
    let cancelled = tokio::spawn({
        let registry = Arc::clone(&registry);
        let path = repo_path.clone();
        let acquired = Arc::clone(&acquired);
        async move {
            let _guard = registry
                .acquire(&path, owner("cancel"), &CancellationToken::new())
                .await
                .unwrap();
            acquired.notify_one();
            std::future::pending::<()>().await;
        }
    });
    acquired.notified().await;
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    drop(
        probe
            .try_acquire(&repo_path, owner("after-cancel"))
            .unwrap()
            .unwrap(),
    );

    // Cooperative pause/quiescence retains ownership while work drains, then
    // releases only after the scheduler generation exits.
    let acquired = Arc::new(tokio::sync::Notify::new());
    let quiesce = Arc::new(tokio::sync::Notify::new());
    let paused = tokio::spawn({
        let registry = Arc::clone(&registry);
        let path = repo_path.clone();
        let acquired = Arc::clone(&acquired);
        let quiesce = Arc::clone(&quiesce);
        async move {
            let _guard = registry
                .acquire(&path, owner("pause"), &CancellationToken::new())
                .await
                .unwrap();
            acquired.notify_one();
            quiesce.notified().await;
        }
    });
    acquired.notified().await;
    assert!(
        probe
            .try_acquire(&repo_path, owner("during-pause"))
            .unwrap()
            .is_none()
    );
    quiesce.notify_one();
    paused.await.unwrap();
    drop(
        probe
            .try_acquire(&repo_path, owner("after-pause"))
            .unwrap()
            .unwrap(),
    );
}
