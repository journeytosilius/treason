//! Controls written as points, for modulation and automation: control
//! changes (MIDI CC), pitch bend and channel pressure.
//!
//! The agent names a controller by a parameter of the track's device, a
//! standard name such as `modulation`, or its number, or names pitch bend
//! or pressure, and gives its values as points in beats, each jumping there
//! or ramping from the point before. [`clip_with_controls`] turns those
//! into changes, a ramp sending its value at most every [`RAMP_STEP`].
//! [`controls_from_clip`] reads a clip's changes back as points that send
//! the same.

use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::f64::consts::PI;
use std::fmt;

use thiserror::Error;
use treason_core::midi::{U7, U14};
use treason_core::model::{Clip, ClipError, ClipEvent, EventKind};
use treason_core::time::{PPQN, Ticks};
use treason_devices::{DeviceProfile, controllers};

use crate::dto::{ControlLane, Ramp};
use crate::notes::{MAX_LOOP_BEATS, beats, ticks};

/// The most changes a clip's controls can hold, control changes, pitch
/// bend and pressure together. It bounds what a loop sends and what each
/// edit keeps for undo.
pub const MAX_CONTROL_CHANGES: usize = 8192;

/// The most points one control can have: one for each change a clip can
/// hold, so the points any control reads back as can be written back.
pub const MAX_POINTS: usize = MAX_CONTROL_CHANGES;

/// How often a ramp sends its value, at most: on a grid of 1/32 beats from
/// the start of the loop, wherever the value has changed.
pub const RAMP_STEP: Ticks = Ticks(PPQN / 32);

/// The first controller number that is a channel mode message, such as All
/// Notes Off, rather than a control.
const MODE_MESSAGES: u8 = 120;

/// The longest loop, in ticks.
const MAX_LOOP: u64 = MAX_LOOP_BEATS as u64 * PPQN;

/// How many ramp values reading a clip's controls back may work out before
/// it settles for jumps. Clips that set_controls writes need far fewer; this
/// bounds the work for any clip.
const READ_BUDGET: u64 = 1 << 22;

/// Controls that can't be set. The messages tell the agent how to fix the
/// call.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ControlsError {
    /// No controls.
    #[error(
        r#"give at least one control, such as {{"modulation": {{"points": [{{"at": 0, "value": 0}}, {{"at": 4, "value": 127, "ramp": "linear"}}]}}}}"#
    )]
    NoControls,
    /// A name that is neither a parameter of the track's device nor a
    /// standard name.
    #[error(
        "`{name}` is not a parameter of {device} ({params}), a standard name such as modulation or cutoff, pitch_bend, pressure, or a controller number 0-119"
    )]
    UnknownParam {
        /// The name as written.
        name: String,
        /// The device's id.
        device: String,
        /// Its parameter names.
        params: String,
    },
    /// A name that isn't a standard name, on a track without a device.
    #[error(
        "`{0}` is not a standard name such as modulation or cutoff, pitch_bend, pressure, or a controller number 0-119; for a device's own names, give the track a device with edit_track (see list_devices)"
    )]
    UnknownControl(String),
    /// A number that isn't a control's controller.
    #[error("`{0}` is not a controller number: controls are 0-119")]
    NumberOutOfRange(String),
    /// A name for a channel mode message, such as All Notes Off.
    #[error(
        "`{name}` is controller {controller}, a channel mode message rather than a control: controls are 0-119"
    )]
    ModeMessage {
        /// The name as written.
        name: String,
        /// The controller it names.
        controller: U7,
    },
    /// Two names for the same control.
    #[error("`{first}` and `{second}` are both {control}: give each control once")]
    SameControl {
        /// One name.
        first: String,
        /// The other.
        second: String,
        /// The control both name.
        control: Control,
    },
    /// A loop length out of range.
    #[error("loop_beats must be more than 0 and at most {MAX_LOOP_BEATS}, not {0}")]
    LoopBeats(f64),
    /// A loop length for a track whose notes set its loop.
    #[error(
        "the track's notes make its loop {0} beats long: leave out loop_beats, or change the loop with set_notes or set_steps"
    )]
    LoopFollowsNotes(f64),
    /// Controls that need a loop longer than the longest, in beats, when
    /// the loop length is left to them.
    #[error(
        "the controls need a {0}-beat loop of whole bars, longer than the longest of {MAX_LOOP_BEATS} beats: pass loop_beats, or move the last points earlier"
    )]
    PastLongestLoop(f64),
    /// More changes than a clip can hold.
    #[error(
        "the track's controls would send more than {MAX_CONTROL_CHANGES} changes each loop: use fewer points, ramps that change less, or a shorter loop"
    )]
    TooManyChanges,
    /// One control is wrong.
    #[error("`{control}`: {error}")]
    Control {
        /// The control as written.
        control: String,
        /// What's wrong with it.
        error: ControlError,
    },
    /// The clip itself was rejected.
    #[error(transparent)]
    Clip(#[from] ClipError),
}

/// What's wrong with one control.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ControlError {
    /// More points than a control can have.
    #[error("{0} points is more than the {MAX_POINTS} a control can have")]
    TooManyPoints(usize),
    /// A repeat length out of range.
    #[error("repeat_beats must be more than 0 and at most {MAX_LOOP_BEATS}, not {0}")]
    RepeatBeats(f64),
    /// A repeat longer than the loop.
    #[error(
        "repeat_beats is {repeat}, longer than the {loop_beats}-beat loop: shorten it, or {lengthen}"
    )]
    RepeatPastLoop {
        /// The repeat, in beats.
        repeat: f64,
        /// The loop, in beats.
        loop_beats: f64,
        /// How to make the loop longer.
        lengthen: &'static str,
    },
    /// A ramp between points at the same time.
    #[error(
        "points {first} and {second} are both at beat {at}: use jumps for successive values at the same time, or move the ramp's end later"
    )]
    SameTime {
        /// The earlier of the two in the list, counting from 1.
        first: usize,
        /// The later of the two.
        second: usize,
        /// The time, in beats.
        at: f64,
    },
    /// One point is wrong.
    #[error("point {position}: {error}")]
    Point {
        /// Which point in the list, counting from 1.
        position: usize,
        /// What's wrong with it.
        error: PointError,
    },
}

/// What's wrong with one point.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum PointError {
    /// A time out of range.
    #[error("at must be from 0 to {MAX_LOOP_BEATS} beats, not {0}")]
    At(f64),
    /// A value past the control's range.
    #[error("value must be {range}, not {value}")]
    Value {
        /// The value as written.
        value: i32,
        /// The control's range.
        range: &'static str,
    },
    /// The first point ramps, from nothing.
    #[error(
        "it ramps, but it's the first point, with none before it to ramp from: drop its ramp, or add a point before it"
    )]
    RampFromNothing,
    /// A point after the end of the loop or the repeat.
    #[error(
        "it's at beat {at}, after the {span} ends at beat {end}: move it earlier, or {lengthen}"
    )]
    PastEnd {
        /// The point's time, in beats.
        at: f64,
        /// "loop" or "repeat".
        span: &'static str,
        /// Where the loop or repeat ends, in beats.
        end: f64,
        /// How to make it longer.
        lengthen: &'static str,
    },
    /// A jump at the end of the loop or the repeat, which sends nothing.
    #[error(
        "it jumps at the end of the {0}, where the next pass starts again from the first point, so it sends nothing: give it a ramp to end on, or move it earlier"
    )]
    JumpAtEnd(&'static str),
}

/// What a control sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Control {
    /// Control changes on this controller.
    Controller(U7),
    /// Channel pressure, or aftertouch.
    Pressure,
    /// Pitch bend.
    PitchBend,
}

impl Control {
    /// The control a clip event sends, and its value as sent, unless it's a
    /// note or a program change.
    pub fn of(kind: EventKind) -> Option<(Self, u16)> {
        match kind {
            EventKind::Control { controller, value } => {
                Some((Self::Controller(controller), value.get().into()))
            }
            EventKind::Pressure { value } => Some((Self::Pressure, value.get().into())),
            EventKind::PitchBend { value } => Some((Self::PitchBend, value.get())),
            EventKind::Note { .. } | EventKind::Program { .. } => None,
        }
    }

