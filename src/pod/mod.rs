//! Pods: the namespaces gantry creates before any container starts.
//!
//! Bringing a pod up means: pin a network namespace (plus UTS and IPC when
//! shared), create one macvlan or ipvlan link per network directly inside
//! it, set addresses and routes (or start a DHCP agent), write the shared
//! /etc files, and apply the firewall. Containers then join the namespaces
//! by path.

pub mod dhcp;
pub mod files;
pub mod link;
pub mod ns;
pub mod state;

use crate::errors::{Context, Error, Result};
use crate::firewall;
use crate::project::{Addressing, NetDriver, Pod, Project};
use ipnet::IpNet;
use link::Netlink;
use state::{InterfaceState, PodState};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// What `ensure` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ensured {
    Created,
    Unchanged,
}

/// Hash of everything that shapes a pod's namespaces. A change means the
/// pod has to be recreated.
pub fn config_hash(_project: &Project, pod: &Pod) -> String {
    let mut shape = pod.clone();
    // The firewall and hosts entries are applied live; they do not force a
    // recreate.
    shape.firewall = None;
    shape.extra_hosts.clear();
    let data = serde_json::to_vec(&shape).unwrap_or_default();
    crate::util::hash_hex(&data)
}

/// Creates the pod if it does not exist or its shape changed. Returns the
/// state and whether it was (re)created.
pub async fn ensure(project: &Project, pod: &Pod) -> Result<(PodState, Ensured)> {
    let hash = config_hash(project, pod);
    if let Some(existing) = PodState::load(&project.name, &pod.name)? {
        if existing.config_hash == hash && ns::is_pinned(&existing.netns) {
            return Ok((existing, Ensured::Unchanged));
        }
        tracing::info!(pod = %pod.name, "pod configuration changed, recreating");
        remove(&project.name, &pod.name).await?;
    }
    let st = create(project, pod, hash).await;
    match st {
        Ok(s) => Ok((s, Ensured::Created)),
        Err(e) => {
            // Leave nothing half built behind.
            if let Err(e2) = remove(&project.name, &pod.name).await {
                tracing::warn!(pod = %pod.name, "cleanup after failed create: {e2}");
            }
            Err(e)
        }
    }
}

