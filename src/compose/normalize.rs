//! Validation and normalization of the raw compose structs.

use super::raw;
use super::units::{parse_bytes, parse_duration};
use crate::errors::{Error, Result};
use crate::project::*;
use indexmap::IndexMap;
use ipnet::IpNet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

pub struct Ctx<'a> {
    pub project_dir: PathBuf,
    pub project_name: Option<String>,
    pub runtime_override: Option<RuntimeKind>,
    pub profiles: Vec<String>,
    pub lookup: &'a dyn Fn(&str) -> Option<String>,
}

/// Service keys kirisame knows and ignores, with the reason shown to the user.
const IGNORED_SERVICE_KEYS: &[(&str, &str)] = &[
    ("logging", "the engine's default log driver is used"),
    ("deploy", "pods run one instance of each service"),
    ("scale", "pods run one instance of each service"),
    (
        "links",
        "services reach each other by name through the pod hosts file",
    ),
    (
        "external_links",
        "services reach each other by name through the pod hosts file",
    ),
    ("platform", "the engine default platform is used"),
    ("stop_grace_period_ms", "unsupported"),
    ("cgroup_parent", "unsupported"),
    ("oom_score_adj", "unsupported"),
    ("isolation", "unsupported"),
];

pub fn normalize(f: raw::File, ctx: &Ctx<'_>) -> Result<(Project, Vec<String>)> {
    let mut warnings = Vec::new();
    for key in f.extra.keys() {
        if key == "version" {
            continue;
        }
        if !key.starts_with("x-") {
            return Err(Error::config(format!("unsupported top-level key {key:?}")));
        }
    }

    let name = project_name(ctx, f.name.as_deref())?;
    let runtime = match (ctx.runtime_override, f.runtime.as_deref()) {
        (Some(r), _) => r,
        (None, Some(r)) => parse_runtime(r, "runtime")?,
        (None, None) => RuntimeKind::Docker,
    };

    // Networks.
    let mut networks = IndexMap::new();
    for (net_name, n) in f.networks {
        let n = network(&net_name, n.unwrap_or_default(), &mut warnings)?;
        networks.insert(net_name, n);
    }

    // Volumes.
    let mut volumes = IndexMap::new();
    for (vname, v) in f.volumes {
        let v = v.unwrap_or_default();
        for key in v.extra.keys() {
            warnings.push(format!("volume {vname}: ignoring {key:?}"));
        }
        let engine_name = match (&v.name, v.external) {
            (Some(n), _) => n.clone(),
            (None, true) => vname.clone(),
            (None, false) => format!("{name}_{vname}"),
        };
        volumes.insert(
            vname.clone(),
            Volume {
                name: vname,
                engine_name,
                external: v.external,
                driver: v.driver,
                driver_opts: v
                    .driver_opts
                    .into_iter()
                    .map(|(k, v)| (k, v.into_string()))
                    .collect(),
            },
        );
    }

    // Explicit pods.
    let mut pods: IndexMap<String, Pod> = IndexMap::new();
    let mut pod_runtimes: IndexMap<String, RuntimeKind> = IndexMap::new();
    for (pod_name, p) in f.pods {
        check_name(&pod_name, "pod")?;
        let p = p.unwrap_or_default();
        if let Some(key) = p.extra.keys().next() {
            return Err(Error::config(format!(
                "pod {pod_name}: unknown key {key:?}"
            )));
        }
        if let Some(r) = &p.runtime {
            pod_runtimes.insert(
                pod_name.clone(),
                parse_runtime(r, &format!("pod {pod_name}: runtime"))?,
            );
        }
        let share = p.share.unwrap_or_else(|| vec!["uts".into(), "ipc".into()]);
        for s in &share {
            if !matches!(s.as_str(), "uts" | "ipc" | "net") {
                return Err(Error::config(format!(
                    "pod {pod_name}: share accepts uts and ipc (net is always shared), not {s:?}"
                )));
            }
        }
        let attachments = attachments(&pod_name, p.networks, &networks, None)?;
        let pod = Pod {
            hostname: p.hostname.unwrap_or_else(|| pod_name.clone()),
            domainname: p.domainname,
            share_uts: share.iter().any(|s| s == "uts"),
            share_ipc: share.iter().any(|s| s == "ipc"),
            attachments,
            dns: ip_list(p.dns, &format!("pod {pod_name}: dns"))?,
            dns_search: p
                .dns_search
                .map(raw::StringOrList::into_vec)
                .unwrap_or_default(),
            dns_options: p.dns_opt.unwrap_or_default(),
            extra_hosts: extra_hosts(p.extra_hosts, &format!("pod {pod_name}"))?,
            sysctls: list_or_map(p.sysctls, ctx, false),
            ipv6: p.ipv6.unwrap_or(true),
            firewall: p.firewall.map(|fw| firewall(&pod_name, fw)).transpose()?,
            implicit: false,
            name: pod_name.clone(),
        };
        pods.insert(pod_name, pod);
    }

    // Services, with implicit pods for services without `pod:`.
    let mut services = IndexMap::new();
    let mut service_mode: IndexMap<String, String> = IndexMap::new();
    for (svc_name, s) in f.services {
        check_name(&svc_name, "service")?;
        let profiles = s.profiles.clone().unwrap_or_default();
        if !profiles.is_empty() && !profiles.iter().any(|p| ctx.profiles.contains(p)) {
            continue;
        }
        for key in s.extra.keys() {
            if key.starts_with("x-") {
                continue;
            }
            if key == "build" {
                return Err(Error::config(format!(
                    "service {svc_name}: build is not supported yet; build the image first and set image:"
                )));
            }
            match IGNORED_SERVICE_KEYS.iter().find(|(k, _)| k == key) {
                Some((_, why)) => {
                    warnings.push(format!("service {svc_name}: ignoring {key} ({why})"))
                }
                None => warnings.push(format!("service {svc_name}: ignoring unknown key {key:?}")),
            }
        }
        if let Some(mode) = &s.network_mode {
            service_mode.insert(svc_name.clone(), mode.clone());
        }
        let explicit_pod = s.pod.clone();
        let pod_name = match &explicit_pod {
            Some(p) => {
                if !pods.contains_key(p) {
                    return Err(Error::config(format!(
                        "service {svc_name}: pod {p:?} is not defined under pods:"
                    )));
                }
                for (set, key) in [
                    (s.networks.is_some(), "networks"),
                    (s.hostname.is_some(), "hostname"),
                    (s.domainname.is_some(), "domainname"),
                    (s.mac_address.is_some(), "mac_address"),
                    (s.dns.is_some(), "dns"),
                    (s.dns_search.is_some(), "dns_search"),
                    (s.dns_opt.is_some(), "dns_opt"),
                    (s.extra_hosts.is_some(), "extra_hosts"),
                    (s.firewall.is_some(), "firewall"),
                    (s.network_mode.is_some(), "network_mode"),
                    (s.ipc.is_some(), "ipc"),
                ] {
                    if set {
                        return Err(Error::config(format!(
                            "service {svc_name}: {key} belongs to the pod; set it under pods.{p}"
                        )));
                    }
                }
                p.clone()
            }
            None => {
                let mode = s.network_mode.as_deref();
                match mode {
                    Some(m) if m.starts_with("service:") || m.starts_with("container:") => {
                        // Resolved below, once every service has a pod.
                        String::new()
                    }
                    Some("none") | None => {
                        if pods.contains_key(&svc_name) {
                            return Err(Error::config(format!(
                                "service {svc_name}: a pod with the same name exists; add `pod: {svc_name}` to the service"
                            )));
                        }
                        let nets = if mode == Some("none") {
                            None
                        } else {
                            s.networks.clone()
                        };
                        let mut atts =
                            attachments(&svc_name, nets, &networks, s.mac_address.as_deref())?;
                        if mode.is_none()
                            && atts.is_empty()
                            && s.networks.is_none()
                            && networks.len() == 1
                        {
                            warnings.push(format!(
                                "service {svc_name} has no networks; its pod only has loopback (add networks: to attach it)"
                            ));
                        }
                        atts.shrink_to_fit();
                        let sysctls_all = list_or_map(s.sysctls.clone(), ctx, false);
                        let pod = Pod {
                            name: svc_name.clone(),
                            hostname: s.hostname.clone().unwrap_or_else(|| svc_name.clone()),
                            domainname: s.domainname.clone(),
                            share_uts: true,
                            share_ipc: true,
                            attachments: atts,
                            dns: ip_list(s.dns.clone(), &format!("service {svc_name}: dns"))?,
                            dns_search: s
                                .dns_search
                                .clone()
                                .map(raw::StringOrList::into_vec)
                                .unwrap_or_default(),
                            dns_options: s.dns_opt.clone().unwrap_or_default(),
                            extra_hosts: extra_hosts(
                                s.extra_hosts.clone(),
                                &format!("service {svc_name}"),
                            )?,
                            sysctls: sysctls_all
                                .into_iter()
                                .filter(|(k, _)| k.starts_with("net."))
                                .collect(),
                            ipv6: true,
                            firewall: s
                                .firewall
                                .clone()
                                .map(|fw| firewall(&svc_name, fw))
                                .transpose()?,
                            implicit: true,
                        };
                        pods.insert(svc_name.clone(), pod);
                        svc_name.clone()
                    }
                    Some(m) => {
                        return Err(Error::config(format!(
                            "service {svc_name}: network_mode {m:?} is not supported; kirisame always runs services in a pod"
                        )));
                    }
                }
            }
        };
        let svc = service(
            &name,
            &svc_name,
            s,
            pod_name,
            runtime,
            ctx,
            &volumes,
            &mut warnings,
        )?;
        services.insert(svc_name, svc);
    }

    // `network_mode: service:x` joins x's pod.
    for (svc_name, mode) in &service_mode {
        let target = mode
            .strip_prefix("service:")
            .or_else(|| mode.strip_prefix("container:"));
        if let Some(t) = target {
            let pod = services
                .get(t)
                .map(|s: &Service| s.pod.clone())
                .filter(|p| !p.is_empty())
                .ok_or_else(|| {
                    Error::config(format!(
                        "service {svc_name}: network_mode {mode:?} refers to an unknown service"
                    ))
                })?;
            if let Some(s) = services.get_mut(svc_name) {
                s.pod = pod;
            }
        }
    }

    // Runtime precedence: CLI override, service, pod, project.
    for s in services.values_mut() {
        if ctx.runtime_override.is_none() && !s.runtime_explicit() {
            if let Some(r) = pod_runtimes.get(&s.pod) {
                s.runtime = *r;
            }
        }
    }

    // Drop implicit pods whose service was moved to another pod by network_mode.
    let used: Vec<String> = services.values().map(|s| s.pod.clone()).collect();
    pods.retain(|n, p| used.contains(n) || !p.implicit);

    // Cross references.
    for s in services.values() {
        for dep in s.depends_on.keys() {
            if !services.contains_key(dep) {
                return Err(Error::config(format!(
                    "service {}: depends_on {dep:?} is not an active service",
                    s.name
                )));
            }
        }
    }
    check_cycles(&services)?;
    for pod in pods.values() {
        if let Some(fw) = &pod.firewall {
            for rule in fw.input.iter().chain(&fw.output) {
                for peer in &rule.peers {
                    let ok = match peer {
                        Peer::Pod(p) => pods.contains_key(p),
                        Peer::Service(s) => services.contains_key(s),
                        Peer::Network(n) => networks.contains_key(n),
                        _ => true,
                    };
                    if !ok {
                        return Err(Error::config(format!(
                            "pod {}: firewall refers to unknown {peer:?}",
                            pod.name
                        )));
                    }
                }
                if let Some(i) = &rule.interface {
                    if !pod.attachments.iter().any(|a| &a.ifname == i) {
                        return Err(Error::config(format!(
                            "pod {}: firewall interface {i:?} is not one of the pod's interfaces",
                            pod.name
                        )));
                    }
                }
            }
        }
    }
    // A static address must not be used twice.
    let mut seen: IndexMap<IpAddr, String> = IndexMap::new();
    for pod in pods.values() {
        for a in &pod.attachments {
            if let Addressing::Static(addrs) = &a.addressing {
                for addr in addrs {
                    if let Some(other) = seen.insert(addr.addr(), pod.name.clone()) {
                        return Err(Error::config(format!(
                            "address {} is used by pods {other} and {}",
                            addr.addr(),
                            pod.name
                        )));
                    }
                }
            }
        }
    }

    let mut project = Project {
        name,
        dir: ctx.project_dir.clone(),
        runtime,
        networks,
        pods,
        services,
        volumes,
    };
    finish(&mut project);
    Ok((project, warnings))
}

