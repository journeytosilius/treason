//! Converting between ticks and wall-clock time.

use std::time::{Duration, Instant};

use treason_core::time::{Bpm, PPQN, Ticks};

/// How long one tick lasts at 0.001 BPM, in nanoseconds: a minute divided by
/// PPQN, times 1000.
const TICK_NANOS_AT_ONE_MILLIBPM: u128 = 60_000_000_000_000 / PPQN as u128;

/// Maps ticks to instants and back, counting from an anchor.
///
/// The anchor is the last start, locate or tempo change. Every conversion is
/// computed from it rather than accumulated, so there is no drift: the
/// millionth tick is as exact as the first. Tempo is kept to a thousandth of a
/// BPM, which makes the arithmetic integer and the round trip exact:
/// `tick_at(time_of(t)) == t` for every tick.
///
/// A tempo change keeps the old mapping for the ticks up to the change, since
/// ports with less latency send those ticks after it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timebase {
    /// The ticks from the anchor on, and before it too when `before` is
    /// `None`.
    current: Segment,
    tempo: Bpm,
    /// The mapping before the last tempo change, for the ticks up to
    /// `current`'s anchor.
    before: Option<Segment>,
}

/// One tempo from an anchor, extrapolated both ways.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Segment {
    anchor_time: Instant,
    anchor_tick: Ticks,
    millibpm: u128,
}

impl Timebase {
    /// Tick `tick` happens at `time`, and time runs at `tempo`.
    pub fn new(time: Instant, tick: Ticks, tempo: Bpm) -> Self {
        Self {
            current: Segment::new(time, tick, tempo),
            tempo,
            before: None,
        }
    }

    /// The tempo.
    pub const fn tempo(&self) -> Bpm {
        self.tempo
    }

    /// When `tick` happens: the first nanosecond at or after its exact time.
    pub fn time_of(&self, tick: Ticks) -> Instant {
        match self.before {
            Some(before) if tick <= self.current.anchor_tick => before.time_of(tick),
            _ => self.current.time_of(tick),
        }
    }

    /// The last tick at or before `time`.
    pub fn tick_at(&self, time: Instant) -> Ticks {
        match self.before {
            Some(before) if time < self.current.anchor_time => {
                before.tick_at(time).min(self.current.anchor_tick)
            }
            _ => self.current.tick_at(time),
        }
    }

    /// Changes to `tempo` after `tick`. The ticks after it count from `time`
    /// at the new tempo, while `tick` and the ticks before it keep their
    /// times. `tick` lasts until `time`, which can't be earlier than its time.
    ///
    /// The ticks up to the previous change lose their mapping, so wait until
    /// the timebase has [`settled`](Self::settled) before changing again.
    pub fn retempo(&mut self, tick: Ticks, time: Instant, tempo: Bpm) {
        assert!(
            time >= self.time_of(tick),
            "a tempo change can't move a tick earlier"
        );
        *self = Self {
            current: Segment::new(time, tick, tempo),
            tempo,
            // As it was, so the earlier ticks round exactly as before.
            before: Some(self.current),
        };
    }

    /// Whether every tick up to the last tempo change has happened by `now`,
    /// so that another change can't need their mapping.
    pub fn settled(&self, now: Instant) -> bool {
        self.before.is_none() || now >= self.current.anchor_time
    }
}

impl Segment {
    fn new(time: Instant, tick: Ticks, tempo: Bpm) -> Self {
        Self {
            anchor_time: time,
            anchor_tick: tick,
            // Bpm keeps tempo between 20 and 400, so this is 20_000 to 400_000.
            millibpm: (tempo.get() * 1000.0).round() as u128,
        }
    }

    fn time_of(&self, tick: Ticks) -> Instant {
        if tick >= self.anchor_tick {
            let ticks = u128::from((tick - self.anchor_tick).0);
            self.anchor_time + nanos((ticks * TICK_NANOS_AT_ONE_MILLIBPM).div_ceil(self.millibpm))
        } else {
            let ticks = u128::from((self.anchor_tick - tick).0);
            self.anchor_time - nanos(ticks * TICK_NANOS_AT_ONE_MILLIBPM / self.millibpm)
        }
    }

    fn tick_at(&self, time: Instant) -> Ticks {
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
        timebase.retempo(bar_two, before, bpm(60.0));
        assert_eq!(timebase.time_of(bar_two), before);
        assert_eq!(
            timebase.time_of(Ticks::quarters(5)),
            before + Duration::from_secs(1)
        );
        assert_eq!(timebase.tempo(), bpm(60.0));
    }

    #[test]
    fn retempo_keeps_earlier_ticks_and_holds_its_tick_until_the_new_time() {
        let start = Instant::now();
        let ms = Duration::from_millis;
        for (tempo, tick_nanos) in [(240.0, 260_417), (60.0, 1_041_667)] {
            let mut timebase = Timebase::new(start, Ticks::ZERO, bpm(120.0));
            let change = Ticks(3840);
            let old = timebase;
            timebase.retempo(change, start + ms(2010), bpm(tempo));
            // Up to the change, nothing moves.
            for tick in (3000..=3840).map(Ticks) {
                assert_eq!(timebase.time_of(tick), old.time_of(tick), "{tick}");
                assert_eq!(timebase.tick_at(old.time_of(tick)), tick);
            }
            // The change's tick lasts until the new tempo starts, 10 ms later.
            assert_eq!(timebase.tick_at(start + ms(2005)), change);
            assert_eq!(timebase.tick_at(start + ms(2010)), change);
            let next = timebase.time_of(Ticks(3841));
            assert_eq!(next, start + ms(2010) + Duration::from_nanos(tick_nanos));
            assert_eq!(timebase.tick_at(next - Duration::from_nanos(1)), change);
            for tick in (3841..5000).map(Ticks) {
                assert_eq!(timebase.tick_at(timebase.time_of(tick)), tick, "{tick}");
            }
            assert!(!timebase.settled(start + ms(2009)));
            assert!(timebase.settled(start + ms(2010)));
            assert!(Timebase::new(start, change, bpm(tempo)).settled(start));
        }
    }
}
