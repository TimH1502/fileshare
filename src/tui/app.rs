use crate::client::{self, RemoteShareInfo};
use crate::config::Config;
use crate::discovery::{Peer, PeerRegistry};
use crate::server::ServerEvent;
use crate::shares::ShareRegistry;
use chrono::{DateTime, Utc};
use std::path::PathBuf;
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq)]
pub enum Focus {
    PeerList,
    PeerFiles,
    MyShares,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub message: String,
    pub kind: LogKind,
}

#[derive(Debug, Clone)]
pub enum LogKind {
    Info,
    Success,
    Warning,
    Download,
}

#[derive(Debug, Clone)]
pub struct DownloadState {
    pub name: String,
    pub bytes_done: u64,
    pub total: u64,
    pub speed_bps: f64,
    pub done: bool,
    pub error: Option<String>,
}

pub enum AppEvent {
    Tick,
    Key(crossterm::event::KeyEvent),
    PeerFilesLoaded(Vec<RemoteShareInfo>),
    DownloadProgress(client::DownloadProgress),
    DownloadDone(PathBuf),
    DownloadError(String),
    ServerEvent(ServerEvent),
    AddShare(PathBuf),
    ShareAdded(crate::shares::SharedItem),
    ShareError(String),
    ZipStarted(String),
}

pub struct App {
    pub config: Config,
    pub peers: PeerRegistry,
    pub shares: ShareRegistry,

    pub focus: Focus,
    pub peer_list_state: usize,
    pub peer_files: Vec<RemoteShareInfo>,
    pub peer_files_state: usize,
    pub peer_files_loading: bool,
    pub my_shares_state: usize,

    pub log: Vec<LogEntry>,
    pub active_download: Option<DownloadState>,

    pub show_help: bool,
    pub manual_ip_input: Option<String>,
    pub status_message: Option<String>,

    pub event_tx: mpsc::Sender<AppEvent>,
}

impl App {
    pub fn new(
        config: Config,
        peers: PeerRegistry,
        shares: ShareRegistry,
        event_tx: mpsc::Sender<AppEvent>,
    ) -> Self {
        Self {
            config,
            peers,
            shares,
            focus: Focus::PeerList,
            peer_list_state: 0,
            peer_files: vec![],
            peer_files_state: 0,
            peer_files_loading: false,
            my_shares_state: 0,
            log: vec![],
            active_download: None,
            show_help: false,
            manual_ip_input: None,
            status_message: None,
            event_tx,
        }
    }

    pub fn log(&mut self, message: impl Into<String>, kind: LogKind) {
        self.log.push(LogEntry {
            timestamp: Utc::now(),
            message: message.into(),
            kind,
        });
        // Keep last 200 entries
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }

    pub fn peer_list(&self) -> Vec<Peer> {
        self.peers.list()
    }

    pub fn selected_peer(&self) -> Option<Peer> {
        let peers = self.peer_list();
        peers.into_iter().nth(self.peer_list_state)
    }