impl Service {
    fn runtime_explicit(&self) -> bool {
        self.labels.contains_key(RUNTIME_EXPLICIT)
    }
}

/// Internal marker removed before the project is returned to callers.
const RUNTIME_EXPLICIT: &str = "io.kirisame.internal.runtime-explicit";

fn project_name(ctx: &Ctx<'_>, from_file: Option<&str>) -> Result<String> {
    let raw = ctx
        .project_name
        .clone()
        .or_else(|| (ctx.lookup)("COMPOSE_PROJECT_NAME"))
        .or_else(|| from_file.map(str::to_string))
        .or_else(|| {
            ctx.project_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "default".into());
    let name: String = raw
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err(Error::config(format!("invalid project name {raw:?}")));
    }
    Ok(name)
}

fn check_name(name: &str, what: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok {
        Ok(())
    } else {
        Err(Error::config(format!("invalid {what} name {name:?}")))
    }
}

fn parse_runtime(s: &str, what: &str) -> Result<RuntimeKind> {
    RuntimeKind::parse(s).ok_or_else(|| {
        Error::config(format!(
            "{what}: unknown runtime {s:?} (docker, podman or kiri)"
        ))
    })
}

fn network(name: &str, n: raw::Network, warnings: &mut Vec<String>) -> Result<Network> {
    for key in n.extra.keys() {
        match key.as_str() {
            "external" | "name" | "labels" | "enable_ipv6" | "internal" | "attachable"
            | "driver_opts" => warnings.push(format!("network {name}: ignoring {key:?}")),
            k if k.starts_with("x-") => {}
            k => return Err(Error::config(format!("network {name}: unknown key {k:?}"))),
        }
    }
    if let Some(d) = n
        .driver
        .as_deref()
        .filter(|d| !matches!(*d, "macvlan" | "ipvlan"))
    {
        return Err(Error::config(format!(
            "network {name}: driver {d:?} is not supported; pods attach directly to the LAN with macvlan or ipvlan"
        )));
    }
    let mut opts: IndexMap<String, String> = n
        .driver_opts
        .into_iter()
        .map(|(k, v)| (k, v.into_string()))
        .collect();
    let parent = n
        .parent
        .or_else(|| opts.shift_remove("parent"))
        .ok_or_else(|| {
            Error::config(format!(
                "network {name}: parent (host interface) is required"
            ))
        })?;
    let mode = n
        .mode
        .or_else(|| opts.shift_remove("mode"))
        .or_else(|| opts.shift_remove("macvlan_mode"))
        .or_else(|| opts.shift_remove("ipvlan_mode"));
    let mtu = match (n.mtu, opts.shift_remove("mtu")) {
        (Some(m), _) => Some(m),
        (None, Some(m)) => Some(
            m.parse()
                .map_err(|_| Error::config(format!("network {name}: invalid mtu {m:?}")))?,
        ),
        (None, None) => None,
    };
    for k in opts.keys() {
        warnings.push(format!("network {name}: ignoring driver option {k:?}"));
    }
    let driver = match n.driver.as_deref().unwrap_or("macvlan") {
        "macvlan" => NetDriver::Macvlan(match mode.as_deref().unwrap_or("bridge") {
            "bridge" => MacvlanMode::Bridge,
            "private" => MacvlanMode::Private,
            "vepa" => MacvlanMode::Vepa,
            "passthru" => MacvlanMode::Passthru,
            m => {
                return Err(Error::config(format!(
                    "network {name}: macvlan mode {m:?} (bridge, private, vepa, passthru)"
                )));
            }
        }),
        "ipvlan" => NetDriver::Ipvlan(match mode.as_deref().unwrap_or("l2") {
            "l2" => IpvlanMode::L2,
            "l3" => IpvlanMode::L3,
            "l3s" => IpvlanMode::L3s,
            m => {
                return Err(Error::config(format!(
                    "network {name}: ipvlan mode {m:?} (l2, l3, l3s)"
                )));
            }
        }),
        d => {
            return Err(Error::config(format!(
                "network {name}: driver {d:?} is not supported; pods attach directly to the LAN with macvlan or ipvlan"
            )));
        }
    };
    let mut subnets = Vec::new();
    for c in n.ipam.map(|i| i.config).unwrap_or_default() {
        let cidr: IpNet = c
            .subnet
            .parse()
            .map_err(|_| Error::config(format!("network {name}: invalid subnet {:?}", c.subnet)))?;
        let gateway = c
            .gateway
            .as_deref()
            .map(|g| {
                g.parse::<IpAddr>()
                    .map_err(|_| Error::config(format!("network {name}: invalid gateway {g:?}")))
            })
            .transpose()?;
        if let Some(g) = gateway {
            if !cidr.contains(&g) {
                return Err(Error::config(format!(
                    "network {name}: gateway {g} is outside {cidr}"
                )));
            }
        }
        for key in c.extra.keys() {
            warnings.push(format!("network {name}: ignoring ipam {key:?}"));
        }
        subnets.push(crate::project::Subnet {
            cidr: cidr.trunc(),
            gateway,
        });
    }
    Ok(Network {
        name: name.into(),
        driver,
        parent,
        subnets,
        mtu,
    })
}

