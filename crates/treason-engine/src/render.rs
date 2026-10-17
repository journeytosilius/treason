//! Turning clips into MIDI events.

use std::collections::BTreeMap;
use std::ops::Range;

use thiserror::Error;
use treason_core::midi::{Channel, MidiMessage, U7};
use treason_core::model::{EventKind, PortId, Project, Track, TrackId};
use treason_core::time::{TICKS_PER_CLOCK_PULSE, Ticks};

use crate::notes::HOLD_PEDALS;

/// Bank select, the most and least significant bytes. A program change on
/// the same tick uses them, so they go out before it.
const BANK_SELECT: [u8; 2] = [0, 32];

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
    /// The order it was rendered in: by track, then as written in the clip,
    /// or for a clock pulse, by port. Breaks ties in the send order without a
    /// sort that allocates.
    sequence: (usize, u64, usize),
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
/// pulses of `clock`, in send order.
///
/// Every unmuted track loops its clip from tick 0, so an event at clip
/// position `p` plays at `p + k × length` for every `k`. A note's note-off
/// lands at its start plus its length, which may be in a later window: each
/// window holds the note-offs that fall inside it. Notes with velocity 0
/// play nothing.
///
/// Events are ordered by tick. At the same tick, clock pulses come first, so
/// the tick starts on time, then note-offs, bank selects (controllers 0 and
/// 32), program changes, other control changes and note-ons. A retriggered
/// note isn't cut short, a program change gets the bank selected with it,
/// and a note plays with the program and controller values set on its tick.
/// Within each of those groups, events keep their order, by track and then
/// as written in the clip, so a sequence such as an RPN's parameter
/// selection and data entry arrives as written.
///
/// `out` is reused from window to window. This unbounded entry point may
/// allocate if its remaining capacity cannot hold the whole window.
pub fn render(
    project: &Project,
    clock: ClockPulses<'_>,
    window: Range<Ticks>,
    out: &mut Vec<RenderedEvent>,
) {
    let first = out.len();
    visit(project, clock, None, window, |event| out.push(event));
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
/// than a port: [`render`]'s events for that span, without MIDI clock, then
/// a note-off at `end` for each note still sounding there, by track, then
/// channel and note. Every note-on gets its own note-off, even where a
/// track starts a note that is already sounding. Active sustain and
/// sostenuto pedals are released after those note-offs, including when
/// earlier note-offs have already ended all keys.
///
/// Counts the events first, and renders nothing if there would be more
/// than `limit`.
pub fn render_song(
    project: &Project,
    end: Ticks,
    limit: u64,
) -> Result<Vec<RenderedEvent>, SongTooLarge> {
    let (events, pedal_releases) = song_size(project, end);
    if events > limit {
        return Err(SongTooLarge { events, limit });
    }
    // Only a hint, so a count that doesn't fit leaves it to the vector.
    let mut out = Vec::with_capacity(usize::try_from(events).unwrap_or(0));
    render(project, ClockPulses::default(), Ticks::ZERO..end, &mut out);

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
                velocity: U7::MIN,
            },
            // After every pass, as the last word on each note.
            sequence: (index, u64::MAX, usize::from(note.get())),
        };
        out.extend(std::iter::repeat_n(off, count as usize));
    }
    out.extend(pedal_releases);
    Ok(out)
}

