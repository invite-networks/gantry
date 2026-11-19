//! Error type shared by the gantry library.

use std::fmt::Display;

/// Every failure gantry reports. Variants group errors by the layer that
/// produced them so the CLI can print a useful prefix.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The compose file is invalid or uses something gantry does not support.
    #[error("config: {0}")]
    Config(String),
    /// A filesystem or process operation failed.
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    /// A system call failed.
    #[error("{context}: {source}")]
    Sys {
        context: String,
        #[source]
        source: nix::Error,
    },
    /// The container engine (docker or kiri) returned an error.
    #[error("engine: {0}")]
    Engine(String),
    /// Namespace, link, address or route setup failed.
    #[error("network: {0}")]
    Network(String),
    /// The nftables ruleset could not be built or applied.
    #[error("firewall: {0}")]
    Firewall(String),
    /// DHCP lease acquisition or renewal failed.
    #[error("dhcp: {0}")]
    Dhcp(String),
    /// Pod or project state on disk is missing or inconsistent.
    #[error("state: {0}")]
    State(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn config(msg: impl Display) -> Self {
        Self::Config(msg.to_string())
    }
    pub fn network(msg: impl Display) -> Self {
        Self::Network(msg.to_string())
    }
    pub fn engine(msg: impl Display) -> Self {
        Self::Engine(msg.to_string())
    }
}

/// Adds a human readable context to I/O and syscall results.
pub trait Context<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> Context<T> for std::result::Result<T, std::io::Error> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| Error::Io {
            context: context(),
            source,
        })
    }
}

impl<T> Context<T> for std::result::Result<T, nix::Error> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| Error::Sys {
            context: context(),
            source,
        })
    }
}

impl From<rtnetlink::Error> for Error {
    fn from(e: rtnetlink::Error) -> Self {
        Error::Network(format!("netlink: {e}"))
    }
}
