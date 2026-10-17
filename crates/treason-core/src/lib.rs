//! Shared vocabulary for treason.
//!
//! Every other crate depends on this one, and it depends on none of ours. It
//! holds:
//!
//! - [`midi`]: MIDI values and [`MidiMessage`](midi::MidiMessage).
//! - [`time`]: musical time in ticks at [`PPQN`](time::PPQN), tempo and meter.
//! - [`model`]: the project the engine plays and the agent edits.
//! - [`output`], [`playback`] and [`export`]: the traits that connect the
//!   other crates, so that none of them depends on another directly.
//!
//! No I/O, no threads, no async: plain data and pure functions.

pub mod export;
pub mod midi;
pub mod model;
pub mod output;
pub mod playback;
pub mod time;
