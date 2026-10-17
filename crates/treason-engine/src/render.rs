//! Turning clips into MIDI events.

use std::collections::{BTreeMap, BTreeSet};
use std::mem;
use std::ops::Range;

use thiserror::Error;
use treason_core::midi::{Channel, MidiMessage, U7};
use treason_core::model::{DEFAULT_RELEASE, EventKind, PortId, Program, Project, Track, TrackId};
use treason_core::time::{TICKS_PER_CLOCK_PULSE, Ticks};

use crate::notes::{self, Expression, HOLD_PEDALS};

/// Bank select, the most and least significant bytes. A program change on
/// the same tick uses them, so they go out before it.
const BANK_SELECT: [u8; 2] = [0, 32];

/// Where a channel's bend range goes in its setup: after the bank selects
/// and the program change, since a program change can reset it.
const BEND_RANGE_SLOT: usize = 3;

/// A message to send at a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderedEvent {
    /// When, in song ticks.
    pub tick: Ticks,
    /// Where.
    pub port: PortId,
    /// The track that owns this event, stable across snapshot reordering.
    /// `None` for MIDI clock.
    pub track: Option<TrackId>,
    /// What.
    pub message: MidiMessage,
    /// Where it goes among the events on its tick.
    rank: Rank,
    /// The order it was rendered in: by track, then as written in the clip;
    /// for a clock pulse, by port; and for a channel's setup, by port,
    /// channel and message. The last slot orders the messages a clip event
    /// sends. Breaks ties without a sort that allocates.
    sequence: (usize, u64, usize, usize),
}

impl RenderedEvent {
    /// Whether it sets up its channel, as part of a track's program or bend
    /// range, including the range restored after a clip program change.
    pub fn is_setup(&self) -> bool {
        self.rank == Rank::Setup
            || (self.rank == Rank::Program
                && matches!(self.message, MidiMessage::ControlChange { .. }))
    }
}

/// Where an event goes among those on its tick, before the order they were
/// rendered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    /// MIDI clock, so the tick starts on time.
    Clock,
    /// Note-offs, so a retriggered note isn't cut short.
    NoteOff,
    /// The tracks' channel setup, so the clips can build on it and their
    /// bends use the new range.
    Setup,
    /// The clips' bank selects, before the program changes they're for.
    BankSelect,
    /// The clips' program changes, each followed by its channel's bend
    /// range when configured, since the program may reset it.
    Program,
    /// Other control changes, pitch bend and channel pressure, so a note
    /// plays with the values set on its tick.
    Control,
    /// Note-ons.
    NoteOn,
    /// Key pressure, so a note's pressure on its first tick reaches a key
    /// that is down.
    NotePressure,
}

impl Rank {
    /// Where `message` goes on its tick, from a clip or the clock.
    fn of(message: MidiMessage) -> Self {
        match message {
            MidiMessage::TimingClock => Self::Clock,
            MidiMessage::NoteOff { .. } => Self::NoteOff,
            MidiMessage::ControlChange { controller, .. }
                if BANK_SELECT.contains(&controller.get()) =>
            {
                Self::BankSelect
            }
            MidiMessage::ProgramChange { .. } => Self::Program,
            MidiMessage::ControlChange { .. }
            | MidiMessage::PitchBend { .. }
            | MidiMessage::ChannelPressure { .. } => Self::Control,
            MidiMessage::PolyPressure { .. } => Self::NotePressure,
            _ => Self::NoteOn,
        }
    }
}

/// Where the tracks' channel setup goes out, along with a project's clips:
/// each channel's program and bend range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Setup<'a> {
    /// The tick: where playback starts, or where a new snapshot takes over.
    pub at: Ticks,
    /// The snapshot playing until then. Only the setup it doesn't already
    /// send on a port and channel goes out there. With none, all of it does.
    pub since: Option<&'a Project>,
}

/// MIDI clock to render along with a project's clips.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockPulses<'a> {
    /// The ports that get a pulse every 40 ticks, counting from tick 0.
    pub ports: &'a [PortId],
    /// The first tick that can get a pulse. Earlier pulses are left out, so
    /// that the receivers wait where a Song Position Pointer put them.
    pub from: Ticks,
}

/// Appends to `out` everything `project` plays in `window`, along with the
/// pulses of `clock` and the tracks' channel `setup`, in send order.
///
/// Every unmuted track loops its clip from tick 0, so an event at clip
/// position `p` plays at `p + k × length` for every `k`. A note's note-off
/// lands at its start plus its length, which may be in a later window: each
/// window holds the note-offs that fall inside it. Notes with velocity 0
/// play nothing.
///
/// A track's setup goes out at `setup.at`, if `window` holds it, muted or
/// not: its program, as bank selects and a program change, then its bend
/// range, as RPN 0. Tracks that share a port and channel share its setup,
/// so only the first track that sets each part sends it.
///
/// Events are ordered by tick. At the same tick, clock pulses come first, so
/// the tick starts on time, then note-offs, the tracks' channel setup, and
/// then the clips' bank selects (controllers 0 and 32), program changes,
/// other control changes with pitch bend and channel pressure, note-ons and
/// key pressure. A retriggered note isn't cut short, the clips build on the
/// setup, a program change gets the bank selected with it, a note plays
/// with the program, controller and bend values set on its tick, and its
/// key is down before its pressure arrives. A note-off goes out with the
/// note's release velocity. Within each of
/// those groups, events keep their order, by track and then as written in
/// the clip, so a sequence such as an RPN's parameter selection and data
/// entry arrives as written. Each clip program change restores its
/// channel's configured bend range before the clip's own controls, so an
/// explicit RPN can override it. The setup goes channel by channel.
///
/// `out` is reused from window to window. This unbounded entry point may
/// allocate if its remaining capacity cannot hold the whole window.
pub fn render(
    project: &Project,
    clock: ClockPulses<'_>,
    setup: Option<Setup<'_>>,
    window: Range<Ticks>,
    out: &mut Vec<RenderedEvent>,
) {
    let first = out.len();
    visit(project, clock, setup, None, window, |event| out.push(event));
    out[first..].sort_unstable_by_key(send_order);
}

/// A song with more events than [`render_song`] may give.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("the song holds {events} MIDI events, more than the limit of {limit}")]
pub struct SongTooLarge {
    /// How many events the song holds.
    pub events: u64,
    /// The most it may hold.
    pub limit: u64,
}

/// Everything `project` plays from tick 0 until `end`, for a file rather
/// than a port: [`render`]'s events for that span, with every track's
/// setup at tick 0 and without MIDI clock, then a note-off at `end` for
/// each note still sounding there, by track, then
/// channel and note. Every note-on gets its own note-off, even where a
/// track starts a note that is already sounding. Active sustain and
/// sostenuto pedals are released after those note-offs, including when
/// earlier note-offs have already ended all keys, and pitch bend left off
/// centre, or pressure above zero, goes back to rest.
///
/// Counts the events first, and renders nothing if there would be more
/// than `limit`.
pub fn render_song(
    project: &Project,
    end: Ticks,
    limit: u64,
) -> Result<Vec<RenderedEvent>, SongTooLarge> {
    let (events, releases) = song_size(project, end);
    if events > limit {
        return Err(SongTooLarge { events, limit });
    }
    // Only a hint, so a count that doesn't fit leaves it to the vector.
    let mut out = Vec::with_capacity(usize::try_from(events).unwrap_or(0));
    let setup = Some(Setup {
        at: Ticks::ZERO,
        since: None,
    });
    render(
        project,
        ClockPulses::default(),
        setup,
        Ticks::ZERO..end,
        &mut out,
    );

    // How many times each track's notes are sounding. A note-off ends one
    // of them, so a note started twice needs two.
    let mut sounding = BTreeMap::<(usize, Channel, U7), u32>::new();
    for event in &out {
        let track = event.sequence.0;
        match event.message {
            MidiMessage::NoteOn {
                channel,
                note,
                velocity,
            } if velocity != U7::MIN => *sounding.entry((track, channel, note)).or_default() += 1,
            MidiMessage::NoteOn { channel, note, .. }
            | MidiMessage::NoteOff { channel, note, .. } => {
                if let Some(count) = sounding.get_mut(&(track, channel, note)) {
                    *count = count.saturating_sub(1);
                }
            }
            _ => {}
        }
    }
    for ((index, channel, note), count) in sounding {
        let track = &project.tracks[index];
        let off = RenderedEvent {
            tick: end,
            port: track.target.port,
            track: Some(track.id),
            message: MidiMessage::NoteOff {
                channel,
                note,
                velocity: DEFAULT_RELEASE,
            },
            rank: Rank::NoteOff,
            // After every pass, as the last word on each note.
            sequence: (index, u64::MAX, usize::from(note.get()), 0),
        };
        out.extend(std::iter::repeat_n(off, count as usize));
    }
    out.extend(releases);
    Ok(out)
}

