//! A small DHCPv4 client. Each DHCP attachment gets a detached agent
//! process (`kirisame internal dhcp-agent`) that lives in the pod's network
//! namespace, holds the lease, renews it, and releases it on shutdown.

use super::link::Netlink;
use crate::errors::{Context, Error, Result};
use dhcproto::v4::{DhcpOption, Flags, Message, MessageType, Opcode, OptionCode};
use dhcproto::{Decodable, Encodable};
use ipnet::{IpNet, Ipv4Net};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::time::{Instant, sleep, timeout};

/// The lease as recorded for other kirisame commands to read.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LeaseFile {
    pub address: Ipv4Addr,
    pub prefix: u8,
    pub router: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub domain: Option<String>,
    pub server: Ipv4Addr,
    pub lease_secs: u32,
    pub acquired: u64,
}

impl LeaseFile {
    pub fn path(pod_dir: &Path, ifname: &str) -> PathBuf {
        pod_dir.join(format!("dhcp-{ifname}.json"))
    }
    pub fn load(pod_dir: &Path, ifname: &str) -> Result<Option<Self>> {
        let p = Self::path(pod_dir, ifname);
        match std::fs::read(&p) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| Error::Dhcp(format!("{}: {e}", p.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).ctx(|| format!("read {}", p.display())),
        }
    }
    pub fn address(&self) -> IpNet {
        IpNet::V4(
            Ipv4Net::new(self.address, self.prefix).unwrap_or_else(|_| Ipv4Net::from(self.address)),
        )
    }
}

pub fn pid_path(pod_dir: &Path, ifname: &str) -> PathBuf {
    pod_dir.join(format!("dhcp-{ifname}.pid"))
}

pub fn log_path(pod_dir: &Path, ifname: &str) -> PathBuf {
    pod_dir.join(format!("dhcp-{ifname}.log"))
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Project name, used to refresh other pods when the address changes.
    pub project: String,
    pub pod_dir: PathBuf,
    pub ifname: String,
    pub hostname: String,
    pub client_id: Vec<u8>,
    pub default_route: bool,
    /// Rewrite the pod's resolv.conf from the lease.
    pub manage_resolv: bool,
    pub dns_search: Vec<String>,
    pub dns_options: Vec<String>,
}

