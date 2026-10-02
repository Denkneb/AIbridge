//! Reusable bounded startup grace tracking for a freshly spawned worker
//! (task 7.2).
//!
//! A detached worker takes the project [`crate::WorkerLock`] only after its own
//! interpreter and storage startup, so the lock is still free for a short window
//! right after the spawn. That free lock is **not** "nothing is running" and
//! **not** "the worker finished": the reference `mcp_server.py`
//! (`SPAWN_STARTUP_GRACE_SECONDS`, the spawn-pending branch of
//! `task_status_impl`) waits through exactly this window. [`StartupGrace`]
//! reproduces that bounded, monotonic observation as a small reusable helper so
//! the future `task_status` wiring does not re-implement the clock.
//!
//! # Observation semantics
//!
//! A `StartupGrace` is created with an explicit monotonic [`Instant`] and a
//! grace [`Duration`] ([`DEFAULT_STARTUP_GRACE`] is the reference 10 seconds),
//! and is fed lock observations with an explicit `now`. It returns a
//! [`StartupObservation`]:
//!
//! * [`StartupObservation::Pending`] — the lock has never been seen held and
//!   `now - started_at` is **at most** the grace. Keep observing.
//! * [`StartupObservation::Held`] — the lock is currently held. The first held
//!   observation closes the startup window; a later free observation then means
//!   the worker finished.
//! * [`StartupObservation::Released`] — the lock was seen held earlier and is
//!   now free, i.e. the worker finished (this is *not* reported during the
//!   startup window).
//! * [`StartupObservation::GraceExpired`] — the grace elapsed without ever
//!   seeing the lock held, so the spawned worker never acquired it. The caller
//!   stops observing instead of waiting for a phantom in-flight worker; a later
//!   call may retry.
//!
//! Expiry is strict: `elapsed > grace`, so an observation exactly at the grace
//! boundary is still [`StartupObservation::Pending`]. Every comparison uses the
//! caller-supplied `Instant`, which makes the boundary deterministic in tests
//! without any real sleep.
//!
//! # Invariants preserved for the future `task_status`
//!
//! * A repeated spawn resets the clock: [`StartupGrace::restart`] sets a new
//!   `started_at` and forgets any previously seen held lock.
//! * Settled task statuses (`complete`/`awaiting_review`/`accepted`/`closed`)
//!   and a `failed` task whose lock is still held are handled by the caller
//!   *before* startup tracking, exactly like the reference. This helper is only
//!   about the lock/clock, not the task state machine, MCP or auto-spawn.
//! * [`StartupGrace::observe_lock`] is the narrow convenience that probes the
//!   Rust-owned project lock through [`crate::WorkerLock::is_held`] and folds
//!   the result into [`StartupGrace::observe`].

use std::time::{Duration, Instant};

use bridge_storage::RustStateLayout;

use crate::lock::{WorkerLock, WorkerLockError};

/// The reference default startup grace (`mcp_server.py`), 10 seconds.
pub const DEFAULT_STARTUP_GRACE: Duration = Duration::from_secs(10);

/// The typed observation of one lock sample during a startup window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StartupObservation {
    /// No lock has been seen held and the grace has not elapsed; keep observing.
    Pending,
    /// The lock is currently held by the spawned worker.
    Held,
    /// A lock was seen held earlier and is now free: the worker finished.
    Released,
    /// The grace elapsed without ever observing a held lock.
    GraceExpired,
}

/// A bounded, monotonic startup-window tracker for one spawned worker.
///
/// The tracker is pure: it holds only the start [`Instant`], the grace and
/// whether a held lock has been observed, so it is cheap to keep in a future
/// `task_status` loop and fully deterministic under an injected clock.
#[derive(Debug, Clone)]
pub struct StartupGrace {
    started_at: Instant,
    grace: Duration,
    seen_held: bool,
}

impl StartupGrace {
    /// Creates a tracker for a spawn that started at `started_at`.
    #[must_use]
    pub const fn new(started_at: Instant, grace: Duration) -> Self {
        Self {
            started_at,
            grace,
            seen_held: false,
        }
    }

    /// Creates a tracker with the reference [`DEFAULT_STARTUP_GRACE`].
    #[must_use]
    pub const fn with_default_grace(started_at: Instant) -> Self {
        Self::new(started_at, DEFAULT_STARTUP_GRACE)
    }

    /// Returns the configured grace.
    #[must_use]
    pub const fn grace(&self) -> Duration {
        self.grace
    }

    /// Returns the instant the current startup window started at.
    #[must_use]
    pub const fn started_at(&self) -> Instant {
        self.started_at
    }

    /// Returns whether a held lock has been observed in this window.
    #[must_use]
    pub const fn seen_held(&self) -> bool {
        self.seen_held
    }

    /// Restarts the startup window at `started_at`.
    ///
    /// This is the repeated-spawn reset: the clock moves to the new spawn and
    /// any previously seen held lock is forgotten, so the new worker gets its
    /// own full grace.
    pub fn restart(&mut self, started_at: Instant) {
        self.started_at = started_at;
        self.seen_held = false;
    }

