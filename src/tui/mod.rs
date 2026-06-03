pub mod app;
pub mod ui;

use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crossterm::event::KeyEventKind;

use crate::config::Config;
use crate::discovery::PeerRegistry;
use crate::server::ServerEvent;
use crate::shares::ShareRegistry;

use app::{App, AppEvent};

/// How long we wait after the last keystroke before deciding a burst is a dropped path.
const PATH_DEBOUNCE_MS: u64 = 30;

fn deduplicate_path(s: &str) -> String {
    // Windows Terminal drag-and-drop duplicates every character: "CC:\\UUsseerrss\\"
    // Detect by checking if every char pair is identical, then collapse.
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= 4 && chars.len().is_multiple_of(2) {
        let all_doubled = chars.chunks(2).all(|c| c[0] == c[1]);
        if all_doubled {
            return chars.iter().step_by(2).collect();
        }
    }
    s.to_string()
}

fn looks_like_path(s: &str) -> bool {
    let s = s.trim();
    // Windows absolute path: C:\... or \\server\...
    // Unix absolute path: /...
    // Also handle quoted paths
    let s = s.trim_matches('"').trim_matches('\'');
    s.starts_with('/') || s.starts_with('\\') || (s.len() >= 3 && s.chars().nth(1) == Some(':'))
}

fn try_share_path(
    raw: &str,
    shares: &ShareRegistry,
    event_tx: &mpsc::Sender<AppEvent>,
) {
    let cleaned = deduplicate_path(raw.trim().trim_matches('"').trim_matches('\''));
    let path = PathBuf::from(&cleaned);
    crate::config::debug_log(&format!("try_share_path: raw={:?} cleaned={:?} exists={}", raw, cleaned, path.exists()));
    let etx = event_tx.clone();
    let shares_c = shares.clone();
    tokio::spawn(async move {
        if !path.exists() {
            etx.send(AppEvent::ShareError(format!("Path not found: {}", cleaned))).await.ok();
            return;
        }
        if path.is_dir() {
            // Analyse folder first, then ask the user whether to zip
            let path_c = path.clone();
            let (file_count, max_depth, total_size) =
                tokio::task::spawn_blocking(move || {
                    crate::shares::analyse_folder_full(&path_c)
                })
                .await
                .unwrap_or((0, 0, 0));

            let folder_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let would_zip = file_count > 20 || max_depth > 5;

            etx.send(AppEvent::ZipConfirmNeeded(app::ZipConfirmRequest {
                path,
                folder_name,
                file_count,
                total_size,
                would_zip,
            }))
            .await
            .ok();
        } else {
            // Plain file — share immediately
            let etx2 = etx.clone();
            match shares_c.add(path, None, None, move |folder_name| {
                let msg = format!("Zipping '{}' — this may take a moment…", folder_name);
                let etx2 = etx2.clone();
                tokio::spawn(async move {
                    let _ = etx2.send(AppEvent::ZipStarted(msg)).await;
                });
            }) {
                Ok(item) => { etx.send(AppEvent::ShareAdded(item)).await.ok(); }
                Err(e) => { etx.send(AppEvent::ShareError(e.to_string())).await.ok(); }
            }
        }
    });
}

