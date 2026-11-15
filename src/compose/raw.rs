//! Serde structs mirroring the YAML as written, with Compose's many
//! short and long forms. `super::normalize` validates them into
//! [`crate::project`] types.

use indexmap::IndexMap;
use serde::Deserialize;
use serde_yaml_ng::Value;

/// Any scalar Compose accepts where a string is meant (`3`, `true`, `"x"`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Scalar {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl Scalar {
    pub fn into_string(self) -> String {
        match self {
            Scalar::Str(s) => s,
            Scalar::Int(i) => i.to_string(),
            Scalar::Float(f) => f.to_string(),
            Scalar::Bool(b) => b.to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringOrList {
    One(String),
    Many(Vec<String>),
}

impl StringOrList {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            StringOrList::One(s) => vec![s],
            StringOrList::Many(v) => v,
        }
    }
}

/// `KEY=VALUE` list or `KEY: VALUE` map (environment, labels, sysctls).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ListOrMap {
    List(Vec<String>),
    Map(IndexMap<String, Option<Scalar>>),
}

#[derive(Debug, Default, Deserialize)]
pub struct File {
    pub name: Option<String>,
    pub runtime: Option<String>,
    #[serde(default)]
    pub services: IndexMap<String, Service>,
    #[serde(default)]
    pub pods: IndexMap<String, Option<Pod>>,
    #[serde(default)]
    pub networks: IndexMap<String, Option<Network>>,
    #[serde(default)]
    pub volumes: IndexMap<String, Option<VolumeDef>>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Network {
    pub driver: Option<String>,
    /// Extension: shorthand for `driver_opts.parent`.
    pub parent: Option<String>,
    /// Extension: shorthand for `driver_opts.mode`.
    pub mode: Option<String>,
    #[serde(default)]
    pub driver_opts: IndexMap<String, Scalar>,
    pub ipam: Option<Ipam>,
    pub mtu: Option<u32>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Ipam {
    pub driver: Option<String>,
    #[serde(default)]
    pub config: Vec<IpamConfig>,
}

#[derive(Debug, Default, Deserialize)]
pub struct IpamConfig {
    pub subnet: String,
    pub gateway: Option<String>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
pub struct VolumeDef {
    pub name: Option<String>,
    #[serde(default)]
    pub external: bool,
    pub driver: Option<String>,
    #[serde(default)]
    pub driver_opts: IndexMap<String, Scalar>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

/// A network attachment, for pods and for services that get an implicit pod.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Attachment {
    pub ipv4_address: Option<String>,
    pub ipv6_address: Option<String>,
    /// Extension: a list of addresses with prefix (`10.0.0.5/24`).
    #[serde(default)]
    pub addresses: Vec<String>,
    /// Extension: lease an IPv4 address with DHCP.
    #[serde(default)]
    pub dhcp: bool,
    pub mac_address: Option<String>,
    /// Extension: install the default route through this network.
    pub default_route: Option<bool>,
    /// Extension: override the subnet gateway.
    pub gateway: Option<String>,
    /// Extension: interface name inside the pod.
    pub interface: Option<String>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Attachments {
    List(Vec<String>),
    Map(IndexMap<String, Option<Attachment>>),
}

impl Attachments {
    pub fn into_pairs(self) -> Vec<(String, Attachment)> {
        match self {
            Attachments::List(v) => v.into_iter().map(|n| (n, Attachment::default())).collect(),
            Attachments::Map(m) => m
                .into_iter()
                .map(|(k, v)| (k, v.unwrap_or_default()))
                .collect(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Pod {
    pub hostname: Option<String>,
    pub domainname: Option<String>,
    /// Namespaces shared besides net: `uts`, `ipc`. Default both.
    pub share: Option<Vec<String>>,
    pub networks: Option<Attachments>,
    pub dns: Option<StringOrList>,
    pub dns_search: Option<StringOrList>,
    pub dns_opt: Option<Vec<String>>,
    pub extra_hosts: Option<ListOrMap>,
    pub sysctls: Option<ListOrMap>,
    pub ipv6: Option<bool>,
    pub firewall: Option<Firewall>,
    /// Default runtime for services in this pod.
    pub runtime: Option<String>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Firewall {
    pub policy: Option<Policy>,
    pub allow_ping: Option<bool>,
    #[serde(default)]
    pub log_dropped: bool,
    #[serde(default)]
    pub input: Vec<Rule>,
    #[serde(default)]
    pub output: Vec<Rule>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub input: Option<String>,
    pub output: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub action: Option<String>,
    pub proto: Option<StringOrList>,
    pub port: Option<Scalar>,
    pub ports: Option<Vec<Scalar>>,
    pub from: Option<StringOrList>,
    pub to: Option<StringOrList>,
    pub interface: Option<String>,
    #[serde(default)]
    pub log: bool,
    pub limit: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Port {
    Num(u32),
    Short(String),
    Long(PortLong),
}

#[derive(Debug, Clone, Deserialize)]
pub struct PortLong {
    pub target: u32,
    pub published: Option<Scalar>,
    pub protocol: Option<String>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum VolumeMount {
    Short(String),
    Long(Box<VolumeLong>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct VolumeLong {
    #[serde(rename = "type")]
    pub kind: String,
    pub source: Option<String>,
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
    pub bind: Option<BindOpts>,
    pub tmpfs: Option<TmpfsOpts>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BindOpts {
    pub propagation: Option<String>,
    pub create_host_path: Option<bool>,
    pub selinux: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TmpfsOpts {
    pub size: Option<Scalar>,
    pub mode: Option<Scalar>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DependsOn {
    List(Vec<String>),
    Map(IndexMap<String, DependsLong>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct DependsLong {
    pub condition: Option<String>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Healthcheck {
    pub test: Option<StringOrList>,
    pub interval: Option<String>,
    pub timeout: Option<String>,
    pub retries: Option<u32>,
    pub start_period: Option<String>,
    pub start_interval: Option<String>,
    #[serde(default)]
    pub disable: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum EnvFile {
    One(String),
    Many(Vec<EnvFileEntry>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum EnvFileEntry {
    Path(String),
    Long {
        path: String,
        required: Option<bool>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Ulimit {
    Single(i64),
    Pair { soft: i64, hard: i64 },
}

#[derive(Debug, Default, Deserialize)]
pub struct Service {
    pub image: Option<String>,
    pub pod: Option<String>,
    pub runtime: Option<String>,
    pub container_name: Option<String>,
    pub command: Option<StringOrList>,
    pub entrypoint: Option<StringOrList>,
    pub environment: Option<ListOrMap>,
    pub env_file: Option<EnvFile>,
    pub working_dir: Option<String>,
    pub user: Option<Scalar>,
    pub labels: Option<ListOrMap>,
    pub volumes: Option<Vec<VolumeMount>>,
    pub tmpfs: Option<StringOrList>,
    pub restart: Option<String>,
    pub healthcheck: Option<Healthcheck>,
    pub depends_on: Option<DependsOn>,
    pub stop_signal: Option<String>,
    pub stop_grace_period: Option<String>,
    pub cap_add: Option<Vec<String>>,
    pub cap_drop: Option<Vec<String>>,
    #[serde(default)]
    pub privileged: bool,
    #[serde(default)]
    pub read_only: bool,
    pub devices: Option<Vec<String>>,
    #[serde(default)]
    pub tty: bool,
    #[serde(default)]
    pub stdin_open: bool,
    #[serde(default)]
    pub init: bool,
    pub mem_limit: Option<Scalar>,
    pub cpus: Option<Scalar>,
    pub pids_limit: Option<i64>,
    pub shm_size: Option<Scalar>,
    pub ulimits: Option<IndexMap<String, Ulimit>>,
    pub security_opt: Option<Vec<String>>,
    pub group_add: Option<Vec<Scalar>>,
    pub sysctls: Option<ListOrMap>,
    pub pull_policy: Option<String>,
    pub ports: Option<Vec<Port>>,
    pub expose: Option<Vec<Scalar>>,
    pub profiles: Option<Vec<String>>,
    // Pod level settings a service may carry when it has no `pod:`.
    pub hostname: Option<String>,
    pub domainname: Option<String>,
    pub networks: Option<Attachments>,
    pub network_mode: Option<String>,
    pub mac_address: Option<String>,
    pub dns: Option<StringOrList>,
    pub dns_search: Option<StringOrList>,
    pub dns_opt: Option<Vec<String>>,
    pub extra_hosts: Option<ListOrMap>,
    pub firewall: Option<Firewall>,
    pub ipc: Option<String>,
    pub pid: Option<String>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}