/// Channel state that outlasts the notes, which a song puts back at rest
/// at its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Held {
    /// A hold pedal, by controller.
    Pedal(U7),
    /// Pitch bend or channel pressure.
    Expression(Expression),
}

/// The state that outlasts the notes which `message` changes, if any, and
/// the message that puts it back at rest.
fn holds(message: MidiMessage) -> Option<(Held, MidiMessage)> {
    match message {
        MidiMessage::ControlChange {
            channel,
            controller,
            ..
        } if HOLD_PEDALS.contains(&controller) => {
            let release = MidiMessage::ControlChange {
                channel,
                controller,
                value: U7::MIN,
            };
            Some((Held::Pedal(controller), release))
        }
        MidiMessage::PitchBend { channel, .. } => Some((
            Held::Expression(Expression::Bend),
            Expression::Bend.rest(channel),
        )),
        MidiMessage::ChannelPressure { channel, .. } => Some((
            Held::Expression(Expression::Pressure),
            Expression::Pressure.rest(channel),
        )),
        _ => None,
    }
}

/// How many events [`render_song`] gives for `project` until `end`: the
/// setup at tick 0, two for each note, its note-on and note-off, and one
/// for each other event, plus any bend range restored by a clip program,
/// for every pass that starts the event before `end`, plus the releases of
/// held pedals, bends and pressure at the end, returned alongside the count.
fn song_size(project: &Project, end: Ticks) -> (u64, Vec<RenderedEvent>) {
    let mut events = 0_u64;
    // Only the last change to each pedal, bend and pressure on each port
    // and channel matters. Find it arithmetically so even an oversized song
    // needn't be rendered first. The map holds at most four entries per
    // MIDI output target.
    let mut held = BTreeMap::<(PortId, Channel, Held), RenderedEvent>::new();
    if end != Ticks::ZERO {
        let setup = Setup {
            at: Ticks::ZERO,
            since: None,
        };
        visit_setup(project, setup, None, &mut |_| events += 1);
    }
    for (index, track) in project.tracks.iter().enumerate() {
        if track.muted {
            continue;
        }
        let length = track.clip.length().0;
        let channel = track.target.channel;
        let range_messages = channel_bend_range(project, track).map_or(0, |range| {
            bend_range_messages(channel, range).count() as u64
        });
        for (event_index, event) in track.clip.events().iter().enumerate() {
            let messages = match event.kind {
                EventKind::Note { velocity, .. } if velocity == U7::MIN => 0,
                EventKind::Note { .. } => 2,
                EventKind::Program { .. } => 1 + range_messages,
                _ => 1,
            };
            let passes = end.0.saturating_sub(event.start.0).div_ceil(length);
            events = events.saturating_add(passes.saturating_mul(messages));
            let Some(message) = channel_message(event.kind, channel).filter(|_| passes != 0) else {
                continue;
            };
            let Some((state, _)) = holds(message) else {
                continue;
            };
            let last = RenderedEvent {
                tick: Ticks(event.start.0 + (passes - 1) * length),
                port: track.target.port,
                track: Some(track.id),
                message,
                rank: Rank::of(message),
                sequence: (index, passes - 1, event_index, 0),
            };
            let previous = held
                .entry((track.target.port, channel, state))
                .or_insert(last);
            // Different tracks can share a channel. Respect the live
            // renderer's order, including changes on the same tick.
            if send_order(&last) > send_order(previous) {
                *previous = last;
            }
        }
    }
    let mut releases = Vec::new();
    for mut event in held.into_values() {
        if notes::engages(event.message)
            && let Some((_, rest)) = holds(event.message)
        {
            event.message = rest;
            event.tick = end;
            releases.push(event);
        }
    }
    releases.sort_unstable_by_key(send_order);
    (events.saturating_add(releases.len() as u64), releases)
}

/// Appends the earliest events for `port` that fit in the remaining
/// capacity, without allocating: [`render`]'s events on that port, in the
/// same order. If the window did not fit, returns its last appended event:
/// resume at that tick, passing it as `after`, to preserve even a split tick.
/// The caller must provide at least one free slot.
pub(crate) fn render_bounded(
    project: &Project,
    clock: ClockPulses<'_>,
    setup: Option<Setup<'_>>,
    port: PortId,
    window: Range<Ticks>,
    after: Option<RenderedEvent>,
    out: &mut Vec<RenderedEvent>,
) -> Option<RenderedEvent> {
    assert!(out.len() < out.capacity());
    let first = out.len();
    let mut overflow = false;
    visit(project, clock, setup, Some(port), window, |event| {
        if after.is_some_and(|after| send_order(&event) <= send_order(&after)) {
            return;
        }
        // A max-heap in the new portion of the existing buffer retains the
        // earliest events even though tracks are visited in track order.
        if out.len() < out.capacity() {
            out.push(event);
            let heap = &mut out[first..];
            let mut child = heap.len() - 1;
            while child > 0 {
                let parent = (child - 1) / 2;
                if send_order(&heap[parent]) >= send_order(&heap[child]) {
                    break;
                }
                heap.swap(parent, child);
                child = parent;
            }
        } else {
            overflow = true;
            let heap = &mut out[first..];
            if send_order(&event) < send_order(&heap[0]) {
                heap[0] = event;
                let mut parent = 0;
                while parent * 2 + 1 < heap.len() {
                    let mut child = parent * 2 + 1;
                    if child + 1 < heap.len()
                        && send_order(&heap[child + 1]) > send_order(&heap[child])
                    {
                        child += 1;
                    }
                    if send_order(&heap[parent]) >= send_order(&heap[child]) {
                        break;
                    }
                    heap.swap(parent, child);
                    parent = child;
                }
            }
        }
    });
    out[first..].sort_unstable_by_key(send_order);
    overflow.then(|| *out.last().unwrap())
}

/// Visits the events in `window`, unsorted: all of them, or only `port`'s.
fn visit(
    project: &Project,
    clock: ClockPulses<'_>,
    setup: Option<Setup<'_>>,
    port: Option<PortId>,
    window: Range<Ticks>,
    mut emit: impl FnMut(RenderedEvent),
) {
    if window.start >= window.end {
        return;
    }
    let wanted = |target: PortId| port.is_none_or(|port| port == target);
    if let Some(setup) = setup.filter(|setup| window.contains(&setup.at)) {
        visit_setup(project, setup, port, &mut emit);
    }
    for tick in clock_pulses(window.start.max(clock.from)..window.end) {
        for &port in clock.ports.iter().filter(|&&port| wanted(port)) {
            emit(RenderedEvent {
                tick,
                port,
                track: None,
                message: MidiMessage::TimingClock,
                rank: Rank::Clock,
                // Pulses rank on their own, so this only orders their ports.
                sequence: (0, 0, usize::from(port.0), 0),
            });
        }
    }
    for (index, track) in project.tracks.iter().enumerate() {
        if !track.muted && wanted(track.target.port) {
            // Looked up only for a clip program change, so the tracks
            // without one cost no pass over the others.
            let bend_range = || channel_bend_range(project, track);
            render_track(index, track, bend_range, window.clone(), &mut emit);
        }
    }
}