    /// The event that sends `value`, as sent.
    fn event(self, value: u16) -> EventKind {
        let u7 = || U7::saturating(u8::try_from(value).unwrap_or(u8::MAX));
        match self {
            Self::Controller(controller) => EventKind::Control {
                controller,
                value: u7(),
            },
            Self::Pressure => EventKind::Pressure { value: u7() },
            Self::PitchBend => EventKind::PitchBend {
                value: U14::new(value).unwrap_or(U14::MAX),
            },
        }
    }

    /// The highest value it sends.
    const fn max(self) -> u16 {
        match self {
            Self::PitchBend => U14::MAX.get(),
            Self::Controller(_) | Self::Pressure => U7::MAX.get() as u16,
        }
    }

    /// What the value the agent writes is short of the value sent: pitch
    /// bend is written from -8192 to 8191, around its centre at 0, and sent
    /// from 0 to 16383.
    const fn offset(self) -> i32 {
        match self {
            Self::PitchBend => U14::CENTER.get() as i32,
            Self::Controller(_) | Self::Pressure => 0,
        }
    }

    /// The values the agent can write, for errors.
    const fn range(self) -> &'static str {
        match self {
            Self::PitchBend => "-8192 to 8191, with 0 the centre",
            Self::Controller(_) | Self::Pressure => "0-127",
        }
    }

    /// The value the agent writes for one sent.
    pub fn written(self, value: u16) -> i32 {
        i32::from(value) - self.offset()
    }

    /// The value sent for one the agent writes, if it's in range.
    fn sent(self, value: i32) -> Option<u16> {
        value
            .checked_add(self.offset())
            .and_then(|sent| u16::try_from(sent).ok())
            .filter(|&sent| sent <= self.max())
    }
}

impl fmt::Display for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Controller(controller) => write!(f, "controller {controller}"),
            Self::Pressure => f.write_str("channel pressure"),
            Self::PitchBend => f.write_str("pitch bend"),
        }
    }
}

/// A control's value at a point in a loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    /// When, from the start of the loop, or of each repeat.
    pub at: Ticks,
    /// The value, as sent.
    pub value: u16,
    /// How the value gets here from the point before. `None` jumps.
    pub ramp: Option<Ramp>,
}

/// One control, checked: its points by time, and how often they repeat.
struct Lane<'a> {
    /// The control as written.
    name: &'a str,
    control: Control,
    /// Sorted by time; successive jumps on one tick keep their input order.
    points: Vec<Point>,
    /// Where each point was in the agent's list, counting from 1.
    positions: Vec<usize>,
    repeat: Option<Ticks>,
}

/// The clip `clip` becomes with `controls` set, on a track whose device is
/// `device`, in a project whose bars are `bar` long.
///
/// A control replaces the changes `clip` sends on its controller, or of
/// pitch bend or pressure, and one with no points removes them. The notes,
/// program changes and other controls stay. A track with notes keeps its loop. On any other,
/// `loop_beats` sets the loop, which otherwise stays, unless the track
/// holds only these controls: then it lasts until the end of the last point
/// or repeat, rounded up to whole bars. A shorter loop cuts off the other
/// changes at its end.
pub fn clip_with_controls(
    clip: &Clip,
    controls: &BTreeMap<String, ControlLane>,
    loop_beats: Option<f64>,
    bar: Ticks,
    device: Option<&DeviceProfile>,
) -> Result<Clip, ControlsError> {
    if controls.is_empty() {
        return Err(ControlsError::NoControls);
    }
    let mut lanes: Vec<Lane<'_>> = Vec::with_capacity(controls.len());
    for (name, lane) in controls {
        let control = resolve(name, device)?;
        if let Some(first) = lanes.iter().find(|lane| lane.control == control) {
            return Err(ControlsError::SameControl {
                first: first.name.to_owned(),
                second: name.clone(),
                control,
            });
        }
        lanes.push(place(name, control, lane)?);
    }

    let replaced = |event: &ClipEvent| {
        Control::of(event.kind)
            .is_some_and(|(control, _)| lanes.iter().any(|lane| lane.control == control))
    };
    let kept: Vec<(usize, ClipEvent)> = clip
        .events()
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, event)| !replaced(event))
        .collect();
    let notes = kept
        .iter()
        .any(|(_, event)| matches!(event.kind, EventKind::Note { .. }));
    let length = match loop_beats {
        Some(beats) => {
            let length = ticks(beats)
                .filter(|&length| length > Ticks::ZERO)
                .ok_or(ControlsError::LoopBeats(beats))?;
            if notes && length != clip.length() {
                return Err(ControlsError::LoopFollowsNotes(self::beats(clip.length())));
            }
            length
        }
        None if !kept.is_empty() || lanes.iter().all(|lane| lane.points.is_empty()) => {
            clip.length()
        }
        None => {
            let end = lanes.iter().map(Lane::end).max().unwrap_or(0);
            let length = end.div_ceil(bar.0) * bar.0;
            if length > MAX_LOOP {
                return Err(ControlsError::PastLongestLoop(beats(Ticks(length))));
            }
            Ticks(length)
        }
    };
    let lengthen = if notes {
        "lengthen the loop with set_notes or set_steps"
    } else {
        "pass a longer loop_beats"
    };
    for lane in &lanes {
        lane.check(length, lengthen)?;
    }

    let mut events: Vec<(usize, ClipEvent)> = kept
        .into_iter()
        .filter(|(_, event)| event.start < length)
        .collect();
    let mut changes = events
        .iter()
        .filter(|(_, event)| Control::of(event.kind).is_some())
        .count();
    // Replace each control's occurrence in its original same-tick slot.
    // Grouping lanes by name must not move RPN selection after Data Entry,
    // or regroup interleaved occurrences of a repeated controller.
    let mut slots: BTreeMap<(Ticks, Control), VecDeque<usize>> = BTreeMap::new();
    for (position, event) in clip.events().iter().enumerate() {
        if replaced(event)
            && let Some((control, _)) = Control::of(event.kind)
        {
            slots
                .entry((event.start, control))
                .or_default()
                .push_back(position);
        }
    }
    for lane in &lanes {
        lane.send(length, |tick, value| {
            changes += 1;
            if changes > MAX_CONTROL_CHANGES {
                return Err(ControlsError::TooManyChanges);
            }
            let position = slots
                .get_mut(&(Ticks(tick), lane.control))
                .and_then(VecDeque::pop_front)
                .unwrap_or(clip.events().len());
            events.push((
                position,
                ClipEvent {
                    start: Ticks(tick),
                    kind: lane.control.event(value),
                },
            ));
            Ok(())
        })?;
    }
    // New occurrences follow the existing events on their tick, in lane
    // order. The stable sort keeps their order when they share a slot.
    events.sort_by_key(|(position, event)| (event.start, *position));
    Ok(Clip::new(
        length,
        events.into_iter().map(|(_, event)| event).collect(),
    )?)
}

/// `notes`, a track's new clip of notes, with the controls and program
/// changes of its `old` clip that start within it, so that writing notes
/// leaves the controls as they are.
pub fn keep_controls(old: &Clip, notes: Clip) -> Result<Clip, ClipError> {
    let length = notes.length();
    let kept = old
        .events()
        .iter()
        .filter(|event| !matches!(event.kind, EventKind::Note { .. }) && event.start < length);
    let events = notes.events().iter().chain(kept).copied().collect();
    Clip::new(length, events)
}

/// The controls `clip` sends, as points that send the same changes when
/// set_controls writes them over the same loop.
///
/// The changes a ramp sends read back as that ramp, so a control that
/// set_controls wrote reads back much as it was written, but for a repeat,
/// which reads back as every pass of it. Other changes read back as jumps.
pub fn controls_from_clip(clip: &Clip) -> BTreeMap<Control, Vec<Point>> {
    let mut changes: BTreeMap<Control, Vec<(u64, u16)>> = BTreeMap::new();
    for event in clip.events() {
        if let Some((control, value)) = Control::of(event.kind) {
            changes
                .entry(control)
                .or_default()
                .push((event.start.0, value));
        }
    }
    let mut budget = READ_BUDGET;
    changes
        .into_iter()
        .map(|(control, changes)| {
            let points = read(&changes, clip.length().0, control.max(), &mut budget);
            (control, points)
        })
        .collect()
}

