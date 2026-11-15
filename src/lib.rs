//! kirisame: a pod-first container orchestrator that reads the Compose
//! format. kirisame creates each pod's namespaces itself, attaches them to
//! the LAN with macvlan or ipvlan, applies an nftables firewall inside
//! them, and then starts the pod's containers with Docker, Podman or kiri.

pub mod compose;
pub mod errors;
pub mod firewall;
pub mod pod;
pub mod project;
pub mod util;

pub use errors::{Error, Result};
pub mod engine;
pub mod oci;
pub mod orchestrator;
