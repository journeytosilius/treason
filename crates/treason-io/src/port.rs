//! Which ports to open.

/// An output port for a backend to open.
///
/// The order of the specs a backend is given sets the
/// [`PortId`](treason_core::model::PortId)s: the first is port 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSpec {
    /// The name of treason's own port, as other MIDI software sees it.
    pub name: String,
    /// Connect to the first existing port whose name contains this text.
    /// `None` leaves the port unconnected, as a virtual port for other
    /// software to connect to.
    pub connect: Option<String>,
}