/// What to call `control` on a track whose device is `device`. A
/// controller goes by the device's name for it, else its standard name
/// unless the device uses that name for another controller, else its
/// number. Each resolves back to `control`.
pub fn control_name(control: Control, device: Option<&DeviceProfile>) -> String {
    let controller = match control {
        Control::Controller(controller) => controller,
        Control::Pressure => return controllers::PRESSURE.to_owned(),
        Control::PitchBend => return controllers::PITCH_BEND.to_owned(),
    };
    if let Some(name) = device.and_then(|device| device.param_name(controller)) {
        return name.to_owned();
    }
    match controllers::standard_name(controller) {
        Some(name) if device.and_then(|device| device.param(name)).is_none() => name.to_owned(),
        _ => controller.get().to_string(),
    }
}

/// The control a name names. Numbers, written as digits, are controller
/// numbers, and pitch_bend and pressure name those. Another name is a
/// parameter of the device first, then a standard name.
fn resolve(name: &str, device: Option<&DeviceProfile>) -> Result<Control, ControlsError> {
    match name {
        controllers::PITCH_BEND => return Ok(Control::PitchBend),
        controllers::PRESSURE => return Ok(Control::Pressure),
        _ => {}
    }
    let controller = if !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()) {
        name.parse()
            .ok()
            .and_then(U7::new)
            .filter(|controller| controller.get() < MODE_MESSAGES)
            .ok_or_else(|| ControlsError::NumberOutOfRange(name.to_owned()))?
    } else if let Some(param) = device.and_then(|device| device.param(name)) {
        param.cc
    } else if let Some(controller) = controllers::standard_controller(name) {
        controller
    } else {
        return Err(match device {
            Some(device) => ControlsError::UnknownParam {
                name: name.to_owned(),
                device: device.id.clone(),
                params: if device.params.is_empty() {
                    "it has none".into()
                } else {
                    device
                        .params
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                },
            },
            None => ControlsError::UnknownControl(name.to_owned()),
        });
    };
    if controller.get() >= MODE_MESSAGES {
        return Err(ControlsError::ModeMessage {
            name: name.to_owned(),
            controller,
        });
    }
    Ok(Control::Controller(controller))
}

/// Checks one control on its own, and puts its points in order.
fn place<'a>(
    name: &'a str,
    control: Control,
    lane: &ControlLane,
) -> Result<Lane<'a>, ControlsError> {
    let wrong = |error| ControlsError::Control {
        control: name.to_owned(),
        error,
    };
    if lane.points.len() > MAX_POINTS {
        return Err(wrong(ControlError::TooManyPoints(lane.points.len())));
    }
    let repeat = lane
        .repeat_beats
        .map(|beats| {
            ticks(beats)
                .filter(|&repeat| repeat > Ticks::ZERO)
                .ok_or(ControlError::RepeatBeats(beats))
        })
        .transpose()
        .map_err(wrong)?;
    let mut points = Vec::with_capacity(lane.points.len());
    for (index, spec) in lane.points.iter().enumerate() {
        let point = match (ticks(spec.at), control.sent(spec.value)) {
            (None, _) => Err(PointError::At(spec.at)),
            (_, None) => Err(PointError::Value {
                value: spec.value,
                range: control.range(),
            }),
            (Some(at), Some(value)) => Ok(Point {
                at,
                value,
                ramp: spec.ramp,
            }),
        };
        let point = point.map_err(|error| {
            wrong(ControlError::Point {
                position: index + 1,
                error,
            })
        })?;
        points.push((index + 1, point));
    }
    points.sort_by_key(|&(position, point)| (point.at, position));
    for pair in points.windows(2) {
        let ((first, earlier), (second, later)) = (pair[0], pair[1]);
        if earlier.at == later.at && later.ramp.is_some() {
            return Err(wrong(ControlError::SameTime {
                first,
                second,
                at: beats(earlier.at),
            }));
        }
    }
    if let Some(&(position, first)) = points.first()
        && first.ramp.is_some()
    {
        return Err(wrong(ControlError::Point {
            position,
            error: PointError::RampFromNothing,
        }));
    }
    let (positions, points) = points.into_iter().unzip();
    Ok(Lane {
        name,
        control,
        points,
        positions,
        repeat,
    })
}

impl Lane<'_> {
    /// How long a loop must be to hold this control: its repeat, or until
    /// its last point. A ramp may end on the loop's end, but a jump must
    /// come before it.
    fn end(&self) -> u64 {
        match (self.repeat, self.points.last()) {
            (Some(repeat), _) => repeat.0,
            (None, Some(last)) if last.ramp.is_some() => last.at.0,
            (None, Some(last)) => last.at.0 + 1,
            (None, None) => 0,
        }
    }

    /// Checks that the points fit in their repeat, or else in a loop of
    /// `length`, which `lengthen` says how to make longer.
    fn check(&self, length: Ticks, lengthen: &'static str) -> Result<(), ControlsError> {
        let wrong = |error| ControlsError::Control {
            control: self.name.to_owned(),
            error,
        };
        let (end, span, lengthen) = match self.repeat {
            Some(repeat) if repeat > length => {
                return Err(wrong(ControlError::RepeatPastLoop {
                    repeat: beats(repeat),
                    loop_beats: beats(length),
                    lengthen,
                }));
            }
            Some(repeat) => (repeat, "repeat", "make repeat_beats longer"),
            None => (length, "loop", lengthen),
        };
        for (&position, point) in self.positions.iter().zip(&self.points) {
            let error = if point.at > end {
                PointError::PastEnd {
                    at: beats(point.at),
                    span,
                    end: beats(end),
                    lengthen,
                }
            } else if point.at == end && point.ramp.is_none() {
                PointError::JumpAtEnd(span)
            } else {
                continue;
            };
            return Err(wrong(ControlError::Point { position, error }));
        }
        Ok(())
    }

    /// Sends the control's changes through a loop of `length`, in time
    /// order: each pass of the repeat sends its points, and between them
    /// the values of the ramps. A point at the end of a pass sends nothing,
    /// since the next pass starts there, and the loop's end cuts off the
    /// last pass.
    fn send<E>(
        &self,
        length: Ticks,
        mut send: impl FnMut(u64, u16) -> Result<(), E>,
    ) -> Result<(), E> {
        let period = self.repeat.unwrap_or(length).0;
        if self.points.is_empty() {
            return Ok(());
        }
        let mut start = 0;
        while start < length.0 {
            let mut before: Option<&Point> = None;
            for point in &self.points {
                let at = start + point.at.0;
                if let (Some(ramp), Some(from)) = (point.ramp, before) {
                    let from = (start + from.at.0, from.value);
                    for (tick, value) in RampValues::new(ramp, from, (at, point.value), length.0) {
                        send(tick, value)?;
                    }
                }
                if point.at.0 < period && at < length.0 {
                    send(at, point.value)?;
                }
                before = Some(point);
            }
            start += period;
        }
        Ok(())
    }
}

/// The values a ramp sends on its way from one point to the next: on each
/// step of the [`RAMP_STEP`] grid strictly between them and before the end
/// of the loop, the value there if it has changed.
struct RampValues {
    ramp: Ramp,
    from: (u64, u16),
    to: (u64, u16),
    /// The next step of the grid to work out.
    tick: u64,
    /// Where to stop: the next point, or the end of the loop.
    stop: u64,
    /// The value sent last.
    last: u16,
}

impl RampValues {
    /// The values a ramp from `from` to `to` sends before `end`.
    fn new(ramp: Ramp, from: (u64, u16), to: (u64, u16), end: u64) -> Self {
        let step = RAMP_STEP.0;
        Self {
            ramp,
            from,
            to,
            tick: (from.0 / step).saturating_add(1).saturating_mul(step),
            stop: to.0.min(end),
            last: from.1,
        }
    }
}

