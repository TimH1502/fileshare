use anyhow::Result;
use chrono::{DateTime, Utc};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

// ---------------------------------------------------------------------------

const SERVICE_TYPE: &str = "_fileshare._tcp.local.";
const PEER_TIMEOUT_SECS: i64 = 15;
const ANNOUNCE_INTERVAL_SECS: u64 = 3;

/// App identifier embedded in mDNS TXT records.
const APP_ID: &str = env!("CARGO_PKG_NAME");
/// Protocol version — bump when the wire format changes, not on every release.
const PROTOCOL_VERSION: &str = "1";

// ---------------------------------------------------------------------------
// Peer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Peer {
    pub username: String,
    pub addr: IpAddr,
    pub port: u16,
    pub share_count: usize,
    pub last_seen: DateTime<Utc>,
    pub manual: bool,
}

impl Peer {
    pub fn http_base(&self) -> String {
        format!("https://{}:{}", self.addr, self.port)
    }

    pub fn is_stale(&self) -> bool {
        if self.manual {
            return false;
        }
        let age = Utc::now()
            .signed_duration_since(self.last_seen)
            .num_seconds();
        age > PEER_TIMEOUT_SECS
    }
}

// ---------------------------------------------------------------------------
// PeerRegistry
// ---------------------------------------------------------------------------

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

    pub fn upsert(&self, addr: IpAddr, port: u16, username: String, share_count: usize) {
        let key = format!("{}:{}", addr, port);
        let mut peers = self.inner.write().unwrap();

        let was_manual = peers.get(&key).map(|p| p.manual).unwrap_or(false);

        peers.insert(
            key,
            Peer {
                username,
                addr,
                port,
                share_count,
                last_seen: Utc::now(),
                manual: was_manual,
            },
        );
    }

    pub fn prune_stale(&self) {
        let mut peers = self.inner.write().unwrap();
        peers.retain(|_, p| !p.is_stale());
    }

    pub fn list(&self) -> Vec<Peer> {
        let peers = self.inner.read().unwrap();
        peers.values().cloned().collect()
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

    pub fn remove_manual(&self, addr: IpAddr, port: u16) {
        let key = format!("{}:{}", addr, port);
        let mut peers = self.inner.write().unwrap();
        peers.remove(&key);
    }


}

// ---------------------------------------------------------------------------
// PUBLIC ENTRY POINT (THIS REPLACES BOTH run_announcer + run_listener)
// ---------------------------------------------------------------------------

pub async fn run_mdns(
    username: String,
    port: u16,
    share_registry: crate::shares::ShareRegistry,
    peer_registry: PeerRegistry,
) -> Result<()> {
    let daemon = ServiceDaemon::new()?;

    let instance = sanitise_instance_name(&username);
    let host = format!("{}.local.", gethostname());

    // -----------------------------------------------------------------------
    // LISTENER (runs on blocking thread)
    // -----------------------------------------------------------------------

    let receiver = daemon.browse(SERVICE_TYPE)?;

    {
        let registry = peer_registry.clone();

        let hostname = host.clone();
        std::thread::spawn(move || {
            for event in receiver {
                if let ServiceEvent::ServiceResolved(info) = event {
                    let new_port = info.get_port();
                    let resolved_host = info.get_hostname();

                    // FILTER YOURSELF HERE
                    if new_port == port && resolved_host == hostname {
                        continue;
                    }

                    let props = info.get_properties();

                    // FILTER foreign services
                    if props.get("app").map(|p| p.val_str()) != Some(APP_ID) {
                        continue;
                    }

                    let username = info
                        .get_properties()
                        .get("username")
                        .map(|p| p.val_str().to_owned())
                        .unwrap_or_else(|| info.get_fullname().to_owned());

                    let share_count: usize = info
                        .get_properties()
                        .get("share_count")
                        .and_then(|p| p.val_str().parse::<usize>().ok())
                        .unwrap_or(0);

                    if let Some(addr) = info
                        .get_addresses_v4()
                        .into_iter()
                        .next()
                        .map(IpAddr::V4)
                    {
                        registry.upsert(addr, new_port, username, share_count);
                    }
                }
            }
        });
    }

    // -----------------------------------------------------------------------
    // STALE CLEANER
    // -----------------------------------------------------------------------

    {
        let registry = peer_registry.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                registry.prune_stale();
            }
        });
    }

    // -----------------------------------------------------------------------
    // ANNOUNCER LOOP (ONLY PLACE THAT TOUCHES daemon.register!)
    // -----------------------------------------------------------------------

    loop {
        tokio::time::sleep(Duration::from_secs(ANNOUNCE_INTERVAL_SECS)).await;

        let count = share_registry.list_available().len();

        let fullname = format!("{}.{}", instance, SERVICE_TYPE);

        // Always re-announce (refresh TTL)
        daemon.unregister(&fullname).ok();

        let props = build_props(&username, count);

        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &host,
            "",
            port,
            Some(props),
        )?
        .enable_addr_auto();

        daemon.register(service)?;

    }
    
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_props(username: &str, share_count: usize) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("username".to_owned(), username.to_owned());
    m.insert("share_count".to_owned(), share_count.to_string());

    m.insert("app".to_owned(), APP_ID.to_owned());
    m.insert("version".to_owned(), PROTOCOL_VERSION.to_owned());
    
    m
}

fn sanitise_instance_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .collect();

    s[..s.len().min(63)].to_owned()
}

fn gethostname() -> String {
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "localhost".to_owned())
}