#[derive(Debug, Clone)]
struct Offer {
    address: Ipv4Addr,
    prefix: u8,
    router: Option<Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    domain: Option<String>,
    server: Ipv4Addr,
    lease_secs: u32,
    t1: u32,
    t2: u32,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Runs the agent until SIGTERM or SIGINT. The calling process must
/// already be inside the pod's network namespace.
pub async fn run_agent(cfg: AgentConfig) -> Result<()> {
    let nl = Netlink::host()?;
    let index = nl.require_index(&cfg.ifname).await?;
    let mac = nl
        .mac(index)
        .await?
        .ok_or_else(|| Error::Dhcp(format!("{} has no MAC address", cfg.ifname)))?;
    let sock = client_socket(&cfg.ifname)?;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .ctx(|| "install SIGTERM handler".into())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .ctx(|| "install SIGINT handler".into())?;
    let client = Client {
        sock,
        mac,
        cfg: cfg.clone(),
    };

    let mut current: Option<Offer> = None;
    // The first bind happens during `up`, which refreshes the project itself.
    let mut last_address: Option<Ipv4Addr> = None;
    loop {
        let lease = match current.take() {
            Some(l) => l,
            None => {
                tokio::select! {
                    l = client.acquire() => l,
                    _ = term.recv() => return Ok(()),
                    _ = int.recv() => return Ok(()),
                }
            }
        };
        if let Err(e) = client.apply(&nl, index, &lease, None).await {
            // A transient netlink or file error must not end the agent:
            // drop this lease attempt and try again shortly.
            tracing::warn!("applying lease {}: {e}", lease.address);
            client.unapply(&nl, index, &lease).await;
            sleep(Duration::from_secs(5)).await;
            continue;
        }
        let previous = last_address.replace(lease.address);
        if previous.is_some_and(|p| p != lease.address) {
            // Other pods' hosts files and firewall rules name this address.
            refresh_project(&cfg.project);
        }
        tracing::info!(address = %lease.address, prefix = lease.prefix, server = %lease.server, lease = lease.lease_secs, "lease bound");

        // Hold the lease: renew at T1, rebind at T2, give up at expiry.
        let bound_at = Instant::now();
        let t1 = bound_at + Duration::from_secs(u64::from(lease.t1));
        let t2 = bound_at + Duration::from_secs(u64::from(lease.t2));
        let expiry = bound_at + Duration::from_secs(u64::from(lease.lease_secs));
        let mut next = t1;
        let outcome: Option<Offer> = loop {
            tokio::select! {
                _ = tokio::time::sleep_until(next) => {}
                _ = term.recv() => { client.release(&nl, index, &lease).await; return Ok(()); }
                _ = int.recv() => { client.release(&nl, index, &lease).await; return Ok(()); }
            }
            let now = Instant::now();
            if now >= expiry {
                tracing::warn!("lease expired");
                break None;
            }
            let rebinding = now >= t2;
            match client.renew(&lease, rebinding).await {
                Ok(Some(newl)) => break Some(newl),
                Ok(None) => {
                    // NAK: the address is no longer ours.
                    tracing::warn!("server refused renewal");
                    break None;
                }
                Err(e) => {
                    tracing::warn!("renewal failed: {e}");
                    let remaining = expiry.saturating_duration_since(now);
                    next = now
                        + (remaining / 2).clamp(Duration::from_secs(10), Duration::from_secs(300));
                }
            }
        };
        match outcome {
            Some(newl) => {
                if newl.address != lease.address || newl.prefix != lease.prefix {
                    client.unapply(&nl, index, &lease).await;
                }
                current = Some(newl);
            }
            None => {
                client.unapply(&nl, index, &lease).await;
                let _ = std::fs::remove_file(LeaseFile::path(&cfg.pod_dir, &cfg.ifname));
            }
        }
    }
}

fn refresh_project(project: &str) {
    match crate::project::Project::load_snapshot(project) {
        Ok(Some(p)) => match crate::pod::refresh(&p) {
            Ok(()) => tracing::info!("refreshed hosts files and firewalls after an address change"),
            Err(e) => tracing::warn!("refresh after address change: {e}"),
        },
        Ok(None) => {}
        Err(e) => tracing::warn!("{e}"),
    }
}

struct Client {
    sock: UdpSocket,
    mac: [u8; 6],
    cfg: AgentConfig,
}

fn client_socket(ifname: &str) -> Result<UdpSocket> {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, bind, setsockopt, socket, sockopt};
    let fd = socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        None,
    )
    .ctx(|| "dhcp socket".into())?;
    setsockopt(&fd, sockopt::ReuseAddr, &true).ctx(|| "SO_REUSEADDR".into())?;
    setsockopt(&fd, sockopt::Broadcast, &true).ctx(|| "SO_BROADCAST".into())?;
    setsockopt(
        &fd,
        sockopt::BindToDevice,
        &std::ffi::OsString::from(ifname),
    )
    .ctx(|| "SO_BINDTODEVICE".into())?;
    let addr = nix::sys::socket::SockaddrIn::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 68));
    bind(std::os::fd::AsRawFd::as_raw_fd(&fd), &addr).ctx(|| "bind 0.0.0.0:68".into())?;
    let std_sock = std::net::UdpSocket::from(fd);
    UdpSocket::from_std(std_sock).ctx(|| "register dhcp socket".into())
}

