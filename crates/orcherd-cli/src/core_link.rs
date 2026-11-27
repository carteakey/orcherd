//! Manages the daemon's `herdr-core` connection and event subscription,
//! reconnecting with exponential backoff whenever `herdr-core` was not running
//! at daemon startup (`orcherd init` before `spawn`) or restarts later.

use orcherd_hc::{Core, Endpoint, Subscription};
use std::time::{Duration, Instant};

/// Initial backoff after a failed `herdr-core` connect attempt (doubles per failure).
pub const RECONNECT_INITIAL: Duration = Duration::from_secs(1);

/// Maximum backoff between `herdr-core` reconnect attempts.
pub const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Exponential backoff delay for the `failures`-th consecutive failure (1-indexed):
/// 1 s, 2 s, 4 s, 8 s, 16 s, capped at 30 s.
pub fn reconnect_delay(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(5);
    RECONNECT_INITIAL
        .saturating_mul(1u32 << shift)
        .min(RECONNECT_MAX)
}

/// Live `herdr-core` connection + event subscription with backoff-gated reconnect.
pub struct CoreLink<C = Core, S = Subscription> {
    core: Option<C>,
    sub: Option<S>,
    failures: u32,
    next_attempt: Option<Instant>,
}

impl<C, S> Default for CoreLink<C, S> {
    fn default() -> Self {
        Self {
            core: None,
            sub: None,
            failures: 0,
            next_attempt: None,
        }
    }
}

impl<C, S> CoreLink<C, S> {
    pub fn core(&self) -> Option<&C> {
        self.core.as_ref()
    }

    pub fn subscription_mut(&mut self) -> Option<&mut S> {
        self.sub.as_mut()
    }

    /// Drops the current connection and subscription so the next check retries
    /// immediately (starting fresh backoff if `herdr-core` is still down).
    pub fn disconnect(&mut self) {
        self.core = None;
        self.sub = None;
        self.next_attempt = None;
    }

    /// Ensures a live connection + subscription, reconnecting when absent or when
    /// `is_lost` reports the current connection dropped. Returns `true` when a
    /// fresh connection was just established (so the daemon can immediately tick).
    pub fn ensure_connected<L, F>(&mut self, now: Instant, is_lost: L, mut connect: F) -> bool
    where
        L: Fn(&C) -> bool,
        F: FnMut() -> Option<(C, S)>,
    {
        if self.core.as_ref().is_some_and(&is_lost) || (self.core.is_some() && self.sub.is_none()) {
            self.disconnect();
        }
        if self.core.is_some() {
            return false;
        }
        if self.next_attempt.is_some_and(|at| now < at) {
            return false;
        }
        if let Some((core, sub)) = connect() {
            self.core = Some(core);
            self.sub = Some(sub);
            self.failures = 0;
            self.next_attempt = None;
            true
        } else {
            self.failures = self.failures.saturating_add(1);
            self.next_attempt = Some(now + reconnect_delay(self.failures));
            false
        }
    }
}

impl CoreLink<Core, Subscription> {
    /// Connects to `herdr-core` from the environment and opens an event
    /// subscription when disconnected or lost.
    pub fn ensure_live(&mut self, now: Instant) -> bool {
        self.ensure_connected(now, Core::is_lost, || {
            let ep = Endpoint::resolve_env().ok()?;
            let core = Core::connect(&ep).ok()?;
            let sub = core.subscribe().ok()?;
            Some((core, sub))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{CoreLink, RECONNECT_MAX, reconnect_delay};
    use std::{
        cell::Cell,
        time::{Duration, Instant},
    };

    #[test]
    fn connects_after_startup_unavailable_with_backoff() {
        assert_eq!(reconnect_delay(1), Duration::from_secs(1));
        assert_eq!(reconnect_delay(2), Duration::from_secs(2));
        assert_eq!(reconnect_delay(3), Duration::from_secs(4));
        assert_eq!(reconnect_delay(6), RECONNECT_MAX);

        let mut link: CoreLink<u32, u32> = CoreLink::default();
        let t0 = Instant::now();
        let mut attempts = 0usize;

        // Startup: herdr-core not up yet -> fails, backs off 1 s.
        assert!(!link.ensure_connected(t0, |_| false, || {
            attempts += 1;
            None
        }));
        assert_eq!(attempts, 1);
        assert!(link.core().is_none());

        // Inside 1 s backoff: skipped.
        assert!(!link.ensure_connected(t0 + Duration::from_millis(500), |_| false, || {
            attempts += 1;
            Some((1, 1))
        }));
        assert_eq!(attempts, 1);

        // At t+1s: second failure -> backs off 2 s (until t+3s).
        assert!(!link.ensure_connected(t0 + Duration::from_secs(1), |_| false, || {
            attempts += 1;
            None
        }));
        assert_eq!(attempts, 2);

        // At t+2s: inside 2 s backoff -> skipped.
        assert!(!link.ensure_connected(t0 + Duration::from_secs(2), |_| false, || {
            attempts += 1;
            Some((1, 1))
        }));
        assert_eq!(attempts, 2);

        // At t+3s: herdr-core is now running -> connects and signals fresh wakeup.
        assert!(link.ensure_connected(t0 + Duration::from_secs(3), |_| false, || {
            attempts += 1;
            Some((10, 10))
        }));
        assert_eq!(attempts, 3);
        assert_eq!(link.core(), Some(&10));
        assert_eq!(link.subscription_mut(), Some(&mut 10));

        // Healthy connection does not reconnect.
        assert!(!link.ensure_connected(t0 + Duration::from_secs(4), |_| false, || {
            attempts += 1;
            Some((20, 20))
        }));
        assert_eq!(attempts, 3);
    }

    #[test]
    fn reconnects_immediately_after_core_lost_or_subscription_drop() {
        let mut link: CoreLink<u32, u32> = CoreLink::default();
        let t0 = Instant::now();
        let lost = Cell::new(false);
        let mut next_id = 1u32;

        // Initial connection succeeds.
        assert!(link.ensure_connected(t0, |_| lost.get(), || {
            let id = next_id;
            next_id += 1;
            Some((id, id))
        }));
        assert_eq!(link.core(), Some(&1));

        // Core restarts (`is_lost` flips true) -> drops old handle and reconnects right away.
        lost.set(true);
        assert!(link.ensure_connected(t0 + Duration::from_secs(5), |id| *id == 1 && lost.get(), || {
            let id = next_id;
            next_id += 1;
            Some((id, id))
        }));
        assert_eq!(link.core(), Some(&2));

        // Explicit disconnect (e.g. `Subscription::next` returned `Error::Lost`) also reconnects.
        link.disconnect();
        assert!(link.core().is_none());
        assert!(link.ensure_connected(t0 + Duration::from_secs(6), |_| false, || {
            let id = next_id;
            next_id += 1;
            Some((id, id))
        }));
        assert_eq!(link.core(), Some(&3));
    }
}
