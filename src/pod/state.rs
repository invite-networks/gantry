//! What gantry records about a running pod, under the run directory.

use crate::errors::{Context, Error, Result};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// Root of gantry's runtime state. Overridable for tests.
pub fn run_dir() -> PathBuf {
    std::env::var_os("GANTRY_RUN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/gantry"))
}

/// Where network namespaces are pinned, shared with `ip netns`.
pub fn netns_dir() -> PathBuf {
    std::env::var_os("GANTRY_NETNS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/netns"))
}

pub fn project_dir(project: &str) -> PathBuf {
    run_dir().join(project)
}

pub fn pod_dir(project: &str, pod: &str) -> PathBuf {
    project_dir(project).join("pods").join(pod)
}

/// `ip netns` name of a pod's network namespace.
pub fn netns_name(project: &str, pod: &str) -> String {
    format!("gt-{project}-{pod}")
}

pub fn netns_path(project: &str, pod: &str) -> PathBuf {
    netns_dir().join(netns_name(project, pod))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InterfaceState {
    pub ifname: String,
    pub network: String,
    pub mac: Option<String>,
    pub addresses: Vec<IpNet>,
    pub gateway: Option<IpAddr>,
    pub dhcp: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PodState {
    /// Random per instance; containers record it to notice a recreated pod.
    pub id: String,
    pub project: String,
    pub name: String,
    pub config_hash: String,
    pub netns: PathBuf,
    pub uts: Option<PathBuf>,
    pub ipc: Option<PathBuf>,
    pub hostname: String,
    pub interfaces: Vec<InterfaceState>,
    pub created: u64,
}

impl PodState {
    pub fn path(project: &str, pod: &str) -> PathBuf {
        pod_dir(project, pod).join("state.json")
    }

    pub fn load(project: &str, pod: &str) -> Result<Option<Self>> {
        let p = Self::path(project, pod);
        match std::fs::read(&p) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| Error::State(format!("{}: {e}", p.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).ctx(|| format!("read {}", p.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let p = Self::path(&self.project, &self.name);
        let data = serde_json::to_vec_pretty(self).map_err(|e| Error::State(e.to_string()))?;
        crate::util::write_atomic(&p, &data)
    }

    /// Addresses currently known for this pod, including DHCP leases.
    pub fn addresses(&self) -> Vec<IpNet> {
        let mut out: Vec<IpNet> = self
            .interfaces
            .iter()
            .flat_map(|i| i.addresses.clone())
            .collect();
        let dir = pod_dir(&self.project, &self.name);
        for i in self.interfaces.iter().filter(|i| i.dhcp) {
            if let Ok(Some(lease)) = super::dhcp::LeaseFile::load(&dir, &i.ifname) {
                out.push(lease.address());
            }
        }
        out
    }
}

/// Lists the pods that have state for a project.
pub fn list_pods(project: &str) -> Vec<String> {
    let dir = project_dir(project).join("pods");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut out: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("state.json").exists())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    out.sort();
    out
}

/// Lists project names with state.
pub fn list_projects() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(run_dir()) else {
        return vec![];
    };
    let mut out: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("pods").is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    out.sort();
    out
}

pub fn ensure_dir(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p).ctx(|| format!("create {}", p.display()))?;
    // The run directory holds project snapshots (environment, build args)
    // and may hold a private Podman API socket: root only. Files below it
    // keep their own modes so containers can read their bind mounts.
    let run = run_dir();
    if p.starts_with(&run) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700))
            .ctx(|| format!("chmod {}", run.display()))?;
    }
    Ok(())
}
