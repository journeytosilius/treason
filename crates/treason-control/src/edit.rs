//! Changes to a project.

use treason_core::model::{Clip, Track, TrackId};
use treason_core::time::{Bpm, TimeSignature};

/// One change to the project, in resolved MIDI terms.
///
/// Adapters translate requests into edits. For example, the MCP server turns
/// "kick on every beat" into a [`Clip`] using the device profiles, then applies
/// [`Edit::SetClip`]. An edit is either applied in full or rejected.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    /// Changes the tempo. Takes effect at once.
    SetTempo(Bpm),
    /// Changes the meter.
    SetTimeSignature(TimeSignature),
    /// Adds a track. Its id must be new.
    AddTrack(Track),
    /// Removes a track.
    RemoveTrack(TrackId),
    /// Replaces a track's clip. Takes effect at the next bar.
    SetClip { track: TrackId, clip: Clip },
    /// Mutes or unmutes a track.
    SetMuted { track: TrackId, muted: bool },
}
