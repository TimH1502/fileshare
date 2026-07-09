use crate::client::{self, calc_eta_seconds, DownloadControl, DownloadResult, RemoteShareInfo};
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
    Transfers,
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

#[derive(Debug)]
pub struct DownloadState {
    pub id: String,
    pub name: String,
    pub bytes_done: u64,
    pub total: u64,
    pub speed_bps: f64,
    pub done: bool,
    pub cancelled: bool,
    pub retrying: bool,
    pub paused: bool,
    pub done_at: Option<std::time::Instant>,
    pub eta_seconds: f64,
    pub last_activity: std::time::Instant,
    /// Signals the download task to pause/resume. None for finished transfers.
    pub pause_tx: Option<tokio::sync::watch::Sender<DownloadControl>>,
}

#[derive(Debug, Clone)]
pub struct UploadState {
    pub id: String, // share id (from ServerEvent)
    pub name: String,
    pub bytes_sent: u64,
    pub total: u64,
    pub speed_bps: f64,
    pub done: bool,
    pub cancelled: bool,
    pub done_at: Option<std::time::Instant>,
    pub last_bytes: u64, // for speed calculation
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

/// State for browsing the contents of a raw folder share (peer files panel).
#[derive(Debug, Clone)]
pub struct BrowsingFolder {
    /// Share id of the folder being browsed.
    pub share_id: String,
    pub folder_name: String,
    pub loading: bool,
    pub error: Option<String>,
    pub files: Vec<client::FolderManifestEntry>,
    pub selected: usize,
}

/// A file being received from the web UI (browser upload).
#[derive(Debug, Clone)]
pub struct WebUploadState {
    pub transfer_id: String,
    pub name: String,
    pub bytes_received: u64,
    pub total: u64, // 0 when Content-Length was absent
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
    PeerFilesError(String),
    /// Progress update keyed by share id
    DownloadProgress {
        id: String,
        progress: client::DownloadProgress,
    },
    DownloadDone {
        id: String,
        result: DownloadResult,
    },
    DownloadError {
        id: String,
        error: String,
    },
    DownloadRetrying {
        id: String,
        attempt: u32,
    },
    ServerEvent(ServerEvent),
    AddShare(PathBuf),
    ZipConfirmNeeded(ZipConfirmRequest),
    ZipConfirmResult(PathBuf, crate::shares::ZipMode),
    ShareAdded(crate::shares::SharedItem),
    ShareError(String),
    /// Live progress tick while zipping: folder name, files done, total files.
    /// Sent with `try_send` (best-effort — a dropped tick just means the
    /// progress bar skips a frame). Only the terminal `ShareAdded` /
    /// `ShareError` event is delivery-guaranteed via a blocking `send`, so a
    /// dropped tick can never leave the log stuck on a stale progress line.
    ZipProgress {
        folder: String,
        done: usize,
        total: usize,
        mode: crate::shares::ZipMode,
    },
    /// Live progress tick while computing a file's checksum: file name,
    /// bytes hashed so far, total bytes. Fired for plain files (no zipping
    /// involved) so large single-file adds don't look like they've hung.
    /// Same best-effort `try_send` guarantee as `ZipProgress` above.
    HashProgress {
        name: String,
        done: u64,
        total: u64,
    },
    /// Manifest fetch for the "browse folder contents" feature succeeded.
    FolderManifestLoaded {
        share_id: String,
        files: Vec<client::FolderManifestEntry>,
    },
    /// Manifest fetch for the "browse folder contents" feature failed.
    FolderManifestError {
        share_id: String,
        error: String,
    },
}

/// Whether transfer speeds are displayed in bytes/s (MB/s) or bits/s (Mb/s)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeedUnit {
    Bytes, // MB/s, KB/s
    Bits,  // Mb/s, Kb/s
}

impl SpeedUnit {
    pub fn toggle(self) -> Self {
        match self {
            SpeedUnit::Bytes => SpeedUnit::Bits,
            SpeedUnit::Bits => SpeedUnit::Bytes,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            SpeedUnit::Bytes => "MB/s",
            SpeedUnit::Bits => "Mb/s",
        }
    }
}

/// All colors needed by the TUI, grouped into one struct so themes can be
/// swapped at runtime without touching any rendering logic.
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: &'static str,
    pub accent: ratatui::style::Color, // borders, titles, highlights
    pub dim: ratatui::style::Color,    // secondary text, inactive elements
    pub success: ratatui::style::Color, // done / live / checkmark
    pub warn: ratatui::style::Color,   // warnings, expiry, retrying
    pub error: ratatui::style::Color,  // cancelled, failed
    pub download: ratatui::style::Color, // download progress bars / icons
    pub upload: ratatui::style::Color, // upload progress (peer-to-peer)
    pub web_upload: ratatui::style::Color, // upload from browser
    pub selected_bg: ratatui::style::Color, // selected row background
    pub bar_bg: ratatui::style::Color, // title bar + status bar background
    pub overlay_bg: ratatui::style::Color, // popup / overlay background
    pub text: ratatui::style::Color,   // primary text
}

use ratatui::style::Color;

