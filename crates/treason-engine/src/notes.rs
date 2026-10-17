//! Keeps track of sounding notes, held pedals, pitch bend and pressure so
//! the engine can always end them.

use treason_core::midi::{Channel, MidiMessage, U7, U14};
use treason_core::model::{DEFAULT_RELEASE, TrackId};

/// Pedals that can hold a note after its key's note-off.
pub(crate) const HOLD_PEDALS: [U7; 2] = [U7::saturating(64), U7::saturating(66)];

/// Whether `message` leaves something cleanup must end: a note sounding, a
/// hold pedal down, pitch bend off centre, or channel pressure above zero.
pub(crate) fn engages(message: MidiMessage) -> bool {
    match message {
        MidiMessage::NoteOn { velocity, .. } => velocity != U7::MIN,
        MidiMessage::ControlChange {
            controller, value, ..
        } => HOLD_PEDALS.contains(&controller) && value.get() >= 64,
        MidiMessage::PitchBend { value, .. } => value != U14::CENTER,
        MidiMessage::ChannelPressure { pressure, .. } => pressure != U7::MIN,
        _ => false,
    }
}

/// The notes sounding on one output port, one bit per channel and note.
///
/// 256 bytes, no allocation, constant-time updates. The engine keeps one per
/// port and uses it to send note-offs on stop, panic, clip changes and
/// shutdown, so external gear is never left with stuck notes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveNotes {
    channels: [u128; 16],
}

impl ActiveNotes {
    /// No notes sounding.
    pub const fn new() -> Self {
        Self { channels: [0; 16] }
    }

    /// Updates the set for a message about to be sent. As in MIDI, a note-on
    /// with velocity 0 counts as a note-off.
    pub fn observe(&mut self, message: MidiMessage) {
        match message {
            MidiMessage::NoteOn {
                channel,
                note,
                velocity,
            } if velocity != U7::MIN => {
                self.channels[usize::from(channel.index())] |= bit(note);
            }
            MidiMessage::NoteOn { channel, note, .. }
            | MidiMessage::NoteOff { channel, note, .. } => {
                self.channels[usize::from(channel.index())] &= !bit(note);
            }
            _ => {}
        }
    }

    /// Whether `note` is sounding on `channel`.
    pub fn is_sounding(&self, channel: Channel, note: U7) -> bool {
        self.channels[usize::from(channel.index())] & bit(note) != 0
    }

    /// Whether nothing is sounding.
    pub fn is_empty(&self) -> bool {
        self.channels.iter().all(|&bits| bits == 0)
    }

    /// A note-off for every sounding note. Each note leaves the set as its
    /// note-off is yielded, so stopping early keeps the rest. Doesn't allocate.
    pub fn drain_note_offs(&mut self) -> impl Iterator<Item = MidiMessage> {
        (0u8..)
            .zip(self.channels.iter_mut())
            .flat_map(|(index, bits)| {
                let channel = Channel::from_index(index).expect("there are 16 channels");
                note_offs(channel, bits)
            })
    }

    /// A note-off for every note sounding on `channel`, leaving the other
    /// channels alone. Like [`drain_note_offs`](Self::drain_note_offs).
    pub fn drain_channel(&mut self, channel: Channel) -> impl Iterator<Item = MidiMessage> {
        note_offs(channel, &mut self.channels[usize::from(channel.index())])
    }
}

/// Active MIDI keys and the track that last started each one, with each
/// channel's pedals, bend and pressure. Storage is fixed by MIDI's 16
/// channels and 128 notes, regardless of track count.
#[derive(Clone)]
pub(crate) struct OwnedNotes {
    pub(crate) notes: ActiveNotes,
    owners: [[TrackId; 128]; 16],
    /// Channels with sustain or sostenuto down, even after all keys end.
    pedals: [u16; 2],
    /// Channels with pitch bend off centre, and with channel pressure
    /// above zero.
    expression: [u16; 2],
    /// The track that last sent each channel's pitch bend, and its
    /// pressure.
    expressed_by: [[TrackId; 16]; 2],
}

/// Channel-wide expression that outlasts the notes: pitch bend and channel
/// pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Expression {
    /// Pitch bend, at rest in the centre.
    Bend,
    /// Channel pressure, at rest at zero.
    Pressure,
}

impl Expression {
    pub(crate) const ALL: [Self; 2] = [Self::Bend, Self::Pressure];

    /// The message that puts it back at rest on `channel`.
    pub(crate) const fn rest(self, channel: Channel) -> MidiMessage {
        match self {
            Self::Bend => MidiMessage::PitchBend {
                channel,
                value: U14::CENTER,
            },
            Self::Pressure => MidiMessage::ChannelPressure {
                channel,
                pressure: U7::MIN,
            },
        }
    }

    /// What `message` does to it: on which channel, and whether it leaves
    /// it off rest.
    const fn of(message: MidiMessage) -> Option<(Self, Channel, bool)> {
        match message {
            MidiMessage::PitchBend { channel, value } => {
                Some((Self::Bend, channel, value.get() != U14::CENTER.get()))
            }
            MidiMessage::ChannelPressure { channel, pressure } => {
                Some((Self::Pressure, channel, pressure.get() != 0))
            }
            _ => None,
        }
    }
}

