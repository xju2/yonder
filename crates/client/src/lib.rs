//! Client side of Yonder's remote connection.
//!
//! [`connect`] runs the user's own `ssh` binary once. Inside that one session
//! a short shell script reports the remote OS and CPU, receives the agent
//! binary if the remote does not have this exact build yet, and then `exec`s
//! it. Using the system `ssh` means `~/.ssh/config`, ProxyJump, ssh-agent and
//! short-lived certificates all work as they do in a terminal; using a single
//! session means one authentication per connect.

pub mod askpass;
mod bootstrap;
mod connection;
pub mod git;

pub use bootstrap::{connect, find_agent, ConnectError, ConnectOptions, SUPPORTED_ARCHES};
pub use connection::Connection;

/// A line for the connection log the app shows while connecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub level: Level,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// A step of the connection sequence.
    Step,
    /// Something worth noticing that did not stop the connection.
    Warn,
    /// What ssh or the remote side printed on stderr.
    Remote,
}

pub type LogFn = std::sync::Arc<dyn Fn(LogLine) + Send + Sync>;
