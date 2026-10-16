//! Turning clips into MIDI events.

use std::ops::Range;

use treason_core::midi::MidiMessage;
use treason_core::model::{PortId, Project};
use treason_core::time::{TICKS_PER_CLOCK_PULSE, Ticks};

/// A message to send at a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderedEvent {
    /// When, in song ticks.
    pub tick: Ticks,
    /// Where.
    pub port: PortId,
    /// What.
    pub message: MidiMessage,
}

/// Appends to `out` everything `project` plays in `window`, in send order.
///
/// Every unmuted track loops its clip from tick 0, so an event at clip
/// position `p` plays at `p + k × length` for every `k`. A note's note-off
/// lands at its start plus its length, which may be in a later window: each
/// window holds the note-offs that fall inside it. Events are ordered by tick,
/// and at the same tick note-offs come first, so a retriggered note isn't cut
/// short.
///
/// `out` is reused from window to window: with its capacity reserved up
/// front, rendering doesn't allocate.
#[expect(unused_variables, clippy::ptr_arg, reason = "not implemented yet")]
pub fn render(project: &Project, window: Range<Ticks>, out: &mut Vec<RenderedEvent>) {
    todo!("render looped clip events; see ARCHITECTURE.md, Timing")
}

/// The ticks in `window` that get a MIDI clock pulse: every 40th tick,
/// counting from 0, which is 24 pulses per quarter note.
pub fn clock_pulses(window: Range<Ticks>) -> impl Iterator<Item = Ticks> {
    let first = window.start.0.div_ceil(TICKS_PER_CLOCK_PULSE);
    (first..)
        .map(|pulse| Ticks(pulse * TICKS_PER_CLOCK_PULSE))
        .take_while(move |tick| *tick < window.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_pulses_fall_every_40_ticks() {
        let pulses: Vec<u64> = clock_pulses(Ticks(0)..Ticks(100)).map(|t| t.0).collect();
        assert_eq!(pulses, [0, 40, 80]);
        let pulses: Vec<u64> = clock_pulses(Ticks(41)..Ticks(121)).map(|t| t.0).collect();
        assert_eq!(pulses, [80, 120]);
        assert_eq!(clock_pulses(Ticks(1)..Ticks(40)).count(), 0);
    }
}
