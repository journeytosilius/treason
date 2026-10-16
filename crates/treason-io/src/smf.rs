//! Standard MIDI File export.
//!
//! Not implemented yet. Export reuses the live engine: run it with a
//! `ManualClock` into a [`RecordingOutput`](crate::RecordingOutput), which
//! takes no real time, then [`write`](fn@write) the recording.

use std::io;

use treason_core::output::TimedMessage;
use treason_core::time::Bpm;

/// Writes `messages`, in tick order, as a format 1 MIDI file at 960 PPQN: a
/// tempo track, then one track per port.
#[expect(unused_variables, reason = "not implemented yet")]
pub fn write(messages: &[TimedMessage], tempo: Bpm, out: impl io::Write) -> io::Result<()> {
    todo!("convert to midly tracks and write them")
}