impl OwnedNotes {
    pub(crate) const fn new() -> Self {
        Self {
            notes: ActiveNotes::new(),
            owners: [[TrackId(0); 128]; 16],
            pedals: [0; 2],
            expression: [0; 2],
            expressed_by: [[TrackId(0); 16]; 2],
        }
    }

    /// Rendered note-ons always carry a track. Engine-wide cleanup has no
    /// owner and only emits note-offs or non-note messages.
    pub(crate) fn observe(&mut self, message: MidiMessage, track: Option<TrackId>) {
        if let MidiMessage::NoteOn {
            channel,
            note,
            velocity,
        } = message
            && velocity != U7::MIN
        {
            self.owners[usize::from(channel.index())][usize::from(note.get())] =
                track.expect("a rendered note-on has a track");
        }
        self.notes.observe(message);
        if let MidiMessage::ControlChange {
            channel,
            controller,
            value,
        } = message
            && let Some(index) = HOLD_PEDALS.iter().position(|&pedal| pedal == controller)
        {
            let bit = 1 << channel.index();
            if value.get() >= 64 {
                self.pedals[index] |= bit;
            } else {
                self.pedals[index] &= !bit;
            }
        }
        if let Some((expression, channel, off_rest)) = Expression::of(message) {
            let index = expression as usize;
            let bit = 1 << channel.index();
            if off_rest {
                self.expression[index] |= bit;
            } else {
                self.expression[index] &= !bit;
            }
            if let Some(track) = track {
                self.expressed_by[index][usize::from(channel.index())] = track;
            }
        }
    }

    pub(crate) fn pedal_is_down(&self, channel: Channel, controller: U7) -> bool {
        HOLD_PEDALS
            .iter()
            .position(|&pedal| pedal == controller)
            .is_some_and(|index| self.pedals[index] & (1 << channel.index()) != 0)
    }

    /// The track that left `channel`'s `expression` off rest, if it is.
    pub(crate) fn off_rest(&self, channel: Channel, expression: Expression) -> Option<TrackId> {
        let index = expression as usize;
        (self.expression[index] & (1 << channel.index()) != 0)
            .then(|| self.expressed_by[index][usize::from(channel.index())])
    }

    pub(crate) fn owns(&self, track: TrackId, channel: Channel, note: U7) -> bool {
        self.notes.is_sounding(channel, note)
            && self.owners[usize::from(channel.index())][usize::from(note.get())] == track
    }

    /// Copies only this track's active keys on its channel for cleanup.
    pub(crate) fn for_track(&self, track: TrackId, channel: Channel) -> ActiveNotes {
        let index = usize::from(channel.index());
        let mut bits = self.notes.channels[index];
        let mut notes = ActiveNotes::new();
        while bits != 0 {
            let note = bits.trailing_zeros() as usize;
            if self.owners[index][note] == track {
                notes.channels[index] |= 1 << note;
            }
            bits &= bits - 1;
        }
        notes
    }
}

const fn bit(note: U7) -> u128 {
    1 << note.get()
}

