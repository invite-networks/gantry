//! The normalized project model. The compose loader turns YAML into these
//! types after validation; everything else in gantry works from them.

use indexmap::IndexMap;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Which container engine runs a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    Docker,
    Kiri,
}

impl RuntimeKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "docker" => Some(Self::Docker),
            "kiri" => Some(Self::Kiri),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Kiri => "kiri",
        }
    }
}

impl std::fmt::Display for RuntimeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The priority of a pod or service that sets none.
pub const DEFAULT_PRIORITY: u32 = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    /// Directory relative paths in the compose file resolve against.
    pub dir: PathBuf,
    pub runtime: RuntimeKind,
    pub pods: IndexMap<String, Pod>,
    pub services: IndexMap<String, Service>,
    pub volumes: IndexMap<String, Volume>,
}

impl Project {
    /// Where the last `up` recorded the normalized project, so helpers such
    /// as the DHCP agent can refresh hosts files and firewalls on their own.
    pub fn snapshot_path(name: &str) -> PathBuf {
        crate::pod::state::project_dir(name).join("project.json")
    }

    pub fn save_snapshot(&self) -> crate::Result<()> {
        let data = serde_json::to_vec(self).map_err(|e| crate::Error::State(e.to_string()))?;
        let path = Self::snapshot_path(&self.name);
        if let Some(dir) = path.parent() {
            crate::pod::state::ensure_dir(dir)?;
        }
        crate::util::write_private(&path, &data)
    }

    pub fn load_snapshot(name: &str) -> crate::Result<Option<Self>> {
        match std::fs::read(Self::snapshot_path(name)) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| crate::Error::State(e.to_string())),
            Err(_) => Ok(None),
        }
    }

    pub fn services_in_pod<'a>(&'a self, pod: &'a str) -> impl Iterator<Item = &'a Service> + 'a {
        self.services.values().filter(move |s| s.pod == pod)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "type", content = "mode")]
