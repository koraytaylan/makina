//! Supervision tree skeleton for Makina's fault-tolerant actor system.
//!
//! # Architecture overview
//!
//! [`RootSupervisor`] is the **fault-tolerance root** of the entire actor hierarchy.
//! It owns every long-lived actor in the system and manages their restart lifecycle.
//! It is intentionally named `RootSupervisor` to avoid colliding with the domain
//! "Supervisor" hub actor that will be introduced in the `actor-traits` task — that
//! domain actor is one of the *children* managed here, not this struct.
//!
//! ```text
//! RootSupervisor  (this module — fault-tolerance root)
//!   ├─ domain::Supervisor     (hub; added by actor-traits)
//!   │    ├─ Planner           (spoke; added by actor-traits)
//!   │    ├─ Developer(s)      (spoke; added by actor-traits)
//!   │    └─ Reviewer          (spoke; added by actor-traits)
//!   └─ (future: other top-level services)
//! ```
//!
//! # Supervision model (kameo 0.20)
//!
//! kameo 0.20 uses `Spawn::supervise(supervisor_ref, args)` to register a child with
//! a supervisor.  The supervisor's `Actor::supervision_strategy()` determines scope
//! (OneForOne / OneForAll / RestForOne).  Per-child restart behaviour is configured on
//! the `SupervisedActorBuilder` returned by `supervise()`:
//!
//! - `.restart_policy(RestartPolicy::Permanent)` — restart on any exit (default)
//! - `.restart_policy(RestartPolicy::Transient)` — restart only on panic/error
//! - `.restart_policy(RestartPolicy::Never)`     — never restart
//! - `.restart_limit(max, window)`               — max restarts per window (default 5/5 s)
//!
//! # Restart strategy
//!
//! [`RestartConfig`] is the lightweight configuration struct exposed to callers.
//! `RootSupervisor` applies it when spawning each supervised child via
//! [`RootSupervisor::spawn_child`].  The defaults are:
//! - policy:  `Transient` (restart on panic/error; don't restart a clean exit)
//! - limit:   10 restarts per 30 seconds
//!
//! Adjust with [`RestartConfig::new`] or the builder methods on [`RestartConfig`].
//!
//! # Seam for `actor-traits`
//!
//! The next task (`actor-traits`) plugs in the real role actors by calling
//! [`RootSupervisor::spawn_child`] from inside `RootSupervisor::on_start` (or later
//! from messages).  The child only needs to implement `Actor` with `Args: Clone + Sync`.

use std::{sync::Arc, time::Duration};

use kameo::{
    actor::{ActorRef, Spawn},
    error::Infallible,
    supervision::{RestartPolicy, SupervisionStrategy},
};

// ─── RestartConfig ───────────────────────────────────────────────────────────

/// Configuration for supervised-child restart behaviour.
///
/// Wraps kameo's per-child restart knobs into a single, clonable struct that
/// callers pass to [`RootSupervisor::spawn_child`].
///
/// # Defaults
///
/// | Field | Value |
/// |-------|-------|
/// | `policy` | [`RestartPolicy::Transient`] |
/// | `max_restarts` | `10` |
/// | `window` | `30 s` |
#[derive(Clone, Debug)]
pub struct RestartConfig {
    /// When to restart the child.
    pub policy: RestartPolicy,
    /// Maximum restart attempts within `window` before giving up.
    pub max_restarts: u32,
    /// Sliding window in which `max_restarts` is counted.
    pub window: Duration,
}

impl Default for RestartConfig {
    fn default() -> Self {
        Self {
            policy: RestartPolicy::Transient,
            max_restarts: 10,
            window: Duration::from_secs(30),
        }
    }
}

impl RestartConfig {
    /// Create a new config with explicit values.
    pub fn new(policy: RestartPolicy, max_restarts: u32, window: Duration) -> Self {
        Self {
            policy,
            max_restarts,
            window,
        }
    }