impl Iterator for RampValues {
    type Item = (u64, u16);

    fn next(&mut self) -> Option<Self::Item> {
        while self.tick < self.stop {
            let tick = self.tick;
            self.tick = self.tick.saturating_add(RAMP_STEP.0);
            let value = value_at(self.ramp, self.from, self.to, tick);
            if value != self.last {
                self.last = value;
                return Some((tick, value));
            }
        }
        None
    }
}

/// A ramp's value at `tick`, between its points `from` and `to`. The
/// change from `from`'s value rounds half away from zero, so ramps up and
/// down mirror each other.
fn value_at(ramp: Ramp, (start, from): (u64, u16), (end, to): (u64, u16), tick: u64) -> u16 {
    let change = i128::from(to) - i128::from(from);
    let (elapsed, span) = (i128::from(tick - start), i128::from(end - start));
    let offset = match ramp {
        Ramp::Linear => {
            let scaled = change * elapsed;
            (2 * scaled + scaled.signum() * span) / (2 * span)
        }
        Ramp::Smooth => {
            let eased = (1.0 - (PI * elapsed as f64 / span as f64).cos()) / 2.0;
            (change as f64 * eased).round() as i128
        }
    };
    // Between the two, which rounding can't overshoot.
    let value = (i128::from(from) + offset).clamp(from.min(to).into(), from.max(to).into());
    value as u16
}

/// Points that send `changes`, the changes one control makes through a
/// loop `length` long, in time order, its values at most `max`. Starting
/// from the first change, each point reaches as far as one ramp can: to the
/// last change a linear or smooth ramp sends exactly the changes up to, or
/// failing that, a jump to the next change. Working out ramps spends
/// `budget`.
fn read(changes: &[(u64, u16)], length: u64, max: u16, budget: &mut u64) -> Vec<Point> {
    let point = |(tick, value): (u64, u16), ramp| Point {
        at: Ticks(tick),
        value,
        ramp,
    };
    let Some(&first) = changes.first() else {
        return Vec::new();
    };
    let mut points = vec![point(first, None)];
    let mut from = 0;
    'points: while from + 1 < changes.len() {
        let last = run_end(changes, from);
        if last == changes.len() - 1
            && let Some(end) = ramp_to_loop_end(changes, from, length, (first.1, max), budget)
        {
            points.push(end);
            break;
        }
        for to in (from + 2..=last).rev() {
            for ramp in [Ramp::Linear, Ramp::Smooth] {
                if sends(changes, from, changes[to], to, ramp, budget) {
                    points.push(point(changes[to], Some(ramp)));
                    from = to;
                    continue 'points;
                }
            }
        }
        from += 1;
        points.push(point(changes[from], None));
    }
    points
}

/// The last change a ramp from `changes[from]` could reach. The changes a
/// ramp sends on its way sit on the grid, each later than the one before
/// and a step the same way.
fn run_end(changes: &[(u64, u16)], from: usize) -> usize {
    let mut end = from + 1;
    let mut direction = None;
    // Could `changes[end]` be a value sent on the way to a later one?
    while end + 1 < changes.len() {
        let ((before, previous), (tick, value)) = (changes[end - 1], changes[end]);
        let step = value.cmp(&previous);
        if !tick.is_multiple_of(RAMP_STEP.0)
            || tick <= before
            || step == Ordering::Equal
            || direction.is_some_and(|direction| direction != step)
        {
            break;
        }
        direction = Some(step);
        end += 1;
    }
    end
}

/// A ramp from `changes[from]` to the end of the loop that sends every
/// change after it, preferring to end on `first`, the loop's first value,
/// so the loop comes round smoothly. Values go up to `max`.
fn ramp_to_loop_end(
    changes: &[(u64, u16)],
    from: usize,
    length: u64,
    (first, max): (u16, u16),
    budget: &mut u64,
) -> Option<Point> {
    let (start, value) = changes[from];
    let (_, last) = *changes.last()?;
    if changes.len() <= from + 1 || length - start > MAX_LOOP {
        return None;
    }
    // The ramp ends at or beyond the last value it sent, most likely just
    // beyond it.
    let beyond: Vec<u16> = if last > value {
        (last..=max).collect()
    } else {
        (0..=last).rev().collect()
    };
    let ends = beyond
        .iter()
        .filter(|&&end| end == first)
        .chain(beyond.iter().filter(|&&end| end != first));
    for ramp in [Ramp::Linear, Ramp::Smooth] {
        for &end in ends.clone() {
            if sends(changes, from, (length, end), changes.len(), ramp, budget) {
                return Some(Point {
                    at: Ticks(length),
                    value: end,
                    ramp: Some(ramp),
                });
            }
        }
    }
    None
}

