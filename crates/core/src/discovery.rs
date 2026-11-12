//! Finding other dextr devices on the local network, and this device's own
//! addresses.
//!
//! Every device broadcasts a small [`Beacon`] on [`DISCOVERY_PORT`] every two
//! seconds and listens for the others'. Beacons carry only a name, OS, port
//! and certificate fingerprint; trust still comes from pairing.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use dextr_proto::{Beacon, DISCOVERY_PORT};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, watch},
};

const INTERVAL: Duration = Duration::from_secs(2);

/// A network address of this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAddress {
    pub interface: String,
    pub ip: IpAddr,
}

/// Interfaces of containers and VMs on this machine, which other devices
/// can't reach.
fn is_virtual(name: &str) -> bool {
    ["docker", "br-", "veth", "virbr", "vboxnet", "vmnet", "podman", "cni", "flannel", "lxc", "lxd"]
        .iter()
        .any(|p| name.starts_with(p))
}

fn interfaces() -> Vec<if_addrs::Interface> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback() && !is_virtual(&i.name))
        .collect()
}

/// This device's addresses other devices can reach: no loopback, no
/// link-local, no container bridges, IPv4 first.
pub fn local_addresses() -> Vec<LocalAddress> {
    let mut out: Vec<LocalAddress> = interfaces()
        .into_iter()
        .filter(|i| match i.ip() {
            IpAddr::V4(v4) => !v4.is_link_local(),
            IpAddr::V6(v6) => !v6.is_unicast_link_local() && !v6.is_loopback(),
        })
        .map(|i| LocalAddress { ip: i.ip(), interface: i.name })
        .collect();
    out.sort_by_key(|a| (a.ip.is_ipv6(), a.interface.clone()));
    out.dedup();
    out
}

/// Where to send beacons: each IPv4 network's broadcast address, or the
/// limited broadcast address if none is known.
fn targets() -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = interfaces()
        .into_iter()
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => v4.broadcast,
            _ => None,
        })
        .map(|b| SocketAddr::new(IpAddr::V4(b), DISCOVERY_PORT))
        .collect();
    out.sort();
    out.dedup();
    if out.is_empty() {
        out.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), DISCOVERY_PORT));
    }
    out
}

/// Broadcasts `beacon` (latest value) and reports every beacon heard, with
/// the address it came from. Runs until `found` is closed.
pub fn spawn(beacon: watch::Receiver<Beacon>, found: mpsc::UnboundedSender<(Beacon, SocketAddr)>) {
    tokio::spawn(async move {
        // Without the port (another app or a second dextr has it) beacons can
        // still be sent, just not received.
        let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT)).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("discovery can't listen on UDP {DISCOVERY_PORT}: {e}");
                match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await {
                    Ok(s) => s,
                    Err(e) => return tracing::warn!("discovery is off: {e}"),
                }
            }
        };
        if let Err(e) = socket.set_broadcast(true) {
            return tracing::warn!("discovery is off: {e}");
        }

        let mut tick = tokio::time::interval(INTERVAL);
        let mut buf = [0u8; 1024];
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let Ok(bytes) = beacon.borrow().to_bytes() else { continue };
                    for target in targets() {
                        let _ = socket.send_to(&bytes, target).await;
                    }
                }
                received = socket.recv_from(&mut buf) => {
                    let Ok((n, from)) = received else { continue };
                    if let Some(b) = Beacon::from_bytes(&buf[..n])
                        && found.send((b, from)).is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
}