pub enum NetDriver {
    Macvlan(MacvlanMode),
    Ipvlan(IpvlanMode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MacvlanMode {
    Bridge,
    Private,
    Vepa,
    Passthru,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IpvlanMode {
    L2,
    L3,
    L3s,
}

/// A pod: one set of namespaces created by gantry, shared by every
/// service in it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pod {
    pub name: String,
    pub hostname: String,
    pub domainname: Option<String>,
    pub share_uts: bool,
    pub share_ipc: bool,
    pub attachments: Vec<Attachment>,
    pub dns: Vec<IpAddr>,
    pub dns_search: Vec<String>,
    pub dns_options: Vec<String>,
    pub extra_hosts: Vec<(String, IpAddr)>,
    pub sysctls: IndexMap<String, String>,
    pub ipv6: bool,
    /// Boot order across the host (`gantry boot`): lower first.
    pub priority: u32,
    pub firewall: Option<Firewall>,
    /// True for the loopback-only pod of a `network_mode: none` service.
    pub implicit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// Interface name inside the pod (eth0, eth1, ...).
    pub ifname: String,
    /// Host interface the macvlan or ipvlan link hangs off.
    pub parent: String,
    pub driver: NetDriver,
    pub addressing: Addressing,
    pub mac: Option<[u8; 6]>,
    pub default_route: bool,
    /// IPv4 gateway: as written, or the first host of the address's subnet.
    pub gateway: Option<IpAddr>,
    /// IPv6 gateway as written; without one, router advertisements decide.
    pub gateway6: Option<IpAddr>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Addressing {
    /// Static addresses with prefix lengths (one IPv4, optionally one IPv6).
    Static(Vec<IpNet>),
    /// DHCPv4 (plus SLAAC for IPv6 if the network offers it).
    Dhcp,
}

/// The firewall applied inside a pod's network namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Firewall {
    pub input_policy: Action,
    pub output_policy: Action,
    /// Add the default rules: any ICMP accepted before the pod's own rules,
    /// and the pod's own subnets rejected after them.
    pub defaults: bool,
    pub log_dropped: bool,
    pub input: Vec<Rule>,
    pub output: Vec<Rule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Accept,
    Drop,
    Reject,
}

impl Action {
    pub fn as_nft(self) -> &'static str {
        match self {
            Action::Accept => "accept",
            Action::Drop => "drop",
            Action::Reject => "reject",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub action: Action,
    pub protocols: Vec<Proto>,
    pub ports: Vec<PortRange>,
    /// Peers: the source for input rules, the destination for output rules.
    pub peers: Vec<Peer>,
    pub interface: Option<String>,
    pub log: bool,
    pub limit: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    Tcp,
    Udp,
    Icmp,
    Icmpv6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl std::fmt::Display for PortRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}-{}", self.start, self.end)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "kind", content = "value")]
pub enum Peer {
    Any,
    Cidr(IpNet),
    Pod(String),
    Service(String),
    /// The subnets of the pod's own addresses (static or leased).
    Subnet,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    pub name: String,
    /// Name in the engine: `<project>_<name>` unless external or renamed.
    pub engine_name: String,
    pub external: bool,
    pub driver: Option<String>,
    pub driver_opts: IndexMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub name: String,
    pub pod: String,
    /// Start order inside the pod: lower first. Turned into depends_on
    /// entries on the pod's lower-priority services.
    pub priority: u32,
    pub runtime: RuntimeKind,
    pub image: String,
    /// Built locally instead of pulled.
    pub build: Option<Build>,
    pub logging: Option<Logging>,
    pub platform: Option<String>,
    pub container_name: String,
    pub command: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub environment: IndexMap<String, String>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub labels: IndexMap<String, String>,
    pub mounts: Vec<Mount>,
    pub restart: Option<String>,
    pub healthcheck: Option<Healthcheck>,
    /// What `gantry supervise` does when the container turns unhealthy.
    pub health_on_failure: HealthOnFailure,
    pub depends_on: IndexMap<String, DependsCondition>,
    pub stop_signal: Option<String>,
    pub stop_grace_period: Option<Duration>,
    pub cap_add: Vec<String>,
    pub cap_drop: Vec<String>,
    pub privileged: bool,
    pub read_only: bool,
    pub devices: Vec<String>,
    pub tty: bool,
    pub stdin_open: bool,
    pub init: bool,
    pub mem_limit: Option<u64>,
    pub cpus: Option<f64>,
    pub pids_limit: Option<i64>,
    pub shm_size: Option<u64>,
    pub ulimits: IndexMap<String, (i64, i64)>,
    pub security_opt: Vec<String>,
    pub group_add: Vec<String>,
    pub sysctls: IndexMap<String, String>,
    pub pull_policy: PullPolicy,
    /// Ports opened to everyone in the pod firewall (`ports:`).
    pub ports: Vec<(Proto, PortRange)>,
    /// Ports opened to the project's other pods in the pod firewall (`expose:`).
    pub expose: Vec<(Proto, PortRange)>,
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mount {
    pub kind: MountKind,
    pub source: String,
    pub target: String,
    pub read_only: bool,
    /// Extra options passed through (propagation, SELinux labels, tmpfs size).
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountKind {
    Bind,
    Volume,
    Tmpfs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Healthcheck {
    /// The image's healthcheck is turned off.
    pub disabled: bool,
    /// `["CMD", ...]` or `["CMD-SHELL", "..."]`. None keeps the image's test.
    pub test: Option<Vec<String>>,
    pub interval: Option<Duration>,
    pub timeout: Option<Duration>,
    pub retries: Option<u32>,
    pub start_period: Option<Duration>,
    pub start_interval: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependsCondition {
    ServiceStarted,
    ServiceHealthy,
    ServiceCompletedSuccessfully,
}

/// The action taken on a container its engine reports as unhealthy
/// (`health_on_failure:`). Engines only mark such a container; Gantry's
/// watcher acts on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthOnFailure {
    /// Leave it running and unhealthy (the engine's own behavior).
    #[default]
    None,
    /// Restart it, with backoff and a cap.
    Restart,
    /// Stop it and leave it stopped.
    Stop,
}

impl HealthOnFailure {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "restart" => Some(Self::Restart),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Restart => "restart",
            Self::Stop => "stop",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PullPolicy {
    Missing,
    Always,
    Never,
    /// Build every time (`pull_policy: build`).
    Build,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Build {
    /// Absolute path of the build context.
    pub context: std::path::PathBuf,
    /// Dockerfile path relative to the context (or absolute).
    pub dockerfile: Option<String>,
    pub dockerfile_inline: Option<String>,
    pub args: IndexMap<String, String>,
    pub target: Option<String>,
    pub labels: IndexMap<String, String>,
    pub pull: bool,
    pub no_cache: bool,
    pub network: Option<String>,
    pub cache_from: Vec<String>,
    /// Extra tags besides the service image.
    pub tags: Vec<String>,
    pub extra_hosts: Vec<String>,
    pub shm_size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Logging {
    pub driver: Option<String>,
    pub options: IndexMap<String, String>,
}
