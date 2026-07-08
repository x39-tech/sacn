//! A `no_std` time abstraction for the library core.
//!
//! [`std::time::Instant`] is unavailable under `no_std`, and the core must not
//! read a clock itself anyway (it has to be deterministically testable). Instead
//! the caller supplies an explicit "now" (an [`Instant`]) on every `poll`, and
//! all protocol timers are expressed against it.
//!
//! An adapter maps its runtime clock onto [`Instant`] (for example, by recording
//! a base `tokio::time::Instant` at start-up and reporting elapsed time since).
//! The exact epoch is irrelevant: only differences between [`Instant`]s are
//! meaningful, and they are required to be monotonic.

pub use core::time::Duration;

/// A monotonic point in time, supplied to the core by its caller.
///
/// Represented as a [`Duration`] since an arbitrary, fixed monotonic epoch.
/// Only differences between `Instant`s carry meaning; the epoch itself does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant {
    since_epoch: Duration,
}

impl Instant {
    /// The instant exactly at the monotonic epoch (a zero duration).
    pub const EPOCH: Instant = Instant {
        since_epoch: Duration::ZERO,
    };

    /// Creates an `Instant` a given [`Duration`] after the monotonic epoch.
    pub const fn from_epoch(since_epoch: Duration) -> Self {
        Self { since_epoch }
    }

    /// Returns the [`Duration`] between this instant and the monotonic epoch.
    pub const fn since_epoch(self) -> Duration {
        self.since_epoch
    }

    /// Returns the amount of time elapsed from `earlier` to `self`, or
    /// [`Duration::ZERO`] if `earlier` is later than `self`.
    pub fn saturating_duration_since(self, earlier: Instant) -> Duration {
        self.since_epoch.saturating_sub(earlier.since_epoch)
    }

    /// Returns `self + duration`, or `None` if the result overflows.
    pub fn checked_add(self, duration: Duration) -> Option<Self> {
        Some(Self {
            since_epoch: self.since_epoch.checked_add(duration)?,
        })
    }

    /// Returns `self - duration`, saturating at [`Instant::EPOCH`].
    pub fn saturating_sub(self, duration: Duration) -> Self {
        Self {
            since_epoch: self.since_epoch.saturating_sub(duration),
        }
    }

    /// Returns `self + duration`, saturating at the maximum representable
    /// instant rather than overflowing.
    ///
    /// Useful for computing timer deadlines (`now + timeout`) without a fallible
    /// or panicking path in the protocol core.
    pub fn saturating_add(self, duration: Duration) -> Self {
        Self {
            since_epoch: self.since_epoch.saturating_add(duration),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_since_saturates() {
        let t0 = Instant::from_epoch(Duration::from_secs(1));
        let t1 = Instant::from_epoch(Duration::from_secs(3));
        assert_eq!(t1.saturating_duration_since(t0), Duration::from_secs(2));
        assert_eq!(t0.saturating_duration_since(t1), Duration::ZERO);
    }

    #[test]
    fn add_and_sub() {
        let t = Instant::EPOCH.checked_add(Duration::from_secs(5)).unwrap();
        assert_eq!(t.since_epoch(), Duration::from_secs(5));
        assert_eq!(t.saturating_sub(Duration::from_secs(10)), Instant::EPOCH);
        assert!(
            Instant::from_epoch(Duration::MAX)
                .checked_add(Duration::from_secs(1))
                .is_none()
        );
    }

    #[test]
    fn saturating_add_saturates() {
        let t = Instant::EPOCH.saturating_add(Duration::from_secs(5));
        assert_eq!(t.since_epoch(), Duration::from_secs(5));
        assert_eq!(
            Instant::from_epoch(Duration::MAX).saturating_add(Duration::from_secs(1)),
            Instant::from_epoch(Duration::MAX)
        );
    }

    #[test]
    fn ordering_is_monotonic() {
        let a = Instant::from_epoch(Duration::from_millis(10));
        let b = Instant::from_epoch(Duration::from_millis(20));
        assert!(a < b);
        assert_eq!(a.min(b), a);
    }
}
