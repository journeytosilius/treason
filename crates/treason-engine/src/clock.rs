//! Time sources for the engine.

use std::time::Instant;

/// Where the engine gets the current time and how it waits.
pub trait Clock {
    /// The current time.
    fn now(&self) -> Instant;

    /// Returns at `deadline`, or at once if it has passed. Real-time clocks
    /// should be accurate to well under a millisecond.
    fn sleep_until(&mut self, deadline: Instant);
}

/// Wall-clock time.
///
/// Waits with `spin_sleep`: an OS sleep for most of the wait, then a short
/// spin, so wake-ups are accurate to microseconds rather than to the OS
/// scheduler's tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep_until(&mut self, deadline: Instant) {
        spin_sleep::sleep_until(deadline);
    }
}

/// A clock that jumps ahead instead of waiting, for tests and for rendering
/// faster than real time.
#[derive(Debug, Clone, Copy)]
pub struct ManualClock {
    now: Instant,
}

impl ManualClock {
    /// Starts at the current wall-clock time.
    pub fn new() -> Self {
        Self {
            now: Instant::now(),
        }
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.now
    }

    fn sleep_until(&mut self, deadline: Instant) {
        self.now = self.now.max(deadline);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn manual_clock_jumps_forward_only() {
        let mut clock = ManualClock::new();
        let start = clock.now();
        clock.sleep_until(start + Duration::from_secs(60));
        assert_eq!(clock.now(), start + Duration::from_secs(60));
        clock.sleep_until(start);
        assert_eq!(clock.now(), start + Duration::from_secs(60));
    }
}
