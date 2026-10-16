//! Converting between ticks and wall-clock time.

use std::time::{Duration, Instant};

use treason_core::time::{Bpm, PPQN, Ticks};

/// How long one tick lasts at 0.001 BPM, in nanoseconds: a minute divided by
/// PPQN, times 1000.
const TICK_NANOS_AT_ONE_MILLIBPM: u128 = 60_000_000_000_000 / PPQN as u128;

/// Maps ticks to instants and back at one tempo, counting from an anchor.
///
/// The anchor is the last start, locate or tempo change. Every conversion is
/// computed from it rather than accumulated, so there is no drift: the
/// millionth tick is as exact as the first. Tempo is kept to a thousandth of a
/// BPM, which makes the arithmetic integer and the round trip exact:
/// `tick_at(time_of(t)) == t` for every tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timebase {
    anchor_time: Instant,
    anchor_tick: Ticks,
    tempo: Bpm,
    millibpm: u128,
}

impl Timebase {
    /// Tick `tick` happens at `time`, and time runs at `tempo`.
    pub fn new(time: Instant, tick: Ticks, tempo: Bpm) -> Self {
        Self {
            anchor_time: time,
            anchor_tick: tick,
            tempo,
            // Bpm keeps tempo between 20 and 400, so this is 20_000 to 400_000.
            millibpm: (tempo.get() * 1000.0).round() as u128,
        }
    }

    /// The tempo.
    pub const fn tempo(&self) -> Bpm {
        self.tempo
    }

    /// When `tick` happens: the first nanosecond at or after its exact time.
    pub fn time_of(&self, tick: Ticks) -> Instant {
        if tick >= self.anchor_tick {
            let ticks = u128::from((tick - self.anchor_tick).0);
            self.anchor_time + nanos((ticks * TICK_NANOS_AT_ONE_MILLIBPM).div_ceil(self.millibpm))
        } else {
            let ticks = u128::from((self.anchor_tick - tick).0);
            self.anchor_time - nanos(ticks * TICK_NANOS_AT_ONE_MILLIBPM / self.millibpm)
        }
    }

    /// The last tick at or before `time`.
    pub fn tick_at(&self, time: Instant) -> Ticks {
        if time >= self.anchor_time {
            let elapsed = (time - self.anchor_time).as_nanos();
            let ticks = elapsed * self.millibpm / TICK_NANOS_AT_ONE_MILLIBPM;
            Ticks(self.anchor_tick.0.saturating_add(saturate(ticks)))
        } else {
            let before = (self.anchor_time - time).as_nanos();
            let ticks = (before * self.millibpm).div_ceil(TICK_NANOS_AT_ONE_MILLIBPM);
            Ticks(self.anchor_tick.0.saturating_sub(saturate(ticks)))
        }
    }

    /// Reanchors at `tick`'s current time with a new tempo. Other ticks,
    /// including earlier ones, are extrapolated at the new tempo.
    pub fn retempo(&mut self, tick: Ticks, tempo: Bpm) {
        *self = Self::new(self.time_of(tick), tick, tempo);
    }
}

fn nanos(nanos: u128) -> Duration {
    Duration::from_nanos(saturate(nanos))
}

fn saturate(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bpm(bpm: f64) -> Bpm {
        Bpm::new(bpm).unwrap()
    }

    #[test]
    fn beats_land_on_exact_times() {
        let start = Instant::now();
        let timebase = Timebase::new(start, Ticks::ZERO, bpm(120.0));
        assert_eq!(
            timebase.time_of(Ticks::quarters(1)),
            start + Duration::from_millis(500)
        );
        // 520_833.3 ns, rounded up to the next whole nanosecond.
        assert_eq!(
            timebase.time_of(Ticks(1)),
            start + Duration::from_nanos(520_834)
        );
    }

    #[test]
    fn no_drift_over_an_hour() {
        let start = Instant::now();
        let timebase = Timebase::new(start, Ticks::ZERO, bpm(120.0));
        let hour = Ticks::quarters(120 * 60);
        assert_eq!(timebase.time_of(hour), start + Duration::from_secs(3600));
        assert_eq!(timebase.tick_at(start + Duration::from_secs(3600)), hour);
    }

    #[test]
    fn ticks_round_trip_at_any_tempo() {
        // Ticks before the anchor map to earlier times; keep them after boot.
        let start = Instant::now() + Duration::from_secs(60);
        for tempo in [20.0, 93.7, 120.0, 133.333, 400.0] {
            let anchor = Ticks(10_000);
            let timebase = Timebase::new(start, anchor, bpm(tempo));
            for tick in (0..20_000).step_by(7).map(Ticks) {
                let time = timebase.time_of(tick);
                assert_eq!(timebase.tick_at(time), tick, "tick {tick} at {tempo} BPM");
                if tick > Ticks::ZERO {
                    let just_before = time - Duration::from_nanos(1);
                    assert_eq!(timebase.tick_at(just_before), tick - Ticks(1));
                }
            }
        }
    }

    #[test]
    fn retempo_keeps_the_anchor_time() {
        let start = Instant::now();
        let mut timebase = Timebase::new(start, Ticks::ZERO, bpm(120.0));
        let bar_two = Ticks::quarters(4);
        let before = timebase.time_of(bar_two);
        timebase.retempo(bar_two, bpm(60.0));
        assert_eq!(timebase.time_of(bar_two), before);
        assert_eq!(
            timebase.time_of(Ticks::quarters(5)),
            before + Duration::from_secs(1)
        );
        assert_eq!(timebase.tempo(), bpm(60.0));
    }
}
