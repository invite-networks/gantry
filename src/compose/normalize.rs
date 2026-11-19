//! Validation and normalization of the raw compose structs.

use super::raw;
use super::units::{parse_bytes, parse_duration};
use crate::errors::{Error, Result};
use crate::project::*;
use indexmap::IndexMap;
use ipnet::{IpNet, Ipv4Net};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

pub struct Ctx<'a> {
    pub project_dir: PathBuf,
    pub project_name: Option<String>,
    pub runtime_override: Option<RuntimeKind>,
    pub profiles: Vec<String>,
    pub lookup: &'a dyn Fn(&str) -> Option<String>,
}

/// Service keys gantry knows and ignores, with the reason shown to the user.
const IGNORED_SERVICE_KEYS: &[(&str, &str)] = &[
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

    if f.networks.is_some() {
        return Err(Error::config(
            "top-level networks: is not supported; define each pod's interfaces under pods.<name>.network (a list of parent and address)",
        ));
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

    // Explicit pods. A top-level `pod:` is the only pod, named after the
    // project.
    let mut raw_pods = f.pods;
    if let Some(p) = f.pod {
        if !raw_pods.is_empty() {
            return Err(Error::config(
                "use pod: for a single pod or pods: for several, not both",
            ));
        }
        raw_pods.insert(name.clone(), Some(p));
    }
    let mut pods: IndexMap<String, Pod> = IndexMap::new();
    let mut pod_runtimes: IndexMap<String, RuntimeKind> = IndexMap::new();
    for (pod_name, p) in raw_pods {
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
        let attachments = pod_networks(&pod_name, p.network.unwrap_or_default())?;
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
            priority: p.priority.unwrap_or(DEFAULT_PRIORITY),
            firewall: Some(firewall(&pod_name, p.firewall.unwrap_or_default())?),
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
        // Keys that shape the pod, so they cannot be set on a service.
        let pod_keys = [
            (s.networks.is_some(), "networks"),
            (s.hostname.is_some(), "hostname"),
            (s.domainname.is_some(), "domainname"),
            (s.mac_address.is_some(), "mac_address"),
            (s.dns.is_some(), "dns"),
            (s.dns_search.is_some(), "dns_search"),
            (s.dns_opt.is_some(), "dns_opt"),
            (s.extra_hosts.is_some(), "extra_hosts"),
            (s.firewall.is_some(), "firewall"),
            (s.ipc.is_some(), "ipc"),
        ];
        let explicit_pods: Vec<String> = pods
            .iter()
            .filter(|(_, p)| !p.implicit)
            .map(|(n, _)| n.clone())
            .collect();
        let target = match (&s.pod, s.network_mode.as_deref()) {
            (Some(p), None) => Some(p.clone()),
            (Some(_), Some(_)) => {
                return Err(Error::config(format!(
                    "service {svc_name}: set pod: or network_mode:, not both"
                )));
            }
            (None, Some(m)) if m.starts_with("service:") || m.starts_with("container:") => {
                // Resolved below, once every service has a pod.
                None
            }
            (None, Some("none")) => {
                if pods.contains_key(&svc_name) {
                    return Err(Error::config(format!(
                        "service {svc_name}: a pod with the same name exists; network_mode: none needs its own pod"
                    )));
                }
                pods.insert(svc_name.clone(), loopback_pod(&svc_name));
                None
            }
            (None, Some(m)) => {
                return Err(Error::config(format!(
                    "service {svc_name}: network_mode {m:?} is not supported; gantry always runs services in a pod"
                )));
            }
            (None, None) => match explicit_pods.as_slice() {
                [only] => Some(only.clone()),
                [] => {
                    return Err(Error::config(format!(
                        "service {svc_name}: no pod to run in; define one under pods: with its network"
                    )));
                }
                _ => {
                    return Err(Error::config(format!(
                        "service {svc_name}: set pod: (the project has more than one pod)"
                    )));
                }
            },
        };
        let pod_name = match target {
            Some(p) => {
                if !pods.contains_key(&p) {
                    return Err(Error::config(format!(
                        "service {svc_name}: pod {p:?} is not defined under pods:"
                    )));
                }
                p
            }
            None if s.network_mode.as_deref() == Some("none") => svc_name.clone(),
            None => String::new(),
        };
        for (set, key) in pod_keys {
            if set {
                return Err(Error::config(format!(
                    "service {svc_name}: {key} belongs to the pod; set it under pods:"
                )));
            }
        }
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
    pod_priorities(&mut services)?;
    check_cycles(&services)?;
    for pod in pods.values() {
        check_pod_names(pod)?;
        if let Some(fw) = &pod.firewall {
            for rule in fw.input.iter().chain(&fw.output) {
                for peer in &rule.peers {
                    let ok = match peer {
                        Peer::Pod(p) => pods.contains_key(p),
                        Peer::Service(s) => services.contains_key(s),
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

    // The kernel will not put macvlan and ipvlan links on one parent.
    let mut parents: IndexMap<&str, (NetDriver, &str)> = IndexMap::new();
    for pod in pods.values() {
        for a in &pod.attachments {
            match parents.get(a.parent.as_str()) {
                Some((d, other))
                    if std::mem::discriminant(d) != std::mem::discriminant(&a.driver) =>
                {
                    return Err(Error::config(format!(
                        "pods {other} and {} use {} with different drivers; a parent carries macvlan or ipvlan links, not both",
                        pod.name, a.parent
                    )));
                }
                Some(_) => {}
                None => {
                    parents.insert(&a.parent, (a.driver, &pod.name));
                }
            }
        }
    }

    let mut project = Project {
        name,
        dir: ctx.project_dir.clone(),
        runtime,
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

/// The restart policy of a service that sets none. `restart: "no"` opts out.
pub const DEFAULT_RESTART: &str = "unless-stopped";

/// Internal marker removed before the project is returned to callers.
const RUNTIME_EXPLICIT: &str = "io.gantry.internal.runtime-explicit";

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

/// Printable ASCII without quotes, backslashes or control characters.
fn safe_text(s: &str, max: usize) -> bool {
    s.len() <= max
        && s.chars()
            .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ')
}

/// A DNS name: labels of letters, digits and hyphens, dot separated.
pub fn is_dns_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.trim_end_matches('.').split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                && !l.starts_with('-')
        })
}

fn check_pod_names(pod: &Pod) -> Result<()> {
    let err = |m: String| Error::config(format!("pod {}: {m}", pod.name));
    if !is_dns_name(&pod.hostname) || pod.hostname.len() > 63 {
        return Err(err(format!("invalid hostname {:?}", pod.hostname)));
    }
    if let Some(d) = &pod.domainname {
        if !is_dns_name(d) {
            return Err(err(format!("invalid domainname {d:?}")));
        }
    }
    for d in &pod.dns_search {
        if !is_dns_name(d) {
            return Err(err(format!("invalid dns_search entry {d:?}")));
        }
    }
    for o in &pod.dns_options {
        if o.is_empty() || !o.chars().all(|c| c.is_ascii_graphic()) {
            return Err(err(format!("invalid dns_opt entry {o:?}")));
        }
    }
    for (h, _) in &pod.extra_hosts {
        if !is_dns_name(h) {
            return Err(err(format!("invalid extra_hosts name {h:?}")));
        }
    }
    Ok(())
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
    RuntimeKind::parse(s)
        .ok_or_else(|| Error::config(format!("{what}: unknown runtime {s:?} (docker or kiri)")))
}

/// The pod a `network_mode: none` service runs in: loopback only.
fn loopback_pod(name: &str) -> Pod {
    Pod {
        name: name.into(),
        hostname: name.into(),
        domainname: None,
        share_uts: true,
        share_ipc: true,
        attachments: vec![],
        dns: vec![],
        dns_search: vec![],
        dns_options: vec![],
        extra_hosts: vec![],
        sysctls: IndexMap::new(),
        ipv6: true,
        priority: DEFAULT_PRIORITY,
        firewall: None,
        implicit: true,
    }
}

/// The first usable host of an IPv4 subnet, the default gateway. A /31 or
/// /32 has none.
pub fn first_host(net: &Ipv4Net) -> Option<Ipv4Addr> {
    if net.prefix_len() >= 31 {
        return None;
    }
    Some(Ipv4Addr::from(u32::from(net.network()) + 1))
}

fn pod_networks(pod: &str, raw: Vec<raw::PodNetwork>) -> Result<Vec<Attachment>> {
    let mut out = Vec::new();
    let mut explicit_default = 0;
    for (i, n) in raw.into_iter().enumerate() {
        let iface = n.interface.clone().unwrap_or_else(|| format!("eth{i}"));
        let err = |m: String| Error::config(format!("pod {pod}: network {iface}: {m}"));
        let parent = n
            .parent
            .clone()
            .ok_or_else(|| err("parent (the host interface, such as vlan4) is required".into()))?;
        if parent.is_empty()
            || parent.len() > 15
            || !parent
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@'))
        {
            return Err(err(format!("invalid parent {parent:?}")));
        }
        let driver = match n.driver.as_deref().unwrap_or("macvlan") {
            "macvlan" => NetDriver::Macvlan(MacvlanMode::Bridge),
            "ipvlan" => NetDriver::Ipvlan(IpvlanMode::L2),
            d => {
                return Err(err(format!(
                    "driver {d:?} is not supported (macvlan or ipvlan)"
                )));
            }
        };
        let parse_ip = |s: &str, what: &str| -> Result<IpAddr> {
            s.parse::<IpAddr>()
                .map_err(|_| err(format!("invalid {what} {s:?}")))
        };
        let address = n.address.as_deref().ok_or_else(|| {
            err("address is required: an address with prefix (10.10.10.10/24) or dhcp".into())
        })?;
        let mut gateway = None;
        let addressing = if address == "dhcp" {
            if n.gateway.is_some() {
                return Err(err(
                    "gateway comes from the DHCP lease; remove gateway:".into()
                ));
            }
            if n.address6.is_some() || n.gateway6.is_some() {
                return Err(err(
                    "address6 together with address: dhcp is not supported yet".into(),
                ));
            }
            Addressing::Dhcp
        } else {
            let v4 = match address.parse::<IpNet>() {
                Ok(IpNet::V4(v)) => v,
                Ok(IpNet::V6(_)) => {
                    return Err(err(
                        "address must be IPv4; put an IPv6 address in address6".into()
                    ));
                }
                Err(_) if address.parse::<IpAddr>().is_ok() => {
                    return Err(err(format!(
                        "address {address} needs a prefix length, like {address}/24"
                    )));
                }
                Err(_) => return Err(err(format!("invalid address {address:?}"))),
            };
            let subnet = v4.trunc();
            if v4.prefix_len() < 31
                && (v4.addr() == subnet.network() || v4.addr() == subnet.broadcast())
            {
                return Err(err(format!(
                    "{} is the network or broadcast address of {subnet}",
                    v4.addr()
                )));
            }
            gateway = match &n.gateway {
                Some(g) => match parse_ip(g, "gateway")? {
                    IpAddr::V4(g4) if subnet.contains(&g4) => Some(IpAddr::V4(g4)),
                    g => return Err(err(format!("gateway {g} is outside {subnet}"))),
                },
                None => first_host(&subnet).map(IpAddr::V4),
            };
            if gateway == Some(IpAddr::V4(v4.addr())) {
                return Err(err(format!(
                    "{} is the gateway of {subnet}; set gateway: or pick another address",
                    v4.addr()
                )));
            }
            let mut addrs = vec![IpNet::V4(v4)];
            if let Some(a6) = &n.address6 {
                match a6.parse::<IpNet>() {
                    Ok(IpNet::V6(v)) => addrs.push(IpNet::V6(v)),
                    _ => {
                        return Err(err(format!(
                            "address6 {a6:?} must be an IPv6 address with prefix, like 2001:db8::10/64"
                        )));
                    }
                }
            }
            Addressing::Static(addrs)
        };
        let gateway6 = match (&n.gateway6, &addressing) {
            (None, _) => None,
            (Some(g), Addressing::Static(addrs)) => {
                let g = parse_ip(g, "gateway6")?;
                let ok = g.is_ipv6()
                    && (addrs.iter().any(|a| a.trunc().contains(&g))
                        || matches!(g, IpAddr::V6(v) if v.is_unicast_link_local()));
                if !ok {
                    return Err(err(format!(
                        "gateway6 {g} is not in the address6 subnet or link local"
                    )));
                }
                if n.address6.is_none() {
                    return Err(err("gateway6 needs address6".into()));
                }
                Some(g)
            }
            // Rejected above for DHCP entries.
            (Some(_), Addressing::Dhcp) => None,
        };
        let mac = n
            .mac_address
            .as_deref()
            .map(|m| parse_mac(m).map_err(&err))
            .transpose()?;
        if mac.is_some() && matches!(driver, NetDriver::Ipvlan(_)) {
            return Err(err(
                "ipvlan links share the parent's MAC address; remove mac_address".into(),
            ));
        }
        if n.default_route == Some(true) {
            explicit_default += 1;
        }
        out.push((
            n.default_route,
            Attachment {
                ifname: iface.clone(),
                parent,
                driver,
                addressing,
                mac,
                default_route: false,
                gateway,
                gateway6,
            },
        ));
    }
    if explicit_default > 1 {
        return Err(Error::config(format!(
            "pod {pod}: only one network entry may set default_route: true"
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
        if a.default_route && a.gateway.is_none() && matches!(a.addressing, Addressing::Static(_)) {
            return Err(Error::config(format!(
                "pod {pod}: network {}: a /31 or /32 address has no first host; set gateway:",
                a.ifname
            )));
        }
        if names.contains(&a.ifname) {
            return Err(Error::config(format!(
                "pod {pod}: interface name {} is used twice",
                a.ifname
            )));
        }
        let ifname_ok = a
            .ifname
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
        if a.ifname.len() > 15
            || a.ifname.is_empty()
            || a.ifname == "lo"
            || !ifname_ok
            || a.ifname.starts_with('.')
        {
            return Err(Error::config(format!(
                "pod {pod}: invalid interface name {:?}",
                a.ifname
            )));
        }
        names.push(a.ifname.clone());
        result.push(a);
    }
    Ok(result)
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
        input_policy: action(policy.input.as_deref(), Action::Accept, "policy.input")?,
        output_policy: action(policy.output.as_deref(), Action::Accept, "policy.output")?,
        defaults: fw.defaults.unwrap_or(true),
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
    let limit = match &r.limit {
        None => None,
        Some(l) => {
            let parsed = l.split_once('/').and_then(|(n, unit)| {
                let n = n.trim().parse::<u32>().ok()?;
                let unit = unit.trim();
                matches!(unit, "second" | "minute" | "hour" | "day").then(|| format!("{n}/{unit}"))
            });
            Some(parsed.ok_or_else(|| {
                err(format!(
                    "limit {l:?} must look like 10/second (second, minute, hour, day)"
                ))
            })?)
        }
    };
    if let Some(c) = &r.comment {
        if !safe_text(c, 120) {
            return Err(err(
                "comment must be under 120 printable ASCII characters without quotes or backslashes".into(),
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
        limit,
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
    if s == "subnet" {
        return Ok(Peer::Subnet);
    }
    if let Ok(n) = s.parse::<IpNet>() {
        return Ok(Peer::Cidr(n.trunc()));
    }
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Ok(Peer::Cidr(IpNet::from(ip)));
    }
    Err(format!(
        "invalid peer {s:?} (any, subnet, a CIDR, pod:<name>, service:<name>)"
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
    let build = s
        .build
        .clone()
        .map(|b| build_spec(b, ctx, warnings, name))
        .transpose()?;
    let image = match (s.image.clone(), &build) {
        (Some(i), _) => i,
        (None, Some(_)) => format!("{project}-{name}"),
        (None, None) => return Err(err("image or build is required".into())),
    };
    let logging = s.logging.clone().map(|l| crate::project::Logging {
        driver: l.driver,
        options: list_or_map(l.options, ctx, false),
    });
    let mut labels = list_or_map(s.labels, ctx, false);
    if let Some(k) = labels.keys().find(|k| k.starts_with("io.gantry.")) {
        return Err(err(format!("label {k} is reserved for gantry")));
    }
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
        mounts.push(mount(v, ctx, volumes).map_err(err)?);
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

    let health_on_failure = match s.health_on_failure.as_deref() {
        None => HealthOnFailure::None,
        Some(v) => HealthOnFailure::parse(v)
            .ok_or_else(|| err(format!("health_on_failure {v:?} (none, restart, stop)")))?,
    };
    if health_on_failure != HealthOnFailure::None
        && healthcheck.as_ref().is_some_and(|h| h.disabled)
    {
        return Err(err(format!(
            "health_on_failure: {} needs a healthcheck, but it is disabled",
            health_on_failure.as_str()
        )));
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
        let range = parse_port_range(&range).map_err(err)?;
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
        expose.push((proto, parse_port_range(spec).map_err(err)?));
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
        "missing" | "if_not_present" => PullPolicy::Missing,
        "build" => PullPolicy::Build,
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
        build,
        logging,
        platform: s.platform.clone(),
        container_name: match s.container_name {
            Some(c) => {
                check_container_name(&c).map_err(err)?;
                c
            }
            None => format!("{project}-{name}-1"),
        },
        command: split(s.command, "command")?,
        entrypoint: split(s.entrypoint, "entrypoint")?,
        environment,
        working_dir: s.working_dir,
        user: s.user.map(raw::Scalar::into_string),
        labels,
        mounts,
        priority: s.priority.unwrap_or(DEFAULT_PRIORITY),
        // Engines restart crashed services; systemd only runs `up` at boot.
        restart: s.restart.or_else(|| Some(DEFAULT_RESTART.into())),
        healthcheck,
        health_on_failure,
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

/// Container names as the engines accept them: `[A-Za-z0-9][A-Za-z0-9_.-]*`.
/// They also end up in API paths, so nothing else may pass.
fn check_container_name(c: &str) -> std::result::Result<(), String> {
    let mut chars = c.chars();
    let ok = c.len() <= 253
        && chars.next().is_some_and(|f| f.is_ascii_alphanumeric())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "container_name {c:?} must start with a letter or digit and contain only letters, digits, '_', '.' and '-'"
        ))
    }
}

fn build_spec(
    b: raw::Build,
    ctx: &Ctx<'_>,
    warnings: &mut Vec<String>,
    service: &str,
) -> Result<crate::project::Build> {
    let l = match b {
        raw::Build::Context(c) => raw::BuildLong {
            context: Some(c),
            ..Default::default()
        },
        raw::Build::Long(l) => *l,
    };
    for key in l.extra.keys().filter(|k| !k.starts_with("x-")) {
        warnings.push(format!("service {service}: ignoring build key {key:?}"));
    }
    let context = l.context.unwrap_or_else(|| ".".into());
    if context.contains("://") || context.starts_with("git@") {
        return Err(Error::config(format!(
            "service {service}: remote build contexts are not supported ({context})"
        )));
    }
    let extra_hosts = list_or_map(l.extra_hosts, ctx, false)
        .into_iter()
        .map(|(k, v)| format!("{k}:{v}"))
        .collect();
    Ok(crate::project::Build {
        context: resolve_path(&ctx.project_dir, &context),
        dockerfile: l.dockerfile,
        dockerfile_inline: l.dockerfile_inline,
        args: list_or_map(l.args, ctx, true),
        target: l.target,
        labels: list_or_map(l.labels, ctx, false),
        pull: l.pull,
        no_cache: l.no_cache,
        network: l.network,
        cache_from: l.cache_from.unwrap_or_default(),
        tags: l.tags.unwrap_or_default(),
        extra_hosts,
        shm_size: l
            .shm_size
            .map(|m| {
                parse_bytes(&m.into_string())
                    .map_err(|e| Error::config(format!("service {service}: build.shm_size: {e}")))
            })
            .transpose()?,
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

/// Turns service priorities into depends_on: a service waits for every
/// service of its pod with a lower priority (healthy when it has a
/// healthcheck, else started). Equal priorities start together.
fn pod_priorities(services: &mut IndexMap<String, Service>) -> Result<()> {
    let snapshot: Vec<(String, String, u32, bool)> = services
        .values()
        .map(|s| {
            let healthy = s
                .healthcheck
                .as_ref()
                .is_some_and(|h| !h.disabled && h.test.is_some());
            (s.name.clone(), s.pod.clone(), s.priority, healthy)
        })
        .collect();
    for (name, pod, prio, _) in &snapshot {
        for (other, opod, oprio, healthy) in &snapshot {
            if opod != pod || oprio >= prio {
                continue;
            }
            if services[other].depends_on.contains_key(name) {
                return Err(Error::config(format!(
                    "service {other} (priority {oprio}) depends on {name} (priority {prio}) in pod {pod}; lower priorities start first, so give {name} the lower priority or drop the depends_on"
                )));
            }
            let cond = if *healthy {
                DependsCondition::ServiceHealthy
            } else {
                DependsCondition::ServiceStarted
            };
            if let Some(s) = services.get_mut(name) {
                s.depends_on.entry(other.clone()).or_insert(cond);
            }
        }
    }
    Ok(())
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

    /// One pod on the documentation subnet.
    const POD: &str = "pods:\n  p:\n    network: [{ parent: eth9, address: 192.0.2.5/24 }]\n";

    #[test]
    fn pod_network_defaults() {
        let p = try_load(&format!(
            "name: t\n{POD}services:\n  web: {{ image: nginx }}\n"
        ))
        .unwrap();
        let a = &p.pods["p"].attachments[0];
        assert_eq!(a.ifname, "eth0");
        assert_eq!(a.parent, "eth9");
        assert_eq!(a.driver, NetDriver::Macvlan(MacvlanMode::Bridge));
        assert_eq!(
            a.addressing,
            Addressing::Static(vec!["192.0.2.5/24".parse().unwrap()])
        );
        assert_eq!(a.gateway, Some("192.0.2.1".parse().unwrap()));
        assert_eq!(a.gateway6, None);
        assert!(a.default_route);
        // The only pod takes services without pod:.
        assert_eq!(p.services["web"].pod, "p");
        assert_eq!(p.services["web"].container_name, "t-web-1");
        // Services restart unless stopped, unless they say otherwise.
        assert_eq!(p.services["web"].restart.as_deref(), Some("unless-stopped"));
        let q = try_load(&format!(
            "name: t\n{POD}services:\n  a: {{ image: x, restart: \"no\" }}\n  b: {{ image: x, restart: on-failure:3 }}\n"
        ))
        .unwrap();
        assert_eq!(q.services["a"].restart.as_deref(), Some("no"));
        assert_eq!(q.services["b"].restart.as_deref(), Some("on-failure:3"));
        assert_eq!(q.services["a"].health_on_failure, HealthOnFailure::None);
        let h = try_load(&format!(
            "name: t\n{POD}services:\n  a: {{ image: x, health_on_failure: restart }}\n  b: {{ image: x, health_on_failure: stop, healthcheck: {{ test: [CMD, 'true'] }} }}\n"
        ))
        .unwrap();
        assert_eq!(h.services["a"].health_on_failure, HealthOnFailure::Restart);
        assert_eq!(h.services["b"].health_on_failure, HealthOnFailure::Stop);
        let bad = try_load(&format!(
            "name: t\n{POD}services:\n  a: {{ image: x, health_on_failure: kill }}\n"
        ));
        assert!(bad.unwrap_err().to_string().contains("none, restart, stop"));
        let off = try_load(&format!(
            "name: t\n{POD}services:\n  a: {{ image: x, health_on_failure: restart, healthcheck: {{ disable: true }} }}\n"
        ));
        assert!(off.unwrap_err().to_string().contains("disabled"));
        // Every pod gets a firewall with the defaults on and input open.
        let fw = p.pods["p"].firewall.as_ref().unwrap();
        assert!(fw.defaults);
        assert_eq!(fw.input_policy, Action::Accept);
    }

    #[test]
    fn pod_network_options() {
        let p = try_load(
            "name: t\npods:\n  p:\n    network:\n      - { parent: eth9, address: 192.0.2.5/24, driver: ipvlan, gateway: 192.0.2.254, address6: '2001:db8::5/64', gateway6: 'fe80::1' }\n      - { parent: eth8, address: dhcp, interface: mgmt, default_route: true }\nservices: {}\n",
        )
        .unwrap();
        let [a, b] = p.pods["p"].attachments.as_slice() else {
            panic!("two attachments")
        };
        assert_eq!(a.driver, NetDriver::Ipvlan(IpvlanMode::L2));
        assert_eq!(a.gateway, Some("192.0.2.254".parse().unwrap()));
        assert_eq!(a.gateway6, Some("fe80::1".parse().unwrap()));
        assert_eq!(
            a.addressing,
            Addressing::Static(vec![
                "192.0.2.5/24".parse().unwrap(),
                "2001:db8::5/64".parse().unwrap()
            ])
        );
        assert!(!a.default_route);
        assert_eq!(b.ifname, "mgmt");
        assert_eq!(b.addressing, Addressing::Dhcp);
        assert!(b.default_route);
    }

    #[test]
    fn single_unnamed_pod() {
        let p = try_load(
            "name: shop\npod:\n  network: [{ parent: eth9, address: 192.0.2.5/24 }]\nservices:\n  web: { image: x }\n  db: { image: y }\n",
        )
        .unwrap();
        assert_eq!(p.pods.keys().collect::<Vec<_>>(), vec!["shop"]);
        assert_eq!(p.pods["shop"].hostname, "shop");
        assert_eq!(p.services["web"].pod, "shop");
        assert_eq!(p.services["db"].pod, "shop");
        let e = try_load(&format!(
            "name: shop\npod:\n  network: [{{ parent: eth9, address: 192.0.2.6/24 }}]\n{POD}services: {{}}\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(e.contains("not both"), "{e}");
    }

    #[test]
    fn service_priority_orders_the_pod() {
        let p = try_load(&format!(
            "name: t\n{POD}    priority: 10\nservices:\n  db: {{ image: x, priority: 10, healthcheck: {{ test: [CMD, 'true'] }} }}\n  cache: {{ image: x, priority: 10 }}\n  app: {{ image: x, depends_on: [cache] }}\n  web: {{ image: x }}\n  late: {{ image: x, priority: 200 }}\n"
        ))
        .unwrap();
        assert_eq!(p.pods["p"].priority, 10);
        let app = &p.services["app"];
        assert_eq!(app.priority, 100);
        // Lower priorities become depends_on: healthy with a healthcheck,
        // else started. An explicit entry keeps its condition.
        assert_eq!(app.depends_on["db"], DependsCondition::ServiceHealthy);
        assert_eq!(app.depends_on["cache"], DependsCondition::ServiceStarted);
        // Equal priorities do not wait for each other.
        assert!(!p.services["db"].depends_on.contains_key("cache"));
        assert!(!p.services["web"].depends_on.contains_key("app"));
        assert_eq!(p.services["late"].depends_on.len(), 4);
        let o = crate::orchestrator::Orchestrator::new(p);
        let levels = o.levels(&o.start_order(&[]).unwrap());
        assert_eq!(levels.len(), 3, "{levels:?}");
        assert!(levels[0].contains(&"db".to_string()) && levels[0].contains(&"cache".to_string()));
        assert!(levels[1].contains(&"app".to_string()) && levels[1].contains(&"web".to_string()));
        assert_eq!(levels[2], vec!["late".to_string()]);
    }

    #[test]
    fn priority_contradicting_depends_on_is_refused() {
        let e = try_load(&format!(
            "name: t\n{POD}services:\n  db: {{ image: x, priority: 10, depends_on: [app] }}\n  app: {{ image: x }}\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(e.contains("lower priorities start first"), "{e}");
    }

    #[test]
    fn first_host_is_the_default_gateway() {
        use super::first_host;
        let f = |s: &str| first_host(&s.parse().unwrap()).map(|a| a.to_string());
        assert_eq!(f("10.10.10.0/24").as_deref(), Some("10.10.10.1"));
        assert_eq!(f("10.10.8.0/22").as_deref(), Some("10.10.8.1"));
        assert_eq!(f("192.0.2.216/29").as_deref(), Some("192.0.2.217"));
        assert_eq!(f("10.0.0.0/31"), None);
        assert_eq!(f("10.0.0.1/32"), None);
    }

    #[test]
    fn network_mode_service_joins_pod() {
        let p = try_load(&format!(
            "name: t\n{POD}  q:\n    network: [{{ parent: eth9, address: 192.0.2.6/24 }}]\nservices:\n  a: {{ image: x, pod: q }}\n  b:\n    image: y\n    network_mode: service:a\n"
        ))
        .unwrap();
        assert_eq!(p.services["b"].pod, "q");
        assert!(!p.pods.contains_key("b"));
    }

    #[test]
    fn errors_are_helpful() {
        let n = |extra: &str| {
            format!(
                "name: t\npods:\n  p:\n    network: [{{ parent: eth9, {extra} }}]\nservices: {{}}\n"
            )
        };
        let cases: Vec<(String, &str)> = vec![
            (
                "name: t\nnetworks:\n  lan: { driver: macvlan }\nservices: {}\n".into(),
                "top-level networks",
            ),
            ("name: t\npods:\n  p:\n    network: [{ address: 192.0.2.5/24 }]\nservices: {}\n".into(), "parent"),
            (n("driver: macvlan"), "address is required"),
            (n("address: 192.0.2.5"), "needs a prefix length"),
            (n("address: 192.0.2.0/24"), "network or broadcast"),
            (n("address: 192.0.2.1/24"), "is the gateway"),
            (n("address: 192.0.2.5/24, gateway: 198.51.100.1"), "outside"),
            (n("address: 192.0.2.5/32"), "set gateway:"),
            (n("address: 192.0.2.5/24, driver: bridge"), "macvlan or ipvlan"),
            (n("address: dhcp, gateway: 192.0.2.1"), "DHCP lease"),
            (n("address: 192.0.2.5/24, address6: 192.0.2.6/24"), "IPv6 address with prefix"),
            (n("address: 192.0.2.5/24, gateway6: 'fe80::1'"), "gateway6 needs address6"),
            (
                n("address: 192.0.2.5/24, driver: ipvlan, mac_address: 02:00:00:00:00:01"),
                "share the parent's MAC",
            ),
            (n("address: 192.0.2.5/24, bogus: 1"), "unknown field"),
            (
                format!("name: t\n{POD}  q:\n    network: [{{ parent: eth9, address: 192.0.2.5/24 }}]\nservices: {{}}\n"),
                "used by pods",
            ),
            (
                format!("name: t\n{POD}  q:\n    network: [{{ parent: eth9, address: 192.0.2.6/24, driver: ipvlan }}]\nservices: {{}}\n"),
                "not both",
            ),
            (
                format!("name: t\n{POD}services:\n  a: {{ image: x, pod: p, hostname: h }}\n"),
                "belongs to the pod",
            ),
            (
                format!("name: t\n{POD}  q:\n    network: [{{ parent: eth9, address: 192.0.2.6/24 }}]\nservices:\n  a: {{ image: x }}\n"),
                "set pod:",
            ),
            ("name: t\nservices:\n  a: { image: x }\n".into(), "no pod to run in"),
            (
                "name: t\nservices:\n  a: { image: x, network_mode: none, depends_on: [b] }\n  b: { image: y, network_mode: none, depends_on: [a] }\n".into(),
                "cycle",
            ),
            (
                "name: t\nservices:\n  a: { build: \"https://example.com/r.git\", network_mode: none }\n".into(),
                "remote build contexts",
            ),
            (
                "name: t\nservices:\n  a: { image: x, network_mode: none, container_name: \"a/../b\" }\n".into(),
                "container_name",
            ),
            ("name: t\nbogus: 1\nservices: {}\n".into(), "unsupported top-level key"),
            (
                format!("name: t\n{POD}    firewall: {{ input: [{{ to: 1.2.3.4 }}] }}\nservices: {{}}\n"),
                "use from:",
            ),
            (
                format!("name: t\n{POD}    firewall: {{ input: [{{ from: pod:nope }}] }}\nservices: {{}}\n"),
                "unknown",
            ),
            (
                format!("name: t\n{POD}    firewall: {{ input: [{{ port: 80, comment: 'a\"b' }}] }}\nservices: {{}}\n"),
                "without quotes",
            ),
            (
                format!("name: t\n{POD}    firewall: {{ input: [{{ port: 80, comment: 'a\\\\' }}] }}\nservices: {{}}\n"),
                "backslashes",
            ),
            (n("address: 192.0.2.5/24, interface: 'e\"0'"), "invalid interface name"),
            (
                format!("name: t\n{POD}    hostname: 'a b'\nservices: {{}}\n"),
                "invalid hostname",
            ),
            (
                format!("name: t\n{POD}    dns_search: ['x\\nnameserver 6.6.6.6']\nservices: {{}}\n"),
                "invalid dns_search",
            ),
        ];
        for (yaml, want) in &cases {
            let err = try_load(yaml).unwrap_err().to_string();
            assert!(err.contains(want), "expected {want:?} in {err:?}\n{yaml}");
        }
    }

    #[test]
    fn profiles_filter_services() {
        let p = try_load("name: t\nservices:\n  a: { image: x, network_mode: none }\n  b: { image: y, network_mode: none, profiles: [debug] }\n").unwrap();
        assert!(p.services.contains_key("a"));
        assert!(!p.services.contains_key("b"));
    }
}