/// Note-offs for the notes in `bits`, clearing each as it is yielded.
fn note_offs(channel: Channel, bits: &mut u128) -> impl Iterator<Item = MidiMessage> + '_ {
    std::iter::from_fn(move || {
        if *bits == 0 {
            return None;
        }
        let note = bits.trailing_zeros() as u8;
        *bits &= *bits - 1;
        Some(MidiMessage::NoteOff {
            channel,
            note: U7::saturating(note),
            velocity: DEFAULT_RELEASE,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_on(channel: Channel, note: u8) -> MidiMessage {
        MidiMessage::NoteOn {
            channel,
            note: U7::new(note).unwrap(),
            velocity: U7::MAX,
        }
    }

    /// A note-off as the engine sends one to end a note itself.
    fn note_off(channel: Channel, note: u8) -> MidiMessage {
        MidiMessage::NoteOff {
            channel,
            note: U7::new(note).unwrap(),
            velocity: DEFAULT_RELEASE,
        }
    }

    #[test]
    fn follows_note_on_and_off() {
        let mut notes = ActiveNotes::new();
        let kick = U7::new(36).unwrap();
        notes.observe(note_on(Channel::DRUMS, 36));
        assert!(notes.is_sounding(Channel::DRUMS, kick));

        notes.observe(MidiMessage::NoteOn {
            channel: Channel::DRUMS,
            note: kick,
            velocity: U7::MIN,
        });
        assert!(notes.is_empty());
    }

    #[test]
    fn drains_a_note_off_for_every_sounding_note() {
        let bass = Channel::from_number(1).unwrap();
        let mut notes = ActiveNotes::new();
        for message in [
            note_on(Channel::DRUMS, 127),
            note_on(bass, 40),
            note_on(Channel::DRUMS, 36),
        ] {
            notes.observe(message);
        }

        let offs: Vec<_> = notes.drain_note_offs().collect();
        assert_eq!(
            offs,
            [
                note_off(bass, 40),
                note_off(Channel::DRUMS, 36),
                note_off(Channel::DRUMS, 127),
            ]
        );
        assert!(notes.is_empty());
    }

    #[test]
    fn drains_one_channel_and_keeps_the_others() {
        let bass = Channel::from_number(1).unwrap();
        let mut notes = ActiveNotes::new();
        for message in [
            note_on(Channel::DRUMS, 36),
            note_on(bass, 40),
            note_on(bass, 43),
        ] {
            notes.observe(message);
        }
        let offs: Vec<_> = notes.drain_channel(bass).collect();
        assert_eq!(offs, [note_off(bass, 40), note_off(bass, 43)]);
        assert!(notes.is_sounding(Channel::DRUMS, U7::new(36).unwrap()));
        assert!(!notes.is_sounding(bass, U7::new(40).unwrap()));
    }

    #[test]
    fn follows_pitch_bend_and_pressure_and_who_left_them_off_rest() {
        let lead = Channel::from_number(1).unwrap();
        let bend = |value| MidiMessage::PitchBend {
            channel: lead,
            value: U14::new(value).unwrap(),
        };
        let press = |pressure| MidiMessage::ChannelPressure {
            channel: lead,
            pressure: U7::new(pressure).unwrap(),
        };
        let mut notes = OwnedNotes::new();
        for expression in Expression::ALL {
            assert_eq!(notes.off_rest(lead, expression), None);
        }
        notes.observe(bend(12_000), Some(TrackId(3)));
        notes.observe(press(1), Some(TrackId(4)));
        assert_eq!(notes.off_rest(lead, Expression::Bend), Some(TrackId(3)));
        assert_eq!(notes.off_rest(lead, Expression::Pressure), Some(TrackId(4)));
        assert_eq!(notes.off_rest(Channel::DRUMS, Expression::Bend), None);
        // The last track to send it owns it, even within the same value.
        notes.observe(bend(0), Some(TrackId(5)));
        assert_eq!(notes.off_rest(lead, Expression::Bend), Some(TrackId(5)));
        // Back at rest, by anyone, it's no one's.
        notes.observe(Expression::Bend.rest(lead), None);
        notes.observe(press(0), Some(TrackId(4)));
        for expression in Expression::ALL {
            assert_eq!(notes.off_rest(lead, expression), None);
        }
        assert!(notes.notes.is_empty());
    }

    #[test]
    fn engaging_messages_are_the_ones_cleanup_ends() {
        let channel = Channel::DRUMS;
        let control = |controller, value| MidiMessage::ControlChange {
            channel,
            controller: U7::new(controller).unwrap(),
            value: U7::new(value).unwrap(),
        };
        for (message, engages_it) in [
            (note_on(channel, 36), true),
            (note_off(channel, 36), false),
            (
                MidiMessage::NoteOn {
                    channel,
                    note: U7::MIN,
                    velocity: U7::MIN,
                },
                false,
            ),
            (control(64, 64), true),
            (control(66, 127), true),
            (control(64, 63), false),
            (control(1, 127), false),
            (Expression::Bend.rest(channel), false),
            (
                MidiMessage::PitchBend {
                    channel,
                    value: U14::MIN,
                },
                true,
            ),
            (Expression::Pressure.rest(channel), false),
            (
                MidiMessage::ChannelPressure {
                    channel,
                    pressure: U7::MAX,
                },
                true,
            ),
            (MidiMessage::TimingClock, false),
        ] {
            assert_eq!(engages(message), engages_it, "{message:?}");
        }
    }

    #[test]
    fn ownership_selects_only_active_notes_of_the_requested_track_and_channel() {
        let first = TrackId(0);
        let second = TrackId(u32::MAX);
        let other = Channel::from_number(1).unwrap();
        let mut notes = OwnedNotes::new();
        notes.observe(note_on(Channel::DRUMS, 60), Some(first));
        notes.observe(note_on(Channel::DRUMS, 62), Some(second));
        notes.observe(note_on(other, 64), Some(first));
        let mut selected = notes.for_track(first, Channel::DRUMS);
        assert_eq!(
            selected.drain_note_offs().collect::<Vec<_>>(),
            [note_off(Channel::DRUMS, 60)]
        );
        assert!(notes.owns(second, Channel::DRUMS, U7::saturating(62)));
        assert!(notes.owns(first, other, U7::saturating(64)));

        // A retrigger transfers ownership. Cleanup or a velocity-zero on
        // clears activity even though the fixed table retains the old ID.
        notes.observe(note_on(Channel::DRUMS, 60), Some(second));
        assert!(notes.for_track(first, Channel::DRUMS).is_empty());
        notes.observe(note_off(Channel::DRUMS, 60), None);
        notes.observe(
            MidiMessage::NoteOn {
                channel: Channel::DRUMS,
                note: U7::saturating(62),
                velocity: U7::MIN,
            },
            Some(second),
        );
        assert!(notes.for_track(second, Channel::DRUMS).is_empty());
        assert!(!notes.owns(second, Channel::DRUMS, U7::saturating(60)));
    }
}
