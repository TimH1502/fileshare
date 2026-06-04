use crate::client::{self, DownloadResult, RemoteShareInfo, calc_eta_seconds};
use crate::config::Config;
use crate::discovery::{Peer, PeerRegistry};
use crate::server::ServerEvent;
use crate::shares::ShareRegistry;
use chrono::{DateTime, Local};
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
    pub timestamp: DateTime<Local>,
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
    pub id: String,
    pub name: String,
    pub bytes_done: u64,
    pub total: u64,
    pub speed_bps: f64,
    pub done: bool,
    pub cancelled: bool,
    pub done_at: Option<std::time::Instant>,
    pub eta_seconds: f64,
    pub last_activity: std::time::Instant,
}

#[derive(Debug, Clone)]
pub struct UploadState {
    pub id: String,       // share id (from ServerEvent)
    pub name: String,
    pub bytes_sent: u64,
    pub total: u64,
    pub speed_bps: f64,
    pub done: bool,
    pub cancelled: bool,
    pub done_at: Option<std::time::Instant>,
    pub last_bytes: u64,  // for speed calculation
    pub last_tick: std::time::Instant,
    pub eta_seconds: f64,
    pub smoothed_speed: f64,
    pub last_display_update: std::time::Instant, // throttle display refresh
}

/// Pending zip-confirmation request shown to the user.
#[derive(Debug, Clone)]
pub struct ZipConfirmRequest {
    pub path: PathBuf,
    pub folder_name: String,
    pub file_count: usize,
    pub total_size: u64,
    pub would_zip: bool,
}

/// A file being received from the web UI (browser upload).
#[derive(Debug, Clone)]
pub struct WebUploadState {
    pub transfer_id: String,
    pub name: String,
    pub bytes_received: u64,
    pub total: u64,          // 0 when Content-Length was absent
    pub speed_bps: f64,
    pub smoothed_speed: f64,
    pub eta_seconds: f64,
    pub done: bool,
    pub failed: bool,
    pub done_at: Option<std::time::Instant>,
    pub last_bytes: u64,
    pub last_tick: std::time::Instant,
    pub last_display_update: std::time::Instant, // throttle display refresh
    pub by_addr: String,
}

pub enum AppEvent {
    Tick,
    PeerFilesLoaded(Vec<RemoteShareInfo>),
    /// Progress update keyed by share id
    DownloadProgress { id: String, progress: client::DownloadProgress },
    DownloadDone { id: String, result: DownloadResult },
    DownloadError { id: String, error: String },
    ServerEvent(ServerEvent),
    AddShare(PathBuf),
    ZipConfirmNeeded(ZipConfirmRequest),
    ZipConfirmResult(PathBuf, bool),
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
    /// Active downloads keyed by share id — supports multiple simultaneous downloads
    pub active_downloads: Vec<DownloadState>,
    /// Active uploads (files being sent to remote peers)
    pub active_uploads: Vec<UploadState>,
    /// Incoming uploads from the web UI
    pub web_uploads: Vec<WebUploadState>,

    pub show_help: bool,
    pub show_qr: bool,
    pub manual_ip_input: Option<String>,
    pub manual_path_input: Option<String>,

    pub zip_confirm: Option<ZipConfirmRequest>,

    pub event_tx: mpsc::Sender<AppEvent>,