/// Visits the setup that goes out at `setup.at`, on `port` or on every
/// port: on each channel, the program of the first track there that sets
/// one, then the bend range of the first that sets one, each unless
/// `setup.since` already sets it there. Tracks on a channel agree on its
/// setup, so the first says. Since a program change can reset a device's
/// bend range, a bend range goes out again after a new program. For one
/// port it takes two passes over the tracks and allocates nothing, for the
/// engine thread.
fn visit_setup(
    project: &Project,
    setup: Setup<'_>,
    port: Option<PortId>,
    emit: &mut impl FnMut(RenderedEvent),
) {
    let Some(port) = port else {
        // Port by port. Only files and tests render every port at once, so
        // this may allocate.
        let ports: BTreeSet<_> = project
            .tracks
            .iter()
            .map(|track| track.target.port)
            .collect();
        for port in ports {
            visit_setup(project, setup, Some(port), emit);
        }
        return;
    };
    let mut playing = [(None, None); 16];
    for old in setup.since.into_iter().flat_map(|since| &since.tracks) {
        if old.target.port == port {
            let (program, range) = &mut playing[usize::from(old.target.channel.index())];
            *program = program.or(old.program);
            *range = range.or(old.bend_range);
        }
    }
    let tracks = || {
        project
            .tracks
            .iter()
            .filter(move |track| track.target.port == port)
    };
    let mut send = |track: &Track, (slot, message)| {
        emit(RenderedEvent {
            tick: setup.at,
            port,
            track: Some(track.id),
            message,
            rank: Rank::Setup,
            sequence: (
                usize::from(port.0),
                u64::from(track.target.channel.index()),
                slot,
                0,
            ),
        });
    };
    let (mut set, mut changed) = ([false; 16], [false; 16]);
    for track in tracks() {
        let Some(program) = track.program else {
            continue;
        };
        let channel = usize::from(track.target.channel.index());
        if mem::replace(&mut set[channel], true) || playing[channel].0 == Some(program) {
            continue;
        }
        changed[channel] = true;
        for message in program_messages(track.target.channel, program) {
            send(track, message);
        }
    }
    let mut set = [false; 16];
    for track in tracks() {
        let Some(range) = track.bend_range else {
            continue;
        };
        let channel = usize::from(track.target.channel.index());
        if mem::replace(&mut set[channel], true)
            || (playing[channel].1 == Some(range) && !changed[channel])
        {
            continue;
        }
        for message in bend_range_messages(track.target.channel, range) {
            send(track, message);
        }
    }
}

/// The bank selects and program change that set `program` on `channel`,
/// each with its place in the channel's setup.
fn program_messages(
    channel: Channel,
    program: Program,
) -> impl Iterator<Item = (usize, MidiMessage)> {
    let bytes = [program.bank_msb, program.bank_lsb];
    let banks = BANK_SELECT.into_iter().zip(bytes).enumerate().filter_map(
        move |(slot, (controller, value))| {
            let message = MidiMessage::ControlChange {
                channel,
                controller: U7::saturating(controller),
                value: value?,
            };
            Some((slot, message))
        },
    );
    let change = MidiMessage::ProgramChange {
        channel,
        program: program.number,
    };
    banks.chain([(BANK_SELECT.len(), change)])
}

/// The messages that set `channel`'s bend range to `semitones`, each with
/// its place in the channel's setup: RPN 0, pitch bend sensitivity,
/// selected with controllers 101 and 100, then set with data entry,
/// semitones (6) and cents (38). Selecting the null RPN afterwards keeps
/// later data entry from changing it.
fn bend_range_messages(
    channel: Channel,
    semitones: U7,
) -> impl Iterator<Item = (usize, MidiMessage)> {
    let changes = [
        (101, 0),
        (100, 0),
        (6, semitones.get()),
        (38, 0),
        (101, 127),
        (100, 127),
    ];
    (BEND_RANGE_SLOT..)
        .zip(changes)
        .map(move |(slot, (controller, value))| {
            let message = MidiMessage::ControlChange {
                channel,
                controller: U7::saturating(controller),
                value: U7::saturating(value),
            };
            (slot, message)
        })
}

/// The message a clip event other than a note sends on `channel`.
fn channel_message(kind: EventKind, channel: Channel) -> Option<MidiMessage> {
    match kind {
        EventKind::Note { .. } => None,
        EventKind::Control { controller, value } => Some(MidiMessage::ControlChange {
            channel,
            controller,
            value,
        }),
        EventKind::Program { program } => Some(MidiMessage::ProgramChange { channel, program }),
        EventKind::PitchBend { value } => Some(MidiMessage::PitchBend { channel, value }),
        EventKind::Pressure { value } => Some(MidiMessage::ChannelPressure {
            channel,
            pressure: value,
        }),
        EventKind::NotePressure { note, value } => Some(MidiMessage::PolyPressure {
            channel,
            note,
            pressure: value,
        }),
    }
}

/// The range configured on a track's port and channel, even by a muted
/// track. Tracks sharing the target agree on it, as in [`visit_setup`].
fn channel_bend_range(project: &Project, track: &Track) -> Option<U7> {
    project
        .tracks
        .iter()
        .filter(|other| other.target == track.target)
        .find_map(|other| other.bend_range)
}

/// The ticks in `window` that get a MIDI clock pulse: every 40th tick,
/// counting from 0, which is 24 pulses per quarter note.
pub fn clock_pulses(window: Range<Ticks>) -> impl Iterator<Item = Ticks> {
    let first = window.start.0.div_ceil(TICKS_PER_CLOCK_PULSE);
    (first..)
        .map(|pulse| Ticks(pulse * TICKS_PER_CLOCK_PULSE))
        .take_while(move |tick| *tick < window.end)
}