fn attachments(
    owner: &str,
    raw: Option<raw::Attachments>,
    networks: &IndexMap<String, Network>,
    mac_for_first: Option<&str>,
) -> Result<Vec<Attachment>> {
    let pairs = raw.map(raw::Attachments::into_pairs).unwrap_or_default();
    let mut out = Vec::new();
    let mut explicit_default = 0;
    for (i, (net_name, a)) in pairs.into_iter().enumerate() {
        let net = networks.get(&net_name).ok_or_else(|| {
            Error::config(format!(
                "{owner}: network {net_name:?} is not defined under networks:"
            ))
        })?;
        for key in a.extra.keys() {
            if key == "aliases" || key == "priority" || key.starts_with("x-") {
                continue;
            }
            return Err(Error::config(format!(
                "{owner}: network {net_name}: unknown key {key:?}"
            )));
        }
        let mut addrs = Vec::new();
        for (s, what) in a
            .ipv4_address
            .iter()
            .map(|s| (s, "ipv4_address"))
            .chain(a.ipv6_address.iter().map(|s| (s, "ipv6_address")))
            .chain(a.addresses.iter().map(|s| (s, "addresses")))
        {
            addrs.push(
                address(s, net).map_err(|e| {
                    Error::config(format!("{owner}: network {net_name}: {what}: {e}"))
                })?,
            );
        }
        let addressing = if a.dhcp {
            if addrs.iter().any(|a| a.addr().is_ipv4()) {
                return Err(Error::config(format!(
                    "{owner}: network {net_name}: dhcp and a static IPv4 address are exclusive"
                )));
            }
            Addressing::Dhcp
        } else if addrs.is_empty() {
            if net.subnets.iter().any(|s| s.cidr.addr().is_ipv4()) {
                return Err(Error::config(format!(
                    "{owner}: network {net_name}: set ipv4_address or dhcp: true (kirisame does not allocate LAN addresses)"
                )));
            }
            Addressing::None
        } else {
            Addressing::Static(addrs.clone())
        };
        // DHCP pods may also carry static IPv6 addresses.
        let addressing = match (addressing, addrs.is_empty()) {
            (Addressing::Dhcp, false) => {
                return Err(Error::config(format!(
                    "{owner}: network {net_name}: static addresses together with dhcp are not supported yet"
                )));
            }
            (a, _) => a,
        };
        let mac_str = a
            .mac_address
            .as_deref()
            .or(if i == 0 { mac_for_first } else { None });
        let mac = mac_str
            .map(|m| parse_mac(m).map_err(|e| Error::config(format!("{owner}: {e}"))))
            .transpose()?;
        if mac.is_some() && matches!(net.driver, NetDriver::Ipvlan(_)) {
            return Err(Error::config(format!(
                "{owner}: network {net_name}: ipvlan links share the parent's MAC address"
            )));
        }
        if a.default_route == Some(true) {
            explicit_default += 1;
        }
        let gateway = a
            .gateway
            .as_deref()
            .map(|g| {
                g.parse::<IpAddr>()
                    .map_err(|_| Error::config(format!("{owner}: invalid gateway {g:?}")))
            })
            .transpose()?;
        out.push((
            a.default_route,
            Attachment {
                network: net_name,
                ifname: a.interface.clone().unwrap_or_else(|| format!("eth{i}")),
                addressing,
                mac,
                default_route: false,
                gateway,
            },
        ));
    }
    if explicit_default > 1 {
        return Err(Error::config(format!(
            "{owner}: only one network may set default_route: true"
        )));
    }
    let first_default = if explicit_default == 1 {
        out.iter().position(|(d, _)| *d == Some(true))
    } else {
        out.iter().position(|(d, _)| *d != Some(false))
    };
    let mut names = Vec::new();
    let mut result = Vec::new();
    for (i, (_, mut a)) in out.into_iter().enumerate() {
        a.default_route = Some(i) == first_default;
        if names.contains(&a.ifname) {
            return Err(Error::config(format!(
                "{owner}: interface name {} is used twice",
                a.ifname
            )));
        }
        if a.ifname.len() > 15 || a.ifname.is_empty() || a.ifname == "lo" {
            return Err(Error::config(format!(
                "{owner}: invalid interface name {:?}",
                a.ifname
            )));
        }
        names.push(a.ifname.clone());
        result.push(a);
    }
    Ok(result)
}

