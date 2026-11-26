//! Framed JSON-RPC client for unmodified `herdr-core` (`hc`).

pub mod agent;
pub mod connection;
pub mod endpoint;
pub mod error;
pub mod event;
pub mod frame;
pub mod hello;
pub mod identity;
pub mod method;
pub mod reply;
pub mod start;
pub mod stream;
pub mod wire;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;

pub use connection::Core;
pub use endpoint::Endpoint;
pub use error::{Error, Result, Violation};
pub use identity::{AgentKind, agent_in_front, agent_kind, agent_name, identify_argv};
pub use method::Method;
pub use start::{PROGRAM, new_command, start};
pub use stream::{Attachment, Chunk, Closed, End, MAX_BUFFERED, Subscription};