/// Visits one track's events in a non-empty `window`, unsorted. A clip
/// program change is followed by the channel's `bend_range`, if it has one.
fn render_track(
    track_index: usize,
    track: &Track,
    bend_range: impl Fn() -> Option<U7>,
    window: Range<Ticks>,
    mut emit: impl FnMut(RenderedEvent),
) {
    let clip = &track.clip;
    if clip.events().is_empty() {
        return;
    }
    let (start, end) = (window.start.0, window.end.0);
    let loop_length = clip.length().0;
    let port = track.target.port;
    let channel = track.target.channel;
    // A note that ends in the window started at most the longest note
    // earlier, and no note is longer than the loop, so this looks back at
    // most one pass.
    let from = start.saturating_sub(clip.longest_note().0);
    for pass in from / loop_length..=(end - 1) / loop_length {
        let offset = pass * loop_length;
        let span = Ticks(from.max(offset) - offset)..Ticks(end.min(offset + loop_length) - offset);
        let first = clip
            .events()
            .partition_point(|event| event.start < span.start);
        for (index, event) in clip.events_in(span).iter().enumerate() {
            // Stable across window boundaries and partial batches, including
            // note-offs from the previous pass through a looping clip.
            let mut emit = |tick, message, slot| {
                emit(RenderedEvent {
                    tick: Ticks(tick),
                    port,
                    track: Some(track.id),
                    message,
                    // Extra messages restore the bend range as part of
                    // the clip program, ahead of explicit clip controls.
                    rank: if slot == 0 {
                        Rank::of(message)
                    } else {
                        Rank::Program
                    },
                    sequence: (track_index, pass, first + index, slot),
                });
            };
            let at = offset + event.start.0;
            match event.kind {
                EventKind::Note { velocity, .. } if velocity == U7::MIN => {}
                EventKind::Note {
                    note,
                    velocity,
                    length,
                    release,
                } => {
                    if at >= start {
                        let on = MidiMessage::NoteOn {
                            channel,
                            note,
                            velocity,
                        };
                        emit(at, on, 0);
                    }
                    let off_at = at + length.0;
                    if (start..end).contains(&off_at) {
                        let off = MidiMessage::NoteOff {
                            channel,
                            note,
                            velocity: release,
                        };
                        emit(off_at, off, 0);
                    }
                }
                kind => {
                    // Before the window, it was already sent.
                    if let Some(message) = channel_message(kind, channel).filter(|_| at >= start) {
                        emit(at, message, 0);
                        if matches!(kind, EventKind::Program { .. })
                            && let Some(range) = bend_range()
                        {
                            for (slot, message) in bend_range_messages(channel, range) {
                                emit(at, message, slot);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Where an event goes in the send order: by tick, then by [`Rank`], then
/// in the order they were rendered. No two events tie, so an unstable sort
/// keeps that order.
fn send_order(event: &RenderedEvent) -> (Ticks, Rank, (usize, u64, usize, usize)) {
    (event.tick, event.rank, event.sequence)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use treason_core::midi::{Channel, U14};
    use treason_core::model::{Clip, ClipEvent, OutputTarget, Program, TrackId};

    use super::*;

    const NO_CLOCK: ClockPulses<'static> = ClockPulses {
        ports: &[],
        from: Ticks::ZERO,
    };

    fn note(start: u64, note: u8, length: u64) -> ClipEvent {
        ClipEvent {
            start: Ticks(start),
            kind: EventKind::Note {
                note: U7::new(note).unwrap(),
                velocity: U7::MAX,
                length: Ticks(length),
                release: DEFAULT_RELEASE,
            },
        }
    }

    fn control(start: u64, controller: u8, value: u8) -> ClipEvent {
        ClipEvent {
            start: Ticks(start),
            kind: EventKind::Control {
                controller: U7::new(controller).unwrap(),
                value: U7::new(value).unwrap(),
            },
        }
    }

    fn program(start: u64, program: u8) -> ClipEvent {
        ClipEvent {
            start: Ticks(start),
            kind: EventKind::Program {
                program: U7::new(program).unwrap(),
            },
        }
    }

    fn bend(start: u64, value: u16) -> ClipEvent {
        ClipEvent {
            start: Ticks(start),
            kind: EventKind::PitchBend {
                value: U14::new(value).unwrap(),
            },
        }
    }

    fn pressure(start: u64, value: u8) -> ClipEvent {
        ClipEvent {
            start: Ticks(start),
            kind: EventKind::Pressure {
                value: U7::new(value).unwrap(),
            },
        }
    }

    fn track(id: u32, length: u64, events: Vec<ClipEvent>) -> Track {
        Track {
            id: TrackId(id),
            name: format!("track {id}"),
            device: None,
            target: OutputTarget {
                port: PortId(0),
                channel: Channel::DRUMS,
            },
            muted: false,
            program: None,
            bend_range: None,
            clip: Clip::new(Ticks(length), events).unwrap(),
        }
    }

    fn project(tracks: Vec<Track>) -> Project {
        Project {
            tracks: tracks.into_iter().map(Arc::new).collect(),
            ..Project::default()
        }
    }

    /// What plays in `window`, one line per event, such as "240 off 36".
    fn play(project: &Project, window: Range<u64>) -> Vec<String> {
        play_clocked(project, NO_CLOCK, window)
    }

    /// What plays in `window` with `clock`, one line per event. Pulses
    /// show their port, such as "40 clock 1".
    fn play_clocked(project: &Project, clock: ClockPulses<'_>, window: Range<u64>) -> Vec<String> {
        let mut out = Vec::new();
        let window = Ticks(window.start)..Ticks(window.end);
        render(project, clock, None, window, &mut out);
        describe(&out)
    }

    /// What plays in `window` with `setup`, one line per event with its
    /// channel, such as "0 ch2 pc 5".
    fn play_setup(project: &Project, setup: Setup<'_>, window: Range<u64>) -> Vec<String> {
        let mut out = Vec::new();
        let window = Ticks(window.start)..Ticks(window.end);
        render(project, NO_CLOCK, Some(setup), window, &mut out);
        out.iter()
            .zip(describe(&out))
            .map(|(event, line)| {
                let channel = event.message.channel().map_or(0, Channel::number);
                let (tick, what) = line.split_once(' ').unwrap();
                format!("{tick} ch{channel} {what}")
            })
            .collect()
    }

    /// A program, with bank select bytes if given.
    fn sound(number: u8, bank_msb: Option<u8>, bank_lsb: Option<u8>) -> Option<Program> {
        Some(Program {
            number: U7::new(number).unwrap(),
            bank_msb: bank_msb.and_then(U7::new),
            bank_lsb: bank_lsb.and_then(U7::new),
        })
    }

    /// `track` on `channel` of port 0, with `program`.
    fn on(channel: u8, program: Option<Program>, track: Track) -> Track {
        Track {
            target: OutputTarget {
                port: PortId(0),
                channel: Channel::from_number(channel).unwrap(),
            },
            program,
            ..track
        }
    }

    /// One line per event, such as "240 off 36".
    fn describe(events: &[RenderedEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| {
                let what = match event.message {
                    MidiMessage::TimingClock => format!("clock {}", event.port.0),
                    MidiMessage::NoteOn { note, .. } => format!("on {}", note.get()),
                    MidiMessage::NoteOff { note, velocity, .. } if velocity != DEFAULT_RELEASE => {
                        format!("off {} at {}", note.get(), velocity.get())
                    }
                    MidiMessage::NoteOff { note, .. } => format!("off {}", note.get()),
                    MidiMessage::PolyPressure { note, pressure, .. } => {
                        format!("key {}={}", note.get(), pressure.get())
                    }
                    MidiMessage::ControlChange {
                        controller, value, ..
                    } => format!("cc {}={}", controller.get(), value.get()),
                    MidiMessage::ProgramChange { program, .. } => {
                        format!("pc {}", program.get())
                    }
                    MidiMessage::PitchBend { value, .. } => format!("bend {}", value.get()),
                    MidiMessage::ChannelPressure { pressure, .. } => {
                        format!("pressure {}", pressure.get())
                    }
                    other => format!("{other:?}"),
                };
                format!("{} {what}", event.tick.0)
            })
            .collect()
    }

    /// The song `project` plays until `end`, one line per event.
    fn song(project: &Project, end: u64) -> Vec<String> {
        describe(&render_song(project, Ticks(end), u64::MAX).unwrap())
    }

    /// A beat with a kick on the downbeat and a snare on the off-beat, each
    /// a 16th long.
    fn beat() -> Project {
        project(vec![track(
            1,
            960,
            vec![note(480, 38, 240), note(0, 36, 240)],
        )])
    }

    #[test]
    fn bounded_batches_preserve_order_within_ticks_across_tracks_and_loops() {
        let project = project(vec![
            Track {
                program: sound(9, Some(3), Some(4)),
                bend_range: U7::new(12),
                ..track(
                    1,
                    480,
                    vec![
                        note(0, 60, 480),
                        control(0, 7, 90),
                        program(0, 5),
                        control(0, 0, 1),
                        control(0, 32, 2),
                        note(360, 62, 240),
                    ],
                )
            },
            Track {
                target: OutputTarget {
                    port: PortId(1),
                    channel: Channel::DRUMS,
                },
                program: sound(11, None, None),
                ..track(
                    2,
                    240,
                    vec![control(0, 101, 0), control(0, 100, 0), control(0, 6, 12)],
                )
            },
        ]);
        // Pulses on both ports crowd the ticks they share with the clips.
        let clock = ClockPulses {
            ports: &[PortId(0), PortId(1)],
            from: Ticks::ZERO,
        };
        // Programs go out at the first tick, crowding it further.
        let programs = Some(Setup {
            at: Ticks::ZERO,
            since: None,
        });
        let mut everything = Vec::new();
        render(
            &project,
            clock,
            programs,
            Ticks::ZERO..Ticks(2000),
            &mut everything,
        );
        for port in [PortId(0), PortId(1)] {
            let expected: Vec<_> = everything
                .iter()
                .filter(|event| event.port == port)
                .copied()
                .collect();
            for capacity in 1..=7 {
                let mut batch = Vec::with_capacity(capacity);
                let mut actual = Vec::new();
                let mut start = Ticks::ZERO;
                let mut after = None;
                loop {
                    let window = start..Ticks(2000);
                    let next =
                        render_bounded(&project, clock, programs, port, window, after, &mut batch);
                    assert_eq!(batch.capacity(), capacity);
                    actual.extend_from_slice(&batch);
                    batch.clear();
                    let Some(last) = next else { break };
                    assert_ne!(next, after, "rendering must progress");
                    start = last.tick;
                    after = next;
                }
                assert_eq!(actual, expected, "port {port:?}, batch capacity {capacity}");
            }
        }
    }

    #[test]
    fn bounded_rendering_counts_retained_events_against_capacity() {
        let project = beat();
        let mut expected = Vec::new();
        render(
            &project,
            NO_CLOCK,
            None,
            Ticks::ZERO..Ticks(960),
            &mut expected,
        );
        let mut batch = Vec::with_capacity(3);
        assert_eq!(
            render_bounded(
                &project,
                NO_CLOCK,
                None,
                PortId(0),
                Ticks::ZERO..Ticks(241),
                None,
                &mut batch
            ),
            None
        );
        assert_eq!(batch, expected[..2]);
        let window = Ticks(241)..Ticks(960);
        let after = render_bounded(
            &project,
            NO_CLOCK,
            None,
            PortId(0),
            window,
            None,
            &mut batch,
        );
        assert_eq!(batch, expected[..3]);
        assert_eq!(batch.capacity(), 3);
        let last = after.expect("the fourth event must remain unrendered");
        batch.clear();
        assert_eq!(
            render_bounded(
                &project,
                NO_CLOCK,
                None,
                PortId(0),
                last.tick..Ticks(960),
                after,
                &mut batch
            ),
            None
        );
        assert_eq!(batch, expected[3..]);
        assert_eq!(batch.capacity(), 3);
    }

    #[test]
    fn programs_go_out_where_playback_starts_before_the_clip() {
        let lead = on(
            1,
            sound(5, Some(1), Some(2)),
            track(
                1,
                960,
                vec![note(0, 60, 240), control(0, 0, 9), program(0, 6)],
            ),
        );
        // Shares the lead's channel and program, which the lead sends.
        let double = on(1, lead.program, track(2, 960, vec![note(480, 64, 240)]));
        // Muted, but its program still goes out, ready for when it plays.
        let pad = Track {
            muted: true,
            ..on(
                2,
                sound(89, None, None),
                track(3, 960, vec![note(0, 48, 960)]),
            )
        };
        let plain = on(3, None, track(4, 960, vec![note(0, 36, 240)]));
        let project = project(vec![lead, double, pad, plain]);
        let start = |at| Setup {
            at: Ticks(at),
            since: None,
        };
        // Each channel's setup, its bank selects and then its program
        // change, before the clips' own bank selects and program changes,
        // then the notes.
        let first_tick = [
            "0 ch1 cc 0=1",
            "0 ch1 cc 32=2",
            "0 ch1 pc 5",
            "0 ch2 pc 89",
            "0 ch1 cc 0=9",
            "0 ch1 pc 6",
            "0 ch1 on 60",
            "0 ch3 on 36",
        ];
        assert_eq!(play_setup(&project, start(0), 0..1), first_tick);
        // Only the window that holds the start sends them.
        let clip_only = ["0 ch1 cc 0=9", "0 ch1 pc 6", "0 ch1 on 60", "0 ch3 on 36"];
        assert_eq!(play_setup(&project, start(960), 0..1), clip_only);
        let second_bar = play_setup(&project, start(960), 960..961);
        assert_eq!(
            second_bar,
            first_tick.map(|line| line.replacen('0', "960", 1))
        );
    }

    /// The RPN messages that set `channel`'s bend range to `semitones` at
    /// `tick`, as [`play_setup`] shows them.
    fn bend_range(tick: u64, channel: u8, semitones: u8) -> Vec<String> {
        [
            (101, 0),
            (100, 0),
            (6, semitones),
            (38, 0),
            (101, 127),
            (100, 127),
        ]
        .map(|(controller, value)| format!("{tick} ch{channel} cc {controller}={value}"))
        .to_vec()
    }

    /// `track` with a bend range of `semitones`.
    fn ranged(semitones: u8, track: Track) -> Track {
        Track {
            bend_range: U7::new(semitones),
            ..track
        }
    }

    #[test]
    fn bend_ranges_go_out_after_their_channels_program() {
        let lead = ranged(
            12,
            on(
                1,
                sound(5, None, None),
                track(1, 960, vec![note(0, 60, 240), bend(0, 16383)]),
            ),
        );
        // Shares the lead's channel and range, which the lead sends.
        let double = ranged(12, on(1, None, track(2, 960, vec![note(0, 64, 240)])));
        // Muted, but its range still goes out, and on a channel whose
        // program comes from a later track, it follows that program.
        let pad = Track {
            muted: true,
            ..ranged(2, on(2, None, track(3, 960, vec![note(0, 48, 960)])))
        };
        let organ = on(2, sound(16, None, None), track(4, 960, vec![]));
        let project = project(vec![lead, double, pad, organ]);
        let start = Setup {
            at: Ticks::ZERO,
            since: None,
        };
        let mut expected = vec!["0 ch1 pc 5".to_owned()];
        expected.extend(bend_range(0, 1, 12));
        expected.push("0 ch2 pc 16".to_owned());
        expected.extend(bend_range(0, 2, 2));
        // The clip's bend uses the new range, before its notes.
        expected.extend(["0 ch1 bend 16383", "0 ch1 on 60", "0 ch1 on 64"].map(String::from));
        assert_eq!(play_setup(&project, start, 0..1), expected);
    }

    #[test]
    fn clip_programs_restore_the_channels_range_before_explicit_rpn_bends_and_notes() {
        for shared in [false, true] {
            let mut lead = on(
                1,
                None,
                track(
                    1,
                    960,
                    vec![
                        note(0, 60, 240),
                        program(0, 5),
                        control(0, 101, 0),
                        control(0, 100, 0),
                        control(0, 6, 24),
                        control(0, 38, 0),
                        control(0, 101, 127),
                        control(0, 100, 127),
                        bend(0, 16383),
                    ],
                ),
            );
            let mut setter = on(1, None, track(2, 960, vec![]));
            if shared {
                // Even a later, muted track supplies the channel's range.
                setter.bend_range = U7::new(12);
                setter.muted = true;
            } else {
                lead.bend_range = U7::new(12);
            }
            // A range on another port must not be used for the lead.
            let other_port = Track {
                target: OutputTarget {
                    port: PortId(1),
                    channel: Channel::from_number(1).unwrap(),
                },
                ..ranged(2, track(3, 960, vec![]))
            };
            let project = project(vec![other_port, lead, setter]);
            let setup = Setup {
                at: Ticks::ZERO,
                since: None,
            };
            for at in [0, 960] {
                let mut expected = Vec::new();
                if at == 0 {
                    expected.extend(bend_range(0, 1, 12));
                    expected.extend(bend_range(0, 1, 2));
                }
                expected.push(format!("{at} ch1 pc 5"));
                expected.extend(bend_range(at, 1, 12));
                // The clip can still override the restored default.
                expected.extend(bend_range(at, 1, 24));
                expected.extend([format!("{at} ch1 bend 16383"), format!("{at} ch1 on 60")]);
                assert_eq!(
                    play_setup(&project, setup, at..at + 1),
                    expected,
                    "shared {shared}, tick {at}",
                );
            }
            for end in [0, 1, 959, 960, 961, 1920] {
                let events = render_song(&project, Ticks(end), u64::MAX).unwrap();
                let size = events.len() as u64;
                assert_eq!(song_size(&project, Ticks(end)).0, size);
                assert!(render_song(&project, Ticks(end), size).is_ok());
                if size > 0 {
                    assert_eq!(
                        render_song(&project, Ticks(end), size - 1),
                        Err(SongTooLarge {
                            events: size,
                            limit: size - 1,
                        }),
                    );
                }
            }
        }
    }

    #[test]
    fn a_new_snapshot_sends_the_bend_ranges_it_changes_and_after_a_new_program() {
        let empty = |id| track(id, 960, vec![]);
        let before = project(vec![
            ranged(2, on(1, None, empty(1))),
            ranged(12, on(2, sound(5, None, None), empty(2))),
            ranged(2, on(3, sound(7, None, None), empty(3))),
            on(4, None, empty(4)),
            ranged(24, on(5, None, empty(5))),
        ]);
        let after = project(vec![
            // Unchanged.
            ranged(2, on(1, None, empty(1))),
            // A new program, which may reset the range, so it goes again.
            ranged(12, on(2, sound(6, None, None), empty(2))),
            // A new range alone.
            ranged(24, on(3, sound(7, None, None), empty(3))),
            // A range where there was none.
            ranged(2, on(4, None, empty(4))),
            // No range any more: the device keeps the one it has.
            on(5, None, empty(5)),
        ]);
        let switch = Setup {
            at: Ticks(1920),
            since: Some(&before),
        };
        let mut expected = vec!["1920 ch2 pc 6".to_owned()];
        expected.extend(bend_range(1920, 2, 12));
        expected.extend(bend_range(1920, 3, 24));
        expected.extend(bend_range(1920, 4, 2));
        assert_eq!(play_setup(&after, switch, 1920..2880), expected);
    }

    #[test]
    fn bends_and_pressure_go_with_the_controls_before_the_notes() {
        let project = project(vec![track(
            1,
            960,
            vec![
                note(0, 60, 240),
                bend(0, 0),
                program(0, 3),
                pressure(0, 90),
                control(0, 1, 64),
                bend(480, 8192),
            ],
        )]);
        assert_eq!(
            play(&project, 0..960),
            [
                "0 pc 3",
                "0 bend 0",
                "0 pressure 90",
                "0 cc 1=64",
                "0 on 60",
                "240 off 60",
                "480 bend 8192",
            ]
        );
    }

    #[test]
    fn songs_end_with_bends_centred_and_pressure_lifted() {
        // Left bent and pressed, then a bend and pressure that come back
        // to rest, on separate channels.
        let left = track(1, 960, vec![bend(0, 0), pressure(480, 90)]);
        let back = on(
            2,
            None,
            track(
                2,
                960,
                vec![
                    bend(0, 0),
                    bend(600, 8192),
                    pressure(0, 1),
                    pressure(700, 0),
                ],
            ),
        );
        let separate = project(vec![left, back]);
        let events = render_song(&separate, Ticks(960), u64::MAX).unwrap();
        let at_end: Vec<_> = events
            .iter()
            .filter(|event| event.tick == Ticks(960))
            .map(|event| (event.message.channel().unwrap().number(), event.message))
            .collect();
        assert_eq!(
            at_end,
            [
                (10, Expression::Bend.rest(Channel::DRUMS)),
                (10, Expression::Pressure.rest(Channel::DRUMS)),
            ]
        );
        assert_eq!(song_size(&separate, Ticks(960)).0, events.len() as u64);

        // Shared by two tracks, the channel ends as the last of them left
        // it, in the live renderer's order: here, the second track's centre.
        let shared = project(vec![
            track(1, 960, vec![bend(0, 0)]),
            track(2, 1920, vec![bend(0, 8192)]),
        ]);
        let ends = |end| {
            let events = render_song(&shared, Ticks(end), u64::MAX).unwrap();
            assert_eq!(song_size(&shared, Ticks(end)).0, events.len() as u64);
            events
                .iter()
                .filter(|event| event.tick == Ticks(end))
                .map(|event| event.message)
                .collect::<Vec<_>>()
        };
        assert_eq!(ends(960), []);
        // Tick 960 is the first track's alone, so it bends last.
        assert_eq!(ends(961), [Expression::Bend.rest(Channel::DRUMS)]);
    }

    #[test]
    fn each_port_has_its_own_channels() {
        let on_port = |port, track: Track| Track {
            target: OutputTarget {
                port: PortId(port),
                ..track.target
            },
            ..track
        };
        let empty = |id| track(id, 960, vec![]);
        let before = project(vec![on_port(1, on(1, sound(7, None, None), empty(2)))]);
        let after = project(vec![
            on(1, sound(5, None, None), empty(1)),
            on_port(1, on(1, sound(7, None, None), empty(2))),
            on_port(2, on(1, sound(9, None, None), empty(3))),
        ]);
        let start = Setup {
            at: Ticks::ZERO,
            since: None,
        };
        let switch = Setup {
            since: Some(&before),
            ..start
        };
        let sent = |programs, port: Option<u16>| {
            let mut out = Vec::new();
            match port {
                None => render(
                    &after,
                    NO_CLOCK,
                    Some(programs),
                    Ticks(0)..Ticks(1),
                    &mut out,
                ),
                Some(port) => {
                    out.reserve(8);
                    let window = Ticks(0)..Ticks(1);
                    let port = PortId(port);
                    render_bounded(
                        &after,
                        NO_CLOCK,
                        Some(programs),
                        port,
                        window,
                        None,
                        &mut out,
                    );
                }
            }
            let sent = |event: &RenderedEvent| match event.message {
                MidiMessage::ProgramChange { program, .. } => (event.port.0, program.get()),
                other => panic!("{other:?}"),
            };
            out.iter().map(sent).collect::<Vec<_>>()
        };
        assert_eq!(sent(start, None), [(0, 5), (1, 7), (2, 9)]);
        assert_eq!(sent(switch, None), [(0, 5), (2, 9)]);
        assert_eq!(sent(switch, Some(1)), []);
        assert_eq!(sent(switch, Some(2)), [(2, 9)]);
    }

    #[test]
    fn a_new_snapshot_sends_only_the_programs_it_changes() {
        let empty = |id| track(id, 960, vec![]);
        let before = project(vec![
            on(1, sound(5, None, None), empty(1)),
            on(2, sound(6, None, None), empty(2)),
            on(3, sound(7, None, None), empty(3)),
            on(4, None, empty(4)),
            on(6, sound(9, None, None), empty(6)),
            on(7, sound(10, None, Some(0)), empty(7)),
        ]);
        let after = project(vec![
            // Unchanged.
            on(1, sound(5, None, None), empty(1)),
            // A new program.
            on(2, sound(16, None, None), empty(2)),
            // The same program, moved to another channel.
            on(5, sound(7, None, None), empty(3)),
            // A program where there was none.
            on(4, sound(8, None, None), empty(4)),
            // A new track whose channel already has its program.
            on(1, sound(5, None, None), empty(5)),
            // No program any more: the device keeps the one it has.
            on(6, None, empty(6)),
            // Another bank is another sound.
            on(7, sound(10, None, Some(1)), empty(7)),
        ]);
        let switch = Setup {
            at: Ticks(1920),
            since: Some(&before),
        };
        assert_eq!(
            play_setup(&after, switch, 1920..2880),
            [
                "1920 ch2 pc 16",
                "1920 ch4 pc 8",
                "1920 ch5 pc 7",
                "1920 ch7 cc 32=1",
                "1920 ch7 pc 10",
            ]
        );
    }

    #[test]
    fn notes_end_after_their_length() {
        assert_eq!(
            play(&beat(), 0..960),
            ["0 on 36", "240 off 36", "480 on 38", "720 off 38"]
        );
    }

    #[test]
    fn clips_loop_from_tick_zero() {
        assert_eq!(
            play(&beat(), 1920..2880),
            ["1920 on 36", "2160 off 36", "2400 on 38", "2640 off 38"]
        );
        // Across the loop point.
        assert_eq!(play(&beat(), 720..1200), ["720 off 38", "960 on 36"]);
    }

    #[test]
    fn each_window_holds_the_events_inside_it() {
        let project = beat();
        let cases: [(Range<u64>, &[&str]); 4] = [
            (0..100, &["0 on 36"]),
            (100..240, &[]),
            (240..241, &["240 off 36"]),
            (960..960, &[]),
        ];
        for (window, expected) in cases {
            assert_eq!(play(&project, window.clone()), expected, "{window:?}");
        }
        let (start, end) = (961, 960);
        assert!(play(&project, start..end).is_empty(), "a backward window");
        // Windows that tile the timeline play everything exactly once.
        let tiled: Vec<String> = (0..48)
            .flat_map(|window| play(&project, window * 80..(window + 1) * 80))
            .collect();
        assert_eq!(tiled, play(&project, 0..3840));
        assert_eq!(tiled.len(), 16);
    }

    #[test]
    fn retriggered_notes_end_before_they_start_again() {
        let project = project(vec![track(
            1,
            480,
            vec![note(0, 36, 240), note(240, 36, 240)],
        )]);
        assert_eq!(
            play(&project, 0..720),
            [
                "0 on 36",
                "240 off 36",
                "240 on 36",
                "480 off 36",
                "480 on 36",
            ]
        );
    }

    #[test]
    fn note_offs_carry_the_release_and_key_pressure_follows_its_note_on() {
        let released = |start, length, release| ClipEvent {
            start: Ticks(start),
            kind: EventKind::Note {
                note: U7::new(36).unwrap(),
                velocity: U7::MAX,
                length: Ticks(length),
                release: U7::new(release).unwrap(),
            },
        };
        let key = |start, value| ClipEvent {
            start: Ticks(start),
            kind: EventKind::NotePressure {
                note: U7::new(36).unwrap(),
                value: U7::new(value).unwrap(),
            },
        };
        // Pressure written before the notes it presses, retriggered.
        let events = vec![
            key(0, 20),
            key(240, 70),
            released(0, 240, 30),
            released(240, 240, 90),
            key(360, 0),
        ];
        let project = project(vec![track(1, 480, events)]);
        assert_eq!(
            play(&project, 0..720),
            [
                "0 on 36",
                "0 key 36=20",
                "240 off 36 at 30",
                "240 on 36",
                "240 key 36=70",
                "360 key 36=0",
                "480 off 36 at 90",
                "480 on 36",
                "480 key 36=20",
            ]
        );
    }

    #[test]
    fn notes_held_over_the_loop_point_end_in_the_next_pass() {
        // A note tied over the loop point, and one as long as the loop.
        let tied = project(vec![track(1, 960, vec![note(720, 60, 480)])]);
        assert_eq!(play(&tied, 0..960), ["720 on 60"]);
        assert_eq!(play(&tied, 960..1920), ["1200 off 60", "1680 on 60"]);
        let drone = project(vec![track(1, 960, vec![note(0, 48, 960)])]);
        assert_eq!(
            play(&drone, 0..1920),
            ["0 on 48", "960 off 48", "960 on 48"]
        );
        assert_eq!(play(&drone, 1000..1920), Vec::<String>::new());
    }

    #[test]
    fn programs_and_controls_come_before_notes_on_their_tick() {
        let controls = vec![
            note(240, 40, 240),
            control(240, 74, 20),
            program(240, 5),
            note(0, 40, 240),
        ];
        let project = project(vec![track(1, 960, controls)]);
        assert_eq!(
            play(&project, 0..480),
            [
                "0 on 40",
                "240 off 40",
                "240 pc 5",
                "240 cc 74=20",
                "240 on 40",
            ]
        );
        // Controls before the window aren't sent again.
        assert_eq!(play(&project, 241..480), Vec::<String>::new());
    }

    #[test]
    fn bank_selects_come_before_their_program_change() {
        // Written with the volume first: it still goes out after the program
        // change, which may reset it.
        let events = vec![
            control(0, 7, 90),
            control(0, 0, 1),
            control(0, 32, 2),
            program(0, 5),
            note(0, 60, 240),
        ];
        let project = project(vec![track(1, 960, events)]);
        assert_eq!(
            play(&project, 0..1),
            ["0 cc 0=1", "0 cc 32=2", "0 pc 5", "0 cc 7=90", "0 on 60"]
        );
    }

    #[test]
    fn controls_on_one_tick_keep_their_order() {
        // An RPN (pitch bend range) is selected before its data entry, and
        // the last of two volumes wins.
        let rpn = vec![
            control(0, 101, 0),
            control(0, 100, 0),
            control(0, 6, 12),
            control(0, 7, 100),
            control(0, 7, 20),
        ];
        // Another track on the same channel follows in track order.
        let later = vec![control(0, 7, 64)];
        let project = project(vec![track(1, 960, rpn), track(2, 960, later)]);
        assert_eq!(
            play(&project, 0..1),
            [
                "0 cc 101=0",
                "0 cc 100=0",
                "0 cc 6=12",
                "0 cc 7=100",
                "0 cc 7=20",
                "0 cc 7=64",
            ]
        );
    }

    #[test]
    fn tracks_play_on_their_own_port_and_channel_and_loop() {
        let mut bass = track(2, 2880, vec![note(0, 40, 240)]);
        bass.target = OutputTarget {
            port: PortId(1),
            channel: Channel::from_number(2).unwrap(),
        };
        let mut muted = track(3, 960, vec![note(0, 50, 240)]);
        muted.muted = true;
        // Four-beat kicks against a three-beat bass line: polymeter.
        let project = project(vec![track(1, 3840, vec![note(0, 36, 240)]), bass, muted]);
        let mut out = Vec::new();
        render(&project, NO_CLOCK, None, Ticks(0)..Ticks(7680), &mut out);
        let ons: Vec<(u64, u16, u8, u8)> = out
            .iter()
            .filter_map(|event| match event.message {
                MidiMessage::NoteOn { channel, note, .. } => {
                    Some((event.tick.0, event.port.0, channel.number(), note.get()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            ons,
            [
                (0, 0, 10, 36),
                (0, 1, 2, 40),
                (2880, 1, 2, 40),
                (3840, 0, 10, 36),
                (5760, 1, 2, 40),
            ]
        );
    }

    #[test]
    fn silent_notes_play_nothing() {
        let mut silent = note(0, 36, 240);
        silent.kind = EventKind::Note {
            note: U7::new(36).unwrap(),
            velocity: U7::MIN,
            length: Ticks(240),
            release: DEFAULT_RELEASE,
        };
        let project = project(vec![track(1, 960, vec![silent])]);
        assert_eq!(play(&project, 0..960), Vec::<String>::new());
    }

    #[test]
    fn rendering_appends_without_reallocating() {
        let mut out = Vec::with_capacity(64);
        let earlier = RenderedEvent {
            tick: Ticks(5000),
            port: PortId(0),
            track: Some(TrackId(1)),
            message: MidiMessage::Stop,
            rank: Rank::NoteOn,
            sequence: (0, 0, 0, 0),
        };
        out.push(earlier);
        let buffer = out.as_ptr();
        render(&beat(), NO_CLOCK, None, Ticks(0)..Ticks(1920), &mut out);
        assert_eq!(out.as_ptr(), buffer);
        assert_eq!(out.len(), 9);
        assert_eq!(out[0], earlier, "what was there stays first");
    }

    #[test]
    fn clock_pulses_come_first_on_their_tick_from_where_they_start() {
        let clock = ClockPulses {
            ports: &[PortId(1), PortId(0)],
            from: Ticks(480),
        };
        assert_eq!(
            play_clocked(&beat(), clock, 440..530),
            [
                "480 clock 0",
                "480 clock 1",
                "480 on 38",
                "520 clock 0",
                "520 clock 1"
            ]
        );
        let clock = ClockPulses {
            ports: &[PortId(1)],
            from: Ticks::ZERO,
        };
        assert_eq!(
            play_clocked(&project(vec![]), clock, 0..100),
            ["0 clock 1", "40 clock 1", "80 clock 1"]
        );
    }

    #[test]
    fn clock_pulses_fall_every_40_ticks() {
        let pulses: Vec<u64> = clock_pulses(Ticks(0)..Ticks(100)).map(|t| t.0).collect();
        assert_eq!(pulses, [0, 40, 80]);
        let pulses: Vec<u64> = clock_pulses(Ticks(41)..Ticks(121)).map(|t| t.0).collect();
        assert_eq!(pulses, [80, 120]);
        assert_eq!(clock_pulses(Ticks(1)..Ticks(40)).count(), 0);
    }

    #[test]
    fn songs_end_the_notes_still_sounding_at_their_end() {
        // A kick on the downbeat, and a snare held over the loop point.
        let project = project(vec![track(
            1,
            960,
            vec![note(0, 36, 240), note(720, 38, 480)],
        )]);
        assert_eq!(
            song(&project, 1920),
            [
                "0 on 36",
                "240 off 36",
                "720 on 38",
                "960 on 36",
                // The first snare ends in the next pass, as it does live.
                "1200 off 38",
                "1200 off 36",
                "1680 on 38",
                // The second would end at 2160, so it ends with the song.
                "1920 off 38",
            ]
        );
        // Notes that would end exactly at the end still get their note-offs.
        assert_eq!(
            song(&project, 1200),
            [
                "0 on 36",
                "240 off 36",
                "720 on 38",
                "960 on 36",
                "1200 off 36",
                "1200 off 38",
            ]
        );
        assert!(song(&project, 0).is_empty());
    }

    #[test]
    fn a_note_started_twice_ends_twice() {
        let project = project(vec![track(
            1,
            960,
            vec![note(0, 36, 960), note(480, 36, 960)],
        )]);
        assert_eq!(
            song(&project, 960),
            ["0 on 36", "480 on 36", "960 off 36", "960 off 36"]
        );
    }

    #[test]
    fn notes_end_with_the_song_track_by_track() {
        let project = project(vec![
            track(1, 960, vec![note(0, 50, 960), note(0, 40, 960)]),
            track(2, 960, vec![note(0, 30, 960)]),
        ]);
        assert_eq!(
            song(&project, 480),
            [
                "0 on 50",
                "0 on 40",
                "0 on 30",
                "480 off 40",
                "480 off 50",
                "480 off 30",
            ]
        );
    }

    #[test]
    fn songs_leave_out_muted_tracks_and_silent_notes() {
        let silent = ClipEvent {
            start: Ticks(480),
            kind: EventKind::Note {
                note: U7::new(42).unwrap(),
                velocity: U7::MIN,
                length: Ticks(240),
                release: DEFAULT_RELEASE,
            },
        };
        let mut muted = track(2, 960, vec![note(0, 40, 240)]);
        muted.muted = true;
        let project = project(vec![track(1, 960, vec![note(0, 36, 240), silent]), muted]);
        assert_eq!(song(&project, 960), ["0 on 36", "240 off 36"]);
    }

    #[test]
    fn song_pedal_cleanup_respects_loops_and_shared_channel_order() {
        let project = project(vec![
            track(1, 960, vec![control(0, 64, 127), control(0, 66, 64)]),
            track(2, 1440, vec![control(0, 64, 0), control(800, 66, 63)]),
        ]);
        // Track 2's sustain release wins at tick 0. Later, the tracks'
        // different loop lengths take turns changing that same channel.
        for (end, expected) in [
            (0, vec![]),
            (1, vec![66]),
            (800, vec![66]), // The release at the cutoff isn't rendered.
            (801, vec![]),
            (961, vec![64, 66]),
            (1441, vec![66]),
            (1920, vec![66]),
            (1921, vec![64, 66]),
        ] {
            let events = render_song(&project, Ticks(end), u64::MAX).unwrap();
            let released: Vec<_> = events
                .iter()
                .filter_map(|event| match event.message {
                    MidiMessage::ControlChange {
                        controller, value, ..
                    } if event.tick == Ticks(end) => {
                        assert_eq!(value, U7::MIN);
                        assert_eq!(event.track, Some(TrackId(1)));
                        Some(controller.get())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(released, expected, "end {end}");
            let count = events.len() as u64;
            assert_eq!(song_size(&project, Ticks(end)).0, count);
            assert!(render_song(&project, Ticks(end), count).is_ok());
            if count > 0 {
                assert_eq!(
                    render_song(&project, Ticks(end), count - 1),
                    Err(SongTooLarge {
                        events: count,
                        limit: count - 1,
                    }),
                );
            }
        }
    }

    #[test]
    fn song_pedal_cleanup_keeps_ports_and_channels_separate_and_ignores_mutes() {
        let first = track(1, 960, vec![control(0, 64, 127)]);
        let mut other_port = track(2, 960, vec![control(0, 64, 0), control(0, 66, 127)]);
        other_port.target.port = PortId(1);
        let mut other_channel = track(3, 960, vec![control(0, 64, 0), control(0, 66, 127)]);
        other_channel.target.channel = Channel::from_number(3).unwrap();
        let mut muted = track(4, 960, vec![control(0, 64, 0), control(0, 66, 127)]);
        muted.muted = true;
        let project = project(vec![first, other_port, other_channel, muted]);
        let events = render_song(&project, Ticks(480), 10).unwrap();
        let released: Vec<_> = events
            .iter()
            .filter_map(|event| match event.message {
                MidiMessage::ControlChange {
                    channel,
                    controller,
                    value,
                } if event.tick == Ticks(480) => {
                    assert_eq!(value, U7::MIN);
                    Some((
                        event.port.0,
                        channel.number(),
                        controller.get(),
                        event.track,
                    ))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            released,
            [
                (0, 10, 64, Some(TrackId(1))),
                (1, 10, 66, Some(TrackId(2))),
                (0, 3, 66, Some(TrackId(3))),
            ]
        );
    }

    #[test]
    fn song_cleanup_releases_only_the_hold_pedals() {
        // Modulation and volume left up are settings, not held notes.
        let project = project(vec![track(
            1,
            960,
            vec![control(0, 1, 127), control(0, 7, 127), control(0, 64, 127)],
        )]);
        let events = render_song(&project, Ticks(480), u64::MAX).unwrap();
        let at_end: Vec<_> = events
            .iter()
            .filter(|event| event.tick == Ticks(480))
            .map(|event| event.message)
            .collect();
        assert_eq!(
            at_end,
            [MidiMessage::ControlChange {
                channel: Channel::DRUMS,
                controller: U7::new(64).unwrap(),
                value: U7::MIN,
            }]
        );
    }

    #[test]
    fn songs_are_counted_exactly_before_they_are_rendered() {
        let silent = ClipEvent {
            start: Ticks(240),
            kind: EventKind::Note {
                note: U7::new(42).unwrap(),
                velocity: U7::MIN,
                length: Ticks(240),
                release: DEFAULT_RELEASE,
            },
        };
        // Muted, but with a program, which goes out all the same.
        let mut muted = on(
            3,
            sound(7, None, None),
            track(3, 700, vec![note(0, 40, 700)]),
        );
        muted.muted = true;
        let project = project(vec![
            Track {
                program: sound(5, Some(1), Some(2)),
                bend_range: U7::new(12),
                ..track(
                    1,
                    960,
                    vec![
                        note(0, 36, 240),
                        note(720, 38, 480),
                        control(0, 74, 64),
                        program(480, 5),
                        bend(240, 0),
                        pressure(600, 30),
                        silent,
                    ],
                )
            },
            // The same program and range on the same channel go out once.
            Track {
                program: sound(5, Some(1), Some(2)),
                bend_range: U7::new(12),
                ..track(2, 1440, vec![note(1000, 50, 1440), control(1439, 1, 2)])
            },
            muted,
        ]);
        for end in [
            0, 1, 239, 240, 241, 959, 960, 1199, 1200, 1439, 1440, 2880, 10_000,
        ] {
            let events = render_song(&project, Ticks(end), u64::MAX).unwrap();
            assert_eq!(
                song_size(&project, Ticks(end)).0,
                events.len() as u64,
                "{end}"
            );
            let count = |off: bool| {
                events
                    .iter()
                    .filter(|event| match event.message {
                        MidiMessage::NoteOn { .. } => !off,
                        MidiMessage::NoteOff { .. } => off,
                        _ => false,
                    })
                    .count()
            };
            assert_eq!(count(false), count(true), "{end}");
            let programs = events
                .iter()
                .filter(|event| event.is_setup() && event.tick == Ticks::ZERO)
                .count();
            // Bank selects, a program change and a bend range's six, then
            // the muted track's program.
            assert_eq!(programs, if end == 0 { 0 } else { 3 + 6 + 1 }, "{end}");
            assert!(events.windows(2).all(|pair| pair[0].tick <= pair[1].tick));
            assert!(events.iter().all(|event| event.tick <= Ticks(end)));
        }

        let size = song_size(&project, Ticks(2880)).0;
        assert_eq!(
            render_song(&project, Ticks(2880), size - 1),
            Err(SongTooLarge {
                events: size,
                limit: size - 1
            })
        );
        assert_eq!(
            render_song(&project, Ticks(2880), size).unwrap().len() as u64,
            size
        );
    }
}
