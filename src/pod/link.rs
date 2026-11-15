//! Netlink operations: creating macvlan and ipvlan links straight into a
//! pod's namespace, and setting addresses and routes inside it.

use super::ns;
use crate::errors::{Error, Result};
use crate::project::{IpvlanMode, MacvlanMode, NetDriver};
use futures_util::TryStreamExt;
use ipnet::IpNet;
use rtnetlink::packet_route::address::AddressAttribute;
use rtnetlink::packet_route::link::{IpVlanMode, LinkAttribute, MacVlanMode};
use rtnetlink::{Handle, LinkIpVlan, LinkMacVlan, LinkUnspec, RouteMessageBuilder};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::AsRawFd;
use std::path::Path;

/// A netlink connection bound to one network namespace.
pub struct Netlink {
    handle: Handle,
}

impl Netlink {
    /// A connection in the caller's (host) namespace.
    pub fn host() -> Result<Self> {
        let (conn, handle, _) =
            rtnetlink::new_connection().map_err(|e| Error::network(format!("netlink: {e}")))?;
        tokio::spawn(conn);
        Ok(Self { handle })
    }

    /// A connection whose socket lives in the namespace at `path`.
    pub fn in_netns(path: &Path) -> Result<Self> {
        let rt = tokio::runtime::Handle::current();
        let (conn, handle) = ns::in_netns(path, move || {
            let _guard = rt.enter();
            let (conn, handle, _) =
                rtnetlink::new_connection().map_err(|e| Error::network(format!("netlink: {e}")))?;
            Ok((conn, handle))
        })?;
        tokio::spawn(conn);
        Ok(Self { handle })
    }