pub async fn run(
    config: Config,
    peers: PeerRegistry,
    shares: ShareRegistry,
    mut server_events: tokio::sync::broadcast::Receiver<ServerEvent>,
    restored: usize,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(256);
    let mut app = App::new(config, peers, shares.clone(), event_tx.clone());

    app.log(
        "fileshare started — drag & drop or [m] to add a path",
        app::LogKind::Info,
    );
    if restored > 0 {
        app.log(
            format!("Restored {} share(s) from last session", restored),
            app::LogKind::Success,
        );
    }
    app.log(
        "Listening for peers on the local network…",
        app::LogKind::Info,
    );

    let mut crossterm_events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    // Forward server events into our event channel.
    // On broadcast::error::RecvError::Lagged, the receiver has automatically
    // advanced past the dropped messages — just loop and continue receiving.
    let etx = event_tx.clone();
    tokio::spawn(async move {
        loop {
            match server_events.recv().await {
                Ok(ev) => { etx.send(AppEvent::ServerEvent(ev)).await.ok(); }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => { /* receiver caught up, keep going */ }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Path accumulation buffer for drag-and-drop via raw key events (Windows Terminal)
    let mut path_buf = String::new();
    let mut last_key_time: Option<Instant> = None;
    // We're accumulating a path burst when this is true — suppress normal key handling
    let mut accumulating = false;

    let _tick_rate = std::time::Duration::from_millis(250);

    loop {
        // Check if the debounce timer has expired — flush pending path buffer
        if let Some(t) = last_key_time {
        if t.elapsed() >= Duration::from_millis(PATH_DEBOUNCE_MS) && !path_buf.is_empty() {
            let raw = path_buf.clone();
            path_buf.clear();
            last_key_time = None;
            accumulating = false;

            // NEW LOGIC HERE
            let deduped = deduplicate_path(&raw);
            if looks_like_path(&deduped) {
                try_share_path(&raw, &shares, &event_tx);
            } else if raw.len() == 1 {
                // Treat as normal key input
                let c = raw.chars().next().unwrap();
                app.handle_key(crossterm::event::KeyEvent::from(KeyCode::Char(c)));
            }
            // else: discard or handle as needed
        }
    }

        terminal.draw(|f| ui::draw(f, &app))?;

        tokio::select! {
            _ = tick.tick() => {
                app.shares.prune_expired();
                // Prune downloads that finished more than 5 seconds ago
                app.active_downloads.retain(|d| {
                    d.done_at.map(|t| t.elapsed().as_secs() < 5).unwrap_or(true)
                });
                // Same for uploads — also catch ones stuck at 100% without a done_at
                for ul in app.active_uploads.iter_mut() {
                    if !ul.done && ul.total > 0 && ul.bytes_sent >= ul.total {
                        ul.done = true;
                        ul.done_at = Some(std::time::Instant::now());
                    }
                }
                app.active_uploads.retain(|u| {
                    u.done_at.map(|t| t.elapsed().as_secs() < 5).unwrap_or(true)
                });

                let _ = event_tx.send(AppEvent::Tick).await; // send tick for auto file refresh 
            }

            Some(Ok(event)) = crossterm_events.next() => {
                match event {
                    Event::Key(key) => {
                        // Only process real key presses (fixes Windows duplication)
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        // Global quit (but not while accumulating a path or in any input mode)
                        let in_input_mode = app.show_qr
                            || app.manual_ip_input.is_some()
                            || app.manual_path_input.is_some()
                            || app.zip_confirm.is_some();
                        if !accumulating && !in_input_mode
                            && (key.code == KeyCode::Char('q')
                                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)))
                            {
                                break;
                            }

                        // Ctrl+C always quits
                        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                            break;
                        }

                        if key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::CONTROL) {
                                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                    if let Ok(text) = clipboard.get_text() {
                                        path_buf.clear();
                                        last_key_time = None;
                                        accumulating = false;

                                        try_share_path(&text, &shares, &event_tx);
                                    }
                                }
                                continue;
                        }

                        match key.code {
                            KeyCode::Char(c) => {
                                // '/' or '\' → definitely starting a path
                                let is_path_start = c == '/' || c == '\\';
                                // Single letter could be a Windows drive letter (C:\ D:\…)
                                // We speculate and wait for the second char to confirm.
                                let is_letter = c.is_ascii_alphabetic();

                                if accumulating || (!path_buf.is_empty()) {
                                    // Already accumulating — check if prev buffer is a lone
                                    // letter and this char is NOT ':', meaning it's a normal
                                    // keystroke like 'jk' — flush and handle normally.
                                    let prev_is_lone_letter = path_buf.len() == 1
                                        && path_buf.chars().next().map(|ch| ch.is_ascii_alphabetic()).unwrap_or(false);
                                    if prev_is_lone_letter && c != ':' && !accumulating {
                                        // Not a drive letter — flush the buffered letter as key,
                                        // then handle current char normally too.
                                        let lone = path_buf.chars().next().unwrap();
                                        path_buf.clear();
                                        last_key_time = None;
                                        app.handle_key(crossterm::event::KeyEvent::from(KeyCode::Char(lone)));
                                        app.handle_key(key);
                                    } else {
                                        path_buf.push(c);
                                        last_key_time = Some(Instant::now());
                                    }
                                } else if (is_path_start || is_letter) && !in_input_mode {
                                    path_buf.push(c);
                                    last_key_time = Some(Instant::now());
                                    accumulating = false; // tentative
                                } else {
                                    app.handle_key(key);
                                }
                            }
                            KeyCode::Enter => {
                                if !path_buf.is_empty() {
                                    // User typed a path and pressed Enter
                                    let raw = path_buf.clone();
                                    path_buf.clear();
                                    last_key_time = None;
                                    accumulating = false;
                                    try_share_path(&raw, &shares, &event_tx);
                                } else {
                                    app.handle_key(key);
                                }
                            }
                            KeyCode::Esc => {
                                if !path_buf.is_empty() {
                                    path_buf.clear();
                                    last_key_time = None;
                                    accumulating = false;
                                } else {
                                    app.handle_key(key);
                                }
                            }
                            _ => {
                                if path_buf.is_empty() {
                                    app.handle_key(key);
                                }
                                // If accumulating, non-char keys (shift etc) are ignored
                            }
                        }

                        // After accumulating enough chars, check if it looks like a path
                        // so we can set the accumulating flag and stop routing to handle_key
                        if !path_buf.is_empty() && path_buf.len() >= 2 {
                            let deduped = deduplicate_path(&path_buf);
                            if looks_like_path(&deduped) || looks_like_path(&path_buf) {
                                accumulating = true;
                            }
                        }
                    }
                    Event::Paste(text) => {
                        // Modern terminals send a proper Paste event — handle directly
                        path_buf.clear();
                        last_key_time = None;
                        accumulating = false;
                        try_share_path(&text, &shares, &event_tx);
                    }
                    _ => {}
                }
            }

            Some(app_event) = event_rx.recv() => {
                // If a single-char buffer didn't grow into a path, flush it as-is before
                // processing the next app event (rare edge case)
                if !path_buf.is_empty() && !accumulating {
                    if let Some(t) = last_key_time {
                        if t.elapsed() >= Duration::from_millis(PATH_DEBOUNCE_MS) {
                            let raw = path_buf.clone();
                            path_buf.clear();
                            last_key_time = None;
                            let _ = raw;
                        }
                    }
                }
                // Process this event
                if let AppEvent::AddShare(ref p) = app_event {
                    let raw = p.to_string_lossy().to_string();
                    try_share_path(&raw, &shares, &event_tx);
                } else {
                    app.handle_event(app_event);
                }
                // Drain any further pending app events without blocking.
                // Critical for high-frequency progress events (large file transfers
                // fire hundreds of UploadProgress events/sec) so they don't pile up
                // and get processed only one-per-100ms-select-iteration.
                loop {
                    match event_rx.try_recv() {
                        Ok(ev) => {
                            if let AppEvent::AddShare(ref p) = ev {
                                let raw = p.to_string_lossy().to_string();
                                try_share_path(&raw, &shares, &event_tx);
                            } else {
                                app.handle_event(ev);
                            }
                        }
                        Err(_) => break, // Empty or closed — done draining
                    }
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