impl Client {
    fn message(&self, kind: MessageType, xid: u32, ciaddr: Ipv4Addr, broadcast: bool) -> Message {
        let mut m = Message::new_with_id(
            xid,
            ciaddr,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            &self.mac,
        );
        m.set_opcode(Opcode::BootRequest);
        if broadcast {
            m.set_flags(Flags::default().set_broadcast());
        }
        let o = m.opts_mut();
        o.insert(DhcpOption::MessageType(kind));
        let mut cid = vec![0u8];
        cid.extend_from_slice(&self.cfg.client_id);
        o.insert(DhcpOption::ClientIdentifier(cid));
        if !self.cfg.hostname.is_empty() {
            o.insert(DhcpOption::Hostname(self.cfg.hostname.clone()));
        }
        o.insert(DhcpOption::ParameterRequestList(vec![
            OptionCode::SubnetMask,
            OptionCode::Router,
            OptionCode::DomainNameServer,
            OptionCode::DomainName,
            OptionCode::AddressLeaseTime,
            OptionCode::Renewal,
            OptionCode::Rebinding,
        ]));
        m
    }

    async fn send(&self, m: &Message, to: Ipv4Addr) -> Result<()> {
        let bytes = m
            .to_vec()
            .map_err(|e| Error::Dhcp(format!("encode: {e}")))?;
        self.sock
            .send_to(&bytes, SocketAddrV4::new(to, 67))
            .await
            .ctx(|| format!("send to {to}:67"))?;
        Ok(())
    }

