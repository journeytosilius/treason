//! Keeps track of sounding notes so the engine can always end them.

use treason_core::midi::{Channel, MidiMessage, U7};

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
            velocity: U7::MIN,
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

    fn note_off(channel: Channel, note: u8) -> MidiMessage {
        MidiMessage::NoteOff {
            channel,
            note: U7::new(note).unwrap(),
            velocity: U7::MIN,
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
}