    pub async fn index(&self, name: &str) -> Result<Option<u32>> {
        let mut links = self
            .handle
            .link()
            .get()
            .match_name(name.to_string())
            .execute();
        match links.try_next().await {
            Ok(Some(l)) => Ok(Some(l.header.index)),
            Ok(None) => Ok(None),
            // The kernel answers ENODEV for an unknown name.
            Err(rtnetlink::Error::NetlinkError(e)) if e.raw_code() == -libc::ENODEV => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn require_index(&self, name: &str) -> Result<u32> {
        self.index(name)
            .await?
            .ok_or_else(|| Error::network(format!("interface {name} not found")))
    }

    /// Returns the MAC address of a link.
    pub async fn mac(&self, index: u32) -> Result<Option<[u8; 6]>> {
        let mut links = self.handle.link().get().match_index(index).execute();
        let Some(link) = links.try_next().await? else {
            return Ok(None);
        };
        Ok(link.attributes.iter().find_map(|a| match a {
            LinkAttribute::Address(m) if m.len() == 6 => {
                let mut out = [0u8; 6];
                out.copy_from_slice(m);
                Some(out)
            }
            _ => None,
        }))
    }

    /// Creates a macvlan or ipvlan link on `parent` directly inside the
    /// namespace at `netns`, under the temporary name `name`.
    pub async fn create_sublink(
        &self,
        parent: &str,
        driver: NetDriver,
        name: &str,
        mac: Option<[u8; 6]>,
        mtu: Option<u32>,
        netns: &Path,
    ) -> Result<()> {
        let parent_index = self.index(parent).await?.ok_or_else(|| {
            Error::network(format!(
                "parent interface {parent} does not exist on this host"
            ))
        })?;
        let ns_file = std::fs::File::open(netns)
            .map_err(|e| Error::network(format!("open {}: {e}", netns.display())))?;
        let msg = match driver {
            NetDriver::Macvlan(mode) => {
                let mode = match mode {
                    MacvlanMode::Bridge => MacVlanMode::Bridge,
                    MacvlanMode::Private => MacVlanMode::Private,
                    MacvlanMode::Vepa => MacVlanMode::Vepa,
                    MacvlanMode::Passthru => MacVlanMode::Passthrough,
                };
                let mut b =
                    LinkMacVlan::new(name, parent_index, mode).setns_by_fd(ns_file.as_raw_fd());
                if let Some(m) = mac {
                    b = b.address(m.to_vec());
                }
                if let Some(m) = mtu {
                    b = b.mtu(m);
                }
                b.build()
            }
            NetDriver::Ipvlan(mode) => {
                let mode = match mode {
                    IpvlanMode::L2 => IpVlanMode::L2,
                    IpvlanMode::L3 => IpVlanMode::L3,
                    IpvlanMode::L3s => IpVlanMode::L3S,
                };
                let mut b =
                    LinkIpVlan::new(name, parent_index, mode).setns_by_fd(ns_file.as_raw_fd());
                if let Some(m) = mtu {
                    b = b.mtu(m);
                }
                b.build()
            }
        };
        self.handle
            .link()
            .add(msg)
            .execute()
            .await
            .map_err(|e| {
                let busy = matches!(&e, rtnetlink::Error::NetlinkError(m) if m.raw_code() == -libc::EBUSY);
                if busy {
                    Error::network(format!(
                        "create {name} on {parent}: device busy; a parent cannot carry macvlan and ipvlan links at the same time (check `ip -d link` for existing sublinks of {parent})"
                    ))
                } else {
                    Error::network(format!("create {name} on {parent}: {e}"))
                }
            })
    }

    /// Renames a link and brings it up.
    pub async fn rename_up(&self, index: u32, name: &str) -> Result<()> {
        let msg = LinkUnspec::new_with_index(index)
            .name(name.to_string())
            .build();
        self.handle.link().set(msg).execute().await?;
        self.set_up(index).await
    }

    pub async fn set_up(&self, index: u32) -> Result<()> {
        let msg = LinkUnspec::new_with_index(index).up().build();
        self.handle.link().set(msg).execute().await?;
        Ok(())
    }

    pub async fn add_address(&self, index: u32, addr: IpNet) -> Result<()> {
        match self
            .handle
            .address()
            .add(index, addr.addr(), addr.prefix_len())
            .execute()
            .await
        {
            Ok(()) => Ok(()),
            Err(rtnetlink::Error::NetlinkError(e)) if e.raw_code() == -libc::EEXIST => Ok(()),
            Err(e) => Err(Error::network(format!("add address {addr}: {e}"))),
        }
    }

    /// Lists the addresses on a link (global scope only for IPv6).
    pub async fn addresses(&self, index: u32) -> Result<Vec<IpNet>> {
        let mut out = Vec::new();
        let mut stream = self
            .handle
            .address()
            .get()
            .set_link_index_filter(index)
            .execute();
        while let Some(msg) = stream.try_next().await? {
            let prefix = msg.header.prefix_len;
            for attr in &msg.attributes {
                if let AddressAttribute::Address(ip) = attr {
                    if let IpAddr::V6(v6) = ip {
                        if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                            continue;
                        }
                    }
                    if let Ok(n) = IpNet::new(*ip, prefix) {
                        out.push(n);
                    }
                }
            }
        }
        Ok(out)
    }

    pub async fn del_address(&self, index: u32, addr: IpNet) -> Result<()> {
        let mut stream = self
            .handle
            .address()
            .get()
            .set_link_index_filter(index)
            .set_address_filter(addr.addr())
            .execute();
        while let Some(msg) = stream.try_next().await? {
            self.handle.address().del(msg).execute().await?;
        }
        Ok(())
    }

    /// Installs the default route through `gateway` on link `index`,
    /// replacing an existing default route of the same family.
    pub async fn set_default_route(&self, index: u32, gateway: IpAddr) -> Result<()> {
        let msg = match gateway {
            IpAddr::V4(g) => RouteMessageBuilder::<Ipv4Addr>::new()
                .gateway(g)
                .output_interface(index)
                .build(),
            IpAddr::V6(g) => RouteMessageBuilder::<Ipv6Addr>::new()
                .gateway(g)
                .output_interface(index)
                .build(),
        };
        self.handle
            .route()
            .add(msg)
            .replace()
            .execute()
            .await
            .map_err(|e| Error::network(format!("default route via {gateway}: {e}")))
    }

    /// Removes the IPv4 default route if there is one.
    pub async fn del_default_route_v4(&self) -> Result<()> {
        let msg = RouteMessageBuilder::<Ipv4Addr>::new().build();
        match self.handle.route().del(msg).execute().await {
            Ok(()) => Ok(()),
            Err(rtnetlink::Error::NetlinkError(e)) if e.raw_code() == -libc::ESRCH => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// A stable, locally administered unicast MAC derived from a seed, so a
/// pod keeps the same MAC (and its DHCP lease) across restarts.
pub fn stable_mac(seed: &str) -> [u8; 6] {
    let h = crate::util::fnv1a64(seed.as_bytes()).to_be_bytes();
    let mut mac = [h[0], h[1], h[2], h[3], h[4], h[5]];
    mac[0] = (mac[0] & 0xfe) | 0x02;
    mac
}

pub fn format_mac(m: &[u8; 6]) -> String {
    m.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_is_stable_and_local() {
        let a = stable_mac("demo/web/lan");
        assert_eq!(a, stable_mac("demo/web/lan"));
        assert_ne!(a, stable_mac("demo/db/lan"));
        assert_eq!(a[0] & 0x03, 0x02);
        assert_eq!(format_mac(&[2, 0, 0, 0, 0xab, 1]), "02:00:00:00:ab:01");
    }
}

/// Sends gratuitous ARP (request and reply forms) for `ip` on interface
/// `index`, so switches and routers drop any stale entry for the address.
/// Must run in the network namespace that owns the interface.
pub fn gratuitous_arp(index: u32, mac: [u8; 6], ip: std::net::Ipv4Addr) -> Result<()> {
    const ETH_P_ARP: u16 = 0x0806;
    // SAFETY: plain socket(2) call; the descriptor is wrapped right away.
    let fd = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            i32::from(ETH_P_ARP.to_be()),
        )
    };
    if fd < 0 {
        return Err(Error::network(format!(
            "arp socket: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: fd is a fresh, owned descriptor.
    let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    // SAFETY: sockaddr_ll is plain old data; zeroed is a valid value.
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as u16;
    sll.sll_protocol = ETH_P_ARP.to_be();
    sll.sll_ifindex = index as i32;
    sll.sll_halen = 6;
    sll.sll_addr[..6].copy_from_slice(&[0xff; 6]);
    for op in [1u16, 2u16] {
        let mut pkt = Vec::with_capacity(28);
        pkt.extend_from_slice(&1u16.to_be_bytes()); // Ethernet
        pkt.extend_from_slice(&0x0800u16.to_be_bytes()); // IPv4
        pkt.push(6);
        pkt.push(4);
        pkt.extend_from_slice(&op.to_be_bytes());
        pkt.extend_from_slice(&mac);
        pkt.extend_from_slice(&ip.octets());
        pkt.extend_from_slice(if op == 1 { &[0u8; 6] } else { &[0xffu8; 6] });
        pkt.extend_from_slice(&ip.octets());
        // SAFETY: pkt and sll outlive the call; lengths match the buffers.
        let r = unsafe {
            libc::sendto(
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                pkt.as_ptr().cast(),
                pkt.len(),
                0,
                std::ptr::addr_of!(sll).cast(),
                std::mem::size_of::<libc::sockaddr_ll>() as u32,
            )
        };
        if r < 0 {
            return Err(Error::network(format!(
                "send gratuitous arp for {ip}: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}
