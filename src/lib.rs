//! gantry: a pod-first container orchestrator that reads the Compose
//! format. gantry creates each pod's namespaces itself, attaches them to
//! the LAN with macvlan or ipvlan, applies an nftables firewall inside
//! them, and then starts the pod's containers with Docker, Podman or kiri.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod boot;
pub mod compose;
pub mod datamove;
pub mod errors;
pub mod firewall;
pub mod installed;
pub mod pod;
pub mod project;
pub mod redact;
pub mod util;

pub use errors::{Error, Result};
pub mod engine;
pub mod oci;
pub mod orchestrator;