/// Parses `10.0.0.5` (prefix from the network's subnet) or `10.0.0.5/24`.
fn address(s: &str, net: &Network) -> std::result::Result<IpNet, String> {
    if let Ok(n) = s.parse::<IpNet>() {
        return Ok(n);
    }
    let ip: IpAddr = s.parse().map_err(|_| format!("invalid address {s:?}"))?;
    let subnet = net
        .subnets
        .iter()
        .find(|sn| sn.cidr.contains(&ip))
        .ok_or_else(|| {
            format!("{ip} is outside the network's subnets; write it with a prefix length")
        })?;
    IpNet::new(ip, subnet.cidr.prefix_len()).map_err(|e| e.to_string())
}

pub fn parse_mac(s: &str) -> std::result::Result<[u8; 6], String> {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    if parts.len() != 6 {
        return Err(format!("invalid MAC address {s:?}"));
    }
    let mut mac = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        mac[i] = u8::from_str_radix(p, 16).map_err(|_| format!("invalid MAC address {s:?}"))?;
    }
    if mac[0] & 1 == 1 {
        return Err(format!("MAC address {s} is multicast"));
    }
    Ok(mac)
}

fn ip_list(v: Option<raw::StringOrList>, what: &str) -> Result<Vec<IpAddr>> {
    v.map(raw::StringOrList::into_vec)
        .unwrap_or_default()
        .iter()
        .map(|s| {
            s.parse()
                .map_err(|_| Error::config(format!("{what}: invalid address {s:?}")))
        })
        .collect()
}

fn extra_hosts(v: Option<raw::ListOrMap>, what: &str) -> Result<Vec<(String, IpAddr)>> {
    let mut out = Vec::new();
    let entries: Vec<(String, String)> = match v {
        None => vec![],
        Some(raw::ListOrMap::List(l)) => l
            .into_iter()
            .map(|e| {
                // host:ip, host=ip; IPv6 addresses contain colons, so split on the first.
                let (h, ip) = e
                    .split_once('=')
                    .or_else(|| e.split_once(':'))
                    .unwrap_or((&e, ""));
                (h.to_string(), ip.trim_matches(['[', ']']).to_string())
            })
            .collect(),
        Some(raw::ListOrMap::Map(m)) => m
            .into_iter()
            .map(|(k, v)| (k, v.map(raw::Scalar::into_string).unwrap_or_default()))
            .collect(),
    };
    for (h, ip) in entries {
        let addr = ip.parse().map_err(|_| {
            Error::config(format!("{what}: extra_hosts {h}: invalid address {ip:?}"))
        })?;
        out.push((h, addr));
    }
    Ok(out)
}

fn list_or_map(
    v: Option<raw::ListOrMap>,
    ctx: &Ctx<'_>,
    from_env: bool,
) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    match v {
        None => {}
        Some(raw::ListOrMap::List(l)) => {
            for e in l {
                match e.split_once('=') {
                    Some((k, v)) => {
                        out.insert(k.to_string(), v.to_string());
                    }
                    None if from_env => {
                        if let Some(v) = (ctx.lookup)(&e) {
                            out.insert(e, v);
                        }
                    }
                    None => {
                        out.insert(e, String::new());
                    }
                }
            }
        }
        Some(raw::ListOrMap::Map(m)) => {
            for (k, v) in m {
                match v {
                    Some(v) => {
                        out.insert(k, v.into_string());
                    }
                    None if from_env => {
                        if let Some(v) = (ctx.lookup)(&k) {
                            out.insert(k, v);
                        }
                    }
                    None => {
                        out.insert(k, String::new());
                    }
                }
            }
        }
    }
    out
}