async fn create(project: &Project, pod: &Pod, hash: String) -> Result<PodState> {
    let dir = state::pod_dir(&project.name, &pod.name);
    state::ensure_dir(&dir)?;
    ns::ensure_shared_dir(&state::netns_dir())?;
    let netns = state::netns_path(&project.name, &pod.name);
    if netns.exists() {
        // A stale namespace from a crash; nothing else owns this name.
        ns::remove(&netns)?;
    }
    ns::create(ns::Kind::Net, &netns, || Ok(()))?;

    let uts = if pod.share_uts {
        let p = dir.join("uts");
        let hostname = pod.hostname.clone();
        let domain = pod.domainname.clone();
        ns::remove(&p)?;
        ns::create(ns::Kind::Uts, &p, move || {
            nix::unistd::sethostname(&hostname).ctx(|| format!("set hostname {hostname}"))?;
            if let Some(d) = domain {
                set_domainname(&d)?;
            }
            Ok(())
        })?;
        Some(p)
    } else {
        None
    };
    let ipc = if pod.share_ipc {
        let p = dir.join("ipc");
        ns::remove(&p)?;
        ns::create(ns::Kind::Ipc, &p, || Ok(()))?;
        Some(p)
    } else {
        None
    };

    let host = Netlink::host()?;
    let inner = Netlink::in_netns(&netns)?;
    let lo = inner.require_index("lo").await?;
    inner.set_up(lo).await?;

    // Sysctls first, so IPv6 settings apply before links come up.
    let mut sysctls: Vec<(String, String)> = pod
        .sysctls
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // Docker and kiri give each container's own network namespace
    // these defaults (non-root can bind low ports and ping). Pods replace
    // that namespace, so they get the same defaults unless the pod sets them.
    for (k, v) in [
        ("net.ipv4.ip_unprivileged_port_start", "0"),
        ("net.ipv4.ping_group_range", "0 2147483647"),
    ] {
        if !pod.sysctls.contains_key(k) {
            sysctls.insert(0, (k.to_string(), v.to_string()));
        }
    }
    // The kernel applies the stricter of `conf.all` and the interface's own
    // rp_filter. Hosts such as Ubuntu set `all` to 2, which a new namespace
    // inherits, so the per-interface 0 below would not take effect for DHCP
    // offers. Static interfaces keep their own (inherited) value.
    let rp_all = "net.ipv4.conf.all.rp_filter";
    if pod
        .attachments
        .iter()
        .any(|a| a.addressing == Addressing::Dhcp)
        && !pod.sysctls.contains_key(rp_all)
    {
        sysctls.push((rp_all.into(), "0".into()));
    }
    if !pod.ipv6 {
        sysctls.push(("net.ipv6.conf.all.disable_ipv6".into(), "1".into()));
        sysctls.push(("net.ipv6.conf.default.disable_ipv6".into(), "1".into()));
    }
    write_sysctls(&netns, &sysctls)?;

    let mut interfaces = Vec::new();
    for att in &pod.attachments {
        let tmp = format!("gt{}", crate::util::random_id(5));
        let mac = match att.driver {
            NetDriver::Macvlan(_) => Some(att.mac.unwrap_or_else(|| {
                link::stable_mac(&format!("{}/{}/{}", project.name, pod.name, att.ifname))
            })),
            NetDriver::Ipvlan(_) => None,
        };
        host.create_sublink(&att.parent, att.driver, &tmp, mac, None, &netns)
            .await?;
        let idx = inner.require_index(&tmp).await?;
        if att.addressing == Addressing::Dhcp {
            // Offers come from a server the pod has no route to yet, which
            // reverse path filtering (inherited from the host) would drop.
            // Pods never forward, so the filter buys nothing here. This
            // needs `conf.all.rp_filter` at 0 as well (set above).
            let path = format!("/proc/sys/net/ipv4/conf/{tmp}/rp_filter");
            ns::in_netns(&netns, move || {
                std::fs::write(&path, "0").ctx(|| format!("write {path}"))
            })?;
        }
        inner.rename_up(idx, &att.ifname).await?;

        let mut addresses = Vec::new();
        let mut gateway = None;
        if let Addressing::Static(addrs) = &att.addressing {
            for a in addrs {
                inner.add_address(idx, *a).await?;
                addresses.push(*a);
            }
            if att.default_route {
                for gw in [att.gateway, att.gateway6].into_iter().flatten() {
                    inner.set_default_route(idx, gw).await?;
                    gateway.get_or_insert(gw);
                }
            }
        }
        // Announce static IPv4 addresses so the LAN forgets stale MACs.
        let link_mac = inner.mac(idx).await?;
        if let Some(m) = link_mac {
            let v4: Vec<std::net::Ipv4Addr> = addresses
                .iter()
                .filter_map(|a| match a.addr() {
                    IpAddr::V4(v) => Some(v),
                    IpAddr::V6(_) => None,
                })
                .collect();
            if !v4.is_empty() {
                ns::in_netns(&netns, move || {
                    for ip in &v4 {
                        if let Err(e) = link::gratuitous_arp(idx, m, *ip) {
                            tracing::warn!("{e}");
                        }
                    }
                    Ok(())
                })?;
            }
        }
        interfaces.push(InterfaceState {
            ifname: att.ifname.clone(),
            parent: att.parent.clone(),
            mac: mac.map(|m| link::format_mac(&m)),
            addresses,
            gateway,
            dhcp: att.addressing == Addressing::Dhcp,
        });
    }

    // Shared /etc files. hosts is filled in by `refresh` once every pod's
    // address is known.
    let (dns, search) = if pod.dns.is_empty() {
        let dhcp = pod
            .attachments
            .iter()
            .any(|a| a.addressing == Addressing::Dhcp);
        if dhcp {
            (vec![], pod.dns_search.clone())
        } else {
            let (d, s) = files::host_nameservers();
            (
                d,
                if pod.dns_search.is_empty() {
                    s
                } else {
                    pod.dns_search.clone()
                },
            )
        }
    } else {
        (pod.dns.clone(), pod.dns_search.clone())
    };
    files::write_if_changed(
        &dir.join("resolv.conf"),
        &files::resolv_conf(&dns, &search, &pod.dns_options),
    )?;
    files::write_if_changed(&dir.join("hosts"), &files::hosts_file(&[]))?;
    let fqdn = match &pod.domainname {
        Some(d) => format!("{}.{d}", pod.hostname),
        None => pod.hostname.clone(),
    };
    files::write_if_changed(&dir.join("hostname"), &format!("{fqdn}\n"))?;

    let st = PodState {
        id: crate::util::random_id(8),
        project: project.name.clone(),
        name: pod.name.clone(),
        config_hash: hash,
        netns: netns.clone(),
        uts,
        ipc,
        hostname: pod.hostname.clone(),
        interfaces,
        created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    st.save()?;

    // DHCP agents run detached and outlive this command.
    for att in pod
        .attachments
        .iter()
        .filter(|a| a.addressing == Addressing::Dhcp)
    {
        start_dhcp_agent(project, pod, &st, &att.ifname, att.default_route)?;
    }
    for att in pod
        .attachments
        .iter()
        .filter(|a| a.addressing == Addressing::Dhcp)
    {
        wait_for_lease(&dir, &att.ifname, Duration::from_secs(30)).await?;
    }
    Ok(st)
}

fn set_domainname(d: &str) -> Result<()> {
    // SAFETY: d is a valid buffer of the given length.
    let r = unsafe { libc::setdomainname(d.as_ptr().cast(), d.len()) };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).ctx(|| format!("set domainname {d}"));
    }
    Ok(())
}

