//! The MCP server: how an AI agent drives treason.
//!
//! [`TreasonServer`] exposes a [`Session`] as Model Context Protocol tools,
//! built with `rmcp`. [`serve_http`] serves them over streamable HTTP, so the
//! sequencer keeps playing when an agent session ends and several agents can
//! connect at once; [`serve_stdio`] serves one agent over stdin and stdout.
//!
//! Tool inputs and outputs ([`dto`]) are separate types designed for the
//! model: voice names, pitch names such as `C4` and step strings such as
//! `"x...x...x..x.x.."` ([`notation`]), controls by name such as
//! `modulation`, programs by name such as `electric_bass_finger`, times in
//! beats, channels 1-16. They are converted to and from the core model
//! here: [`pattern`] turns step patterns into clips and back, [`notes`]
//! turns lists of notes into clips, [`controls`] turns points of control
//! values into control changes and back, and [`programs`] turns program
//! names into programs and back. Then the
//! agent's interface can stay stable while the internal model changes, and
//! the other way round. The `get_guide` tool returns `docs/guide.md`, which
//! explains how the tools fit together.
//!
//! [`Session`]: treason_control::Session

pub mod controls;
pub mod dto;
pub mod notation;
pub mod notes;
pub mod pattern;
pub mod programs;
#[cfg(test)]
mod reference;
mod serve;
pub mod server;

pub use serve::{ServeError, serve_http, serve_http_on, serve_stdio};
pub use server::{SharedSession, TreasonServer};