fn firewall(owner: &str, fw: raw::Firewall) -> Result<Firewall> {
    let policy = fw.policy.unwrap_or_default();
    let action = |s: Option<&str>, default: Action, what: &str| -> Result<Action> {
        match s {
            None => Ok(default),
            Some("accept" | "allow") => Ok(Action::Accept),
            Some("drop" | "deny") => Ok(Action::Drop),
            Some("reject") => Ok(Action::Reject),
            Some(o) => Err(Error::config(format!(
                "pod {owner}: firewall {what}: unknown action {o:?} (accept, drop, reject)"
            ))),
        }
    };
    let mut input = Vec::new();
    for (i, r) in fw.input.into_iter().enumerate() {
        if r.to.is_some() {
            return Err(Error::config(format!(
                "pod {owner}: firewall input[{i}]: use from: (to: is for output rules)"
            )));
        }
        let peers = r.from.clone();
        input.push(rule(owner, &format!("input[{i}]"), r, peers, &action)?);
    }
    let mut output = Vec::new();
    for (i, r) in fw.output.into_iter().enumerate() {
        if r.from.is_some() {
            return Err(Error::config(format!(
                "pod {owner}: firewall output[{i}]: use to: (from: is for input rules)"
            )));
        }
        let peers = r.to.clone();
        output.push(rule(owner, &format!("output[{i}]"), r, peers, &action)?);
    }
    Ok(Firewall {
        input_policy: action(policy.input.as_deref(), Action::Drop, "policy.input")?,
        output_policy: action(policy.output.as_deref(), Action::Accept, "policy.output")?,
        allow_ping: fw.allow_ping.unwrap_or(true),
        log_dropped: fw.log_dropped,
        input,
        output,
    })
}

type ActionParser<'a> = &'a dyn Fn(Option<&str>, Action, &str) -> Result<Action>;

fn rule(
    owner: &str,
    what: &str,
    r: raw::Rule,
    peers: Option<raw::StringOrList>,
    action: ActionParser<'_>,
) -> Result<Rule> {
    let err = |m: String| Error::config(format!("pod {owner}: firewall {what}: {m}"));
    let mut protocols = Vec::new();
    for p in r.proto.map(raw::StringOrList::into_vec).unwrap_or_default() {
        match p.to_ascii_lowercase().as_str() {
            "tcp" => protocols.push(Proto::Tcp),
            "udp" => protocols.push(Proto::Udp),
            "icmp" => protocols.push(Proto::Icmp),
            "icmpv6" | "ipv6-icmp" => protocols.push(Proto::Icmpv6),
            "any" | "all" => {}
            o => return Err(err(format!("unknown proto {o:?}"))),
        }
    }
    let mut ports = Vec::new();
    for p in r.port.into_iter().chain(r.ports.unwrap_or_default()) {
        ports.push(parse_port_range(&p.into_string()).map_err(err)?);
    }
    if !ports.is_empty() {
        if protocols.is_empty() {
            protocols.push(Proto::Tcp);
        }
        if protocols
            .iter()
            .any(|p| matches!(p, Proto::Icmp | Proto::Icmpv6))
        {
            return Err(err("ports only apply to tcp and udp".into()));
        }
    }
    let mut parsed_peers = Vec::new();
    for p in peers.map(raw::StringOrList::into_vec).unwrap_or_default() {
        parsed_peers.push(parse_peer(&p).map_err(err)?);
    }
    if parsed_peers.contains(&Peer::Any) {
        parsed_peers.clear();
    }
    if let Some(l) = &r.limit {
        let ok = l.split_once('/').is_some_and(|(n, unit)| {
            n.trim().parse::<u32>().is_ok()
                && matches!(unit.trim(), "second" | "minute" | "hour" | "day")
        });
        if !ok {
            return Err(err(format!(
                "limit {l:?} must look like 10/second (second, minute, hour, day)"
            )));
        }
    }
    if let Some(c) = &r.comment {
        if c.contains('"') || c.len() > 120 {
            return Err(err(
                "comment must be under 120 characters with no double quotes".into(),
            ));
        }
    }
    Ok(Rule {
        action: action(r.action.as_deref(), Action::Accept, what)?,
        protocols,
        ports,
        peers: parsed_peers,
        interface: r.interface,
        log: r.log,
        limit: r.limit,
        comment: r.comment,
    })
}

pub fn parse_port_range(s: &str) -> std::result::Result<PortRange, String> {
    let parse = |p: &str| p.trim().parse::<u16>().ok().filter(|n| *n > 0);
    match s.split_once('-') {
        Some((a, b)) => match (parse(a), parse(b)) {
            (Some(a), Some(b)) if a <= b => Ok(PortRange { start: a, end: b }),
            _ => Err(format!("invalid port range {s:?}")),
        },
        None => parse(s)
            .map(|p| PortRange { start: p, end: p })
            .ok_or_else(|| format!("invalid port {s:?}")),
    }
}

fn parse_peer(s: &str) -> std::result::Result<Peer, String> {
    if s == "any" || s == "all" {
        return Ok(Peer::Any);
    }
    if let Some(p) = s.strip_prefix("pod:") {
        return Ok(Peer::Pod(p.into()));
    }
    if let Some(p) = s.strip_prefix("service:") {
        return Ok(Peer::Service(p.into()));
    }
    if let Some(p) = s.strip_prefix("network:") {
        return Ok(Peer::Network(p.into()));
    }
    if let Ok(n) = s.parse::<IpNet>() {
        return Ok(Peer::Cidr(n.trunc()));
    }
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Ok(Peer::Cidr(IpNet::from(ip)));
    }
    Err(format!(
        "invalid peer {s:?} (any, a CIDR, pod:<name>, service:<name>, network:<name>)"
    ))
}