    /// Waits for a reply with `xid` of one of the given types.
    async fn recv(
        &self,
        xid: u32,
        want: &[MessageType],
        wait: Duration,
    ) -> Result<Option<Message>> {
        let deadline = Instant::now() + wait;
        let mut buf = [0u8; 1500];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            let Ok(r) = timeout(left, self.sock.recv_from(&mut buf)).await else {
                return Ok(None);
            };
            let (n, _) = r.ctx(|| "receive".into())?;
            let Ok(m) = Message::from_bytes(&buf[..n]) else {
                continue;
            };
            if m.xid() != xid
                || m.opcode() != Opcode::BootReply
                || m.chaddr().get(..6) != Some(&self.mac[..])
            {
                continue;
            }
            match m.opts().msg_type() {
                Some(t) if want.contains(&t) => return Ok(Some(m)),
                _ => continue,
            }
        }
    }

    /// DISCOVER, OFFER, REQUEST, ACK. Retries with backoff until it works.
    async fn acquire(&self) -> Offer {
        let mut backoff = Duration::from_secs(2);
        loop {
            match self.try_acquire().await {
                Ok(Some(o)) => return o,
                Ok(None) => tracing::info!("no answer from a DHCP server, retrying in {backoff:?}"),
                Err(e) => tracing::warn!("dhcp: {e}, retrying in {backoff:?}"),
            }
            sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }

    async fn try_acquire(&self) -> Result<Option<Offer>> {
        let xid: u32 = u32::from_str_radix(&crate::util::random_id(4), 16).unwrap_or(0x6b697269);
        let discover = self.message(MessageType::Discover, xid, Ipv4Addr::UNSPECIFIED, true);
        let mut offer = None;
        for _ in 0..3 {
            self.send(&discover, Ipv4Addr::BROADCAST).await?;
            if let Some(m) = self
                .recv(xid, &[MessageType::Offer], Duration::from_secs(4))
                .await?
            {
                offer = Some(m);
                break;
            }
        }
        let Some(offer) = offer else { return Ok(None) };
        let server = server_id(&offer)
            .ok_or_else(|| Error::Dhcp("offer without server identifier".into()))?;
        let mut req = self.message(MessageType::Request, xid, Ipv4Addr::UNSPECIFIED, true);
        req.opts_mut()
            .insert(DhcpOption::RequestedIpAddress(offer.yiaddr()));
        req.opts_mut().insert(DhcpOption::ServerIdentifier(server));
        for _ in 0..3 {
            self.send(&req, Ipv4Addr::BROADCAST).await?;
            match self
                .recv(
                    xid,
                    &[MessageType::Ack, MessageType::Nak],
                    Duration::from_secs(4),
                )
                .await?
            {
                Some(m) if m.opts().msg_type() == Some(MessageType::Ack) => {
                    return Ok(Some(parse_ack(&m, server)?));
                }
                Some(_) => return Err(Error::Dhcp("server answered the request with NAK".into())),
                None => {}
            }
        }
        Ok(None)
    }

    /// RENEWING (unicast to the server) or REBINDING (broadcast).
    /// Ok(None) means NAK.
    async fn renew(&self, lease: &Offer, rebinding: bool) -> Result<Option<Offer>> {
        let xid: u32 = u32::from_str_radix(&crate::util::random_id(4), 16).unwrap_or(0x6b697269);
        let req = self.message(MessageType::Request, xid, lease.address, false);
        let to = if rebinding {
            Ipv4Addr::BROADCAST
        } else {
            lease.server
        };
        self.send(&req, to).await?;
        match self
            .recv(
                xid,
                &[MessageType::Ack, MessageType::Nak],
                Duration::from_secs(8),
            )
            .await?
        {
            Some(m) if m.opts().msg_type() == Some(MessageType::Ack) => {
                let server = server_id(&m).unwrap_or(lease.server);
                Ok(Some(parse_ack(&m, server)?))
            }
            Some(_) => Ok(None),
            None => Err(Error::Dhcp(format!("no answer to renewal from {to}"))),
        }
    }

    async fn release(&self, nl: &Netlink, index: u32, lease: &Offer) {
        let xid: u32 = u32::from_str_radix(&crate::util::random_id(4), 16).unwrap_or(0x6b697269);
        let mut m = self.message(MessageType::Release, xid, lease.address, false);
        m.opts_mut()
            .insert(DhcpOption::ServerIdentifier(lease.server));
        if let Err(e) = self.send(&m, lease.server).await {
            tracing::warn!("release: {e}");
        }
        self.unapply(nl, index, lease).await;
        let _ = std::fs::remove_file(LeaseFile::path(&self.cfg.pod_dir, &self.cfg.ifname));
        tracing::info!(address = %lease.address, "lease released");
    }

    async fn apply(
        &self,
        nl: &Netlink,
        index: u32,
        lease: &Offer,
        _old: Option<&Offer>,
    ) -> Result<()> {
        let net = IpNet::V4(
            Ipv4Net::new(lease.address, lease.prefix).map_err(|e| Error::Dhcp(e.to_string()))?,
        );
        nl.add_address(index, net).await?;
        if let Err(e) = super::link::gratuitous_arp(index, self.mac, lease.address) {
            tracing::warn!("{e}");
        }
        if self.cfg.default_route {
            if let Some(r) = lease.router {
                nl.set_default_route(index, IpAddr::V4(r)).await?;
            }
        }
        let file = LeaseFile {
            address: lease.address,
            prefix: lease.prefix,
            router: lease.router,
            dns: lease.dns.clone(),
            domain: lease.domain.clone(),
            server: lease.server,
            lease_secs: lease.lease_secs,
            acquired: now_unix(),
        };
        let data = serde_json::to_vec_pretty(&file).map_err(|e| Error::Dhcp(e.to_string()))?;
        crate::util::write_atomic(&LeaseFile::path(&self.cfg.pod_dir, &self.cfg.ifname), &data)?;
        if self.cfg.manage_resolv && !lease.dns.is_empty() {
            let mut search = self.cfg.dns_search.clone();
            if search.is_empty() {
                search.extend(lease.domain.clone());
            }
            let dns: Vec<IpAddr> = lease.dns.iter().map(|d| IpAddr::V4(*d)).collect();
            let text = super::files::resolv_conf(&dns, &search, &self.cfg.dns_options);
            crate::util::write_in_place(&self.cfg.pod_dir.join("resolv.conf"), text.as_bytes())?;
        }
        Ok(())
    }

    async fn unapply(&self, nl: &Netlink, index: u32, lease: &Offer) {
        if let Ok(net) = Ipv4Net::new(lease.address, lease.prefix) {
            if let Err(e) = nl.del_address(index, IpNet::V4(net)).await {
                tracing::warn!("remove {}: {e}", lease.address);
            }
        }
    }
}