fn write_sysctls(netns: &Path, sysctls: &[(String, String)]) -> Result<()> {
    if sysctls.is_empty() {
        return Ok(());
    }
    let list = sysctls.to_vec();
    ns::in_netns(netns, move || {
        for (k, v) in &list {
            if !k.starts_with("net.") {
                return Err(Error::config(format!(
                    "pod sysctl {k}: only net.* sysctls can be set on a pod"
                )));
            }
            let path = format!("/proc/sys/{}", k.replace('.', "/"));
            std::fs::write(&path, v).ctx(|| format!("sysctl {k}={v}"))?;
        }
        Ok(())
    })
}

fn start_dhcp_agent(
    project: &Project,
    pod: &Pod,
    st: &PodState,
    ifname: &str,
    default_route: bool,
) -> Result<()> {
    let dir = state::pod_dir(&project.name, &pod.name);
    let exe = std::env::current_exe().ctx(|| "locate gantry binary".into())?;
    let log =
        std::fs::File::create(dhcp::log_path(&dir, ifname)).ctx(|| "create dhcp log".into())?;
    let log2 = log.try_clone().ctx(|| "dup dhcp log".into())?;
    let mut cmd = Command::new(exe);
    cmd.args([
        "internal",
        "dhcp-agent",
        "--project",
        &project.name,
        "--netns",
    ])
    .arg(&st.netns)
    .arg("--pod-dir")
    .arg(&dir)
    .args(["--ifname", ifname, "--hostname", &pod.hostname])
    .args([
        "--client-id",
        &format!("gantry-{}-{}-{ifname}", project.name, pod.name),
    ]);
    if default_route {
        cmd.arg("--default-route");
    }
    if pod.dns.is_empty() {
        cmd.arg("--manage-resolv");
    }
    for s in &pod.dns_search {
        cmd.args(["--dns-search", s]);
    }
    for o in &pod.dns_options {
        cmd.args(["--dns-option", o]);
    }
    cmd.stdin(Stdio::null()).stdout(log).stderr(log2);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
            nix::unistd::setsid()
                .map(drop)
                .map_err(std::io::Error::from)
        });
    }
    let child = cmd.spawn().ctx(|| "start dhcp agent".into())?;
    std::fs::write(dhcp::pid_path(&dir, ifname), child.id().to_string())
        .ctx(|| "write dhcp pid".into())?;
    Ok(())
}

