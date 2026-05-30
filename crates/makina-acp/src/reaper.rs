//! Process-wide registry of live agent process groups + a last-resort reaper.
//!
//! Each [`AcpClient`](crate::AcpClient) that spawns an agent puts the agent in
//! its **own process group** (`process_group(0)`, see [`spawn_transport`]) whose
//! pgid equals the agent's pid. That pgid is registered here the instant the
//! subprocess is spawned and deregistered once the client's own group-kill has
//! run (`shutdown`/`Drop`). The registry is the safety net for the paths a
//! single client cannot cover on its own:
//!
//! * a **panic** mid-run (the TUI panic hook),
//! * an **out-of-band signal** (`SIGINT`/`SIGTERM` reaching the binary).
//!
//! In either case the binary calls [`kill_all_agents`] — from a **sync** context
//! (the panic hook) or an **async** one (the signal task) — which drains the set
//! and `killpg(SIGKILL)`s every pgid still in it, best-effort. A client that shut
//! down normally has already removed its pgid, so it is **not** re-killed.
//!
//! # Test seam
//!
//! The real `killpg` is routed through a swappable [`KillFn`] so unit tests can
//! substitute a recording stub and assert which pgids the reaper targeted
//! without signalling any real process. See [`set_kill_fn_for_test`].

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// The kill seam: a function that delivers a (SIGKILL) group-kill to `pgid`.
///
/// In production this is [`real_killpg`]; tests can swap in a recording stub via
/// [`set_kill_fn_for_test`] so registry bookkeeping is exercised without
/// signalling any real process.
type KillFn = fn(i32);

/// Process-wide set of pgids for agents that are currently live (spawned and not
/// yet group-killed by their owning client). Guarded by a `Mutex`; a poisoned
/// lock is recovered (we only ever mutate a `HashSet<i32>`, so a panic mid-update
/// cannot leave a logically-inconsistent invariant we care about).
static AGENT_PGIDS: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();

/// The currently-installed kill seam. `None` ⇒ use [`real_killpg`]. Only set by
/// tests via [`set_kill_fn_for_test`].
static KILL_FN: OnceLock<Mutex<Option<KillFn>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashSet<i32>> {
    AGENT_PGIDS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn kill_fn_slot() -> &'static Mutex<Option<KillFn>> {
    KILL_FN.get_or_init(|| Mutex::new(None))
}

/// The production kill seam: SIGKILL the whole process group `pgid`. Best-effort
/// — a group that already exited yields `ESRCH`, which we ignore.
#[cfg_attr(not(unix), allow(unused_variables))]
fn real_killpg(pgid: i32) {
    #[cfg(unix)]
    {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pgid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

/// Resolve the active kill seam (the test override if installed, else the real
/// `killpg`).
fn active_kill_fn() -> KillFn {
    (*kill_fn_slot().lock().unwrap_or_else(|e| e.into_inner())).unwrap_or(real_killpg as KillFn)
}

/// Record `pgid` as a live agent process group. Called right after the agent
/// subprocess is spawned.
pub(crate) fn register(pgid: i32) {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(pgid);
}

/// Drop `pgid` from the live set. Called by a client after it has group-killed
/// its own agent (`shutdown`/`Drop`), so the reaper does not re-kill a pgid that
/// has already been reaped (and whose number may have been recycled).
pub(crate) fn deregister(pgid: i32) {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&pgid);
}

/// Reap **every** agent process group still registered: drain the set and
/// `killpg(SIGKILL)` each pgid, best-effort (ESRCH is ignored by the kill seam).
///
/// Safe to call from a **sync** context (the TUI panic hook) and an **async**
/// one (the signal handler) — it takes no locks across an `.await` and does no
/// async work itself. Clients that shut down normally have already deregistered,
/// so they are not re-killed. Idempotent: a second call finds the set empty.
pub fn kill_all_agents() {
    // Drain under the lock, then signal outside it so a slow/blocked kill cannot
    // hold the registry lock.
    let targets: Vec<i32> = {
        let mut set = registry().lock().unwrap_or_else(|e| e.into_inner());
        set.drain().collect()
    };
    let kill = active_kill_fn();
    for pgid in targets {
        kill(pgid);
    }
}

// ── Test seam ──────────────────────────────────────────────────────────────────

/// Install a recording kill stub for the duration of a test, returning the
/// previously-installed seam (so a test can restore it).
///
/// Exposed (behind the `test-util` feature) so downstream crates — notably the
/// `makina` TUI binary — can drive the exit-cleanup path in their own tests and
/// assert which pgids the reaper targeted, without signalling any real process.
/// Not compiled into production builds.
#[cfg(any(test, feature = "test-util"))]
#[doc(hidden)]
pub fn set_kill_fn_for_test(f: KillFn) -> Option<KillFn> {
    kill_fn_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(f)
}

/// Register `pgid` as a live agent process group (test seam).
///
/// The production `register` is `pub(crate)`; this `test-util`-gated wrapper lets
/// downstream-crate tests seed the registry with a *fake* pgid so they can drive
/// the exit-cleanup path and assert the registry is drained. Not compiled into
/// production builds.
#[cfg(any(test, feature = "test-util"))]
#[doc(hidden)]
pub fn register_for_test(pgid: i32) {
    register(pgid);
}

/// Snapshot the set of currently-registered pgids (test seam).
///
/// Lets a test assert the registry was drained by [`kill_all_agents`]. Not
/// compiled into production builds.
#[cfg(any(test, feature = "test-util"))]
#[doc(hidden)]
pub fn registered_pgids_for_test() -> Vec<i32> {
    let mut v: Vec<i32> = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .collect();
    v.sort_unstable();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Serialize tests in this module: they all mutate the single process-wide
    /// registry + kill seam, so they must not run concurrently.
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    /// Where the recording stub appends every pgid it is asked to kill.
    static RECORDED: StdMutex<Vec<i32>> = StdMutex::new(Vec::new());

    /// The recording kill seam: instead of signalling, record the pgid.
    fn recording_kill(pgid: i32) {
        RECORDED.lock().unwrap().push(pgid);
    }

    /// Reset shared state to a clean slate for one test.
    fn reset() {
        registry().lock().unwrap_or_else(|e| e.into_inner()).clear();
        RECORDED.lock().unwrap().clear();
        set_kill_fn_for_test(recording_kill);
    }

    #[test]
    fn kill_all_agents_records_targets_and_empties_set() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();

        // Register two live agent pgids.
        register(1001);
        register(1002);
        assert_eq!(
            registry().lock().unwrap().len(),
            2,
            "both pgids should be registered"
        );

        kill_all_agents();

        // Both targets must have been routed through the kill seam …
        let mut recorded = RECORDED.lock().unwrap().clone();
        recorded.sort_unstable();
        assert_eq!(
            recorded,
            vec![1001, 1002],
            "kill_all_agents should target every registered pgid"
        );
        // … and the set must be empty afterwards.
        assert!(
            registry().lock().unwrap().is_empty(),
            "kill_all_agents should drain the registry"
        );
    }

    #[test]
    fn deregistered_pgid_is_not_re_killed() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();

        // A client registers, then (on normal shutdown) deregisters its pgid.
        register(2001);
        register(2002);
        deregister(2001); // 2001 shut down normally

        kill_all_agents();

        // Only the still-live pgid (2002) should have been killed; the normally
        // shut-down pgid (2001) must NOT be re-killed.
        let recorded = RECORDED.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![2002],
            "a normally-deregistered pgid must not be re-killed"
        );
        assert!(
            registry().lock().unwrap().is_empty(),
            "registry should be drained after reaping"
        );
    }
}
