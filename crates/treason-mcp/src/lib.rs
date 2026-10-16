//! The MCP server: how an AI agent drives treason.
//!
//! [`TreasonServer`] exposes a [`Session`] as Model Context Protocol tools,
//! built with `rmcp`. [`serve_http`] serves them over streamable HTTP, so the
//! sequencer keeps playing when an agent session ends and several agents can
//! connect at once; [`serve_stdio`] serves one agent over stdin and stdout.
//!
//! Tool inputs and outputs ([`dto`]) are separate types designed for the
//! model: voice names, step strings such as `"x...x...x..x.x.."`
//! ([`notation`]), channels 1-16. They are converted to and from the core
//! model here; [`pattern`] turns step patterns into clips and back. Then the
//! agent's interface can stay stable while the internal model changes, and
//! the other way round.
//!
//! `set_notes` is a placeholder until pitch names are written.
//!
//! [`Session`]: treason_control::Session

pub mod dto;
pub mod notation;
pub mod pattern;
mod serve;
pub mod server;

pub use serve::{ServeError, serve_http, serve_stdio};
pub use server::{SharedSession, TreasonServer};