// Two color sets exist because terminal emulators disagree on how "named"
// ANSI colors (e.g. DarkGray) and near-black RGB backgrounds render. Windows
// Terminal / ConHost render the original values fine. macOS's Terminal.app
// (and some Linux terminal profiles) remap dim/bright-black colors much
// closer to the background, and pack backgrounds close enough to true black
// that they can visibly shimmer on redraw. The non-Windows set below keeps
// each theme's original hue/character but raises every foreground color to
// >=4.5:1 contrast against its background (WCAG AA text minimum) and spaces
// background layers (bar/overlay/selection) further apart from black and
// from each other, computed against sRGB relative luminance.
#[cfg(target_family = "windows")]
pub const THEMES: &[Theme] = &[
    // ── Ocean (default) ─────────────────────────────────────────────────────
    Theme {
        name: "Ocean",
        accent: Color::Cyan,
        dim: Color::DarkGray,
        success: Color::Green,
        warn: Color::Yellow,
        error: Color::Red,
        download: Color::Magenta,
        upload: Color::Rgb(255, 165, 0),
        web_upload: Color::Rgb(100, 200, 255),
        selected_bg: Color::Rgb(30, 50, 60),
        bar_bg: Color::Rgb(15, 20, 30),
        overlay_bg: Color::Rgb(10, 15, 25),
        text: Color::White,
    },
    // ── Dracula ─────────────────────────────────────────────────────────────
    Theme {
        name: "Dracula",
        accent: Color::Rgb(189, 147, 249), // purple
        dim: Color::Rgb(98, 114, 164),
        success: Color::Rgb(80, 250, 123),     // green
        warn: Color::Rgb(255, 184, 108),       // orange
        error: Color::Rgb(255, 85, 85),        // red
        download: Color::Rgb(255, 121, 198),   // pink
        upload: Color::Rgb(255, 184, 108),     // orange
        web_upload: Color::Rgb(139, 233, 253), // cyan
        selected_bg: Color::Rgb(68, 71, 90),
        bar_bg: Color::Rgb(33, 34, 44),
        overlay_bg: Color::Rgb(22, 22, 30),
        text: Color::Rgb(248, 248, 242),
    },
    // ── Nord ────────────────────────────────────────────────────────────────
    Theme {
        name: "Nord",
        accent: Color::Rgb(136, 192, 208), // frost blue
        dim: Color::Rgb(76, 86, 106),
        success: Color::Rgb(163, 190, 140),    // aurora green
        warn: Color::Rgb(235, 203, 139),       // aurora yellow
        error: Color::Rgb(191, 97, 106),       // aurora red
        download: Color::Rgb(180, 142, 173),   // aurora purple
        upload: Color::Rgb(208, 135, 112),     // aurora orange
        web_upload: Color::Rgb(143, 188, 187), // teal frost
        selected_bg: Color::Rgb(59, 66, 82),
        bar_bg: Color::Rgb(36, 41, 51),
        overlay_bg: Color::Rgb(29, 33, 42),
        text: Color::Rgb(229, 233, 240),
    },
    // ── Gruvbox ─────────────────────────────────────────────────────────────
    Theme {
        name: "Gruvbox",
        accent: Color::Rgb(214, 152, 33), // bright yellow
        dim: Color::Rgb(146, 131, 116),
        success: Color::Rgb(152, 151, 26),     // olive green
        warn: Color::Rgb(215, 153, 33),        // orange
        error: Color::Rgb(204, 36, 29),        // red
        download: Color::Rgb(177, 98, 134),    // purple
        upload: Color::Rgb(214, 93, 14),       // bright orange
        web_upload: Color::Rgb(104, 157, 106), // aqua
        selected_bg: Color::Rgb(80, 73, 69),
        bar_bg: Color::Rgb(40, 36, 32),
        overlay_bg: Color::Rgb(29, 26, 24),
        text: Color::Rgb(235, 219, 178),
    },
    // ── Matrix ──────────────────────────────────────────────────────────────
    Theme {
        name: "Matrix",
        accent: Color::Rgb(0, 255, 70), // bright green
        dim: Color::Rgb(0, 100, 30),
        success: Color::Rgb(0, 255, 70),
        warn: Color::Rgb(180, 255, 0),
        error: Color::Rgb(255, 50, 50),
        download: Color::Rgb(0, 200, 100),
        upload: Color::Rgb(100, 255, 150),
        web_upload: Color::Rgb(0, 220, 180),
        selected_bg: Color::Rgb(0, 40, 15),
        bar_bg: Color::Rgb(0, 15, 5),
        overlay_bg: Color::Rgb(0, 8, 3),
        text: Color::Rgb(180, 255, 200),
    },
];

// ─────────────────────────────────────────────────────────────────────────
// Non-Windows (macOS, Linux, etc.): every value below is explicit RGB (no
// named ANSI colors, which some terminal profiles remap unpredictably) and
// every foreground color measures >=4.5:1 contrast against both its theme's
// bar_bg and pure black. Background layers (bar_bg/overlay_bg/selected_bg)
// are also spaced apart from black and from each other by a real luminance
// step, which is what actually stops near-black backgrounds from visibly
// shimmering on redraw in some terminal renderers.
#[cfg(not(target_family = "windows"))]
pub const THEMES: &[Theme] = &[
    // ── Ocean (default) ─────────────────────────────────────────────────────
    Theme {
        name: "Ocean",
        accent: Color::Rgb(64, 220, 255),
        dim: Color::Rgb(150, 165, 180),
        success: Color::Rgb(90, 235, 140),
        warn: Color::Rgb(255, 210, 80),
        error: Color::Rgb(255, 110, 110),
        download: Color::Rgb(225, 130, 255),
        upload: Color::Rgb(255, 180, 80),
        web_upload: Color::Rgb(130, 215, 255),
        selected_bg: Color::Rgb(40, 68, 84),
        bar_bg: Color::Rgb(22, 32, 44),
        overlay_bg: Color::Rgb(14, 20, 30),
        text: Color::Rgb(235, 240, 245),
    },
    // ── Dracula ─────────────────────────────────────────────────────────────
    Theme {
        name: "Dracula",
        accent: Color::Rgb(198, 157, 255),
        dim: Color::Rgb(155, 170, 215),
        success: Color::Rgb(120, 255, 160),
        warn: Color::Rgb(255, 195, 130),
        error: Color::Rgb(255, 120, 120),
        download: Color::Rgb(255, 150, 210),
        upload: Color::Rgb(255, 195, 130),
        web_upload: Color::Rgb(160, 240, 255),
        selected_bg: Color::Rgb(80, 84, 108),
        bar_bg: Color::Rgb(44, 46, 60),
        overlay_bg: Color::Rgb(28, 28, 38),
        text: Color::Rgb(248, 248, 242),
    },
    // ── Nord ────────────────────────────────────────────────────────────────
    Theme {
        name: "Nord",
        accent: Color::Rgb(160, 210, 225),
        dim: Color::Rgb(160, 175, 195),
        success: Color::Rgb(185, 210, 160),
        warn: Color::Rgb(240, 208, 145),
        error: Color::Rgb(232, 140, 148),
        download: Color::Rgb(205, 170, 200),
        upload: Color::Rgb(225, 160, 135),
        web_upload: Color::Rgb(170, 210, 209),
        selected_bg: Color::Rgb(70, 79, 98),
        bar_bg: Color::Rgb(48, 54, 67),
        overlay_bg: Color::Rgb(34, 38, 48),
        text: Color::Rgb(233, 237, 244),
    },
    // ── Gruvbox ─────────────────────────────────────────────────────────────
    Theme {
        name: "Gruvbox",
        accent: Color::Rgb(250, 189, 47),
        dim: Color::Rgb(178, 163, 142),
        success: Color::Rgb(195, 190, 50),
        warn: Color::Rgb(250, 189, 47),
        error: Color::Rgb(255, 110, 90),
        download: Color::Rgb(222, 145, 166),
        upload: Color::Rgb(254, 140, 45),
        web_upload: Color::Rgb(155, 205, 137),
        selected_bg: Color::Rgb(96, 88, 82),
        bar_bg: Color::Rgb(56, 50, 44),
        overlay_bg: Color::Rgb(38, 34, 30),
        text: Color::Rgb(251, 241, 199),
    },
    // ── Matrix ──────────────────────────────────────────────────────────────
    Theme {
        name: "Matrix",
        accent: Color::Rgb(70, 255, 130),
        dim: Color::Rgb(85, 180, 105),
        success: Color::Rgb(70, 255, 130),
        warn: Color::Rgb(200, 255, 80),
        error: Color::Rgb(255, 110, 110),
        download: Color::Rgb(70, 230, 140),
        upload: Color::Rgb(150, 255, 185),
        web_upload: Color::Rgb(70, 240, 200),
        selected_bg: Color::Rgb(12, 60, 29),
        bar_bg: Color::Rgb(6, 28, 13),
        overlay_bg: Color::Rgb(3, 15, 7),
        text: Color::Rgb(195, 255, 210),
    },
];

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

    /// Which transfer row is selected (index into active_downloads)
    pub transfer_cursor: usize,
    pub show_help: bool,
    pub show_qr: bool,
    pub manual_ip_input: Option<String>,
    pub manual_path_input: Option<String>,

    pub zip_confirm: Option<ZipConfirmRequest>,

    /// Set when the user has drilled into a folder's contents in the peer
    /// files panel. `Some` means we're showing that folder's manifest
    /// instead of the peer's top-level share list.
    pub browsing_folder: Option<BrowsingFolder>,

    /// Index of the live zip-progress log entry (updated in-place each tick)
    pub zip_progress_log_idx: Option<usize>,
    /// Index of the live checksum-progress log entry (updated in-place each tick)
    pub hash_progress_log_idx: Option<usize>,

    /// Whether speeds are shown in bytes (MB/s) or bits (Mb/s); toggled with `u`
    pub speed_unit: SpeedUnit,

    /// Index into THEMES; cycled with `t`
    pub theme_idx: usize,

    pub event_tx: mpsc::Sender<AppEvent>,

    pub last_peer_refresh: std::time::Instant,
}

