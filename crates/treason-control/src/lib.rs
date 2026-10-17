//! The application layer: the one place where the project changes.
//!
//! Adapters (the MCP server, a CLI, a future UI) call a [`Session`] and never
//! touch the engine. A session validates each [`Edit`], keeps undo history and
//! publishes the new snapshot through the
//! [`Playback`](treason_core::playback::Playback) trait. It works the same
//! with the real engine or a test double.
//!
//! A session also saves its project to a [`ProjectStore`] by name and loads
//! it back, as a [`file`](mod@file) that keeps everything the project holds.
//! An [`ExportStore`] keeps MIDI files exported from projects, written the
//! same way.

pub mod edit;
pub mod file;
pub mod session;
pub mod store;

pub use edit::Edit;
pub use file::FileError;
pub use session::{ControlError, Session};
pub use store::{ExportStore, ProjectError, ProjectList, ProjectName, ProjectStore};