    /// Override the restart policy.
    pub fn policy(mut self, policy: RestartPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Override the restart limit.
    pub fn limit(mut self, max_restarts: u32, window: Duration) -> Self {
        self.max_restarts = max_restarts;
        self.window = window;
        self
    }
}

// ─── RootSupervisor ──────────────────────────────────────────────────────────

/// Root of Makina's supervision tree.
///
/// This actor is the single fault-tolerance root for all long-lived actors in the
/// system.  Spawn it first; then use [`RootSupervisor::spawn_child`] (from within
/// `on_start` or via a message) to register supervised children.
///
/// The default strategy is [`SupervisionStrategy::OneForOne`]: only the crashed
/// child is restarted; siblings are unaffected.  Override
/// `supervision_strategy()` if you need tighter coupling.
///
/// # Relationship to the domain `Supervisor` actor
///
/// The `actor-traits` task will introduce a domain `Supervisor` hub that routes
/// messages between Planner, Developer(s), and Reviewer.  That hub will be one of
/// the children registered here — it is *not* this struct.
pub struct RootSupervisor;

impl kameo::actor::Actor for RootSupervisor {
    /// `RootSupervisor` itself has no initialisation args.
    type Args = ();
    type Error = Infallible;

    fn supervision_strategy() -> SupervisionStrategy {
        // Only restart the failed child; siblings keep running.
        SupervisionStrategy::OneForOne
    }

    async fn on_start(_: (), _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(RootSupervisor)
    }
}

impl RootSupervisor {
    /// Spawn `RootSupervisor` and return its `ActorRef`.
    ///
    /// Convenience wrapper so callers don't need to import `Spawn` themselves.
    pub fn start() -> ActorRef<Self> {
        RootSupervisor::spawn(())
    }

    /// Register and spawn a supervised child under this root supervisor.
    ///
    /// # Type parameters
    ///
    /// `C` — the child actor type.  Its `Args` must be `Clone + Sync` so kameo
    /// can clone the args on each restart.  If your args are not `Sync`, use
    /// `kameo::actor::Spawn::supervise_with` directly on the `ActorRef`.
    ///
    /// # Parameters
    ///
    /// - `supervisor_ref` — the live `ActorRef<RootSupervisor>` (typically `self`
    ///   inside `on_start`, or passed in from outside).
    /// - `args` — initial arguments forwarded to `C::on_start`; cloned on every
    ///   restart.
    /// - `config` — restart policy and intensity limit; use
    ///   [`RestartConfig::default()`] for sensible defaults.
    ///
    /// # Returns
    ///
    /// The `ActorRef<C>` of the freshly-spawned child.
    ///
    /// # Seam for `actor-traits`
    ///
    /// The `actor-traits` task should call this method (or `Spawn::supervise`
    /// directly) to wire up the domain Supervisor, Planner, Developer, and
    /// Reviewer actors as supervised children of this root.
    pub async fn spawn_child<C>(
        supervisor_ref: &ActorRef<Self>,
        args: C::Args,
        config: RestartConfig,
    ) -> ActorRef<C>
    where
        C: kameo::actor::Actor,
        C::Args: Clone + Sync,
    {
        C::supervise(supervisor_ref, args)
            .restart_policy(config.policy)
            .restart_limit(config.max_restarts, config.window)
            .spawn()
            .await
    }
}

// ─── Placeholder child actors ─────────────────────────────────────────────────

/// A generic placeholder child demonstrating supervised actor lifecycle.
///
/// `PlaceholderWorker` is not role-specific.  It exists to:
/// 1. Show how a child is registered with `RootSupervisor`.
/// 2. Enable the restart test (see the `tests` module).
///
/// Real role actors (domain Supervisor, Planner, Developer, Reviewer) will replace
/// or join it when `actor-traits` is implemented.
#[derive(Clone)]
pub struct PlaceholderWorker {
    /// Shared restart counter: incremented in `on_start` on every (re)start.
    pub start_count: Arc<std::sync::atomic::AtomicU32>,
}

impl PlaceholderWorker {
    /// Create a new `PlaceholderWorker` sharing the given counter.
    pub fn new(start_count: Arc<std::sync::atomic::AtomicU32>) -> Self {
        Self { start_count }
    }
}

impl kameo::actor::Actor for PlaceholderWorker {
    type Args = Self;
    type Error = Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        args.start_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(args)
    }
}