    /// Folds one lock sample into the tracker and returns the observation.
    ///
    /// `lock_held` is the current lock state and `now` is the monotonic sample
    /// instant. The first `true` sample closes the startup window; before that,
    /// a `false` sample is [`StartupObservation::Pending`] until `now` is
    /// strictly past the grace, then [`StartupObservation::GraceExpired`].
    #[must_use]
    pub fn observe(&mut self, lock_held: bool, now: Instant) -> StartupObservation {
        if self.seen_held {
            return if lock_held {
                StartupObservation::Held
            } else {
                StartupObservation::Released
            };
        }
        if lock_held {
            self.seen_held = true;
            return StartupObservation::Held;
        }
        if now.saturating_duration_since(self.started_at) > self.grace {
            StartupObservation::GraceExpired
        } else {
            StartupObservation::Pending
        }
    }

    /// Probes the Rust-owned project lock and folds it into the tracker.
    ///
    /// This is the reusable bridge to the future `task_status` loop: it samples
    /// [`WorkerLock::is_held`] for `layout` at `now` and returns the resulting
    /// [`StartupObservation`].
    ///
    /// # Errors
    ///
    /// Returns the typed [`WorkerLockError`] of the probe when the state
    /// ownership guard or the lock file access fails.
    pub fn observe_lock(
        &mut self,
        layout: &RustStateLayout,
        now: Instant,
    ) -> Result<StartupObservation, WorkerLockError> {
        let held = WorkerLock::is_held(layout)?;
        Ok(self.observe(held, now))
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_STARTUP_GRACE, StartupGrace, StartupObservation};
    use std::time::{Duration, Instant};

    fn grace_of(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn default_grace_is_ten_seconds() {
        let start = Instant::now();
        let tracker = StartupGrace::with_default_grace(start);
        assert_eq!(tracker.grace(), DEFAULT_STARTUP_GRACE);
        assert_eq!(DEFAULT_STARTUP_GRACE, Duration::from_secs(10));
        assert!(!tracker.seen_held());
    }

    #[test]
    fn free_lock_right_after_spawn_stays_pending_not_released() {
        let start = Instant::now();
        let mut tracker = StartupGrace::new(start, grace_of(10));

        assert_eq!(tracker.observe(false, start), StartupObservation::Pending);
        assert_eq!(
            tracker.observe(false, start + Duration::from_secs(9)),
            StartupObservation::Pending
        );
        assert!(!tracker.seen_held());
    }

    #[test]
    fn expiry_is_strictly_past_the_grace_boundary() {
        let start = Instant::now();
        let mut tracker = StartupGrace::new(start, grace_of(10));

        assert_eq!(
            tracker.observe(false, start + grace_of(10)),
            StartupObservation::Pending,
            "elapsed == grace must not expire"
        );
        assert_eq!(
            tracker.observe(false, start + grace_of(10) + Duration::from_nanos(1)),
            StartupObservation::GraceExpired,
            "elapsed > grace must expire"
        );
    }

    #[test]
    fn held_then_free_is_released_and_not_pending() {
        let start = Instant::now();
        let mut tracker = StartupGrace::new(start, grace_of(10));

        assert_eq!(
            tracker.observe(true, start + Duration::from_secs(1)),
            StartupObservation::Held
        );
        assert!(tracker.seen_held());
        assert_eq!(
            tracker.observe(true, start + Duration::from_secs(2)),
            StartupObservation::Held
        );
        assert_eq!(
            tracker.observe(false, start + Duration::from_secs(3)),
            StartupObservation::Released
        );
        assert_eq!(
            tracker.observe(false, start + Duration::from_secs(20)),
            StartupObservation::Released,
            "once released the startup window stays closed"
        );
    }

    #[test]
    fn never_acquires_is_bounded_by_grace() {
        let start = Instant::now();
        let mut tracker = StartupGrace::new(start, grace_of(10));

        assert_eq!(tracker.observe(false, start), StartupObservation::Pending);
        assert_eq!(
            tracker.observe(false, start + grace_of(10)),
            StartupObservation::Pending
        );
        assert_eq!(
            tracker.observe(false, start + grace_of(11)),
            StartupObservation::GraceExpired
        );
        assert_eq!(
            tracker.observe(false, start + grace_of(12)),
            StartupObservation::GraceExpired,
            "the bounded never-acquire outcome is stable"
        );
    }

    #[test]
    fn repeated_spawn_resets_the_clock_and_seen_held() {
        let start = Instant::now();
        let mut tracker = StartupGrace::new(start, grace_of(10));

        assert_eq!(
            tracker.observe(true, start + Duration::from_secs(1)),
            StartupObservation::Held
        );
        assert_eq!(
            tracker.observe(false, start + Duration::from_secs(2)),
            StartupObservation::Released
        );

        let restarted = start + Duration::from_secs(100);
        tracker.restart(restarted);
        assert_eq!(tracker.started_at(), restarted);
        assert!(!tracker.seen_held());
        assert_eq!(
            tracker.observe(false, restarted),
            StartupObservation::Pending,
            "the new spawn gets a fresh grace window"
        );
    }

    #[test]
    fn delayed_acquisition_within_grace_becomes_held() {
        let start = Instant::now();
        let mut tracker = StartupGrace::new(start, grace_of(10));

        assert_eq!(tracker.observe(false, start), StartupObservation::Pending);
        assert_eq!(
            tracker.observe(false, start + Duration::from_secs(1)),
            StartupObservation::Pending
        );
        assert_eq!(
            tracker.observe(true, start + Duration::from_secs(2)),
            StartupObservation::Held
        );
    }
}
