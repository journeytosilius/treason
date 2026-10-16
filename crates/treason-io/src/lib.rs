//! MIDI backends: implementations of
//! [`MidiOutput`](treason_core::output::MidiOutput) for real ports and files.
//!
//! Each backend sits behind a Cargo feature, so a program compiles and links
//! only the system libraries it uses:
//!
//! - `midir` (default): `MidirOutput`, portable real-time output with
//!   immediate delivery, including virtual ports on Linux and macOS. On Linux
//!   it needs the ALSA development package (`libasound2-dev`).
//! - `alsa-seq`: `AlsaSeqOutput`, an ALSA sequencer queue on Linux. Scheduled
//!   delivery: the engine hands messages over up to the lookahead early, and
//!   the kernel sends each at its time, within a millisecond.
//! - `smf`: `smf::write`, Standard MIDI Files through `midly`. Not
//!   implemented yet.
//!
//! Always available: [`RecordingOutput`], which keeps messages in memory for
//! tests and offline rendering, and [`PortSpec`], which says which ports to
//! open.

pub mod memory;
mod port;

#[cfg(feature = "alsa-seq")]
pub mod alsa_seq;
#[cfg(feature = "midir")]
pub mod midir_output;
#[cfg(feature = "smf")]
pub mod smf;

pub use memory::RecordingOutput;
pub use port::PortSpec;

#[cfg(feature = "alsa-seq")]
pub use alsa_seq::{AlsaSeqError, AlsaSeqOutput};
#[cfg(feature = "midir")]
pub use midir_output::{MidirError, MidirOutput};
