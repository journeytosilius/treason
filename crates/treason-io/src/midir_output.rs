//! Real-time output through `midir`: ALSA on Linux, CoreMIDI on macOS and
//! WinMM on Windows.

use std::time::Instant;

use thiserror::Error;
use treason_core::model::PortId;
use treason_core::output::{Delivery, MidiOutput, TimedMessage};

use crate::port::PortSpec;

/// One midir connection per port.
///
/// midir sends each message the moment it gets it, so the engine waits until
/// a message is due before sending it ([`Delivery::Immediate`]).
pub struct MidirOutput {
    connections: Vec<midir::MidiOutputConnection>,
}

impl MidirOutput {
    /// Opens `ports` in order. `client_name` is how treason appears to other
    /// MIDI software.
    pub fn open(client_name: &str, ports: &[PortSpec]) -> Result<Self, MidirError> {
        let connections = ports
            .iter()
            .map(|spec| open_port(client_name, spec))
            .collect::<Result<_, _>>()?;
        Ok(Self { connections })
    }

    /// The names of the output ports on this machine.
    pub fn port_names(client_name: &str) -> Result<Vec<String>, MidirError> {
        let client = midir::MidiOutput::new(client_name)?;
        client
            .ports()
            .iter()
            .map(|port| client.port_name(port).map_err(MidirError::from))
            .collect()
    }
}

impl MidiOutput for MidirOutput {
    type Error = MidirError;

    fn delivery(&self) -> Delivery {
        Delivery::Immediate
    }

    fn send(&mut self, message: &TimedMessage) -> Result<(), Self::Error> {
        let connection = self
            .connections
            .get_mut(usize::from(message.port.0))
            .ok_or(MidirError::UnknownPort(message.port))?;
        let mut bytes = [0; 3];
        connection.send(message.message.encode(&mut bytes))?;
        Ok(())
    }

    fn cancel_from(
        &mut self,
        _from: Instant,
        _cancelled: &mut dyn FnMut(&TimedMessage),
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn open_port(
    client_name: &str,
    spec: &PortSpec,
) -> Result<midir::MidiOutputConnection, MidirError> {
    let client = midir::MidiOutput::new(client_name)?;
    let Some(pattern) = &spec.connect else {
        return create_virtual(client, &spec.name);
    };
    let port = client
        .ports()
        .into_iter()
        .find(|port| {
            client
                .port_name(port)
                .is_ok_and(|name| name.contains(pattern.as_str()))
        })
        .ok_or_else(|| MidirError::NoSuchPort(pattern.clone()))?;
    client
        .connect(&port, &spec.name)
        .map_err(|error| MidirError::Connect {
            port: spec.name.clone(),
            kind: error.kind(),
        })
}

#[cfg(unix)]
fn create_virtual(
    client: midir::MidiOutput,
    name: &str,
) -> Result<midir::MidiOutputConnection, MidirError> {
    use midir::os::unix::VirtualOutput;

    client
        .create_virtual(name)
        .map_err(|error| MidirError::Connect {
            port: name.to_owned(),
            kind: error.kind(),
        })
}

#[cfg(not(unix))]
fn create_virtual(
    _client: midir::MidiOutput,
    name: &str,
) -> Result<midir::MidiOutputConnection, MidirError> {
    Err(MidirError::VirtualUnsupported(name.to_owned()))
}

/// Why midir couldn't open a port or send.
#[derive(Debug, Error)]
pub enum MidirError {
    /// The platform's MIDI system is unavailable.
    #[error("could not start a MIDI client")]
    Init(#[from] midir::InitError),
    /// A port disappeared while being listed.
    #[error("could not read a MIDI port's name")]
    PortInfo(#[from] midir::PortInfoError),
    /// No port matches a [`PortSpec::connect`] pattern.
    #[error("no MIDI output port has `{0}` in its name")]
    NoSuchPort(String),
    /// The port was found but couldn't be opened.
    #[error("could not open MIDI port `{port}`: {kind}")]
    Connect {
        /// treason's name for the port.
        port: String,
        /// What went wrong.
        kind: midir::ConnectErrorKind,
    },
    /// Virtual ports need ALSA, JACK or CoreMIDI.
    #[error("port `{0}` needs `connect`: this platform has no virtual ports")]
    VirtualUnsupported(String),
    /// A message names a port that wasn't opened.
    #[error("there is no output port {0}")]
    UnknownPort(PortId),
    /// The MIDI system refused a message.
    #[error("could not send MIDI")]
    Send(#[from] midir::SendError),
}