#[allow(clippy::too_many_arguments)]
fn service(
    project: &str,
    name: &str,
    s: raw::Service,
    pod: String,
    project_runtime: RuntimeKind,
    ctx: &Ctx<'_>,
    volumes: &IndexMap<String, Volume>,
    warnings: &mut Vec<String>,
) -> Result<Service> {
    let err = |m: String| Error::config(format!("service {name}: {m}"));
    let image = s.image.ok_or_else(|| err("image is required".into()))?;
    let mut labels = list_or_map(s.labels, ctx, false);
    let runtime = match (ctx.runtime_override, &s.runtime) {
        (Some(r), _) => r,
        (None, Some(r)) => {
            labels.insert(RUNTIME_EXPLICIT.into(), String::new());
            parse_runtime(r, &format!("service {name}: runtime"))?
        }
        (None, None) => project_runtime,
    };

    let mut environment = IndexMap::new();
    let env_files: Vec<(String, bool)> = match s.env_file {
        None => vec![],
        Some(raw::EnvFile::One(p)) => vec![(p, true)],
        Some(raw::EnvFile::Many(v)) => v
            .into_iter()
            .map(|e| match e {
                raw::EnvFileEntry::Path(p) => (p, true),
                raw::EnvFileEntry::Long { path, required } => (path, required.unwrap_or(true)),
            })
            .collect(),
    };
    for (path, required) in env_files {
        let full = resolve_path(&ctx.project_dir, &path);
        if !full.exists() && !required {
            continue;
        }
        environment.extend(super::read_env_file(&full)?);
    }
    environment.extend(list_or_map(s.environment, ctx, true));

    let mut mounts = Vec::new();
    for v in s.volumes.unwrap_or_default() {
        mounts.push(mount(v, ctx, volumes).map_err(&err)?);
    }
    for t in s.tmpfs.map(raw::StringOrList::into_vec).unwrap_or_default() {
        let (target, opts) = t.split_once(':').unwrap_or((&t, ""));
        mounts.push(Mount {
            kind: MountKind::Tmpfs,
            source: String::new(),
            target: target.into(),
            read_only: false,
            options: opts
                .split(',')
                .filter(|o| !o.is_empty())
                .map(str::to_string)
                .collect(),
        });
    }

    let healthcheck = s
        .healthcheck
        .map(|h| -> Result<Healthcheck> {
            let dur = |v: Option<String>, what: &str| -> Result<_> {
                v.map(|d| parse_duration(&d).map_err(|e| err(format!("healthcheck.{what}: {e}"))))
                    .transpose()
            };
            let mut disabled = h.disable;
            let test = match h.test {
                None => None,
                Some(raw::StringOrList::One(s)) => Some(vec!["CMD-SHELL".to_string(), s]),
                Some(raw::StringOrList::Many(v)) => match v.first().map(String::as_str) {
                    Some("NONE") => {
                        disabled = true;
                        None
                    }
                    Some("CMD") | Some("CMD-SHELL") if v.len() > 1 => Some(v),
                    _ => {
                        return Err(err(
                            "healthcheck.test must start with CMD, CMD-SHELL or NONE".into(),
                        ));
                    }
                },
            };
            Ok(Healthcheck {
                disabled,
                test,
                interval: dur(h.interval, "interval")?,
                timeout: dur(h.timeout, "timeout")?,
                retries: h.retries,
                start_period: dur(h.start_period, "start_period")?,
                start_interval: dur(h.start_interval, "start_interval")?,
            })
        })
        .transpose()?;

    let depends_on = match s.depends_on {
        None => IndexMap::new(),
        Some(raw::DependsOn::List(l)) => l
            .into_iter()
            .map(|d| (d, DependsCondition::ServiceStarted))
            .collect(),
        Some(raw::DependsOn::Map(m)) => {
            let mut out = IndexMap::new();
            for (d, long) in m {
                let c = match long.condition.as_deref().unwrap_or("service_started") {
                    "service_started" => DependsCondition::ServiceStarted,
                    "service_healthy" => DependsCondition::ServiceHealthy,
                    "service_completed_successfully" => {
                        DependsCondition::ServiceCompletedSuccessfully
                    }
                    o => return Err(err(format!("depends_on {d}: unknown condition {o:?}"))),
                };
                out.insert(d, c);
            }
            out
        }
    };

    if let Some(r) = &s.restart {
        let base = r.split(':').next().unwrap_or("");
        if !matches!(base, "no" | "always" | "on-failure" | "unless-stopped") {
            return Err(err(format!(
                "restart {r:?} (no, always, on-failure[:N], unless-stopped)"
            )));
        }
    }

    let mut ports = Vec::new();
    for p in s.ports.unwrap_or_default() {
        let (proto, range, published) = match p {
            raw::Port::Num(n) => (None, n.to_string(), None),
            raw::Port::Short(s) => {
                let (spec, proto) = match s.rsplit_once('/') {
                    Some((a, b)) => (a.to_string(), Some(b.to_string())),
                    None => (s.clone(), None),
                };
                // [ip:][published:]target ; IPv6 host addresses are bracketed.
                let parts: Vec<&str> = spec.rsplitn(2, ':').collect();
                let target = parts[0].to_string();
                let published = parts
                    .get(1)
                    .map(|p| p.rsplit(':').next().unwrap_or("").to_string());
                (proto, target, published.filter(|p| !p.is_empty()))
            }
            raw::Port::Long(l) => (
                l.protocol,
                l.target.to_string(),
                l.published.map(raw::Scalar::into_string),
            ),
        };
        let range = parse_port_range(&range).map_err(&err)?;
        if let Some(pubp) = published {
            if parse_port_range(&pubp).ok() != Some(range) {
                warnings.push(format!(
                    "service {name}: ports {pubp}:{range}: pods are reached on their own address, so only {range} is opened"
                ));
            }
        }
        let proto = match proto.as_deref().unwrap_or("tcp") {
            "tcp" => Proto::Tcp,
            "udp" => Proto::Udp,
            o => return Err(err(format!("ports: unknown protocol {o:?}"))),
        };
        ports.push((proto, range));
    }
    let mut expose = Vec::new();
    for e in s.expose.unwrap_or_default() {
        let e = e.into_string();
        let (spec, proto) = e.split_once('/').unwrap_or((&e, "tcp"));
        let proto = match proto {
            "tcp" => Proto::Tcp,
            "udp" => Proto::Udp,
            o => return Err(err(format!("expose: unknown protocol {o:?}"))),
        };
        expose.push((proto, parse_port_range(spec).map_err(&err)?));
    }

    let sysctls = list_or_map(s.sysctls, ctx, false);
    if let Some(k) = sysctls.keys().find(|k| k.starts_with("net.")) {
        if !pod.is_empty() && pod != name {
            return Err(err(format!(
                "sysctl {k} applies to the shared network namespace; set it under pods.{pod}.sysctls"
            )));
        }
    }
    let sysctls: IndexMap<String, String> = sysctls
        .into_iter()
        .filter(|(k, _)| !k.starts_with("net."))
        .collect();

    let ulimits = s
        .ulimits
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| match v {
            raw::Ulimit::Single(n) => (k, (n, n)),
            raw::Ulimit::Pair { soft, hard } => (k, (soft, hard)),
        })
        .collect();

    let pull_policy = match s.pull_policy.as_deref().unwrap_or("missing") {
        "missing" | "if_not_present" | "build" => PullPolicy::Missing,
        "always" => PullPolicy::Always,
        "never" => PullPolicy::Never,
        o => return Err(err(format!("pull_policy {o:?} (missing, always, never)"))),
    };

    let split = |v: Option<raw::StringOrList>, what: &str| -> Result<Option<Vec<String>>> {
        match v {
            None => Ok(None),
            Some(raw::StringOrList::Many(v)) => Ok(Some(v)),
            Some(raw::StringOrList::One(s)) => shell_words::split(&s)
                .map(Some)
                .map_err(|e| err(format!("{what}: {e}"))),
        }
    };

    Ok(Service {
        name: name.into(),
        pod,
        runtime,
        image,
        container_name: s
            .container_name
            .unwrap_or_else(|| format!("{project}-{name}-1")),
        command: split(s.command, "command")?,
        entrypoint: split(s.entrypoint, "entrypoint")?,
        environment,
        working_dir: s.working_dir,
        user: s.user.map(raw::Scalar::into_string),
        labels,
        mounts,
        restart: s.restart,
        healthcheck,
        depends_on,
        stop_signal: s.stop_signal,
        stop_grace_period: s
            .stop_grace_period
            .map(|d| parse_duration(&d).map_err(|e| err(format!("stop_grace_period: {e}"))))
            .transpose()?,
        cap_add: s.cap_add.unwrap_or_default(),
        cap_drop: s.cap_drop.unwrap_or_default(),
        privileged: s.privileged,
        read_only: s.read_only,
        devices: s.devices.unwrap_or_default(),
        tty: s.tty,
        stdin_open: s.stdin_open,
        init: s.init,
        mem_limit: s
            .mem_limit
            .map(|m| parse_bytes(&m.into_string()).map_err(|e| err(format!("mem_limit: {e}"))))
            .transpose()?,
        cpus: s
            .cpus
            .map(|c| {
                c.into_string()
                    .parse::<f64>()
                    .map_err(|_| err("cpus must be a number".into()))
            })
            .transpose()?,
        pids_limit: s.pids_limit,
        shm_size: s
            .shm_size
            .map(|m| parse_bytes(&m.into_string()).map_err(|e| err(format!("shm_size: {e}"))))
            .transpose()?,
        ulimits,
        security_opt: s.security_opt.unwrap_or_default(),
        group_add: s
            .group_add
            .unwrap_or_default()
            .into_iter()
            .map(raw::Scalar::into_string)
            .collect(),
        sysctls,
        pull_policy,
        ports,
        expose,
        profiles: s.profiles.unwrap_or_default(),
    })
}

