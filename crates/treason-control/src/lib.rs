//! The application layer: the one place where the project changes.
//!
//! Adapters (the MCP server, a CLI, a future UI) call a [`Session`] and never
//! touch the engine. A session validates each [`Edit`], keeps undo history and
//! publishes the new snapshot through the
//! [`Playback`](treason_core::playback::Playback) trait. It works the same
//! with the real engine or a test double.

pub mod edit;
pub mod session;

pub use edit::Edit;
pub use session::{ControlError, Session};