/// Shorten a name for a single-line log entry so a long filename can't push
/// the progress bar and percentage off the edge of a narrow terminal.
/// Keeps a short prefix and suffix (so extensions like ".tar.gz" stay
/// visible) and drops the middle, e.g. "very_long_name_11…111111.zip".
/// Operates on chars, not bytes, so it never panics on multi-byte UTF-8.
fn truncate_middle(name: &str, max_chars: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max_chars {
        return name.to_string();
    }
    // Reserve 1 char for the ellipsis itself; split the rest between
    // a slightly longer head (more useful for recognizing the file) and tail.
    let budget = max_chars.saturating_sub(1);
    let head_len = budget.div_ceil(2);
    let tail_len = budget - head_len;
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    format!("{}…{}", head, tail)
}

impl App {
    pub fn new(
        config: Config,
        peers: PeerRegistry,
        shares: ShareRegistry,
        event_tx: mpsc::Sender<AppEvent>,
    ) -> Self {
        let speed_unit = if config.speed_unit_bits {
            SpeedUnit::Bits
        } else {
            SpeedUnit::Bytes
        };

        let theme_idx = config.theme_idx.min(crate::tui::app::THEMES.len() - 1);

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
            transfer_cursor: 0,
            show_help: false,
            show_qr: false,
            manual_ip_input: None,
            manual_path_input: None,
            zip_confirm: None,
            browsing_folder: None,
            zip_progress_log_idx: None,
            hash_progress_log_idx: None,
            speed_unit,
            theme_idx,
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
                        tx.send(AppEvent::ZipConfirmResult(
                            path,
                            crate::shares::ZipMode::Compressed,
                        ))
                        .await
                        .ok();
                    });
                }
                KeyCode::Char('f') | KeyCode::Char('F') => {
                    let path = req.path.clone();
                    self.zip_confirm = None;
                    let tx = self.event_tx.clone();
                    tokio::spawn(async move {
                        tx.send(AppEvent::ZipConfirmResult(
                            path,
                            crate::shares::ZipMode::FastBundle,
                        ))
                        .await
                        .ok();
                    });
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    let path = req.path.clone();
                    self.zip_confirm = None;
                    let tx = self.event_tx.clone();
                    tokio::spawn(async move {
                        tx.send(AppEvent::ZipConfirmResult(
                            path,
                            crate::shares::ZipMode::NoZip,
                        ))
                        .await
                        .ok();
                    });
                }
                KeyCode::Esc => {
                    self.zip_confirm = None;
                }
                _ => {}
            }
            return;
        }

        // Folder-browse view takes priority over the normal peer-files list
        if self.browsing_folder.is_some() {
            self.handle_browsing_folder_key(key);
            return;
        }

        // Manual path input mode
        if let Some(ref mut input) = self.manual_path_input {
            match key.code {
                KeyCode::Char(c) => input.push(c),
                KeyCode::Backspace => {
                    input.pop();
                }
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
                KeyCode::Backspace => {
                    input.pop();
                }
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
                    Focus::MyShares => Focus::Transfers,
                    Focus::Transfers => Focus::PeerList,
                };
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::PeerList => Focus::Transfers,
                    Focus::PeerFiles => Focus::PeerList,
                    Focus::MyShares => Focus::PeerFiles,
                    Focus::Transfers => Focus::MyShares,
                };
            }
            KeyCode::Char('?') | KeyCode::Char('h') => {
                self.show_help = !self.show_help;
            }
            KeyCode::Char('r') => {
                self.show_qr = !self.show_qr;
                self.show_help = false;
            }
            KeyCode::Char('u') => {
                self.speed_unit = self.speed_unit.toggle();
            }
            KeyCode::Char('t') => {
                self.theme_idx = (self.theme_idx + 1) % crate::tui::app::THEMES.len();
                let name = crate::tui::app::THEMES[self.theme_idx].name;
                self.log(format!("Theme: {}", name), LogKind::Info);
            }
            KeyCode::Char('L') => {
                self.log.clear();
                self.zip_progress_log_idx = None;
                self.hash_progress_log_idx = None;
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
                Focus::Transfers => self.handle_transfers_key(key),
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
            KeyCode::Char('b') => {
                self.browse_selected_folder();
            }
            KeyCode::Left => {
                self.focus = Focus::PeerList;
            }
            _ => {}
        }
    }

    /// Fetch and enter the "browse folder contents" view for the currently
    /// selected peer share, if it's a raw (unzipped) folder.
    fn browse_selected_folder(&mut self) {
        let peer = match self.selected_peer() {
            Some(p) => p,
            None => return,
        };
        let file = match self.peer_files.get(self.peer_files_state) {
            Some(f) => f.clone(),
            None => return,
        };
        if file.kind != "folder" {
            self.log(
                "Only raw (unzipped) folders can be browsed \u{2014} this share is a single file or a zip",
                LogKind::Info,
            );
            return;
        }

        self.browsing_folder = Some(BrowsingFolder {
            share_id: file.id.clone(),
            folder_name: file.name.clone(),
            loading: true,
            error: None,
            files: vec![],
            selected: 0,
        });

        let base_url = peer.http_base();
        let tx = self.event_tx.clone();
        let share_id = file.id.clone();
        tokio::spawn(async move {
            match client::fetch_folder_manifest(&base_url, &share_id).await {
                Ok(manifest) => {
                    tx.send(AppEvent::FolderManifestLoaded {
                        share_id,
                        files: manifest.files,
                    })
                    .await
                    .ok();
                }
                Err(e) => {
                    tx.send(AppEvent::FolderManifestError {
                        share_id,
                        error: e.to_string(),
                    })
                    .await
                    .ok();
                }
            }
        });
    }

    fn handle_browsing_folder_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        let file_count = self
            .browsing_folder
            .as_ref()
            .map(|b| b.files.len())
            .unwrap_or(0);
        match key.code {
            KeyCode::Esc | KeyCode::Left => {
                self.browsing_folder = None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(b) = self.browsing_folder.as_mut() {
                    if file_count > 0 {
                        b.selected = (b.selected + 1) % file_count;
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(b) = self.browsing_folder.as_mut() {
                    b.selected = b.selected.saturating_sub(1);
                }
            }
            KeyCode::Enter | KeyCode::Char('d') => {
                self.download_selected_folder_file();
            }
            KeyCode::Char('a') => {
                self.download_selected();
            }
            _ => {}
        }
    }

    /// Download a single file out of the folder currently being browsed.
    fn download_selected_folder_file(&mut self) {
        let (share_id, rel_path, file_size, folder_name) = match self.browsing_folder.as_ref() {
            Some(b) => match b.files.get(b.selected) {
                Some(f) => (
                    b.share_id.clone(),
                    f.path.clone(),
                    f.size,
                    b.folder_name.clone(),
                ),
                None => return,
            },
            None => return,
        };
        let peer = match self.selected_peer() {
            Some(p) => p,
            None => return,
        };

        // Reuse a synthetic id so progress/active-download tracking (keyed by
        // id) doesn't collide with a concurrent whole-folder download of the
        // same share.
        let synthetic_id = format!("{}::{}", share_id, rel_path);
        if self
            .active_downloads
            .iter()
            .any(|d| d.id == synthetic_id && !d.done)
        {
            self.log(
                format!("'{}' is already downloading", rel_path),
                LogKind::Info,
            );
            return;
        }

        let dest_name = std::path::Path::new(&rel_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| rel_path.clone());

        // Watch channel: TUI sends true=paused, false=running — same pattern
        // as the regular single-file/whole-folder download paths, so pause
        // and cancel work identically here.
        let (pause_tx, pause_rx) = tokio::sync::watch::channel(DownloadControl::Running);

        self.active_downloads.push(DownloadState {
            id: synthetic_id.clone(),
            name: format!("{}/{}", folder_name, rel_path),
            bytes_done: 0,
            total: file_size,
            speed_bps: 0.0,
            done: false,
            cancelled: false,
            retrying: false,
            paused: false,
            done_at: None,
            pause_tx: Some(pause_tx),
            eta_seconds: 0.0,
            last_activity: std::time::Instant::now(),
        });

        self.log(format!("Downloading '{}'\u{2026}", rel_path), LogKind::Info);

        let base_url = peer.http_base();
        // Save into <download_dir>/<folder_name>/ to mirror where a full
        // folder download would have placed this same file.
        let download_dir = self.config.download_dir.join(&folder_name);
        let tx = self.event_tx.clone();

        tokio::spawn(async move {
            let (prog_tx, mut prog_rx) = tokio::sync::mpsc::channel(32);
            let (retry_tx, mut retry_rx) = tokio::sync::mpsc::channel::<u32>(8);
            let tx2 = tx.clone();
            let tx3 = tx.clone();
            let id2 = synthetic_id.clone();
            let id3 = synthetic_id.clone();

            tokio::spawn(async move {
                while let Some(p) = prog_rx.recv().await {
                    tx2.send(AppEvent::DownloadProgress {
                        id: id2.clone(),
                        progress: p,
                    })
                    .await
                    .ok();
                }
            });
            tokio::spawn(async move {
                while let Some(attempt) = retry_rx.recv().await {
                    tx3.send(AppEvent::DownloadRetrying {
                        id: id3.clone(),
                        attempt,
                    })
                    .await
                    .ok();
                }
            });

            let url = format!(
                "{}/download/{}/file?path={}",
                base_url,
                share_id,
                client::urlencoding_encode(&rel_path)
            );
            let result = client::download_url_to_dir(
                &url,
                &format!("browse_{}", synthetic_id),
                &dest_name,
                &download_dir,
                prog_tx,
                retry_tx,
                pause_rx,
            )
            .await;

            match result {
                Ok(r) => {
                    tx.send(AppEvent::DownloadDone {
                        id: synthetic_id,
                        result: r,
                    })
                    .await
                    .ok();
                }
                Err(e) => {
                    tx.send(AppEvent::DownloadError {
                        id: synthetic_id,
                        error: e.to_string(),
                    })
                    .await
                    .ok();
                }
            }
        });
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
                        self.log(
                            format!("Removed '{}' from shares", removed.name),
                            LogKind::Info,
                        );
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
                        tx.send(AppEvent::PeerFilesError(e.to_string())).await.ok();
                    }
                }
            });
        }
    }

    fn handle_transfers_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        let dl_count = self.active_downloads.len();
        if dl_count == 0 {
            return;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.transfer_cursor > 0 {
                    self.transfer_cursor -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.transfer_cursor + 1 < dl_count {
                    self.transfer_cursor += 1;
                }
            }
            KeyCode::Char('p') | KeyCode::Char(' ') => {
                // Clamp cursor in case transfers finished
                self.transfer_cursor = self.transfer_cursor.min(dl_count.saturating_sub(1));
                if let Some(dl) = self.active_downloads.get_mut(self.transfer_cursor) {
                    if dl.done || dl.cancelled {
                        return;
                    }
                    let new_paused = !dl.paused;
                    dl.paused = new_paused;
                    if !new_paused {
                        // Reset stale timer on resume so it gets a fresh 10s window
                        dl.last_activity = std::time::Instant::now();
                    }
                    if let Some(tx) = &dl.pause_tx {
                        let _ = tx.send(if new_paused {
                            DownloadControl::Paused
                        } else {
                            DownloadControl::Running
                        });
                    }
                    let name = dl.name.clone();
                    self.log(
                        format!(
                            "{} '{}'",
                            if new_paused {
                                "⏸ Paused"
                            } else {
                                "▶ Resumed"
                            },
                            name
                        ),
                        LogKind::Info,
                    );
                }
            }
            KeyCode::Char('c') | KeyCode::Delete => {
                self.transfer_cursor = self.transfer_cursor.min(dl_count.saturating_sub(1));
                if let Some(dl) = self.active_downloads.get_mut(self.transfer_cursor) {
                    if dl.done || dl.cancelled {
                        return;
                    }
                    // Signal cancelled, then drop sender
                    if let Some(tx) = &dl.pause_tx {
                        let _ = tx.send(DownloadControl::Cancelled);
                    }
                    dl.pause_tx = None;
                    dl.cancelled = true;
                    dl.done_at = Some(std::time::Instant::now());
                    let name = dl.name.clone();
                    self.log(format!("✖ Cancelled '{}'", name), LogKind::Warning);
                }
            }
            _ => {}
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
        if self
            .active_downloads
            .iter()
            .any(|d| d.id == file.id && !d.done)
        {
            self.log(
                format!("'{}' is already downloading", file.name),
                LogKind::Warning,
            );
            return;
        }

        // Watch channel: TUI sends true=paused, false=running
        let (pause_tx, pause_rx) = tokio::sync::watch::channel(DownloadControl::Running);

        self.active_downloads.push(DownloadState {
            id: file.id.clone(),
            name: file.name.clone(),
            bytes_done: 0,
            total: file.size,
            speed_bps: 0.0,
            done: false,
            cancelled: false,
            retrying: false,
            paused: false,
            done_at: None,
            pause_tx: Some(pause_tx),
            eta_seconds: 0.0,
            last_activity: std::time::Instant::now(),
        });

        self.log(format!("Downloading '{}'…", file.name), LogKind::Info);

        let base_url = peer.http_base();
        let download_dir = self.config.download_dir.clone();
        let tx = self.event_tx.clone();
        let file_id = file.id.clone();
        let is_folder = file.kind == "folder";

        tokio::spawn(async move {
            let (prog_tx, mut prog_rx) = tokio::sync::mpsc::channel(32);
            let (retry_tx, mut retry_rx) = tokio::sync::mpsc::channel::<u32>(8);
            let tx2 = tx.clone();
            let tx3 = tx.clone();
            let fid2 = file_id.clone();
            let fid3 = file_id.clone();

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

            tokio::spawn(async move {
                while let Some(attempt) = retry_rx.recv().await {
                    tx3.send(AppEvent::DownloadRetrying {
                        id: fid3.clone(),
                        attempt,
                    })
                    .await
                    .ok();
                }
            });

            let result = if is_folder {
                // Raw folders: no zip, ever. Fetch the manifest and download
                // every file individually, reconstructing the real folder on
                // disk. Aggregate per-file progress into the same
                // DownloadProgress shape the single-file path uses, so the
                // TUI shows one ordinary progress bar for the whole folder
                // with no extra UI code needed.
                let (folder_prog_tx, mut folder_prog_rx) =
                    tokio::sync::mpsc::channel::<client::FolderDownloadProgress>(32);
                let bridge_prog_tx = prog_tx.clone();
                let bridge = tokio::spawn(async move {
                    while let Some(fp) = folder_prog_rx.recv().await {
                        let _ = bridge_prog_tx
                            .send(client::DownloadProgress {
                                bytes_downloaded: fp.bytes_downloaded,
                                total_bytes: fp.total_bytes,
                                speed_bps: fp.speed_bps,
                                eta_seconds: fp.eta_seconds,
                            })
                            .await;
                    }
                });

                let r = client::download_folder(
                    &base_url,
                    &file.id,
                    &download_dir,
                    folder_prog_tx,
                    retry_tx,
                    pause_rx,
                )
                .await;
                bridge.abort();
                r
            } else {
                client::download_file(
                    &base_url,
                    &file.id,
                    &file.name,
                    &download_dir,
                    prog_tx,
                    retry_tx,
                    pause_rx,
                )
                .await
            };

            match result {
                Ok(result) if result.cancelled => {
                    let _ = result;
                }
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
            AppEvent::PeerFilesError(_e) => {
                // Peer went offline or is unreachable — clear silently.
                // Write to debug log only; don't spam the activity log on
                // every refresh tick.
                self.peer_files = vec![];
                self.peer_files_loading = false;
                crate::config::debug_log(&format!("peer files fetch failed: {}", _e));
            }
            AppEvent::DownloadRetrying { id, attempt } => {
                let msg = if let Some(dl) = self.active_downloads.iter_mut().find(|w| w.id == id) {
                    dl.retrying = true;
                    dl.last_activity = std::time::Instant::now();
                    Some(format!(
                        "Connection lost, retrying '{}' (attempt {}/{})...",
                        dl.name, attempt, 5
                    ))
                } else {
                    None
                };
                if let Some(m) = msg {
                    self.log(m, LogKind::Warning);
                }
            }
            AppEvent::DownloadProgress { id, progress } => {
                if let Some(dl) = self.active_downloads.iter_mut().find(|d| d.id == id) {
                    dl.bytes_done = progress.bytes_downloaded;
                    dl.total = progress.total_bytes;
                    dl.speed_bps = progress.speed_bps;
                    dl.eta_seconds = progress.eta_seconds;
                    dl.retrying = false; // resumed successfully
                    dl.last_activity = std::time::Instant::now();
                }
            }
            AppEvent::DownloadDone { id, result } => {
                if let Some(dl) = self.active_downloads.iter_mut().find(|d| d.id == id) {
                    if dl.cancelled {
                        return;
                    }
                    dl.done = true;
                    dl.pause_tx = None;
                    dl.done_at = Some(std::time::Instant::now());
                    // For very small/fast files total may still be 0 if no progress
                    // events fired. Ensure the bar renders as 100% either way.
                    if dl.total == 0 {
                        dl.total = 1;
                    }
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
                if let Some(ul) = self
                    .active_uploads
                    .iter_mut()
                    .find(|u| u.name == item_name && !u.done)
                {
                    ul.done = true;
                    ul.done_at = Some(std::time::Instant::now());
                    ul.bytes_sent = ul.total;
                }
            }
            AppEvent::ServerEvent(ServerEvent::UploadProgress {
                item_id,
                bytes_sent,
                total,
            }) => {
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
                    let (new_eta, new_smooth) = calc_eta_seconds(
                        ul.smoothed_speed,
                        instant_speed,
                        alpha,
                        ul.total,
                        ul.bytes_sent,
                    );
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
                    let name = self
                        .shares
                        .get(&item_id)
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
                        done_at: if total > 0 && bytes_sent >= total {
                            Some(now)
                        } else {
                            None
                        },
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
                self.zip_progress_log_idx = None;
                self.hash_progress_log_idx = None;
                self.log(
                    format!("Shared '{}' ({})", item.name, item.size_human()),
                    LogKind::Success,
                );
            }
            AppEvent::ZipConfirmNeeded(req) => {
                self.zip_confirm = Some(req);
            }
            AppEvent::ZipConfirmResult(path, zip_mode) => {
                let etx = self.event_tx.clone();
                let shares_c = self.shares.clone();
                tokio::spawn(async move {
                    let etx2 = etx.clone();
                    let etx3 = etx.clone();
                    let mode_for_progress = zip_mode;
                    // Zipping and checksumming are blocking, CPU/disk-bound work —
                    // run them on a blocking-pool thread so they never stall an
                    // async worker thread (which would otherwise freeze input
                    // handling and progress rendering for other transfers).
                    let result = tokio::task::spawn_blocking(move || {
                        shares_c.add_with_zip_choice(
                            path,
                            None,
                            None,
                            zip_mode,
                            move |folder_name, done, total| {
                                // Send a live progress event; the TUI updates the same
                                // log line in-place so it never spams the log.
                                let folder = folder_name.to_string();
                                let etx2 = etx2.clone();
                                // Use try_send to avoid blocking the zip thread
                                let _ = etx2.try_send(AppEvent::ZipProgress {
                                    folder,
                                    done,
                                    total,
                                    mode: mode_for_progress,
                                });
                            },
                            move |name, done, total| {
                                let name = name.to_string();
                                let etx3 = etx3.clone();
                                let _ = etx3.try_send(AppEvent::HashProgress { name, done, total });
                            },
                        )
                    })
                    .await;

                    match result {
                        Ok(Ok(item)) => {
                            etx.send(AppEvent::ShareAdded(item)).await.ok();
                        }
                        Ok(Err(e)) => {
                            etx.send(AppEvent::ShareError(e.to_string())).await.ok();
                        }
                        Err(e) => {
                            etx.send(AppEvent::ShareError(format!("Internal error: {}", e)))
                                .await
                                .ok();
                        }
                    }
                });
            }
            AppEvent::ZipProgress {
                folder,
                done,
                total,
                mode,
            } => {
                let pct = (done * 100).checked_div(total).unwrap_or(0);
                // Build a compact progress bar: [████░░░░] 42/76 (55%)
                const BAR_W: usize = 20;
                let filled = (BAR_W * done).checked_div(total).unwrap_or(0);
                let bar = format!("[{}{}]", "█".repeat(filled), "░".repeat(BAR_W - filled),);
                let verb = match mode {
                    crate::shares::ZipMode::FastBundle => "Bundling",
                    _ => "Zipping",
                };
                let short_folder = truncate_middle(&folder, 30);
                let msg = format!(
                    "📦 {} '{}' {} {}/{} files ({}%)",
                    verb, short_folder, bar, done, total, pct
                );
                match self.zip_progress_log_idx {
                    // Update the existing entry in-place — no new line
                    Some(idx) if idx < self.log.len() => {
                        self.log[idx].message = msg;
                        self.log[idx].timestamp = chrono::Local::now();
                    }
                    // First progress event — create the entry and remember its index
                    _ => {
                        self.log(msg, LogKind::Info);
                        self.zip_progress_log_idx = Some(self.log.len() - 1);
                    }
                }
            }
            AppEvent::HashProgress { name, done, total } => {
                let pct = (done * 100).checked_div(total);
                const BAR_W: usize = 20;
                let filled = (BAR_W as u64 * done).checked_div(total);
                let bar = format!(
                    "[{}{}]",
                    "█".repeat(filled.unwrap_or_default() as usize),
                    "░".repeat(BAR_W - filled.unwrap_or_default() as usize),
                );
                let short_name = truncate_middle(&name, 30);
                let msg = format!(
                    "🔒 Checksumming '{}' {} {} / {} ({}%)",
                    short_name,
                    bar,
                    crate::shares::human_size(done),
                    crate::shares::human_size(total),
                    pct.unwrap_or(0)
                );
                match self.hash_progress_log_idx {
                    // Update the existing entry in-place — no new line
                    Some(idx) if idx < self.log.len() => {
                        self.log[idx].message = msg;
                        self.log[idx].timestamp = chrono::Local::now();
                    }
                    // First progress event — create the entry and remember its index
                    _ => {
                        self.log(msg, LogKind::Info);
                        self.hash_progress_log_idx = Some(self.log.len() - 1);
                    }
                }
            }
            AppEvent::ShareError(e) => {
                // If a zip/hash progress line was still live, turn that exact
                // line into the failure message in place — the person can
                // then see which step was running (and how far it got, since
                // the line keeps its progress text as context) when it broke,
                // instead of the progress line vanishing and a generic
                // "Share failed" appearing disconnected from it.
                let stale_idx = self
                    .zip_progress_log_idx
                    .or(self.hash_progress_log_idx)
                    .filter(|&idx| idx < self.log.len());
                self.zip_progress_log_idx = None;
                self.hash_progress_log_idx = None;
                match stale_idx {
                    Some(idx) => {
                        self.log[idx].message = format!("✗ {}", e);
                        self.log[idx].kind = LogKind::Warning;
                        self.log[idx].timestamp = chrono::Local::now();
                    }
                    None => {
                        self.log(format!("✗ Share failed: {}", e), LogKind::Warning);
                    }
                }
            }

            AppEvent::ServerEvent(ServerEvent::WebUploadStarted {
                transfer_id,
                filename,
                total,
                by_addr,
            }) => {
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
            AppEvent::ServerEvent(ServerEvent::WebUploadProgress {
                transfer_id,
                bytes_received,
                total,
            }) => {
                let now = std::time::Instant::now();
                if let Some(wu) = self
                    .web_uploads
                    .iter_mut()
                    .find(|w| w.transfer_id == transfer_id)
                {
                    let elapsed = now.duration_since(wu.last_tick).as_secs_f64().max(0.001);
                    let delta = bytes_received.saturating_sub(wu.last_bytes) as f64;
                    let instant_speed = delta / elapsed;
                    wu.last_bytes = bytes_received;
                    wu.last_tick = now;
                    wu.bytes_received = bytes_received;
                    if total > 0 {
                        wu.total = total;
                    }

                    // Heavy EMA -- alpha 0.05 keeps a ~20-sample rolling average
                    let alpha = 0.05;
                    let (new_eta, new_smooth) = calc_eta_seconds(
                        wu.smoothed_speed,
                        instant_speed,
                        alpha,
                        wu.total,
                        wu.bytes_received,
                    );
                    wu.smoothed_speed = new_smooth;

                    // Only push to display fields every 500 ms to prevent flickering
                    if wu.last_display_update.elapsed().as_millis() >= 500 {
                        wu.speed_bps = new_smooth;
                        wu.eta_seconds = new_eta;
                        wu.last_display_update = now;
                    }
                }
            }
            AppEvent::ServerEvent(ServerEvent::WebUploadFinished { transfer_id }) => {
                let msg = if let Some(wu) = self
                    .web_uploads
                    .iter_mut()
                    .find(|w| w.transfer_id == transfer_id)
                {
                    wu.done = true;
                    wu.done_at = Some(std::time::Instant::now());
                    if wu.total > 0 {
                        wu.bytes_received = wu.total;
                    }
                    Some(format!(
                        "\u{2714} '{}' received from {} via web UI",
                        wu.name, wu.by_addr
                    ))
                } else {
                    None
                };
                if let Some(m) = msg {
                    self.log(m, LogKind::Success);
                }
            }
            AppEvent::ServerEvent(ServerEvent::WebUploadFailed { transfer_id }) => {
                let msg = if let Some(wu) = self
                    .web_uploads
                    .iter_mut()
                    .find(|w| w.transfer_id == transfer_id)
                {
                    wu.failed = true;
                    wu.done_at = Some(std::time::Instant::now());
                    Some(format!(
                        "\u{2717} '{}' upload from {} failed",
                        wu.name, wu.by_addr
                    ))
                } else {
                    None
                };
                if let Some(m) = msg {
                    self.log(m, LogKind::Warning);
                }
            }
            AppEvent::Tick => {
                // Auto-refresh peer files every 3 seconds
                if self.focus == Focus::PeerFiles {
                    let now = std::time::Instant::now();
                    let interval = std::time::Duration::from_secs(3);

                    if now.duration_since(self.last_peer_refresh) >= interval
                        && !self.peer_files_loading
                        && self.selected_peer().is_some()
                    {
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
                    if !dl.done
                        && !dl.cancelled
                        && !dl.paused
                        && dl.last_activity.elapsed().as_secs() >= STALE_SECS
                    {
                        dl.cancelled = true;
                        dl.pause_tx = None;
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
                self.active_downloads
                    .retain(|d| d.done_at.map(|t| t.elapsed().as_secs() < 3).unwrap_or(true));
                // Keep cursor in bounds after pruning
                if !self.active_downloads.is_empty() {
                    self.transfer_cursor =
                        self.transfer_cursor.min(self.active_downloads.len() - 1);
                }
                self.active_uploads
                    .retain(|u| u.done_at.map(|t| t.elapsed().as_secs() < 3).unwrap_or(true));
                self.web_uploads
                    .retain(|w| w.done_at.map(|t| t.elapsed().as_secs() < 3).unwrap_or(true));
            }
            AppEvent::FolderManifestLoaded { share_id, files } => {
                if let Some(b) = self.browsing_folder.as_mut() {
                    if b.share_id == share_id {
                        b.loading = false;
                        b.files = files;
                        b.selected = 0;
                    }
                }
            }
            AppEvent::FolderManifestError { share_id, error } => {
                if let Some(b) = self.browsing_folder.as_mut() {
                    if b.share_id == share_id {
                        b.loading = false;
                        b.error = Some(error);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DownloadControl;
    use tokio::sync::{mpsc, watch};

    fn make_app() -> App {
        let (tx, _rx) = mpsc::channel(64);
        let config = crate::config::Config::default();
        let peers = crate::discovery::PeerRegistry::new();
        let tmp = std::env::temp_dir().join("fileshare_test_shares");
        let shares = crate::shares::ShareRegistry::new(tmp.clone(), tmp.join("index.json"));
        App::new(config, peers, shares, tx)
    }

    fn fake_download(name: &str, paused: bool) -> DownloadState {
        let (pause_tx, _) = watch::channel(DownloadControl::Running);
        DownloadState {
            id: name.to_string(),
            name: name.to_string(),
            bytes_done: 1024,
            total: 10_000,
            speed_bps: 1_000_000.0,
            done: false,
            cancelled: false,
            retrying: false,
            paused,
            done_at: None,
            eta_seconds: 5.0,
            last_activity: std::time::Instant::now(),
            pause_tx: Some(pause_tx),
        }
    }

    // -----------------------------------------------------------------------
    // Stale / prune logic
    // -----------------------------------------------------------------------

    #[test]
    fn test_stale_active_download_gets_cancelled() {
        let mut app = make_app();
        let mut dl = fake_download("stale.bin", false);
        // Wind last_activity back beyond STALE_SECS
        dl.last_activity = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(15))
            .unwrap();
        app.active_downloads.push(dl);

        app.handle_event(AppEvent::Tick);

        let dl = &app.active_downloads[0];
        assert!(dl.cancelled, "stale download should be cancelled");
        assert!(dl.done_at.is_some());
    }

    #[test]
    fn test_paused_download_not_stale() {
        let mut app = make_app();
        let mut dl = fake_download("paused.bin", true);
        // Old last_activity — but paused, so must NOT be cancelled
        dl.last_activity = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(30))
            .unwrap();
        app.active_downloads.push(dl);

        app.handle_event(AppEvent::Tick);

        let dl = &app.active_downloads[0];
        assert!(
            !dl.cancelled,
            "paused download must never be stale-cancelled"
        );
    }

    #[test]
    fn test_done_download_pruned_after_3s() {
        let mut app = make_app();
        let mut dl = fake_download("done.bin", false);
        dl.done = true;
        dl.done_at = Some(
            std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(4))
                .unwrap(),
        );
        app.active_downloads.push(dl);

        app.handle_event(AppEvent::Tick);

        assert!(
            app.active_downloads.is_empty(),
            "finished download should be pruned after 3s"
        );
    }

    #[test]
    fn test_recent_done_download_not_yet_pruned() {
        let mut app = make_app();
        let mut dl = fake_download("recent.bin", false);
        dl.done = true;
        dl.done_at = Some(std::time::Instant::now()); // just finished
        app.active_downloads.push(dl);

        app.handle_event(AppEvent::Tick);

        assert!(
            !app.active_downloads.is_empty(),
            "recently finished download should still be visible"
        );
    }

    // -----------------------------------------------------------------------
    // Pause / resume / cancel state transitions
    // -----------------------------------------------------------------------

    #[test]
    fn test_pause_toggles_state() {
        let mut app = make_app();
        app.active_downloads.push(fake_download("file.bin", false));
        app.focus = Focus::Transfers;
        app.transfer_cursor = 0;

        let key = crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('p'));
        app.handle_key(key);

        assert!(
            app.active_downloads[0].paused,
            "download should be paused after p"
        );

        app.handle_key(key);
        assert!(
            !app.active_downloads[0].paused,
            "download should resume after second p"
        );
    }

    #[test]
    fn test_cancel_sets_cancelled_and_drops_sender() {
        let mut app = make_app();
        app.active_downloads.push(fake_download("file.bin", false));
        app.focus = Focus::Transfers;
        app.transfer_cursor = 0;

        let key = crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('c'));
        app.handle_key(key);

        let dl = &app.active_downloads[0];
        assert!(dl.cancelled, "download should be cancelled");
        assert!(dl.done_at.is_some(), "done_at should be set on cancel");
        assert!(
            dl.pause_tx.is_none(),
            "pause_tx should be dropped on cancel"
        );
    }

    #[test]
    fn test_cancel_paused_download() {
        let mut app = make_app();
        app.active_downloads.push(fake_download("file.bin", true)); // starts paused
        app.focus = Focus::Transfers;
        app.transfer_cursor = 0;

        let key = crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('c'));
        app.handle_key(key);

        assert!(app.active_downloads[0].cancelled);
        assert!(app.active_downloads[0].pause_tx.is_none());
    }

    #[test]
    fn test_cannot_cancel_already_done() {
        let mut app = make_app();
        let mut dl = fake_download("done.bin", false);
        dl.done = true;
        dl.done_at = Some(std::time::Instant::now());
        app.active_downloads.push(dl);
        app.focus = Focus::Transfers;
        app.transfer_cursor = 0;

        let key = crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('c'));
        app.handle_key(key);

        // Should remain done, not switch to cancelled
        assert!(!app.active_downloads[0].cancelled);
        assert!(app.active_downloads[0].done);
    }

    #[test]
    fn test_cursor_navigation() {
        let mut app = make_app();
        app.active_downloads.push(fake_download("a.bin", false));
        app.active_downloads.push(fake_download("b.bin", false));
        app.active_downloads.push(fake_download("c.bin", false));
        app.focus = Focus::Transfers;
        app.transfer_cursor = 0;

        let down = crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Down);
        let up = crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Up);

        app.handle_key(down);
        assert_eq!(app.transfer_cursor, 1);
        app.handle_key(down);
        assert_eq!(app.transfer_cursor, 2);
        // At bottom — should not go past end
        app.handle_key(down);
        assert_eq!(app.transfer_cursor, 2);
        app.handle_key(up);
        assert_eq!(app.transfer_cursor, 1);
        // At top — should not go negative
        app.handle_key(up);
        app.handle_key(up);
        assert_eq!(app.transfer_cursor, 0);
    }

    // -----------------------------------------------------------------------
    // DownloadDone ignored when already cancelled
    // -----------------------------------------------------------------------

    #[test]
    fn test_download_done_ignored_if_cancelled() {
        let mut app = make_app();
        let mut dl = fake_download("file.bin", false);
        dl.cancelled = true;
        dl.done_at = Some(std::time::Instant::now());
        app.active_downloads.push(dl);

        // Simulate late DownloadDone arriving after cancel
        app.handle_event(AppEvent::DownloadDone {
            id: "file.bin".to_string(),
            result: crate::client::DownloadResult {
                path: std::path::PathBuf::from("/fake"),
                checksum_ok: None,
                cancelled: false,
            },
        });

        // Must remain cancelled, not flipped to done
        assert!(app.active_downloads[0].cancelled);
        assert!(!app.active_downloads[0].done);
    }
}
