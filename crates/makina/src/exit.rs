//! No-orphan exit cleanup: reap agents (and cancel runs) on **every** exit path.
//!
//! The TUI subprocesses ACP agents (each in its own process group, registered in
//! [`makina_acp`]'s process registry). Whenever the binary exits we must drain
//! that registry so no `grok`/agent process is orphaned. There are four exit
//! paths the binary has to cover:
//!
//! * **clean quit** — `event::run` returns `Ok` (the in-TUI `q`/`Esc`/`Ctrl-C`);
//! * **event-loop error** — `event::run` returns `Err`;
//! * **panic** mid-run — handled by the TUI panic hook (see [`crate::tui`]);
//! * **out-of-band signal** — an external `SIGINT`/`SIGTERM` (e.g.
//!   `kill <makina-pid>`), handled by the signal task installed here.
//!
//! The clean and error paths both run [`reap_open_runs`] (cancel every still-open
//! run, then [`makina_acp::kill_all_agents`]). The signal path runs the
//! best-effort sync reaper directly — it cannot drive async `CancelRun`s before
//! the terminal is torn down and `std::process::exit` is called, but
//! `kill_all_agents()` alone guarantees no orphans.

use makina_core::api::{Api, Command};

/// Cancel every still-open run, then reap any agent process group still live.
///
/// This is the shared cleanup for the two **async** exit paths (clean quit and
/// event-loop error). It:
///
/// 1. snapshots `api.runs()` and issues [`Command::CancelRun`] for each open run
///    (best-effort — a run that already finished returns `UnknownRun`, which we
///    ignore), so the orchestrator tears down its supervisors/worktrees; then
/// 2. calls [`makina_acp::kill_all_agents`] as a backstop, so any pgid a client
///    failed to deregister is `killpg(SIGKILL)`ed and the registry is drained.
///
/// Idempotent and safe to call when no runs are open (it simply drains an empty
/// registry).
pub async fn reap_open_runs(api: &dyn Api) {
    for run in api.runs().await {
        // Best-effort: ignore errors (e.g. a run that completed between the
        // snapshot and the cancel) — the kill_all_agents backstop below covers
        // any agent the orchestrator could not stop.
        let _ = api.execute(Command::CancelRun { run: run.id }).await;
    }
    makina_acp::kill_all_agents();
}

/// Spawn a background task that reaps agents on an out-of-band `SIGINT`/`SIGTERM`.
///
/// The in-TUI `Ctrl-C` is already a clean quit (it maps to `AppEvent::Quit` in
/// `event::translate_key`), so this handler only covers signals that arrive from
/// *outside* the TUI — e.g. `kill <makina-pid>` or a `SIGTERM` from a process
/// manager — which would otherwise orphan the agents. On receipt of either
/// signal it:
///
/// 1. calls [`makina_acp::kill_all_agents`] (drains the registry, SIGKILLs each
///    pgid) — done first so an orphan can never outlive the binary;
/// 2. runs `restore` to leave the alternate screen / raw mode (so the user's
///    shell is readable); then
/// 3. `std::process::exit(130)` (128 + SIGINT's signal number — the conventional
///    "terminated by Ctrl-C" exit code).
///
/// `restore` is a plain `Fn` (not a `&mut Tui`) because the signal task cannot
/// share the `Tui` the render loop owns; the binary passes the same static
/// terminal-restore used by the panic hook.
///
/// No-op on non-unix targets (the TUI is unix-only in practice; the signal
/// primitives are unix-specific).
#[cfg(unix)]
pub fn install_signal_reaper<F>(restore: F)
where
    F: Fn() + Send + 'static,
{
    use tokio::signal::unix::{SignalKind, signal};

    tokio::spawn(async move {
        // If either stream fails to install, fall back to letting the default
        // disposition terminate the process (which still won't orphan agents
        // beyond the OS reparenting — the panic/clean paths remain the primary
        // guarantee). We only proceed when at least one stream installs.
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return,
        };

        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }

        // Reap first — an orphaned agent must never outlive the binary.
        makina_acp::kill_all_agents();
        // Then make the shell readable again.
        restore();
        // 130 = 128 + SIGINT(2): the conventional "terminated by Ctrl-C" code.
        std::process::exit(130);
    });
}

