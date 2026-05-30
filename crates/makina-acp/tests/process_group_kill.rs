//! Process-group reaping for the ACP agent subprocess.
//!
//! The agent is spawned as the **leader of its own process group** (via
//! `process_group(0)`), so `AcpClient::shutdown` / `Drop` can `killpg` the whole
//! tree — not just the direct child. This test forks a long-lived *grandchild*
//! from the mock agent and asserts that, after `shutdown()`, that grandchild is
//! gone (i.e. it was reaped by the group kill, not orphaned).

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use makina_acp::{AcpClient, AcpCommand};
use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;

/// Locate `sh` (present on POSIX systems).
fn which_sh() -> Option<PathBuf> {
    for p in ["/bin/sh", "/usr/bin/sh"] {
        if std::path::Path::new(p).exists() {
            return Some(PathBuf::from(p));
        }
    }
    None
}

/// True if `pid` is still alive: `kill(pid, None)` probes existence without
/// sending a signal — `Ok(())` means alive, `Err(ESRCH)` means gone.
fn is_alive(pid: Pid) -> bool {
    match kill(pid, None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        // EPERM etc. → the process exists but we can't signal it; treat as alive.
        Err(_) => true,
    }
}

#[tokio::test]
async fn agent_group_kill_reaps_descendants() {
    if which_sh().is_none() {
        eprintln!("skipping: /bin/sh not found");
        return;
    }

    // The grandchild must belong to the AcpClient-owned process group, so we
    // capture its pid via a side channel: the agent writes the grandchild's pid
    // to a temp file, which the test reads after connect. (We can't read it from
    // the agent's stdout because the client consumes that during the handshake.)
    let pid_file = std::env::temp_dir().join(format!(
        "makina-acp-grandchild-{}-{}.pid",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let pid_file_str = pid_file.display().to_string();

    // Agent forks a long-lived grandchild, records its pid to a temp file, then
    // answers the handshake and idles. The grandchild shares the agent's process
    // group (the agent is the group leader via process_group(0)).
    let script = format!(
        r#"
sleep 300 &
grandchild=$!
printf '%s' "$grandchild" > '{pid_file_str}'
read -r _initialize
printf '%s\n' '{{"jsonrpc":"2.0","id":0,"result":{{"protocolVersion":1,"agentInfo":{{"name":"sh-mock","version":"0"}}}}}}'
read -r _session_new
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"sessionId":"sh-session"}}}}'
while read -r _line; do :; done
"#
    );

    let command = AcpCommand::new("sh", std::env::temp_dir()).args([String::from("-c"), script]);
    let mut client = AcpClient::connect(command)
        .await
        .expect("sh mock agent should complete the handshake");
    assert_eq!(client.session_id(), "sh-session");

    // Read the grandchild pid the agent recorded. The grandchild is forked
    // before the handshake completes, but give it ample time to register so the
    // assertion is not racy.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let grandchild_pid = loop {
        if let Ok(contents) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = contents.trim().parse::<i32>()
            && pid > 0
        {
            break Pid::from_raw(pid);
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = std::fs::remove_file(&pid_file);
            panic!("grandchild pid file was never populated");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    // Sanity: the grandchild should be alive before we tear the agent down.
    assert!(
        is_alive(grandchild_pid),
        "grandchild should be alive before shutdown"
    );

    // Group-kill the whole tree.
    client.shutdown().await.expect("shutdown ok");

    // Give the SIGKILL a short grace to propagate / be reaped before asserting.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        // Probe with no signal: ESRCH ⇒ the grandchild is gone (reaped).
        match kill(grandchild_pid, None) {
            Err(Errno::ESRCH) => break, // success
            _ => {
                if tokio::time::Instant::now() >= deadline {
                    let _ = std::fs::remove_file(&pid_file);
                    panic!(
                        "grandchild {grandchild_pid} survived group kill (descendant not reaped)"
                    );
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    // Final, explicit assertion required by the spec.
    assert_eq!(
        kill(grandchild_pid, None),
        Err(Errno::ESRCH),
        "grandchild must be gone after group kill"
    );

    let _ = std::fs::remove_file(&pid_file);
}
