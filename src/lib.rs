//! gantry: a pod-first container orchestrator that reads the Compose
//! format. gantry creates each pod's namespaces itself, attaches them to
//! the LAN with macvlan or ipvlan, applies an nftables firewall inside
//! them, and then starts the pod's containers with Docker or kiri.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

#[cfg(feature = "runtime")]
pub mod boot;
pub mod compose;
#[cfg(feature = "runtime")]
pub mod datamove;
pub mod errors;
#[cfg(feature = "runtime")]
pub mod firewall;
#[cfg(feature = "runtime")]
pub mod heal;
#[cfg(feature = "runtime")]
pub mod installed;
#[cfg(feature = "runtime")]
pub mod pod;
pub mod project;
#[cfg(feature = "runtime")]
pub mod redact;
#[cfg(feature = "runtime")]
pub mod util;

pub use errors::{Error, Result};

/// Features this build supports, printed by `gantry version`. Callers
/// such as gantry-agent check for a name here rather than comparing
/// version numbers.
pub const FEATURES: &[&str] = &["health_on_failure", "supervise", "plugins"];
#[cfg(feature = "runtime")]
pub mod engine;
#[cfg(feature = "runtime")]
pub mod oci;
#[cfg(feature = "runtime")]
pub mod orchestrator;
#[cfg(feature = "runtime")]
pub mod plugin;
#[cfg(feature = "runtime")]
pub mod stats;