fn server_id(m: &Message) -> Option<Ipv4Addr> {
    match m.opts().get(OptionCode::ServerIdentifier) {
        Some(DhcpOption::ServerIdentifier(s)) => Some(*s),
        _ => None,
    }
}

fn parse_ack(m: &Message, server: Ipv4Addr) -> Result<Offer> {
    let opts = m.opts();
    let prefix = match opts.get(OptionCode::SubnetMask) {
        // Only contiguous masks are meaningful as a prefix length.
        Some(DhcpOption::SubnetMask(mask))
            if u32::from(*mask).leading_ones() == u32::from(*mask).count_ones() =>
        {
            u32::from(*mask).count_ones() as u8
        }
        Some(DhcpOption::SubnetMask(mask)) => {
            return Err(Error::Dhcp(format!(
                "server sent a non-contiguous subnet mask {mask}"
            )));
        }
        _ => 24,
    };
    let router = match opts.get(OptionCode::Router) {
        Some(DhcpOption::Router(r)) => r.first().copied(),
        _ => None,
    };
    let dns = match opts.get(OptionCode::DomainNameServer) {
        Some(DhcpOption::DomainNameServer(d)) => d.clone(),
        _ => vec![],
    };
    let domain = match opts.get(OptionCode::DomainName) {
        // Untrusted input from the LAN: keep it only if it is a DNS name.
        Some(DhcpOption::DomainName(d)) if crate::compose::is_dns_name(d) => Some(d.clone()),
        _ => None,
    };
    let lease_secs = match opts.get(OptionCode::AddressLeaseTime) {
        Some(DhcpOption::AddressLeaseTime(t)) => *t,
        _ => 3600,
    };
    // An infinite lease (0xffffffff) is renewed once a day anyway.
    let lease_secs = lease_secs.clamp(60, 86_400 * 365);
    let t1 = match opts.get(OptionCode::Renewal) {
        Some(DhcpOption::Renewal(t)) if *t < lease_secs => *t,
        _ => lease_secs / 2,
    };
    let t2 = match opts.get(OptionCode::Rebinding) {
        Some(DhcpOption::Rebinding(t)) if *t < lease_secs && *t > t1 => *t,
        _ => lease_secs / 8 * 7,
    };
    let y = m.yiaddr();
    if y.is_unspecified() || y.is_loopback() || y.is_multicast() || y.is_broadcast() {
        return Err(Error::Dhcp(format!(
            "server offered an unusable address {y}"
        )));
    }
    Ok(Offer {
        address: m.yiaddr(),
        prefix,
        router,
        dns,
        domain,
        server,
        lease_secs,
        t1,
        t2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_parsing() {
        let mut m = Message::new_with_id(
            7,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(10, 1, 2, 3),
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            &[2, 0, 0, 0, 0, 1],
        );
        m.opts_mut()
            .insert(DhcpOption::SubnetMask(Ipv4Addr::new(255, 255, 255, 0)));
        m.opts_mut()
            .insert(DhcpOption::Router(vec![Ipv4Addr::new(10, 1, 2, 1)]));
        m.opts_mut()
            .insert(DhcpOption::DomainNameServer(vec![Ipv4Addr::new(
                9, 9, 9, 9,
            )]));
        m.opts_mut().insert(DhcpOption::AddressLeaseTime(600));
        let o = parse_ack(&m, Ipv4Addr::new(10, 1, 2, 1)).unwrap();
        assert_eq!(o.address, Ipv4Addr::new(10, 1, 2, 3));
        assert_eq!(o.prefix, 24);
        assert_eq!(o.router, Some(Ipv4Addr::new(10, 1, 2, 1)));
        assert_eq!((o.t1, o.t2), (300, 525));
    }
}