/// Whether a ramp from `changes[from]` to `to` sends exactly the changes
/// between them, `changes[from + 1..until]`, spending `budget` on the
/// values it works out.
fn sends(
    changes: &[(u64, u16)],
    from: usize,
    to: (u64, u16),
    until: usize,
    ramp: Ramp,
    budget: &mut u64,
) -> bool {
    let start = changes[from];
    if *budget == 0 || to.0 <= start.0 || to.0 - start.0 > MAX_LOOP {
        return false;
    }
    let mut values = RampValues::new(ramp, start, to, u64::MAX);
    let first = values.tick;
    let mut expected = changes[from + 1..until].iter();
    let matched = values.all(|sent| expected.next() == Some(&sent));
    let worked_out = values.tick.saturating_sub(first) / RAMP_STEP.0;
    *budget = budget.saturating_sub(worked_out);
    matched && expected.next().is_none()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use treason_core::midi::Channel;
    use treason_core::time::TimeSignature;
    use treason_devices::{DeviceRegistry, Param};

    use super::*;
    use crate::dto::ControlPoint;

    const BAR: Ticks = TimeSignature::COMMON.bar_length();

    /// A synth that names its filter and mod wheel, and calls controller 33
    /// modulation.
    fn synth() -> DeviceProfile {
        let param = |cc| Param {
            cc: U7::new(cc).unwrap(),
        };
        DeviceProfile {
            id: "synth".into(),
            name: "Synth".into(),
            manufacturer: None,
            default_channel: Channel::from_number(1).unwrap(),
            voices: BTreeMap::new(),
            aliases: BTreeMap::new(),
            programs: BTreeMap::new(),
            params: BTreeMap::from([
                ("filter".into(), param(74)),
                ("wheel".into(), param(1)),
                ("modulation".into(), param(33)),
            ]),
        }
    }

    fn drums() -> DeviceProfile {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../devices");
        let registry = DeviceRegistry::load_dir(&dir).unwrap();
        registry.get("gm-drums").unwrap().clone()
    }

    /// A point at `at` beats.
    fn at(at: f64, value: i32, ramp: Option<Ramp>) -> ControlPoint {
        ControlPoint { at, value, ramp }
    }

    /// Controller `number`.
    fn cc(number: u8) -> Control {
        Control::Controller(U7::new(number).unwrap())
    }

    fn lane(points: Vec<ControlPoint>, repeat_beats: Option<f64>) -> ControlLane {
        ControlLane {
            points,
            repeat_beats,
        }
    }

    fn controls(lanes: Vec<(&str, ControlLane)>) -> BTreeMap<String, ControlLane> {
        lanes
            .into_iter()
            .map(|(name, lane)| (name.to_owned(), lane))
            .collect()
    }

    /// The changes `clip` sends on `controller`, as (tick, value).
    fn changes(clip: &Clip, controller: u8) -> Vec<(u64, u8)> {
        clip.events()
            .iter()
            .filter_map(|event| match event.kind {
                EventKind::Control {
                    controller: number,
                    value,
                } if number.get() == controller => Some((event.start.0, value.get())),
                _ => None,
            })
            .collect()
    }

    fn note(start: u64, length: u64) -> ClipEvent {
        ClipEvent {
            start: Ticks(start),
            kind: EventKind::Note {
                note: U7::new(48).unwrap(),
                velocity: U7::new(100).unwrap(),
                length: Ticks(length),
            },
        }
    }

    fn set(
        clip: &Clip,
        lanes: Vec<(&str, ControlLane)>,
        loop_beats: Option<f64>,
        device: Option<&DeviceProfile>,
    ) -> Result<Clip, ControlsError> {
        clip_with_controls(clip, &controls(lanes), loop_beats, BAR, device)
    }

    fn empty() -> Clip {
        Clip::new(BAR, Vec::new()).unwrap()
    }

    #[test]
    fn names_resolve_through_the_device_then_the_standard_then_numbers() {
        let synth = synth();
        let cases = [
            ("filter", Some(&synth), 74),
            ("wheel", Some(&synth), 1),
            // The device's own meaning of a standard name comes first.
            ("modulation", Some(&synth), 33),
            ("modulation", None, 1),
            ("cutoff", Some(&synth), 74),
            ("expression", None, 11),
            ("7", Some(&synth), 7),
            ("0", None, 0),
            ("119", None, 119),
        ];
        for (name, device, expected) in cases {
            assert_eq!(resolve(name, device), Ok(cc(expected)), "{name}");
        }
        // Pitch bend and pressure aren't controllers, on any track.
        for device in [None, Some(&synth)] {
            assert_eq!(resolve("pitch_bend", device), Ok(Control::PitchBend));
            assert_eq!(resolve("pressure", device), Ok(Control::Pressure));
        }
        assert_eq!(
            resolve("120", None),
            Err(ControlsError::NumberOutOfRange("120".into()))
        );
        assert_eq!(
            resolve("300", None),
            Err(ControlsError::NumberOutOfRange("300".into()))
        );
        assert_eq!(
            resolve("wobble", None),
            Err(ControlsError::UnknownControl("wobble".into()))
        );
        let Err(ControlsError::UnknownParam { params, .. }) = resolve("wobble", Some(&synth))
        else {
            panic!("a name the device doesn't have");
        };
        assert_eq!(params, "filter, modulation, wheel");
    }

    #[test]
    fn channel_mode_messages_are_not_controls() {
        let mut device = synth();
        device.params.insert(
            "all_sound_off".into(),
            Param {
                cc: U7::new(120).unwrap(),
            },
        );
        assert_eq!(
            resolve("all_sound_off", Some(&device)),
            Err(ControlsError::ModeMessage {
                name: "all_sound_off".into(),
                controller: U7::new(120).unwrap(),
            })
        );
    }

    #[test]
    fn names_read_back_as_names_that_resolve_to_the_same_controller() {
        let synth = synth();
        let drums = drums();
        let mut unnamed = synth.clone();
        unnamed.params.remove("wheel");
        for (control, device, expected) in [
            (cc(74), Some(&synth), "filter"),
            (cc(1), Some(&synth), "wheel"),
            (cc(33), Some(&synth), "modulation"),
            // The synth calls 33 modulation, so 1 can't go by that name.
            (cc(1), None, "modulation"),
            (cc(7), Some(&drums), "volume"),
            (cc(7), Some(&synth), "volume"),
            (cc(3), None, "3"),
            // Without its own name for 1, the synth's modulation still means 33.
            (cc(1), Some(&unnamed), "1"),
            (Control::PitchBend, Some(&synth), "pitch_bend"),
            (Control::Pressure, None, "pressure"),
        ] {
            let name = control_name(control, device);
            assert_eq!(name, expected);
            assert_eq!(resolve(&name, device), Ok(control), "{name}");
        }
    }

    #[test]
    fn jumps_send_their_values_where_they_are() {
        let lanes = vec![(
            "modulation",
            lane(
                vec![at(2.0, 127, None), at(0.0, 0, None), at(3.5, 64, None)],
                None,
            ),
        )];
        let clip = set(&empty(), lanes, None, None).unwrap();
        assert_eq!(clip.length(), BAR);
        assert_eq!(changes(&clip, 1), [(0, 0), (1920, 127), (3360, 64)]);
    }

    #[test]
    fn linear_ramps_step_evenly_and_send_only_changes() {
        // From 0 to 4 over one beat: a new value every quarter of it.
        let lanes = vec![(
            "1",
            lane(
                vec![at(0.0, 0, None), at(1.0, 4, Some(Ramp::Linear))],
                Some(2.0),
            ),
        )];
        let clip = set(&empty(), lanes, None, None).unwrap();
        // Values round half away from zero: 1 at 1/8 of a beat, then every
        // quarter. The pass at beat 2 repeats it.
        let pass = [(0, 0), (120, 1), (360, 2), (600, 3), (840, 4), (960, 4)];
        let expected: Vec<(u64, u8)> = pass
            .iter()
            .chain(&pass)
            .enumerate()
            .map(|(index, &(tick, value))| (tick + if index < 6 { 0 } else { 1920 }, value))
            .collect();
        assert_eq!(changes(&clip, 1), expected);
        // Every ramp value sits on the 1/32-beat grid.
        assert!(
            changes(&clip, 1)
                .iter()
                .all(|&(tick, _)| tick.is_multiple_of(RAMP_STEP.0))
        );
    }

    #[test]
    fn ramps_down_mirror_ramps_up() {
        let up = vec![(
            "1",
            lane(vec![at(0.0, 0, None), at(1.0, 4, Some(Ramp::Linear))], None),
        )];
        let down = vec![(
            "1",
            lane(vec![at(0.0, 4, None), at(1.0, 0, Some(Ramp::Linear))], None),
        )];
        let up = changes(&set(&empty(), up, None, None).unwrap(), 1);
        let down = changes(&set(&empty(), down, None, None).unwrap(), 1);
        let mirrored: Vec<_> = up.iter().map(|&(tick, value)| (tick, 4 - value)).collect();
        assert_eq!(down, mirrored);
    }

    #[test]
    fn smooth_ramps_between_a_low_and_a_high_make_a_sine_wave() {
        let lfo = vec![(
            "cutoff",
            lane(
                vec![
                    at(0.0, 0, None),
                    at(0.5, 127, Some(Ramp::Smooth)),
                    at(1.0, 0, Some(Ramp::Smooth)),
                ],
                Some(1.0),
            ),
        )];
        let clip = set(&empty(), lfo, None, None).unwrap();
        let sent = changes(&clip, 74);
        // A cosine from its trough: each change sits where the wave is.
        let mut last = None;
        for &(tick, value) in &sent {
            let phase = (tick % PPQN) as f64 / PPQN as f64;
            let wave = 63.5 - 63.5 * (2.0 * PI * phase).cos();
            assert!(
                (f64::from(value) - wave).abs() <= 0.5 + 1e-9,
                "{tick}: {value} vs {wave}"
            );
            assert_ne!(Some(value), last, "{tick} repeats a value");
            last = Some(value);
        }
        // Four cycles in the bar, each with its low and its high.
        for beat in 0..4 {
            assert!(sent.contains(&(beat * PPQN, 0)));
            assert!(sent.contains(&(beat * PPQN + PPQN / 2, 127)));
        }
        // At most one change on each step of the grid.
        assert!(
            sent.windows(2)
                .all(|pair| pair[1].0 - pair[0].0 >= RAMP_STEP.0)
        );
    }

    #[test]
    fn a_repeat_that_doesnt_divide_the_loop_starts_again_with_it() {
        let lanes = vec![(
            "1",
            lane(
                vec![at(0.0, 10, None), at(1.0, 20, None), at(2.5, 30, None)],
                Some(3.0),
            ),
        )];
        // A 3-beat repeat in a 4-beat loop: the second pass is cut off.
        let clip = set(&empty(), lanes, Some(4.0), None).unwrap();
        assert_eq!(
            changes(&clip, 1),
            [(0, 10), (960, 20), (2400, 30), (2880, 10)]
        );
    }

    #[test]
    fn ramps_are_cut_off_at_the_end_of_the_loop() {
        let lanes = vec![(
            "1",
            lane(
                vec![at(0.0, 0, None), at(6.0, 120, Some(Ramp::Linear))],
                None,
            ),
        )];
        let clip = set(&empty(), lanes, Some(4.0), None);
        // The point is past the loop, so the call says how to fix it.
        let Err(ControlsError::Control { error, .. }) = clip else {
            panic!("a point past the loop");
        };
        assert!(
            error.to_string().contains("pass a longer loop_beats"),
            "{error}"
        );

        // In a repeat longer than the loop's last pass, the ramp stops at
        // the loop's end.
        let lanes = vec![(
            "1",
            lane(
                vec![at(0.0, 0, None), at(3.0, 120, Some(Ramp::Linear))],
                Some(3.0),
            ),
        )];
        let clip = set(&empty(), lanes, Some(4.0), None).unwrap();
        let sent = changes(&clip, 1);
        let second_pass: Vec<_> = sent.iter().filter(|&&(tick, _)| tick >= 2880).collect();
        assert_eq!(second_pass.first(), Some(&&(2880, 0)));
        assert!(second_pass.iter().all(|&&(tick, _)| tick < 3840));
        assert_eq!(second_pass.last(), Some(&&(3810, 39)));
    }

    #[test]
    fn the_loop_follows_the_controls_on_a_track_of_nothing_else() {
        let swell = |end: f64, ramp| {
            vec![(
                "modulation",
                lane(vec![at(0.0, 0, None), at(end, 127, ramp)], None),
            )]
        };
        let length = |lanes, loop_beats| set(&empty(), lanes, loop_beats, None).unwrap().length().0;
        // A ramp may end on the loop's end; a jump must come before it.
        assert_eq!(length(swell(8.0, Some(Ramp::Linear)), None), 2 * BAR.0);
        assert_eq!(length(swell(8.0, None), None), 3 * BAR.0);
        assert_eq!(length(swell(1.0, None), None), BAR.0);
        assert_eq!(length(swell(8.0, Some(Ramp::Linear)), Some(10.0)), 9600);
        // A repeat sets the loop's length too, rounded up to the bar.
        let lfo = vec![(
            "1",
            lane(
                vec![at(0.0, 0, None), at(3.0, 127, Some(Ramp::Linear))],
                Some(6.0),
            ),
        )];
        assert_eq!(length(lfo, None), 2 * BAR.0);
    }

    #[test]
    fn notes_keep_their_loop_and_the_controls_join_them() {
        let bass = Clip::new(Ticks(2 * BAR.0), vec![note(0, 960), note(4800, 960)]).unwrap();
        let swell = || vec![("1", lane(vec![at(0.0, 0, None), at(2.0, 64, None)], None))];
        let clip = set(&bass, swell(), None, None).unwrap();
        assert_eq!(clip.length(), bass.length());
        assert_eq!(changes(&clip, 1), [(0, 0), (1920, 64)]);
        assert_eq!(
            clip.events()
                .iter()
                .filter(|event| matches!(event.kind, EventKind::Note { .. }))
                .count(),
            2
        );
        // The same loop length may be given; another may not.
        assert!(set(&bass, swell(), Some(8.0), None).is_ok());
        assert_eq!(
            set(&bass, swell(), Some(4.0), None),
            Err(ControlsError::LoopFollowsNotes(8.0))
        );
        // A point past the notes' loop says to lengthen it with the notes.
        let late = vec![("1", lane(vec![at(9.0, 1, None)], None))];
        let Err(ControlsError::Control { error, .. }) = set(&bass, late, None, None) else {
            panic!("a point past the loop");
        };
        assert!(
            error.to_string().contains("set_notes or set_steps"),
            "{error}"
        );

        // Only notes hold the loop: a program change stays, but goes with
        // the loop the controls ask for.
        let program = ClipEvent {
            start: Ticks::ZERO,
            kind: EventKind::Program {
                program: U7::new(5).unwrap(),
            },
        };
        let voiced = Clip::new(BAR, vec![program]).unwrap();
        let clip = set(&voiced, swell(), Some(8.0), None).unwrap();
        assert_eq!(clip.length(), Ticks(2 * BAR.0));
        assert_eq!(clip.events()[0], program);
    }

    #[test]
    fn controls_replace_only_their_own_changes() {
        let first = set(
            &empty(),
            vec![
                ("modulation", lane(vec![at(0.0, 10, None)], None)),
                ("cutoff", lane(vec![at(1.0, 20, None)], None)),
            ],
            None,
            None,
        )
        .unwrap();
        let second = set(
            &first,
            vec![("modulation", lane(vec![at(2.0, 30, None)], None))],
            None,
            None,
        )
        .unwrap();
        assert_eq!(changes(&second, 1), [(1920, 30)]);
        assert_eq!(changes(&second, 74), [(960, 20)]);
        // No points removes a control and keeps the loop.
        let removed = set(
            &second,
            vec![("cutoff", lane(Vec::new(), None))],
            None,
            None,
        )
        .unwrap();
        assert_eq!(changes(&removed, 74), []);
        assert_eq!(changes(&removed, 1), [(1920, 30)]);
        let long = set(
            &empty(),
            vec![("1", lane(vec![at(6.0, 5, None)], None))],
            None,
            None,
        )
        .unwrap();
        let cleared = set(&long, vec![("1", lane(Vec::new(), None))], None, None).unwrap();
        assert_eq!(
            (cleared.length(), cleared.events().len()),
            (long.length(), 0)
        );
        // A shorter loop cuts off the other controls.
        let cut = set(
            &first,
            vec![("modulation", lane(vec![at(0.0, 1, None)], None))],
            Some(0.5),
            None,
        )
        .unwrap();
        assert_eq!(
            (changes(&cut, 1), changes(&cut, 74)),
            (vec![(0, 1)], vec![])
        );
    }

    #[test]
    fn writing_notes_keeps_the_controls_that_fit() {
        let controlled = set(
            &Clip::new(Ticks(2 * BAR.0), vec![note(0, 960)]).unwrap(),
            vec![("1", lane(vec![at(1.0, 5, None), at(6.0, 6, None)], None))],
            None,
            None,
        )
        .unwrap();
        let notes = Clip::new(BAR, vec![note(480, 240)]).unwrap();
        let clip = keep_controls(&controlled, notes).unwrap();
        assert_eq!(clip.length(), BAR);
        assert_eq!(changes(&clip, 1), [(960, 5)]);
        let starts: Vec<u64> = clip.events().iter().map(|event| event.start.0).collect();
        assert_eq!(starts, [480, 960], "the old note is gone");
    }

    #[test]
    fn bad_controls_say_how_to_fix_them() {
        let one = |points, repeat| vec![("modulation", lane(points, repeat))];
        // The controls, loop_beats, and what the error says.
        type Case<'a> = (Vec<(&'a str, ControlLane)>, Option<f64>, &'a str);
        let cases: Vec<Case<'_>> = vec![
            (Vec::new(), None, "give at least one control"),
            (
                vec![
                    ("modulation", lane(vec![], None)),
                    ("1", lane(vec![], None)),
                ],
                None,
                "`1` and `modulation` are both controller 1",
            ),
            (
                vec![("121", lane(vec![], None))],
                None,
                "controls are 0-119",
            ),
            (
                vec![("wobble", lane(vec![], None))],
                None,
                "give the track a device",
            ),
            (
                one(vec![at(0.0, 128, None)], None),
                None,
                "`modulation`: point 1: value must be 0-127, not 128",
            ),
            (
                one(vec![at(0.0, -1, None)], None),
                None,
                "`modulation`: point 1: value must be 0-127, not -1",
            ),
            (
                vec![("pressure", lane(vec![at(0.0, 128, None)], None))],
                None,
                "`pressure`: point 1: value must be 0-127, not 128",
            ),
            (
                vec![("pitch_bend", lane(vec![at(0.0, 8192, None)], None))],
                None,
                "`pitch_bend`: point 1: value must be -8192 to 8191, with 0 the centre, not 8192",
            ),
            (
                vec![("pitch_bend", lane(vec![at(0.0, i32::MIN, None)], None))],
                None,
                "not -2147483648",
            ),
            (
                one(vec![at(-1.0, 1, None)], None),
                None,
                "at must be from 0",
            ),
            (
                one(vec![at(f64::NAN, 1, None)], None),
                None,
                "at must be from 0",
            ),
            (
                one(
                    vec![
                        at(1.0, 1, None),
                        at(0.0, 2, None),
                        at(1.0, 3, Some(Ramp::Linear)),
                    ],
                    None,
                ),
                None,
                "points 1 and 3 are both at beat 1",
            ),
            (
                one(vec![at(1.0, 1, None), at(0.5, 2, Some(Ramp::Linear))], None),
                None,
                "point 2: it ramps, but it's the first point",
            ),
            (
                one(vec![at(0.0, 1, None), at(4.0, 2, None)], None),
                Some(4.0),
                "point 2: it jumps at the end of the loop",
            ),
            (
                one(vec![at(0.0, 1, None), at(2.0, 2, None)], Some(2.0)),
                None,
                "jumps at the end of the repeat",
            ),
            (
                one(vec![at(0.0, 1, None), at(3.0, 2, None)], Some(2.0)),
                None,
                "after the repeat ends at beat 2: move it earlier, or make repeat_beats longer",
            ),
            (
                one(vec![at(0.0, 1, None)], Some(8.0)),
                Some(4.0),
                "repeat_beats is 8, longer than the 4-beat loop",
            ),
            (
                one(vec![at(0.0, 1, None)], Some(0.0)),
                None,
                "repeat_beats must be",
            ),
            (
                one(vec![at(0.0, 1, None)], None),
                Some(0.0),
                "loop_beats must be",
            ),
            (
                one(vec![at(0.0, 1, None)], None),
                Some(2000.0),
                "loop_beats must be",
            ),
            (
                one(vec![at(0.0, 1, None); MAX_POINTS + 1], None),
                None,
                "8193 points is more than the 8192",
            ),
        ];
        for (lanes, loop_beats, expected) in cases {
            let error = set(&empty(), lanes, loop_beats, None)
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }
        // 7/8 bars don't divide the longest loop, so rounding up the last
        // point's bar can pass it.
        let seven_eight = TimeSignature::new(7, 8).unwrap().bar_length();
        let late = controls(vec![("1", lane(vec![at(1023.5, 1, None)], None))]);
        assert_eq!(
            clip_with_controls(&empty(), &late, None, seven_eight, None),
            Err(ControlsError::PastLongestLoop(1025.5))
        );
    }

    #[test]
    fn a_clip_holds_a_bounded_number_of_control_changes() {
        // A control can have a point for each change a clip holds.
        let points = (0..MAX_POINTS)
            .map(|tick| at(tick as f64 / 960.0, i32::from(tick % 2 == 1), None))
            .collect();
        let full = set(&empty(), vec![("1", lane(points, None))], None, None).unwrap();
        assert_eq!(changes(&full, 1).len(), MAX_CONTROL_CHANGES);
        // Changes already on the track count too.
        let another = vec![("2", lane(vec![at(0.0, 1, None)], None))];
        assert_eq!(
            set(&full, another, None, None),
            Err(ControlsError::TooManyChanges)
        );
        // A jump every tick of a 1-tick repeat, one tick too long.
        let buzz = vec![("1", lane(vec![at(0.0, 1, None)], Some(1.0 / 960.0)))];
        let loop_beats = (MAX_CONTROL_CHANGES + 1) as f64 / 960.0;
        assert_eq!(
            set(&empty(), buzz, Some(loop_beats), None),
            Err(ControlsError::TooManyChanges)
        );
    }

    #[test]
    fn ramps_send_on_the_grid_wherever_their_points_are() {
        // Points at ticks 10 and 970, off the grid.
        let lanes = vec![(
            "1",
            lane(
                vec![
                    at(10.0 / 960.0, 0, None),
                    at(970.0 / 960.0, 64, Some(Ramp::Linear)),
                ],
                None,
            ),
        )];
        let sent = changes(&set(&empty(), lanes, None, None).unwrap(), 1);
        assert_eq!(
            (sent.first(), sent.last()),
            (Some(&(10, 0)), Some(&(970, 64)))
        );
        let ramp = &sent[1..sent.len() - 1];
        assert_eq!(ramp.first(), Some(&(30, 1)));
        assert!(
            ramp.iter()
                .all(|&(tick, _)| tick.is_multiple_of(RAMP_STEP.0)),
            "{ramp:?}"
        );
    }

    /// Reads `clip`'s controls back, writes them back over the same loop,
    /// and checks the clip is the same.
    fn round_trip(clip: &Clip) -> BTreeMap<Control, Vec<Point>> {
        let read = controls_from_clip(clip);
        let lanes = read
            .iter()
            .map(|(&control, points)| {
                let points = points
                    .iter()
                    .map(|point| at(beats(point.at), control.written(point.value), point.ramp))
                    .collect();
                (control_name(control, None), lane(points, None))
            })
            .collect();
        let rewritten =
            clip_with_controls(clip, &lanes, Some(beats(clip.length())), BAR, None).unwrap();
        assert_eq!(rewritten, *clip);
        read
    }

    /// Points as (beat, value as sent, ramp).
    fn written(points: &[Point]) -> Vec<(f64, u16, Option<Ramp>)> {
        points
            .iter()
            .map(|point| (beats(point.at), point.value, point.ramp))
            .collect()
    }

    /// The pitch bends `clip` sends, as (tick, value as sent).
    fn bends(clip: &Clip) -> Vec<(u64, u16)> {
        clip.events()
            .iter()
            .filter_map(|event| match event.kind {
                EventKind::PitchBend { value } => Some((event.start.0, value.get())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn pitch_bend_is_written_around_the_centre_and_sent_as_14_bits() {
        let points = vec![
            at(0.0, -8192, None),
            at(1.0, 8191, None),
            at(2.0, 0, None),
            at(3.0, -1, None),
        ];
        let clip = set(
            &empty(),
            vec![("pitch_bend", lane(points, None))],
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            bends(&clip),
            [(0, 0), (960, 16_383), (1920, 8192), (2880, 8191)]
        );
        let read = round_trip(&clip);
        let written: Vec<_> = read[&Control::PitchBend]
            .iter()
            .map(|point| Control::PitchBend.written(point.value))
            .collect();
        assert_eq!(written, [-8192, 8191, 0, -1]);
    }

    #[test]
    fn bend_ramps_move_on_every_step_of_the_grid_and_read_back_as_ramps() {
        // Up a whole range over a beat, then smoothly back over two: every
        // 1/32 beat sends a new value.
        let points = vec![
            at(0.0, 0, None),
            at(1.0, 8191, Some(Ramp::Linear)),
            at(3.0, 0, Some(Ramp::Smooth)),
            at(3.5, -4096, None),
            // Back to the centre at the loop's end, where the next pass
            // starts again.
            at(4.0, 0, Some(Ramp::Linear)),
        ];
        let clip = set(
            &empty(),
            vec![("pitch_bend", lane(points.clone(), None))],
            None,
            None,
        )
        .unwrap();
        let sent = bends(&clip);
        // The first point and a value at each of the 31 steps on its way to
        // the top, which the second point sends; 63 steps and the point on
        // the way down; then the jump and 15 steps of the last ramp, which
        // ends where the next pass starts again.
        assert_eq!(sent.len(), 1 + 31 + 1 + 63 + 1 + 1 + 15);
        assert!(sent.iter().all(|(tick, _)| tick % RAMP_STEP.0 == 0));
        assert_eq!(sent[32], (960, 16_383));
        let read = round_trip(&clip);
        let read_back: Vec<_> = read[&Control::PitchBend]
            .iter()
            .map(|point| {
                let value = Control::PitchBend.written(point.value);
                (beats(point.at), value, point.ramp)
            })
            .collect();
        let written: Vec<_> = points
            .iter()
            .map(|point| (point.at, point.value, point.ramp))
            .collect();
        assert_eq!(read_back, written);
    }

    #[test]
    fn pressure_is_a_control_of_its_own() {
        let swell = |value| {
            lane(
                vec![at(0.0, 0, None), at(2.0, value, Some(Ramp::Linear))],
                None,
            )
        };
        let clip = set(
            &empty(),
            vec![("pressure", swell(127)), ("expression", swell(127))],
            None,
            None,
        )
        .unwrap();
        let pressure = |clip: &Clip| {
            clip.events()
                .iter()
                .filter_map(|event| match event.kind {
                    EventKind::Pressure { value } => Some((event.start.0, value.get())),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        // It moves just as a controller does.
        assert_eq!(pressure(&clip), changes(&clip, 11));
        // Setting it again replaces only its own changes, and no points
        // remove them.
        let quieter = set(&clip, vec![("pressure", swell(10))], None, None).unwrap();
        assert_eq!(pressure(&quieter).last(), Some(&(1920, 10)));
        assert_eq!(changes(&quieter, 11), changes(&clip, 11));
        let gone = set(&clip, vec![("pressure", lane(vec![], None))], None, None).unwrap();
        assert_eq!(pressure(&gone), []);
        assert_eq!(changes(&gone, 11), changes(&clip, 11));
        let read = controls_from_clip(&clip);
        assert_eq!(
            read.keys().copied().collect::<Vec<_>>(),
            [cc(11), Control::Pressure]
        );
    }

    #[test]
    fn controls_read_back_as_the_points_that_wrote_them() {
        let linear = Some(Ramp::Linear);
        let smooth = Some(Ramp::Smooth);
        let cases = [
            vec![(0.0, 0, None), (4.0, 127, linear)],
            vec![(0.0, 64, None), (1.5, 0, None), (3.0, 100, None)],
            vec![(0.0, 0, None), (2.0, 64, linear), (3.0, 100, None)],
            vec![(0.0, 0, None), (1.0, 127, linear), (2.0, 127, None)],
            vec![(0.0, 20, None), (2.0, 90, smooth), (4.0, 20, smooth)],
            vec![
                (0.5, 30, None),
                (2.5, 31, linear),
                (3.0, 90, None),
                (4.0, 0, smooth),
            ],
            // Slow ramps that send only a few values.
            vec![(0.0, 64, None), (4.0, 66, linear)],
            vec![(0.0, 64, None), (4.0, 67, linear)],
            // Ramps to 52-55 would all send 51 here; the loop's first value
            // wins.
            vec![
                (0.0, 54, None),
                (3800.0 / 960.0, 50, None),
                (4.0, 54, linear),
            ],
        ];
        for points in cases {
            let specs = points
                .iter()
                .map(|&(beat, value, ramp)| at(beat, i32::from(value), ramp))
                .collect();
            let clip = set(&empty(), vec![("1", lane(specs, None))], Some(4.0), None).unwrap();
            let read = round_trip(&clip);
            assert_eq!(written(&read[&cc(1)]), points);
        }
    }

    #[test]
    fn repeats_read_back_as_every_pass() {
        // A sine LFO once a beat reads back as its highs and lows.
        let lfo = vec![(
            "1",
            lane(
                vec![
                    at(0.0, 32, None),
                    at(0.5, 96, Some(Ramp::Smooth)),
                    at(1.0, 32, Some(Ramp::Smooth)),
                ],
                Some(1.0),
            ),
        )];
        let clip = set(&empty(), lfo, None, None).unwrap();
        let read = round_trip(&clip);
        let mut expected = vec![(0.0, 32, None)];
        for half in 1..=8 {
            let value = if half % 2 == 1 { 96 } else { 32 };
            expected.push((f64::from(half) / 2.0, value, Some(Ramp::Smooth)));
        }
        assert_eq!(written(&read[&cc(1)]), expected);

        // A step sequence repeated every bar reads back as every step of
        // every pass, and writes back the same, however many there are.
        let steps = (0..16)
            .map(|step| at(f64::from(step) / 4.0, 8 * step, None))
            .collect();
        let sequence = vec![("1", lane(steps, Some(4.0)))];
        let clip = set(&empty(), sequence, Some(512.0), None).unwrap();
        assert_eq!(round_trip(&clip)[&cc(1)].len(), 16 * 128);
    }

    #[test]
    fn arbitrary_changes_round_trip_through_public_write_validation() {
        // Changes no ramp makes, including two on one tick, off the grid,
        // and runs that look like a ramp for a while.
        let control = |start, value| ClipEvent {
            start: Ticks(start),
            kind: EventKind::Control {
                controller: U7::new(7).unwrap(),
                value: U7::new(value).unwrap(),
            },
        };
        let mut seed = 7u64;
        let mut random = move |below: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % below
        };
        for _ in 0..200 {
            let mut events = Vec::new();
            let mut tick = random(3) * 30;
            let mut value = random(128) as u8;
            for _ in 0..random(40) {
                events.push(control(tick, value));
                tick += [0, 1, 30, 30, 60, 90][random(6) as usize];
                let step = random(5) as u8;
                value = if random(2) == 0 {
                    value.saturating_add(step).min(127)
                } else {
                    value.saturating_sub(step)
                };
            }
            let length = Ticks(tick + 1 + random(200));
            let clip = Clip::new(length, events.clone()).unwrap();
            if !events.is_empty() {
                round_trip(&clip);
            }
        }
    }

    #[test]
    fn new_changes_follow_the_others_on_their_tick() {
        let control = |controller, value| ClipEvent {
            start: Ticks::ZERO,
            kind: EventKind::Control {
                controller: U7::new(controller).unwrap(),
                value: U7::new(value).unwrap(),
            },
        };
        // RPN 0 is selected on tick 0, so Data Entry, new to the clip,
        // must come after it.
        let rpn = Clip::new(BAR, vec![control(101, 0), control(100, 0)]).unwrap();
        let entry = vec![("6", lane(vec![at(0.0, 12, None)], None))];
        let clip = set(&rpn, entry, None, None).unwrap();
        assert_eq!(
            clip.events(),
            [control(101, 0), control(100, 0), control(6, 12)]
        );
    }

    #[test]
    fn same_tick_jumps_are_written_in_input_order_including_repeats() {
        let points = vec![at(0.0, 100, None), at(0.0, 20, None), at(0.0, 20, None)];
        let clip = set(
            &empty(),
            vec![("7", lane(points, Some(1.0)))],
            Some(2.0),
            None,
        )
        .unwrap();
        assert_eq!(
            changes(&clip, 7),
            [(0, 100), (0, 20), (0, 20), (960, 100), (960, 20), (960, 20)]
        );
        round_trip(&clip);
    }

    #[test]
    fn reading_back_stops_working_out_ramps_when_its_budget_runs_out() {
        // A long, slow ramp: it checks out with budget to spare, and falls
        // back to jumps without it.
        let changes: Vec<(u64, u16)> =
            RampValues::new(Ramp::Linear, (0, 0), (MAX_LOOP, 127), MAX_LOOP).collect();
        let start = [(0, 0)];
        let all: Vec<_> = start.iter().chain(&changes).copied().collect();
        let mut budget = READ_BUDGET;
        assert!(sends(
            &all,
            0,
            (MAX_LOOP, 127),
            all.len(),
            Ramp::Linear,
            &mut budget
        ));
        assert_eq!(READ_BUDGET - budget, MAX_LOOP / RAMP_STEP.0 - 1);
        let mut empty = 0;
        assert!(!sends(
            &all,
            0,
            (MAX_LOOP, 127),
            all.len(),
            Ramp::Linear,
            &mut empty
        ));
        // Reading back, the ramp is one point, or without budget, a jump
        // for each change.
        let mut budget = READ_BUDGET;
        let read_back = read(&all, MAX_LOOP, 127, &mut budget);
        assert_eq!(
            written(&read_back),
            [(0.0, 0, None), (1024.0, 127, Some(Ramp::Linear))]
        );
        let jumps = read(&all, MAX_LOOP, 127, &mut empty);
        assert_eq!(jumps.len(), all.len());
        assert!(jumps.iter().all(|point| point.ramp.is_none()));
    }
}