fn resolve_path(base: &Path, p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        normalize_path(&base.join(path))
    }
}

/// Removes `.` and `..` components without touching the filesystem.
fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn mount(
    v: raw::VolumeMount,
    ctx: &Ctx<'_>,
    volumes: &IndexMap<String, Volume>,
) -> std::result::Result<Mount, String> {
    let is_path = |s: &str| s.starts_with('.') || s.starts_with('/') || s.starts_with('~');
    match v {
        raw::VolumeMount::Short(s) => {
            let parts: Vec<&str> = s.split(':').collect();
            let (source, target, opts) = match parts.as_slice() {
                [t] => ("", *t, ""),
                [s, t] => (*s, *t, ""),
                [s, t, o] => (*s, *t, *o),
                _ => return Err(format!("invalid volume {s:?}")),
            };
            let mut read_only = false;
            let mut options = Vec::new();
            for o in opts.split(',').filter(|o| !o.is_empty()) {
                match o {
                    "ro" => read_only = true,
                    "rw" => {}
                    other => options.push(other.to_string()),
                }
            }
            if !target.starts_with('/') {
                return Err(format!("volume target {target:?} must be absolute"));
            }
            if source.is_empty() {
                return Ok(Mount {
                    kind: MountKind::Volume,
                    source: String::new(),
                    target: target.into(),
                    read_only,
                    options,
                });
            }
            if is_path(source) {
                Ok(Mount {
                    kind: MountKind::Bind,
                    source: resolve_path(&ctx.project_dir, source).display().to_string(),
                    target: target.into(),
                    read_only,
                    options,
                })
            } else {
                let vol = volumes
                    .get(source)
                    .ok_or_else(|| format!("volume {source:?} is not defined under volumes:"))?;
                Ok(Mount {
                    kind: MountKind::Volume,
                    source: vol.engine_name.clone(),
                    target: target.into(),
                    read_only,
                    options,
                })
            }
        }
        raw::VolumeMount::Long(l) => {
            let l = *l;
            if !l.target.starts_with('/') {
                return Err(format!("volume target {:?} must be absolute", l.target));
            }
            let mut options = Vec::new();
            match l.kind.as_str() {
                "bind" => {
                    let src = l.source.ok_or("bind mounts need a source")?;
                    if let Some(b) = l.bind {
                        options.extend(b.propagation);
                        options.extend(b.selinux);
                    }
                    Ok(Mount {
                        kind: MountKind::Bind,
                        source: resolve_path(&ctx.project_dir, &src).display().to_string(),
                        target: l.target,
                        read_only: l.read_only,
                        options,
                    })
                }
                "volume" => {
                    let source = match l.source {
                        None => String::new(),
                        Some(s) => volumes
                            .get(&s)
                            .map(|v| v.engine_name.clone())
                            .ok_or_else(|| format!("volume {s:?} is not defined under volumes:"))?,
                    };
                    Ok(Mount {
                        kind: MountKind::Volume,
                        source,
                        target: l.target,
                        read_only: l.read_only,
                        options,
                    })
                }
                "tmpfs" => {
                    if let Some(t) = l.tmpfs {
                        if let Some(size) = t.size {
                            let size = size.into_string();
                            let bytes = parse_bytes(&size)?;
                            options.push(format!("size={bytes}"));
                        }
                        if let Some(mode) = t.mode {
                            options.push(format!("mode={}", mode.into_string()));
                        }
                    }
                    Ok(Mount {
                        kind: MountKind::Tmpfs,
                        source: String::new(),
                        target: l.target,
                        read_only: false,
                        options,
                    })
                }
                o => Err(format!(
                    "volume type {o:?} is not supported (bind, volume, tmpfs)"
                )),
            }
        }
    }
}

