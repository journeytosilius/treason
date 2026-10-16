//! The engine thread.

use std::io;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use thiserror::Error;
use treason_core::output::MidiOutput;

use crate::clock::Clock;
use crate::link::EngineLink;
use crate::notes::ActiveNotes;
use crate::render::RenderedEvent;
use crate::transport::Transport;

/// How many events one window can hold before rendering has to allocate.
const RENDER_CAPACITY: usize = 4096;

/// The engine thread's `SCHED_FIFO` priority on Unix, from 1 to 99.
#[cfg(unix)]
const REALTIME_PRIORITY: u8 = 70;

/// Settings for one output port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortSettings {
    /// How long the device takes to respond. Its messages are sent this much
    /// earlier, so every port sounds in time.
    pub latency: Duration,
    /// Whether the port gets MIDI clock and transport messages.
    pub clock: bool,
}

/// How the engine runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    /// How far ahead the engine renders. Longer rides out scheduling hiccups;
    /// shorter makes commands take effect sooner.
    pub lookahead: Duration,
    /// One entry per output port, in `PortId` order.
    pub ports: Vec<PortSettings>,
    /// Whether to ask the OS for real-time scheduling for the engine thread.
    pub realtime: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            lookahead: Duration::from_millis(20),
            ports: Vec::new(),
            realtime: true,
        }
    }
}

/// Everything the engine thread owns.
///
/// Generic over the output and the clock: [`SystemClock`] and a MIDI port
/// for live playback, `ManualClock` and a recording to render offline or to
/// test.
///
/// [`SystemClock`]: crate::SystemClock
#[expect(
    dead_code,
    reason = "read by the engine loop, which is not written yet"
)]
pub struct Engine<O, C> {
    link: EngineLink,
    output: O,
    clock: C,
    config: EngineConfig,
    transport: Transport,
    sounding: Vec<ActiveNotes>,
    rendered: Vec<RenderedEvent>,
}

impl<O: MidiOutput, C: Clock> Engine<O, C> {
    /// An engine that plays what arrives through `link` on `output`, timed by
    /// `clock`.
    pub fn new(link: EngineLink, output: O, clock: C, config: EngineConfig) -> Self {
        Self {
            link,
            output,
            clock,
            transport: Transport::new(),
            sounding: vec![ActiveNotes::new(); config.ports.len()],
            rendered: Vec::with_capacity(RENDER_CAPACITY),
            config,
        }
    }

    /// Runs on the calling thread until the
    /// [`EngineHandle`](crate::EngineHandle) is dropped, then ends every
    /// sounding note and returns. Returns early if the output fails.
    ///
    /// Each cycle: apply waiting commands, pick up a new snapshot (tempo at
    /// once, clips at the next bar), render the lookahead window, wait until
    /// each event is due (or hand it over early to a scheduled output), send,
    /// and report the position.
    pub fn run(self) -> Result<(), O::Error> {
        todo!("the engine loop; see ARCHITECTURE.md, Next steps")
    }
}

impl<O, C> Engine<O, C>
where
    O: MidiOutput + 'static,
    C: Clock + Send + 'static,
{
    /// Runs the engine on a new thread named `treason-engine`, at real-time
    /// priority if the configuration asks for it and the OS allows it.
    pub fn spawn(self) -> io::Result<EngineThread<O::Error>> {
        let thread = thread::Builder::new()
            .name("treason-engine".to_owned())
            .spawn(move || {
                if self.config.realtime
                    && let Err(error) = raise_priority()
                {
                    tracing::warn!(
                        %error,
                        "no real-time priority for the engine thread; timing may suffer under load"
                    );
                }
                self.run()
            })?;
        Ok(EngineThread { thread })
    }
}

/// The running engine thread.
#[derive(Debug)]
pub struct EngineThread<E> {
    thread: JoinHandle<Result<(), E>>,
}

impl<E> EngineThread<E> {
    /// Waits for the engine to finish. It finishes once the
    /// [`EngineHandle`](crate::EngineHandle) is dropped.
    pub fn join(self) -> Result<(), EngineError<E>> {
        match self.thread.join() {
            Ok(result) => result.map_err(EngineError::Output),
            Err(_) => Err(EngineError::Panicked),
        }
    }
}

/// Why the engine stopped before it was asked to.
#[derive(Debug, Error)]
pub enum EngineError<E> {
    /// The MIDI output failed.
    #[error("MIDI output failed")]
    Output(#[source] E),
    /// The engine thread panicked.
    #[error("the engine thread panicked")]
    Panicked,
}

#[cfg(unix)]
fn raise_priority() -> Result<(), thread_priority::Error> {
    use thread_priority::{
        RealtimeThreadSchedulePolicy, ThreadPriority, ThreadPriorityValue, ThreadSchedulePolicy,
    };

    let priority = ThreadPriorityValue::try_from(REALTIME_PRIORITY)
        .map_err(|_| thread_priority::Error::Priority("real-time priority out of range"))?;
    thread_priority::set_thread_priority_and_policy(
        thread_priority::thread_native_id(),
        ThreadPriority::Crossplatform(priority),
        ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo),
    )
}

#[cfg(not(unix))]
fn raise_priority() -> Result<(), thread_priority::Error> {
    thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max)
}