async fn wait_for_lease(dir: &Path, ifname: &str, wait: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if dhcp::LeaseFile::load(dir, ifname)?.is_some() {
            return Ok(());
        }
        if !agent_alive(dir, ifname) {
            let log = std::fs::read_to_string(dhcp::log_path(dir, ifname)).unwrap_or_default();
            return Err(Error::Dhcp(format!(
                "agent for {ifname} exited: {}",
                log.trim()
            )));
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                "no DHCP lease on {ifname} after {wait:?}; the agent keeps trying in the background"
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The agent's PID, only if that process really is the DHCP agent for this
/// pod directory and interface. A stale PID file (after a crash or reboot)
/// must never lead to signalling an unrelated process.
fn agent_pid(dir: &Path, ifname: &str) -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(dhcp::pid_path(dir, ifname))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    let has = |want: &[u8]| args.contains(&want);
    let dir_s = dir.to_string_lossy();
    (has(b"dhcp-agent") && has(dir_s.as_bytes()) && has(ifname.as_bytes())).then_some(pid)
}

/// Alive and not a zombie (the agent is never reaped by its parent).
fn agent_alive(dir: &Path, ifname: &str) -> bool {
    let Some(pid) = agent_pid(dir, ifname) else {
        return false;
    };
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(") ")
            .and_then(|(_, rest)| rest.chars().next())
            .is_some_and(|state| state != 'Z' && state != 'X'),
        Err(_) => false,
    }
}

/// Stops DHCP agents and removes the pod's namespaces and directory.
/// Containers must already be gone or the namespaces stay alive until
/// they exit.
pub async fn remove(project: &str, pod: &str) -> Result<()> {
    let dir = state::pod_dir(project, pod);
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(ifname) = name
                .strip_prefix("dhcp-")
                .and_then(|n| n.strip_suffix(".pid"))
            {
                stop_agent(&dir, ifname).await;
            }
        }
    }
    // Keep going after a failure so as much as possible is cleaned up,
    // then report the first error.
    let mut first: Option<Error> = None;
    for r in [
        ns::remove(&state::netns_path(project, pod)),
        ns::remove(&dir.join("uts")),
        ns::remove(&dir.join("ipc")),
    ] {
        if let Err(e) = r {
            tracing::warn!(pod, "{e}");
            first.get_or_insert(e);
        }
    }
    if first.is_none() && dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dir).ctx(|| format!("remove {}", dir.display())) {
            first = Some(e);
        }
    }
    first.map_or(Ok(()), Err)
}

async fn stop_agent(dir: &Path, ifname: &str) {
    use nix::sys::signal::{Signal, kill};
    let Some(pid) = agent_pid(dir, ifname) else {
        return;
    };
    let pid = nix::unistd::Pid::from_raw(pid);
    if kill(pid, Signal::SIGTERM).is_err() {
        return;
    }
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if kill(pid, None).is_err() {
            return;
        }
    }
    let _ = kill(pid, Signal::SIGKILL);
}