/// Non-unix stub: there are no unix signals to await, so installing the reaper is
/// a no-op. (The `restore`/`api` handles are kept in the signature for parity.)
#[cfg(not(unix))]
pub fn install_signal_reaper<F>(_restore: F)
where
    F: Fn() + Send + 'static,
{
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placeholder::PlaceholderApi;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The exit tests mutate the single process-wide agent registry + kill seam,
    /// so they must not run concurrently with each other.
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    /// How many times the recording kill seam was invoked across the current
    /// test (reset by each test before it drives the path under test).
    static KILL_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// A kill seam that records (but does not signal): every pgid the reaper
    /// targets bumps the counter, so a test can assert the reaper ran.
    fn recording_kill(_pgid: i32) {
        KILL_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    /// Install the recording kill seam and clear shared counters/registry.
    fn arm_recording_seam() {
        // Drain any pgids a prior test/path left behind, then start counting.
        makina_acp::kill_all_agents();
        KILL_CALLS.store(0, Ordering::SeqCst);
        makina_acp::set_kill_fn_for_test(recording_kill);
    }

    /// The clean-exit cleanup path drains the agent registry via the
    /// `acp-agent-registry` kill seam.
    ///
    /// Registers a FAKE agent pgid, runs the main-loop teardown
    /// ([`reap_open_runs`]), and asserts the registry was drained (the recording
    /// kill seam saw the pgid and `registered_pgids_for_test()` is now empty).
    #[test]
    fn clean_exit_teardown_drains_agent_registry() {
        // Plain `#[test]` (not `#[tokio::test]`) so the serializing std `Mutex`
        // guard is never held across an `.await`: we drive the async teardown via
        // a current-thread runtime's `block_on`, a synchronous call.
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        arm_recording_seam();

        // A live agent (its process group registered in the reaper's registry).
        makina_acp::register_for_test(4242);
        assert_eq!(
            makina_acp::registered_pgids_for_test(),
            vec![4242],
            "the fake agent pgid should be registered before teardown"
        );

        // Drive the exact main-loop teardown the binary runs on clean exit.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build current-thread runtime");
        let api = PlaceholderApi::new();
        rt.block_on(reap_open_runs(&api));

        // The registry must be drained via the kill seam …
        assert!(
            KILL_CALLS.load(Ordering::SeqCst) >= 1,
            "teardown should route the live pgid through the kill seam"
        );
        // … and no pgid may remain registered.
        assert!(
            makina_acp::registered_pgids_for_test().is_empty(),
            "teardown should drain the agent registry"
        );
    }

    /// The panic hook invokes the reaper before the terminal restore + default
    /// handler.
    ///
    /// Installs the TUI panic hook (via `Tui::init`'s seam), registers a fake
    /// agent pgid, triggers a panic on a side thread, and asserts the registry
    /// was drained through the recording kill seam — i.e. the hook reaped the
    /// agent.
    #[test]
    fn panic_hook_invokes_reaper() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        arm_recording_seam();

        // Install the production panic hook (it now calls kill_all_agents first).
        // It chains the harness's current hook as its default, so after the test
        // we restore that original hook to avoid leaking ours into sibling tests.
        let original = crate::tui::install_panic_hook_for_test();

        // A live agent registered just before a panic mid-run.
        makina_acp::register_for_test(7777);

        // Trigger a panic through `catch_unwind` (so the process is not aborted);
        // the hook fires on the unwinding thread before the catch returns.
        let result = std::panic::catch_unwind(|| {
            panic!("simulated mid-run panic");
        });
        assert!(result.is_err(), "the closure should have panicked");

        // Restore the harness's original hook so sibling tests are unaffected.
        std::panic::set_hook(original);

        // The panic hook must have reaped the agent via the kill seam …
        assert!(
            KILL_CALLS.load(Ordering::SeqCst) >= 1,
            "the panic hook should route the live pgid through the kill seam"
        );
        // … draining the registry so nothing is left behind.
        assert!(
            makina_acp::registered_pgids_for_test().is_empty(),
            "the panic hook should drain the agent registry"
        );
    }
}
