//! The real-time playback engine.
//!
//! The engine runs on its own thread. It reads the published
//! [`Project`](treason_core::model::Project), renders the events due in a
//! short lookahead window and hands them to a
//! [`MidiOutput`](treason_core::output::MidiOutput). It depends only on
//! `treason-core`: the output and the [`Clock`] are injected, so the same
//! engine can drive hardware in real time, render a file faster than real
//! time, or run deterministic tests.
//!
//! Rules for the engine thread: no locks, no allocating or freeing in steady
//! state, and no blocking except on the output. It talks to the rest of the
//! program only through its [`EngineLink`]: a wait-free snapshot pointer, a
//! lock-free command queue and status atomics.
//!
//! - [`link`](mod@link): the connection to the control side.
//! - [`runtime`]: the [`Engine`] and its thread.
//! - [`transport`]: play, stop, continue, locate and panic, and the MIDI
//!   transport messages they send.
//! - [`timebase`]: ticks to wall-clock time and back.
//! - [`render`]: the MIDI events clips and the clock play in a window.
//! - [`notes`]: sounding notes, so they can always be ended.
//! - [`clock`]: time sources.

pub mod clock;
pub mod link;
pub mod notes;
pub mod render;
pub mod runtime;
pub mod timebase;
pub mod transport;

pub use clock::{Clock, ManualClock, SystemClock};
pub use link::{EngineHandle, EngineLink, link};
pub use notes::ActiveNotes;
pub use render::RenderedEvent;
pub use runtime::{Engine, EngineConfig, EngineError, EngineThread, PortSettings};
pub use timebase::Timebase;
pub use transport::{Transport, TransportEffect};
