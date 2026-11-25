//! gantry: a pod-first container orchestrator that reads the Compose
//! format. gantry creates each pod's namespaces itself, attaches them to
//! the LAN with macvlan or ipvlan, applies an nftables firewall inside
//! them, and then starts the pod's containers with Docker or kiri.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod boot;
pub mod compose;
pub mod datamove;
pub mod errors;
pub mod firewall;
pub mod heal;
pub mod installed;
pub mod pod;
pub mod project;
pub mod redact;
pub mod util;

pub use errors::{Error, Result};

/// Features this build supports, printed by `gantry version`. Callers
/// such as gantry-agent check for a name here rather than comparing
/// version numbers.
pub const FEATURES: &[&str] = &["health_on_failure", "supervise", "plugins"];
pub mod engine;
pub mod oci;
pub mod orchestrator;
pub mod plugin;