/// Rewrites every pod's hosts file and reapplies firewalls, now that all
/// addresses are known. Safe to run any time.
pub fn refresh(project: &Project) -> Result<()> {
    let mut first_err: Option<Error> = None;
    let states: Vec<PodState> = project
        .pods
        .keys()
        .filter_map(|p| PodState::load(&project.name, p).ok().flatten())
        .collect();
    let addr_of = |pod: &str| -> Vec<IpNet> {
        states
            .iter()
            .find(|s| s.name == pod)
            .map(PodState::addresses)
            .unwrap_or_default()
    };
    for st in &states {
        let Some(pod) = project.pods.get(&st.name) else {
            continue;
        };
        let dir = state::pod_dir(&project.name, &pod.name);
        let mut entries = Vec::new();
        // Services in this pod resolve to loopback.
        let mut local: Vec<String> = vec![pod.hostname.clone()];
        if let Some(d) = &pod.domainname {
            local.insert(0, format!("{}.{d}", pod.hostname));
        }
        local.push(pod.name.clone());
        local.extend(project.services_in_pod(&pod.name).map(|s| s.name.clone()));
        // IPv4 loopback only: resolvers that prefer IPv6 would otherwise
        // pick ::1 and miss servers bound to 127.0.0.1 or 0.0.0.0.
        entries.push(files::HostEntry {
            addr: IpAddr::from([127, 0, 0, 1]),
            names: local,
        });
        // Other pods resolve to their LAN addresses.
        for other in states.iter().filter(|o| o.name != st.name) {
            let Some(op) = project.pods.get(&other.name) else {
                continue;
            };
            let mut names = vec![op.hostname.clone(), op.name.clone()];
            names.extend(project.services_in_pod(&op.name).map(|s| s.name.clone()));
            let addrs = other.addresses();
            for fam_v4 in [true, false] {
                if let Some(a) = addrs.iter().find(|a| a.addr().is_ipv4() == fam_v4) {
                    entries.push(files::HostEntry {
                        addr: a.addr(),
                        names: names.clone(),
                    });
                }
            }
        }
        for (h, ip) in &pod.extra_hosts {
            entries.push(files::HostEntry {
                addr: *ip,
                names: vec![h.clone()],
            });
        }
        files::write_if_changed(&dir.join("hosts"), &files::hosts_file(&entries))?;
        if let Err(e) = apply_firewall(project, pod, st, &addr_of) {
            tracing::warn!(pod = %pod.name, "{e}");
            first_err.get_or_insert(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

pub fn apply_firewall(
    project: &Project,
    pod: &Pod,
    st: &PodState,
    book: &dyn firewall::AddressBook,
) -> Result<()> {
    let dir = state::pod_dir(&project.name, &pod.name);
    let path = dir.join("firewall.nft");
    match firewall::render(project, pod, book)? {
        Some(rules) => {
            files::write_if_changed(&path, &rules)?;
            nft(&st.netns, &["-f", &path.to_string_lossy()])
        }
        None => {
            if path.exists() {
                let _ = nft(&st.netns, &["delete", "table", "inet", firewall::TABLE]);
                let _ = std::fs::remove_file(&path);
            }
            Ok(())
        }
    }
}

/// Runs `nft` inside a pod's network namespace.
pub fn nft(netns: &Path, args: &[&str]) -> Result<()> {
    let out = nft_output(netns, args)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::Firewall(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

pub fn nft_output(netns: &Path, args: &[&str]) -> Result<std::process::Output> {
    // A binary override is for development only: a root process must not
    // let its environment choose what it executes.
    let bin = if cfg!(debug_assertions) {
        std::env::var("GANTRY_NFT").unwrap_or_else(|_| "nft".into())
    } else {
        "nft".to_string()
    };
    let mut cmd = Command::new(&bin);
    cmd.args(args);
    ns::command_in_netns(&mut cmd, netns)?;
    cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Firewall(format!(
                "{bin} not found; install nftables to use pod firewalls"
            ))
        } else {
            Error::Firewall(format!("run {bin}: {e}"))
        }
    })
}

/// Paths a container mounts to share the pod's /etc files.
pub fn etc_mounts(st: &PodState) -> Vec<(PathBuf, &'static str)> {
    let dir = state::pod_dir(&st.project, &st.name);
    let mut v = vec![
        (dir.join("hosts"), "/etc/hosts"),
        (dir.join("resolv.conf"), "/etc/resolv.conf"),
    ];
    if st.uts.is_some() {
        v.push((dir.join("hostname"), "/etc/hostname"));
    }
    v
}