    pub fn my_shares(&self) -> Vec<crate::shares::SharedItem> {
        self.shares.list()
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;

        // Manual IP input mode
        if let Some(ref mut input) = self.manual_ip_input {
            match key.code {
                KeyCode::Char(c) => input.push(c),
                KeyCode::Backspace => { input.pop(); }
                KeyCode::Enter => {
                    let addr_str = input.clone();
                    self.manual_ip_input = None;
                    self.try_connect_manual(&addr_str);
                }
                KeyCode::Esc => {
                    self.manual_ip_input = None;
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::PeerList => Focus::PeerFiles,
                    Focus::PeerFiles => Focus::MyShares,
                    Focus::MyShares => Focus::PeerList,
                };
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::PeerList => Focus::MyShares,
                    Focus::PeerFiles => Focus::PeerList,
                    Focus::MyShares => Focus::PeerFiles,
                };
            }
            KeyCode::Char('?') | KeyCode::Char('h') => {
                self.show_help = !self.show_help;
            }
            KeyCode::Char('m') => {
                self.manual_ip_input = Some(String::new());
            }
            _ => {
                match self.focus {
                    Focus::PeerList => self.handle_peer_list_key(key),
                    Focus::PeerFiles => self.handle_peer_files_key(key),
                    Focus::MyShares => self.handle_my_shares_key(key),
                }
            }
        }
    }

    fn handle_peer_list_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        let peers = self.peer_list();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if !peers.is_empty() {
                    self.peer_list_state = (self.peer_list_state + 1) % peers.len();
                    self.load_peer_files();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !peers.is_empty() {
                    self.peer_list_state = self.peer_list_state.saturating_sub(1);
                    self.load_peer_files();
                }
            }
            KeyCode::Enter | KeyCode::Right => {
                self.focus = Focus::PeerFiles;
                self.load_peer_files();
            }
            _ => {}
        }
    }

    fn handle_peer_files_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.peer_files.is_empty() {
                    self.peer_files_state = (self.peer_files_state + 1) % self.peer_files.len();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.peer_files_state = self.peer_files_state.saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char('d') => {
                self.download_selected();
            }
            KeyCode::Left => {
                self.focus = Focus::PeerList;
            }
            _ => {}
        }
    }

    fn handle_my_shares_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        let shares = self.my_shares();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if !shares.is_empty() {
                    self.my_shares_state = (self.my_shares_state + 1) % shares.len();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.my_shares_state = self.my_shares_state.saturating_sub(1);
            }
            KeyCode::Delete | KeyCode::Char('x') => {
                if let Some(item) = shares.into_iter().nth(self.my_shares_state) {
                    let removed = self.shares.remove(&item.id);
                    if removed {
                        if self.my_shares_state > 0 {
                            self.my_shares_state -= 1;
                        }
                        self.log(format!("Removed '{}' from shares", item.name), LogKind::Info);
                    }
                }
            }
            _ => {}
        }
    }

    fn load_peer_files(&mut self) {
        if let Some(peer) = self.selected_peer() {
            let base_url = peer.http_base();
            let tx = self.event_tx.clone();
            self.peer_files_loading = true;
            self.peer_files = vec![];
            self.peer_files_state = 0;

            tokio::spawn(async move {
                match client::fetch_peer_shares(&base_url).await {
                    Ok(resp) => {
                        tx.send(AppEvent::PeerFilesLoaded(resp.items)).await.ok();
                    }
                    Err(e) => {
                        tx.send(AppEvent::PeerFilesLoaded(vec![])).await.ok();
                        eprintln!("Failed to fetch peer shares: {}", e);
                    }
                }
            });
        }
    }

    fn download_selected(&mut self) {
        if self.active_download.is_some() {
            self.log("A download is already in progress", LogKind::Warning);
            return;
        }
        let peer = match self.selected_peer() {
            Some(p) => p,
            None => return,
        };
        let file = match self.peer_files.get(self.peer_files_state) {
            Some(f) if f.available => f.clone(),
            _ => return,
        };

        self.active_download = Some(DownloadState {
            name: file.name.clone(),
            bytes_done: 0,
            total: file.size,
            speed_bps: 0.0,
            done: false,
            error: None,
        });

        self.log(format!("Downloading '{}'…", file.name), LogKind::Info);

        let base_url = peer.http_base();
        let download_dir = self.config.download_dir.clone();
        let tx = self.event_tx.clone();

        tokio::spawn(async move {
            let (prog_tx, mut prog_rx) = mpsc::channel(32);
            let tx2 = tx.clone();

            tokio::spawn(async move {
                while let Some(p) = prog_rx.recv().await {
                    tx2.send(AppEvent::DownloadProgress(p)).await.ok();
                }
            });

            match client::download_file(&base_url, &file.id, &file.name, &download_dir, prog_tx).await {
                Ok(path) => { tx.send(AppEvent::DownloadDone(path)).await.ok(); }
                Err(e) => { tx.send(AppEvent::DownloadError(e.to_string())).await.ok(); }
            }
        });
    }

    fn try_connect_manual(&mut self, addr_str: &str) {
        let parts: Vec<&str> = addr_str.split(':').collect();
        let (ip_str, port) = if parts.len() == 2 {
            (parts[0], parts[1].parse::<u16>().unwrap_or(7777))
        } else {
            (addr_str, 7777u16)
        };

        match ip_str.parse::<std::net::IpAddr>() {
            Ok(ip) => {
                self.peers.add_manual(ip, port);
                self.log(format!("Added manual peer {}:{}", ip, port), LogKind::Info);
            }
            Err(_) => {
                self.log(format!("Invalid IP address: {}", ip_str), LogKind::Warning);
            }
        }
    }

    pub fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::PeerFilesLoaded(files) => {
                self.peer_files = files;
                self.peer_files_loading = false;
            }
            AppEvent::DownloadProgress(p) => {
                if let Some(ref mut dl) = self.active_download {
                    dl.bytes_done = p.bytes_downloaded;
                    dl.total = p.total_bytes;
                    dl.speed_bps = p.speed_bps;
                }
            }
            AppEvent::DownloadDone(path) => {
                let name = self.active_download.as_ref().map(|d| d.name.clone());
                self.active_download = None;
                if let Some(name) = name {
                    self.log(
                        format!("✓ Downloaded '{}' → {}", name, path.display()),
                        LogKind::Success,
                    );
                }
            }
            AppEvent::DownloadError(e) => {
                if let Some(ref dl) = self.active_download {
                    self.log(format!("✗ Download '{}' failed: {}", dl.name, e), LogKind::Warning);
                }
                self.active_download = None;
            }
            AppEvent::ServerEvent(ServerEvent::Downloaded { item_name, by_addr }) => {
                self.log(
                    format!("⬇ '{}' downloaded by {}", item_name, by_addr),
                    LogKind::Download,
                );
            }
            AppEvent::ShareAdded(item) => {
                self.log(
                    format!("+ Sharing '{}' ({})", item.name, item.size_human()),
                    LogKind::Success,
                );
            }
            AppEvent::ZipStarted(msg) => {
                self.log(msg, LogKind::Info);
            }
            AppEvent::ShareError(e) => {
                self.log(format!("✗ Share failed: {}", e), LogKind::Warning);
            }
            _ => {}
        }
    }
}