fn check_cycles(services: &IndexMap<String, Service>) -> Result<()> {
    fn visit<'a>(
        n: &'a str,
        services: &'a IndexMap<String, Service>,
        state: &mut IndexMap<&'a str, u8>,
        stack: &mut Vec<&'a str>,
    ) -> Result<()> {
        match state.get(n) {
            Some(2) => return Ok(()),
            Some(1) => {
                stack.push(n);
                return Err(Error::config(format!(
                    "depends_on cycle: {}",
                    stack.join(" -> ")
                )));
            }
            _ => {}
        }
        state.insert(n, 1);
        stack.push(n);
        if let Some(s) = services.get(n) {
            for d in s.depends_on.keys() {
                visit(d, services, state, stack)?;
            }
        }
        stack.pop();
        state.insert(n, 2);
        Ok(())
    }
    let mut state = IndexMap::new();
    for n in services.keys() {
        visit(n, services, &mut state, &mut Vec::new())?;
    }
    Ok(())
}

/// Strips internal markers.
fn finish(project: &mut Project) {
    for s in project.services.values_mut() {
        s.labels.shift_remove(RUNTIME_EXPLICIT);
    }
}

#[cfg(test)]
mod tests {
    use crate::compose::{LoadOptions, load};
    use crate::project::*;

    fn try_load(yaml: &str) -> crate::Result<Project> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("compose.yaml"), yaml).unwrap();
        load(&LoadOptions {
            cwd: Some(dir.path().to_path_buf()),
            ..Default::default()
        })
        .map(|l| l.project)
    }

    const NET: &str = "networks:\n  lan: { driver: macvlan, parent: eth9, ipam: { config: [{ subnet: 192.0.2.0/24, gateway: 192.0.2.1 }] } }\n";

    #[test]
    fn implicit_pod_from_service_networks() {
        let p = try_load(&format!(
            "name: t\n{NET}services:\n  web:\n    image: nginx\n    hostname: www\n    mac_address: 02:00:00:00:00:01\n    networks:\n      lan: {{ ipv4_address: 192.0.2.5 }}\n"
        ))
        .unwrap();
        let pod = &p.pods["web"];
        assert!(pod.implicit);
        assert_eq!(pod.hostname, "www");
        assert_eq!(
            pod.attachments[0].addressing,
            Addressing::Static(vec!["192.0.2.5/24".parse().unwrap()])
        );
        assert_eq!(pod.attachments[0].mac, Some([2, 0, 0, 0, 0, 1]));
        assert!(pod.attachments[0].default_route);
        assert_eq!(p.services["web"].container_name, "t-web-1");
    }

    #[test]
    fn network_mode_service_joins_pod() {
        let p = try_load(&format!(
            "name: t\n{NET}services:\n  a:\n    image: x\n    networks: {{ lan: {{ ipv4_address: 192.0.2.5 }} }}\n  b:\n    image: y\n    network_mode: service:a\n"
        ))
        .unwrap();
        assert_eq!(p.services["b"].pod, "a");
        assert!(!p.pods.contains_key("b"));
    }

    #[test]
    fn errors_are_helpful() {
        let cases = [
            (
                "name: t\nnetworks:\n  n: { driver: bridge }\nservices: {}\n",
                "macvlan or ipvlan",
            ),
            (
                &format!("name: t\n{NET}services:\n  a: {{ image: x, networks: [lan] }}\n"),
                "ipv4_address or dhcp",
            ),
            (
                &format!(
                    "name: t\n{NET}pods:\n  p: {{ networks: {{ lan: {{ ipv4_address: 192.0.2.5 }} }} }}\nservices:\n  a: {{ image: x, pod: p, hostname: h }}\n"
                ),
                "belongs to the pod",
            ),
            (
                &format!(
                    "name: t\n{NET}pods:\n  p: {{ networks: {{ lan: {{ ipv4_address: 192.0.2.5 }} }} }}\n  q: {{ networks: {{ lan: {{ ipv4_address: 192.0.2.5 }} }} }}\nservices: {{}}\n"
                ),
                "used by pods",
            ),
            (
                "name: t\nservices:\n  a: { image: x, depends_on: [b] }\n  b: { image: y, depends_on: [a] }\n",
                "cycle",
            ),
            (
                "name: t\nservices:\n  a: { build: . }\n",
                "build is not supported",
            ),
            (
                "name: t\nbogus: 1\nservices: {}\n",
                "unsupported top-level key",
            ),
            (
                &format!(
                    "name: t\n{NET}pods:\n  p:\n    networks: {{ lan: {{ ipv4_address: 192.0.2.5 }} }}\n    firewall: {{ input: [{{ to: 1.2.3.4 }}] }}\nservices: {{}}\n"
                ),
                "use from:",
            ),
            (
                &format!(
                    "name: t\n{NET}pods:\n  p:\n    networks: {{ lan: {{ ipv4_address: 192.0.2.5 }} }}\n    firewall: {{ input: [{{ from: pod:nope }}] }}\nservices: {{}}\n"
                ),
                "unknown",
            ),
            (
                &format!(
                    "name: t\n{NET}pods:\n  p:\n    networks: {{ lan: {{ ipv4_address: 192.0.2.5 }} }}\n    firewall: {{ input: [{{ port: 80, comment: 'a\"b' }}] }}\nservices: {{}}\n"
                ),
                "double quotes",
            ),
        ];
        for (yaml, want) in cases {
            let err = try_load(yaml).unwrap_err().to_string();
            assert!(err.contains(want), "expected {want:?} in {err:?}");
        }
    }

    #[test]
    fn profiles_filter_services() {
        let p = try_load("name: t\nservices:\n  a: { image: x, network_mode: none }\n  b: { image: y, network_mode: none, profiles: [debug] }\n").unwrap();
        assert!(p.services.contains_key("a"));
        assert!(!p.services.contains_key("b"));
    }
}