    pub last_peer_refresh: std::time::Instant,
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
            active_downloads: vec![],
            active_uploads: vec![],
            web_uploads: vec![],
            show_help: false,
            show_qr: false,
            manual_ip_input: None,
            manual_path_input: None,
            zip_confirm: None,
            event_tx,
            last_peer_refresh: std::time::Instant::now(),
        }
    }

    pub fn log(&mut self, message: impl Into<String>, kind: LogKind) {
        self.log.push(LogEntry {
            timestamp: Local::now(),
            message: message.into(),
            kind,
        });
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

        // QR overlay — Esc or r closes it
        if self.show_qr {
            match key.code {
                KeyCode::Esc | KeyCode::Char('r') | KeyCode::Char('q') => {
                    self.show_qr = false;
                }
                _ => {}
            }
            return;
        }

        // Zip-confirm popup takes priority
        if let Some(ref req) = self.zip_confirm.clone() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let path = req.path.clone();
                    self.zip_confirm = None;
                    let tx = self.event_tx.clone();
                    tokio::spawn(async move {
                        tx.send(AppEvent::ZipConfirmResult(path, true)).await.ok();
                    });
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    let path = req.path.clone();
                    self.zip_confirm = None;
                    let tx = self.event_tx.clone();
                    tokio::spawn(async move {
                        tx.send(AppEvent::ZipConfirmResult(path, false)).await.ok();
                    });
                }
                KeyCode::Esc => {
                    self.zip_confirm = None;
                }
                _ => {}
            }
            return;
        }

        // Manual path input mode
        if let Some(ref mut input) = self.manual_path_input {
            match key.code {
                KeyCode::Char(c) => input.push(c),
                KeyCode::Backspace => { input.pop(); }
                KeyCode::Enter => {
                    let path_str = input.clone();
                    self.manual_path_input = None;
                    let tx = self.event_tx.clone();
                    tokio::spawn(async move {
                        tx.send(AppEvent::AddShare(std::path::PathBuf::from(path_str)))
                            .await
                            .ok();
                    });
                }
                KeyCode::Esc => {
                    self.manual_path_input = None;
                }
                _ => {}
            }
            return;
        }

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
            KeyCode::Char('r') => {
                self.show_qr = !self.show_qr;
                self.show_help = false;
            }
            KeyCode::Char('m') => match self.focus {
                Focus::MyShares => {
                    self.manual_path_input = Some(String::new());
                }
                _ => {
                    self.manual_ip_input = Some(String::new());
                }
            },
            _ => match self.focus {
                Focus::PeerList => self.handle_peer_list_key(key),
                Focus::PeerFiles => self.handle_peer_files_key(key),
                Focus::MyShares => self.handle_my_shares_key(key),
            },
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
            KeyCode::Delete | KeyCode::Char('x') => {
                if let Some(peer) = peers.into_iter().nth(self.peer_list_state) {
                    if peer.manual {
                        self.peers.remove_manual(peer.addr, peer.port);
                        if self.peer_list_state > 0 {
                            self.peer_list_state -= 1;
                        }
                        self.peer_files = vec![];
                        self.log(
                            format!("Removed manual peer {}:{}", peer.addr, peer.port),
                            LogKind::Info,
                        );
                    } else {
                        self.log("Only manually added peers can be removed", LogKind::Warning);
                    }
                }
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
                    if let Some(removed) = self.shares.remove(&item.id) {
                        if self.my_shares_state > 0 {
                            self.my_shares_state -= 1;
                        }
                        self.log(format!("Removed '{}' from shares", removed.name), LogKind::Info);
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
        let peer = match self.selected_peer() {
            Some(p) => p,
            None => return,
        };
        let file = match self.peer_files.get(self.peer_files_state) {
            Some(f) if f.available => f.clone(),
            _ => return,
        };

        // Check if this file is already downloading
        if self.active_downloads.iter().any(|d| d.id == file.id && !d.done) {
            self.log(
                format!("'{}' is already downloading", file.name),
                LogKind::Warning,
            );
            return;
        }

        self.active_downloads.push(DownloadState {
            id: file.id.clone(),
            name: file.name.clone(),
            bytes_done: 0,
            total: file.size,
            speed_bps: 0.0,
            done: false,
            cancelled: false,
            done_at: None,
            eta_seconds: 0.0,
            last_activity: std::time::Instant::now(),
        });

        self.log(format!("Downloading '{}'…", file.name), LogKind::Info);

        let base_url = peer.http_base();
        let download_dir = self.config.download_dir.clone();
        let tx = self.event_tx.clone();
        let file_id = file.id.clone();

        tokio::spawn(async move {
            let (prog_tx, mut prog_rx) = tokio::sync::mpsc::channel(32);
            let tx2 = tx.clone();
            let fid2 = file_id.clone();

            tokio::spawn(async move {
                while let Some(p) = prog_rx.recv().await {
                    tx2.send(AppEvent::DownloadProgress {
                        id: fid2.clone(),
                        progress: p,
                    })
                    .await
                    .ok();
                }
            });

            match client::download_file(&base_url, &file.id, &file.name, &download_dir, prog_tx)
                .await
            {
                Ok(result) => {
                    tx.send(AppEvent::DownloadDone {
                        id: file_id,
                        result,
                    })
                    .await
                    .ok();
                }
                Err(e) => {
                    tx.send(AppEvent::DownloadError {
                        id: file_id,
                        error: e.to_string(),
                    })
                    .await
                    .ok();
                }
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
            AppEvent::DownloadProgress { id, progress } => {
                if let Some(dl) = self.active_downloads.iter_mut().find(|d| d.id == id) {
                    dl.bytes_done = progress.bytes_downloaded;
                    dl.total = progress.total_bytes;
                    dl.speed_bps = progress.speed_bps;
                    dl.eta_seconds = progress.eta_seconds;
                    dl.last_activity = std::time::Instant::now();
                }
            }
            AppEvent::DownloadDone { id, result } => {
                if let Some(dl) = self.active_downloads.iter_mut().find(|d| d.id == id) {
                    dl.done = true;
                    dl.done_at = Some(std::time::Instant::now());
                    // For very small/fast files total may still be 0 if no progress
                    // events fired. Ensure the bar renders as 100% either way.
                    if dl.total == 0 { dl.total = 1; }
                    dl.bytes_done = dl.total; // show 100%
                    let name = dl.name.clone();
                    let checksum_note = match result.checksum_ok {
                        Some(true) => " ✓ checksum ok",
                        Some(false) => " ⚠ checksum MISMATCH",
                        None => "",
                    };
                    self.log(
                        format!(
                            "✓ Downloaded '{}' → {}{}",
                            name,
                            result.path.display(),
                            checksum_note
                        ),
                        if result.checksum_ok == Some(false) {
                            LogKind::Warning
                        } else {
                            LogKind::Success
                        },
                    );
                }
                // Keep visible for 3s; Tick handler prunes old entries
            }

            AppEvent::DownloadError { id, error } => {
                if let Some(dl) = self.active_downloads.iter().find(|d| d.id == id) {
                    self.log(
                        format!("✗ Download '{}' failed: {}", dl.name, error),
                        LogKind::Warning,
                    );
                }
                self.active_downloads.retain(|d| d.id != id);
            }
            AppEvent::ServerEvent(ServerEvent::Downloaded { item_name, by_addr }) => {
                self.log(
                    format!("⬇ '{}' downloaded by {}", item_name, by_addr),
                    LogKind::Download,
                );
            }
            AppEvent::ServerEvent(ServerEvent::Uploaded { item_name, by_addr }) => {
                self.log(
                    format!("⬆ '{}' uploaded by {}", item_name, by_addr),
                    LogKind::Success,
                );
                // Mark the matching upload as done (may already be pruned if tiny file)
                if let Some(ul) = self.active_uploads.iter_mut().find(|u| u.name == item_name && !u.done) {
                    ul.done = true;
                    ul.done_at = Some(std::time::Instant::now());
                    ul.bytes_sent = ul.total;
                }
            }
            AppEvent::ServerEvent(ServerEvent::UploadProgress { item_id, bytes_sent, total }) => {
                let now = std::time::Instant::now();
                if let Some(ul) = self.active_uploads.iter_mut().find(|u| u.id == item_id) {
                    // Always update position and accumulate into smoothed speed
                    let elapsed = now.duration_since(ul.last_tick).as_secs_f64().max(0.001);
                    let delta = bytes_sent.saturating_sub(ul.last_bytes) as f64;
                    let instant_speed = delta / elapsed;
                    ul.last_bytes = bytes_sent;
                    ul.last_tick = now;
                    ul.bytes_sent = bytes_sent;
                    ul.total = total;

                    // Heavy EMA -- alpha 0.05 keeps a ~20-sample rolling average
                    let alpha = 0.05;
                    let (new_eta, new_smooth) = calc_eta_seconds(ul.smoothed_speed, instant_speed, alpha, ul.total, ul.bytes_sent);
                    ul.smoothed_speed = new_smooth;

                    // Only push to display fields every 500 ms to prevent flickering
                    if ul.last_display_update.elapsed().as_millis() >= 500 {
                        ul.speed_bps = new_smooth;
                        ul.eta_seconds = new_eta;
                        ul.last_display_update = now;
                    }

                    // Mark done if all bytes sent — don't wait for UploadDone which can be dropped
                    if total > 0 && bytes_sent >= total && !ul.done {
                        ul.done = true;
                        ul.done_at = Some(now);
                    }
                } else {
                    let name = self.shares.get(&item_id)
                        .map(|s| s.name.clone())
                        .unwrap_or_else(|| item_id.clone());
                    self.active_uploads.push(UploadState {
                        id: item_id,
                        name,
                        bytes_sent,
                        total,
                        speed_bps: 0.0,
                        done: total > 0 && bytes_sent >= total,
                        cancelled: false,
                        done_at: if total > 0 && bytes_sent >= total { Some(now) } else { None },
                        last_bytes: bytes_sent,
                        last_tick: now,
                        eta_seconds: 0.0,
                        smoothed_speed: 0.0,
                        last_display_update: std::time::Instant::now(),
                    });
                }
            }
            AppEvent::ServerEvent(ServerEvent::UploadDone { item_id }) => {
                if let Some(ul) = self.active_uploads.iter_mut().find(|u| u.id == item_id) {
                    ul.done = true;
                    ul.done_at = Some(std::time::Instant::now());
                    ul.bytes_sent = ul.total;
                }
            }
            AppEvent::ServerEvent(ServerEvent::Deleted { item_name }) => {
                self.log(
                    format!("🗑 '{}' deleted via web UI", item_name),
                    LogKind::Warning,
                );
            }
            AppEvent::ShareAdded(item) => {
                self.log(
                    format!("+ Sharing '{}' ({})", item.name, item.size_human()),
                    LogKind::Success,
                );
            }
            AppEvent::ZipConfirmNeeded(req) => {
                self.zip_confirm = Some(req);
            }
            AppEvent::ZipConfirmResult(path, should_zip) => {
                let etx = self.event_tx.clone();
                let shares_c = self.shares.clone();
                tokio::spawn(async move {
                    let etx2 = etx.clone();
                    match shares_c.add_with_zip_choice(
                        path,
                        None,
                        None,
                        should_zip,
                        move |folder_name| {
                            let msg = format!("Zipping '{}' — this may take a moment…", folder_name);
                            let etx2 = etx2.clone();
                            tokio::spawn(async move {
                                let _ = etx2.send(AppEvent::ZipStarted(msg)).await;
                            });
                        },
                    ) {
                        Ok(item) => {
                            etx.send(AppEvent::ShareAdded(item)).await.ok();
                        }
                        Err(e) => {
                            etx.send(AppEvent::ShareError(e.to_string())).await.ok();
                        }
                    }
                });
            }
            AppEvent::ZipStarted(msg) => {
                self.log(msg, LogKind::Info);
            }
            AppEvent::ShareError(e) => {
                self.log(format!("✗ Share failed: {}", e), LogKind::Warning);
            }

            AppEvent::ServerEvent(ServerEvent::WebUploadStarted { transfer_id, filename, total, by_addr }) => {
                let now = std::time::Instant::now();
                self.log(
                    format!("\u{2b06} '{}' upload started from {}", filename, by_addr),
                    LogKind::Info,
                );
                self.web_uploads.push(WebUploadState {
                    transfer_id,
                    name: filename,
                    bytes_received: 0,
                    total,
                    speed_bps: 0.0,
                    smoothed_speed: 0.0,
                    eta_seconds: 0.0,
                    done: false,
                    failed: false,
                    done_at: None,
                    last_bytes: 0,
                    last_tick: now,
                    last_display_update: std::time::Instant::now(),
                    by_addr,
                });
            }
            AppEvent::ServerEvent(ServerEvent::WebUploadProgress { transfer_id, bytes_received, total }) => {
                let now = std::time::Instant::now();
                if let Some(wu) = self.web_uploads.iter_mut().find(|w| w.transfer_id == transfer_id) {
                    let elapsed = now.duration_since(wu.last_tick).as_secs_f64().max(0.001);
                    let delta = bytes_received.saturating_sub(wu.last_bytes) as f64;
                    let instant_speed = delta / elapsed;
                    wu.last_bytes = bytes_received;
                    wu.last_tick = now;
                    wu.bytes_received = bytes_received;
                    if total > 0 { wu.total = total; }

                    // Heavy EMA -- alpha 0.05 keeps a ~20-sample rolling average
                    let alpha = 0.05;
                    let (new_eta, new_smooth) = calc_eta_seconds(wu.smoothed_speed, instant_speed, alpha, wu.total, wu.bytes_received);
                    wu.smoothed_speed = new_smooth;

                    // Only push to display fields every 500 ms to prevent flickering
                    if wu.last_display_update.elapsed().as_millis() >= 500 {
                        wu.speed_bps = new_smooth;
                        wu.eta_seconds = new_eta;
                        wu.last_display_update = now;
                    }
                }
            }
            AppEvent::ServerEvent(ServerEvent::WebUploadFinished { transfer_id, share_id: _ }) => {
                let msg = if let Some(wu) = self.web_uploads.iter_mut().find(|w| w.transfer_id == transfer_id) {
                    wu.done = true;
                    wu.done_at = Some(std::time::Instant::now());
                    if wu.total > 0 { wu.bytes_received = wu.total; }
                    Some(format!("\u{2714} '{}' received from {} via web UI", wu.name, wu.by_addr))
                } else { None };
                if let Some(m) = msg { self.log(m, LogKind::Success); }
            }
            AppEvent::ServerEvent(ServerEvent::WebUploadFailed { transfer_id }) => {
                let msg = if let Some(wu) = self.web_uploads.iter_mut().find(|w| w.transfer_id == transfer_id) {
                    wu.failed = true;
                    wu.done_at = Some(std::time::Instant::now());
                    Some(format!("\u{2717} '{}' upload from {} failed", wu.name, wu.by_addr))
                } else { None };
                if let Some(m) = msg { self.log(m, LogKind::Warning); }
            }
            AppEvent::Tick => {
                // Auto-refresh peer files every 3 seconds
                if self.focus == Focus::PeerFiles {
                    let now = std::time::Instant::now();
                    let interval = std::time::Duration::from_secs(3);

                    if now.duration_since(self.last_peer_refresh) >= interval
                        && !self.peer_files_loading && self.selected_peer().is_some() {
                            self.load_peer_files();
                            self.last_peer_refresh = now;
                        }
                }

                // Mark stale (cancelled/dropped) transfers as done so they get pruned.
                // Any transfer with no activity for 10 seconds that hasn't finished
                // normally is considered abandoned.
                const STALE_SECS: u64 = 10;
                let now = std::time::Instant::now();
                for dl in self.active_downloads.iter_mut() {
                    if !dl.done && !dl.cancelled && dl.last_activity.elapsed().as_secs() >= STALE_SECS {
                        dl.cancelled = true;
                        dl.done_at = Some(now);
                    }
                }
                for ul in self.active_uploads.iter_mut() {
                    if !ul.done && !ul.cancelled && ul.last_tick.elapsed().as_secs() >= STALE_SECS {
                        ul.cancelled = true;
                        ul.done_at = Some(now);
                    }
                    // Also catch any stuck at 100% without a done_at
                    if !ul.done && ul.total > 0 && ul.bytes_sent >= ul.total {
                        ul.done = true;
                        ul.done_at = Some(now);
                    }
                }
                for wu in self.web_uploads.iter_mut() {
                    if !wu.done && !wu.failed && wu.last_tick.elapsed().as_secs() >= STALE_SECS {
                        wu.failed = true;
                        wu.done_at = Some(now);
                    }
                }
                // Prune finished/failed transfers after 3 seconds visible
                self.active_downloads.retain(|d| {
                    d.done_at.map(|t| t.elapsed().as_secs() < 3).unwrap_or(true)
                });
                self.active_uploads.retain(|u| {
                    u.done_at.map(|t| t.elapsed().as_secs() < 3).unwrap_or(true)
                });
                self.web_uploads.retain(|w| {
                    w.done_at.map(|t| t.elapsed().as_secs() < 3).unwrap_or(true)
                });
            }
            _ => {}
        }
    }
}
