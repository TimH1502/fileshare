use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::net::UdpSocket;

const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 99);
const MULTICAST_PORT: u16 = 7778;
const ANNOUNCE_INTERVAL_SECS: u64 = 3;
// A peer is stale if we haven't heard from it in 5x the announce interval
const PEER_TIMEOUT_SECS: i64 = 15;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Announcement {
    pub username: String,
    pub port: u16,
    pub share_count: usize,
}

#[derive(Debug, Clone)]
pub struct Peer {
    pub username: String,
    pub addr: IpAddr,
    pub port: u16,
    pub share_count: usize,
    pub last_seen: DateTime<Utc>,
    pub manual: bool, // manually added peers are never pruned
}

impl Peer {
    pub fn http_base(&self) -> String {
        format!("http://{}:{}", self.addr, self.port)
    }

    pub fn is_stale(&self) -> bool {
        if self.manual {
            return false; // manually added peers stay until the app exits
        }
        let age = Utc::now()
            .signed_duration_since(self.last_seen)
            .num_seconds();
        age > PEER_TIMEOUT_SECS
    }
}

#[derive(Clone)]
pub struct PeerRegistry {
    inner: Arc<RwLock<HashMap<String, Peer>>>,
}

impl PeerRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn upsert(&self, addr: IpAddr, ann: Announcement) {
        let key = format!("{}:{}", addr, ann.port);
        let mut peers = self.inner.write().unwrap();
        // Preserve manual=true if it was manually added — now we also know its real username
        let was_manual = peers.get(&key).map(|p| p.manual).unwrap_or(false);
        peers.insert(
            key,
            Peer {
                username: ann.username,
                addr,
                port: ann.port,
                share_count: ann.share_count,
                last_seen: Utc::now(),
                manual: was_manual,
            },
        );
    }

    pub fn list(&self) -> Vec<Peer> {
        let peers = self.inner.read().unwrap();
        let mut v: Vec<_> = peers.values().cloned().collect();
        v.sort_by(|a, b| a.username.cmp(&b.username));
        v
    }

    pub fn prune_stale(&self) {
        let mut peers = self.inner.write().unwrap();
        peers.retain(|_, p| !p.is_stale());
    }

    pub fn add_manual(&self, addr: IpAddr, port: u16) {
        let key = format!("{}:{}", addr, port);
        let mut peers = self.inner.write().unwrap();
        peers.insert(
            key,
            Peer {
                username: format!("{}:{}", addr, port),
                addr,
                port,
                share_count: 0,
                last_seen: Utc::now(),
                manual: true,
            },
        );
    }

    /// Remove a manually-added peer by key (used if user wants to forget it)
    pub fn remove_manual(&self, addr: IpAddr, port: u16) {
        let key = format!("{}:{}", addr, port);
        let mut peers = self.inner.write().unwrap();
        peers.remove(&key);
    }
}

fn make_multicast_socket() -> Result<Socket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MULTICAST_PORT).into())?;
    socket.join_multicast_v4(&MULTICAST_ADDR, &Ipv4Addr::UNSPECIFIED)?;
    Ok(socket)
}

pub async fn run_announcer(
    username: String,
    port: u16,
    share_registry: crate::shares::ShareRegistry,
) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.set_multicast_ttl_v4(4)?;
    let dest = SocketAddr::new(IpAddr::V4(MULTICAST_ADDR), MULTICAST_PORT);

    loop {
        let share_count = share_registry.list_available().len();
        let ann = Announcement {
            username: username.clone(),
            port,
            share_count,
        };
        if let Ok(msg) = serde_json::to_vec(&ann) {
            socket.send_to(&msg, dest).await.ok();
        }
        tokio::time::sleep(Duration::from_secs(ANNOUNCE_INTERVAL_SECS)).await;
    }
}

pub async fn run_listener(
    peer_registry: PeerRegistry,
    own_port: u16,
) -> Result<()> {
    // Use socket2 to create a proper multicast socket, then convert to tokio
    let std_socket = make_multicast_socket()?;
    let std_udp: std::net::UdpSocket = std_socket.into();
    let socket = UdpSocket::from_std(std_udp)?;

    let mut buf = vec![0u8; 4096];

    // Spawn pruner
    let registry_clone = peer_registry.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            registry_clone.prune_stale();
        }
    });

    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, src)) => {
                // Ignore our own announcements by port
                if let Ok(ann) = serde_json::from_slice::<Announcement>(&buf[..len]) {
                    if ann.port != own_port {
                        peer_registry.upsert(src.ip(), ann);
                    }
                }
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}
