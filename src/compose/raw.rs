//! Serde structs mirroring the YAML as written, with Compose's many
//! short and long forms. `super::normalize` validates them into
//! [`crate::project`] types.

use indexmap::IndexMap;
use serde::Deserialize;
use serde_norway::Value;

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
    /// Extension: the project's only pod, without a name (it takes the
    /// project's name). Exclusive with `pods`.
    pub pod: Option<Pod>,
    /// Not supported: networks are defined on each pod. Kept to say so.
    pub networks: Option<Value>,
    #[serde(default)]
    pub volumes: IndexMap<String, Option<VolumeDef>>,
    /// Extension: per project plugin settings, `plugins.<name>`.
    #[serde(default)]
    pub plugins: IndexMap<String, Option<Value>>,
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

/// A pod network's `address`: text, or a map asking a plugin for one.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Address {
    Text(String),
    Plugin(IndexMap<String, Value>),
}

/// One interface of a pod: a macvlan or ipvlan link on a host interface.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PodNetwork {
    /// Host interface the link hangs off, such as `vlan4`.
    pub parent: Option<String>,
    /// `10.10.10.10/24`, `dhcp`, or `{ from: <plugin>, ... }`.
    pub address: Option<Address>,
    /// `macvlan` (default) or `ipvlan`.
    pub driver: Option<String>,
    /// IPv4 gateway. Default: the first host of the address's subnet.
    pub gateway: Option<String>,
    /// Optional static IPv6 address with prefix.
    pub address6: Option<String>,
    /// IPv6 gateway. Default: from router advertisements.
    pub gateway6: Option<String>,
    /// Interface name inside the pod. Default: eth0, eth1, ... in order.
    pub interface: Option<String>,
    /// Install the default route through this interface. Default: the first.
    pub default_route: Option<bool>,
    /// macvlan only. Default: a stable MAC derived from the names.
    pub mac_address: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Pod {
    pub hostname: Option<String>,
    pub domainname: Option<String>,
    /// Namespaces shared besides net: `uts`, `ipc`. Default both.
    pub share: Option<Vec<String>>,
    /// The pod's interfaces.
    pub network: Option<Vec<PodNetwork>>,
    pub dns: Option<StringOrList>,
    pub dns_search: Option<StringOrList>,
    pub dns_opt: Option<Vec<String>>,
    pub extra_hosts: Option<ListOrMap>,
    pub sysctls: Option<ListOrMap>,
    pub ipv6: Option<bool>,
    pub firewall: Option<Firewall>,
    /// Boot order across the host: lower starts first, equal together.
    pub priority: Option<u32>,
    /// Default runtime for services in this pod.
    pub runtime: Option<String>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Firewall {
    pub policy: Option<Policy>,
    /// Add the default rules (any ICMP, reject the pod's own subnet).
    /// Default true.
    pub defaults: Option<bool>,
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
    pub build: Option<Build>,
    pub logging: Option<Logging>,
    pub platform: Option<String>,
    pub pod: Option<String>,
    /// Start order inside the pod: lower starts first, equal together.
    pub priority: Option<u32>,
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
    pub health_on_failure: Option<String>,
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
    /// Not supported: networks belong to the pod. Kept to say so.
    pub networks: Option<Value>,
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

/// `build:` as a context path or the long form.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Build {
    Context(String),
    Long(Box<BuildLong>),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BuildLong {
    pub context: Option<String>,
    pub dockerfile: Option<String>,
    pub dockerfile_inline: Option<String>,
    pub args: Option<ListOrMap>,
    pub target: Option<String>,
    pub labels: Option<ListOrMap>,
    #[serde(default)]
    pub pull: bool,
    #[serde(default)]
    pub no_cache: bool,
    pub network: Option<String>,
    pub cache_from: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub extra_hosts: Option<ListOrMap>,
    pub shm_size: Option<Scalar>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Logging {
    pub driver: Option<String>,
    pub options: Option<ListOrMap>,
}