// ─── Messages for PlaceholderWorker ──────────────────────────────────────────

/// Ask the worker how many times it has been (re)started.
pub struct QueryStartCount;

impl kameo::message::Message<QueryStartCount> for PlaceholderWorker {
    type Reply = u32;

    async fn handle(
        &mut self,
        _: QueryStartCount,
        _ctx: &mut kameo::message::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.start_count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Deliberately panic to test supervised restart.
pub struct TriggerCrash;

impl kameo::message::Message<TriggerCrash> for PlaceholderWorker {
    type Reply = ();

    async fn handle(
        &mut self,
        _: TriggerCrash,
        _ctx: &mut kameo::message::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        panic!("deliberate crash for supervision test");
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        },
        time::Duration,
    };

    use kameo::supervision::RestartPolicy;

    use super::{PlaceholderWorker, QueryStartCount, RestartConfig, RootSupervisor, TriggerCrash};

    /// Poll a shared counter until it reaches `target`, with a bounded 2s deadline.
    /// Avoids fixed sleeps so the assertion is robust under loaded CI.
    async fn poll_until(counter: &Arc<AtomicU32>, target: u32, what: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if counter.load(Ordering::SeqCst) >= target {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting: {what} (counter never reached {target})");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Prove that `RootSupervisor` restarts a child that panics.
    ///
    /// # How determinism is achieved
    ///
    /// A shared `AtomicU32` is incremented in `PlaceholderWorker::on_start`.  After
    /// triggering a crash we poll the counter in a tight loop (up to 2 seconds) and
    /// break as soon as it reaches 2.  This avoids both fixed sleeps and flaky races:
    /// - If restart happens quickly we exit the loop immediately.
    /// - If kameo is unusually slow we still have a 2-second budget before failing.
    #[tokio::test]
    async fn root_supervisor_restarts_crashing_child() {
        let root = RootSupervisor::start();

        let start_count = Arc::new(AtomicU32::new(0));
        let config = RestartConfig::default()
            .policy(RestartPolicy::Permanent) // restart on any exit, including panic
            .limit(5, Duration::from_secs(10));

        let child = RootSupervisor::spawn_child::<PlaceholderWorker>(
            &root,
            PlaceholderWorker::new(start_count.clone()),
            config,
        )
        .await;

        // Wait for initial start — poll (no fixed sleep) so this is CI-robust.
        poll_until(&start_count, 1, "child should have started once").await;

        // Trigger crash (fire-and-forget; the actor will panic processing this message).
        let _ = child.tell(TriggerCrash).await;

        // Poll for restart — deterministic bounded wait (no fixed sleep required).
        poll_until(&start_count, 2, "child should have restarted after crash").await;

        // The child is now restarted — confirm it is responsive.
        let count = child
            .ask(QueryStartCount)
            .send()
            .await
            .expect("restarted child should respond to messages");

        assert_eq!(count, 2, "start_count should be 2 after one restart");

        // Clean shutdown.
        root.kill();
    }

    /// Additional: verify the default `RestartConfig` values.
    #[test]
    fn restart_config_defaults() {
        let cfg = RestartConfig::default();
        assert_eq!(cfg.policy, RestartPolicy::Transient);
        assert_eq!(cfg.max_restarts, 10);
        assert_eq!(cfg.window, Duration::from_secs(30));
    }

    /// Additional: builder methods chain correctly.
    #[test]
    fn restart_config_builder() {
        let cfg = RestartConfig::default()
            .policy(RestartPolicy::Permanent)
            .limit(3, Duration::from_secs(60));
        assert_eq!(cfg.policy, RestartPolicy::Permanent);
        assert_eq!(cfg.max_restarts, 3);
        assert_eq!(cfg.window, Duration::from_secs(60));
    }
}