/// How many events [`render_song`] gives for `project` until `end`: two for
/// each note, its note-on and note-off, and one for each other event, for
/// every pass through the loop that starts the event before `end`, plus
/// the pedal releases returned alongside the count.
fn song_size(project: &Project, end: Ticks) -> (u64, Vec<RenderedEvent>) {
    let mut events = 0_u64;
    // Only the last change on each port/channel/pedal matters. Find it
    // arithmetically so even an oversized song needn't be rendered first.
    // The map holds at most two entries per MIDI output target.
    let mut pedals = BTreeMap::<(PortId, Channel, U7), RenderedEvent>::new();
    for (index, track) in project.tracks.iter().enumerate() {
        if track.muted {
            continue;
        }
        let length = track.clip.length().0;
        for (event_index, event) in track.clip.events().iter().enumerate() {
            let messages = match event.kind {
                EventKind::Note { velocity, .. } if velocity == U7::MIN => 0,
                EventKind::Note { .. } => 2,
                EventKind::Control { .. } | EventKind::Program { .. } => 1,
            };
            let passes = end.0.saturating_sub(event.start.0).div_ceil(length);
            events = events.saturating_add(passes.saturating_mul(messages));
            if let EventKind::Control { controller, value } = event.kind
                && HOLD_PEDALS.contains(&controller)
                && passes != 0
            {
                let last = RenderedEvent {
                    tick: Ticks(event.start.0 + (passes - 1) * length),
                    port: track.target.port,
                    track: Some(track.id),
                    message: MidiMessage::ControlChange {
                        channel: track.target.channel,
                        controller,
                        value,
                    },
                    sequence: (index, passes - 1, event_index),
                };
                let previous = pedals
                    .entry((track.target.port, track.target.channel, controller))
                    .or_insert(last);
                // Different tracks can share a pedal. Respect the live
                // renderer's order, including changes on the same tick.
                if send_order(&last) > send_order(previous) {
                    *previous = last;
                }
            }
        }
    }
    let mut releases = Vec::new();
    for mut event in pedals.into_values() {
        if let MidiMessage::ControlChange { value, .. } = &mut event.message
            && value.get() >= 64
        {
            *value = U7::MIN;
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
    port: PortId,
    window: Range<Ticks>,
    after: Option<RenderedEvent>,
    out: &mut Vec<RenderedEvent>,
) -> Option<RenderedEvent> {
    assert!(out.len() < out.capacity());
    let first = out.len();
    let mut overflow = false;
    visit(project, clock, Some(port), window, |event| {
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
    port: Option<PortId>,
    window: Range<Ticks>,
    mut emit: impl FnMut(RenderedEvent),
) {
    if window.start >= window.end {
        return;
    }
    let wanted = |target: PortId| port.is_none_or(|port| port == target);
    for tick in clock_pulses(window.start.max(clock.from)..window.end) {
        for &port in clock.ports.iter().filter(|&&port| wanted(port)) {
            emit(RenderedEvent {
                tick,
                port,
                track: None,
                message: MidiMessage::TimingClock,
                // Pulses rank on their own, so this only orders their ports.
                sequence: (0, 0, usize::from(port.0)),
            });
        }
    }
    for (index, track) in project.tracks.iter().enumerate() {
        if !track.muted && wanted(track.target.port) {
            render_track(index, track, window.clone(), &mut emit);
        }
    }
}

/// The ticks in `window` that get a MIDI clock pulse: every 40th tick,
/// counting from 0, which is 24 pulses per quarter note.
pub fn clock_pulses(window: Range<Ticks>) -> impl Iterator<Item = Ticks> {
    let first = window.start.0.div_ceil(TICKS_PER_CLOCK_PULSE);
    (first..)
        .map(|pulse| Ticks(pulse * TICKS_PER_CLOCK_PULSE))
        .take_while(move |tick| *tick < window.end)
}

/// Visits one track's events in a non-empty `window`, unsorted.
fn render_track(
    track_index: usize,
    track: &Track,
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
            let mut emit = |tick, message| {
                emit(RenderedEvent {
                    tick: Ticks(tick),
                    port,
                    track: Some(track.id),
                    message,
                    sequence: (track_index, pass, first + index),
                });
            };
            let at = offset + event.start.0;
            match event.kind {
                EventKind::Note { velocity, .. } if velocity == U7::MIN => {}
                EventKind::Note {
                    note,
                    velocity,
                    length,
                } => {
                    if at >= start {
                        let on = MidiMessage::NoteOn {
                            channel,
                            note,
                            velocity,
                        };
                        emit(at, on);
                    }
                    let off_at = at + length.0;
                    if (start..end).contains(&off_at) {
                        let off = MidiMessage::NoteOff {
                            channel,
                            note,
                            velocity: U7::MIN,
                        };
                        emit(off_at, off);
                    }
                }
                EventKind::Control { controller, value } if at >= start => {
                    let message = MidiMessage::ControlChange {
                        channel,
                        controller,
                        value,
                    };
                    emit(at, message);
                }
                EventKind::Program { program } if at >= start => {
                    emit(at, MidiMessage::ProgramChange { channel, program });
                }
                // Before the window, so already sent.
                EventKind::Control { .. } | EventKind::Program { .. } => {}
            }
        }
    }
}

/// Where an event goes in the send order: by tick, then clock pulses,
/// note-offs, bank selects, program changes, other control changes and
/// note-ons, then in the order they were rendered. No two events tie, so an
/// unstable sort keeps that order.
fn send_order(event: &RenderedEvent) -> (Ticks, u8, (usize, u64, usize)) {
    let rank = match event.message {
        MidiMessage::TimingClock => 0,
        MidiMessage::NoteOff { .. } => 1,
        MidiMessage::ControlChange { controller, .. }
            if BANK_SELECT.contains(&controller.get()) =>
        {
            2
        }
        MidiMessage::ProgramChange { .. } => 3,
        MidiMessage::ControlChange { .. } => 4,
        _ => 5,
    };
    (event.tick, rank, event.sequence)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use treason_core::midi::Channel;
    use treason_core::model::{Clip, ClipEvent, OutputTarget, TrackId};

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
        render(
            project,
            clock,
            Ticks(window.start)..Ticks(window.end),
            &mut out,
        );
        describe(&out)
    }

    /// One line per event, such as "240 off 36".
    fn describe(events: &[RenderedEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| {
                let what = match event.message {
                    MidiMessage::TimingClock => format!("clock {}", event.port.0),
                    MidiMessage::NoteOn { note, .. } => format!("on {}", note.get()),
                    MidiMessage::NoteOff { note, .. } => format!("off {}", note.get()),
                    MidiMessage::ControlChange {
                        controller, value, ..
                    } => format!("cc {}={}", controller.get(), value.get()),
                    MidiMessage::ProgramChange { program, .. } => {
                        format!("pc {}", program.get())
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
            track(
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
            ),
            Track {
                target: OutputTarget {
                    port: PortId(1),
                    channel: Channel::DRUMS,
                },
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
        let mut everything = Vec::new();
        render(&project, clock, Ticks::ZERO..Ticks(2000), &mut everything);
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
                    let next = render_bounded(&project, clock, port, window, after, &mut batch);
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
        render(&project, NO_CLOCK, Ticks::ZERO..Ticks(960), &mut expected);
        let mut batch = Vec::with_capacity(3);
        assert_eq!(
            render_bounded(
                &project,
                NO_CLOCK,
                PortId(0),
                Ticks::ZERO..Ticks(241),
                None,
                &mut batch
            ),
            None
        );
        assert_eq!(batch, expected[..2]);
        let window = Ticks(241)..Ticks(960);
        let after = render_bounded(&project, NO_CLOCK, PortId(0), window, None, &mut batch);
        assert_eq!(batch, expected[..3]);
        assert_eq!(batch.capacity(), 3);
        let last = after.expect("the fourth event must remain unrendered");
        batch.clear();
        assert_eq!(
            render_bounded(
                &project,
                NO_CLOCK,
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
        render(&project, NO_CLOCK, Ticks(0)..Ticks(7680), &mut out);
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
            sequence: (0, 0, 0),
        };
        out.push(earlier);
        let buffer = out.as_ptr();
        render(&beat(), NO_CLOCK, Ticks(0)..Ticks(1920), &mut out);
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
            },
        };
        let mut muted = track(3, 700, vec![note(0, 40, 700)]);
        muted.muted = true;
        let project = project(vec![
            track(
                1,
                960,
                vec![
                    note(0, 36, 240),
                    note(720, 38, 480),
                    control(0, 74, 64),
                    program(480, 5),
                    silent,
                ],
            ),
            track(2, 1440, vec![note(1000, 50, 1440), control(1439, 1, 2)]),
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
