use crate::shares::ShareKind;
use crate::tui::app::{App, DownloadState, Focus, LogKind, ZipConfirmRequest};
use qrcode::QrCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const SUCCESS: Color = Color::Green;
const WARN: Color = Color::Yellow;
const DOWNLOAD_COLOR: Color = Color::Magenta;
const SELECTED_BG: Color = Color::Rgb(30, 50, 60);

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.size();

    let dl_count = app.active_downloads.len();
    // Always allocate at least 5 rows for the download panel so layout
    // doesn't jump when a fast download starts and immediately finishes.
    // Cap at 3 simultaneous entries (10 rows) to leave room for the log.
    let dl_height = (dl_count as u16 * 3 + 2).max(5).min(11);

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),          // title bar
            Constraint::Min(10),            // main content
            Constraint::Length(dl_height),  // downloads panel (always present)
            Constraint::Length(6),          // log
            Constraint::Length(1),          // status bar
        ])
        .split(area);

    draw_title_bar(f, app, root[0]);
    draw_main(f, app, root[1]);
    draw_downloads_panel(f, &app.active_downloads, root[2]);
    draw_log(f, app, root[3]);
    draw_status_bar(f, app, root[4]);

    if app.show_help {
        draw_help_overlay(f, area);
    }
    if app.show_qr {
        draw_qr_overlay(f, app, area);
    }
    if app.manual_ip_input.is_some() {
        draw_manual_ip_overlay(f, app, area);
    }
    if app.manual_path_input.is_some() {
        draw_manual_path_overlay(f, app, area);
    }
    if let Some(ref req) = app.zip_confirm {
        draw_zip_confirm_overlay(f, req, area);
    }
}

fn draw_title_bar(f: &mut Frame, app: &App, area: Rect) {
    let title = Line::from(vec![
        Span::styled(
            " 📡 fileshare ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("│ ", Style::default().fg(DIM)),
        Span::styled(
            &app.config.username,
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" @ port {}", app.config.port),
            Style::default().fg(DIM),
        ),
        Span::styled("  │  ", Style::default().fg(DIM)),
        Span::styled("? help", Style::default().fg(DIM)),
        Span::styled("  r qr", Style::default().fg(DIM)),
    ]);
    let bar = Paragraph::new(title).style(Style::default().bg(Color::Rgb(15, 20, 30)));
    f.render_widget(bar, area);
}

fn draw_main(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(35),
            Constraint::Percentage(65),
        ])
        .split(area);

    draw_left_panel(f, app, chunks[0]);
    draw_my_shares(f, app, chunks[1]);
}

fn draw_left_panel(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);

    draw_peer_list(f, app, chunks[0]);
    draw_peer_files(f, app, chunks[1]);
}

fn draw_peer_list(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::PeerList;
    let peers = app.peer_list();

    let border_style = if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    };

    let selected_is_manual = peers
        .get(app.peer_list_state)
        .map(|p| p.manual)
        .unwrap_or(false);
    let title_str = if focused && selected_is_manual {
        format!(" Peers ({}) — [x] remove ", peers.len())
    } else {
        format!(" Peers ({}) ", peers.len())
    };

    let block = Block::default()
        .title(Span::styled(
            title_str,
            Style::default().fg(if focused { ACCENT } else { Color::White }),
        ))
        .borders(Borders::ALL)
        .border_style(border_style);

    let items: Vec<ListItem> = peers
        .iter()
        .enumerate()
        .map(|(i, peer)| {
            let selected = i == app.peer_list_state;
            let style = if selected && focused {
                Style::default().bg(SELECTED_BG).add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let marker = if selected { "▶ " } else { "  " };
            let manual_tag = if peer.manual {
                Span::styled(" [m]", Style::default().fg(Color::Yellow))
            } else {
                Span::raw("")
            };
            let line = Line::from(vec![
                Span::raw(marker),
                Span::styled(&peer.username, style.fg(Color::White)),
                Span::styled(format!(" {}", peer.addr), Style::default().fg(DIM)),
                Span::styled(format!(" [{}]", peer.share_count), Style::default().fg(ACCENT)),
                manual_tag,
            ]);
            ListItem::new(line)
        })
        .collect();

    let empty_msg = if peers.is_empty() {
        vec![
            ListItem::new(Line::from(Span::styled("  No peers found", Style::default().fg(DIM)))),
            ListItem::new(Line::from(Span::styled(
                "  Press 'm' to add manually",
                Style::default().fg(DIM),
            ))),
        ]
    } else {
        vec![]
    };

    let list = List::new(if peers.is_empty() { empty_msg } else { items }).block(block);
    f.render_widget(list, area);
}

fn draw_peer_files(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::PeerFiles;
    let border_style = if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    };

    let title = if app.peer_files_loading {
        " Loading… ".to_string()
    } else {
        match app.selected_peer() {
            Some(p) => format!(" {}'s files ({}) ", p.username, app.peer_files.len()),
            None => " Files ".to_string(),
        }
    };

    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(if focused { ACCENT } else { Color::White }),
        ))
        .borders(Borders::ALL)
        .border_style(border_style);

    let downloading_ids: Vec<&str> = app
        .active_downloads
        .iter()
        .filter(|d| !d.done)
        .map(|d| d.id.as_str())
        .collect();

    let items: Vec<ListItem> = app
        .peer_files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            let selected = i == app.peer_files_state;
            let is_downloading = downloading_ids.contains(&file.id.as_str());

            let icon = match file.kind.as_str() {
                "folder" => "📁",
                "zipped_folder" => "🗜 ",
                _ => "📄",
            };
            let name_style = if !file.available {
                Style::default().fg(DIM)
            } else if is_downloading {
                Style::default().fg(DOWNLOAD_COLOR)
            } else if selected && focused {
                Style::default()
                    .fg(Color::White)
                    .bg(SELECTED_BG)
                    .add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let marker = if selected { "▶ " } else { "  " };
            let avail_marker = if !file.available { " ✗" } else { "" };
            let dl_marker = if is_downloading { " ⬇" } else { "" };

            let line = Line::from(vec![
                Span::raw(marker),
                Span::raw(icon),
                Span::raw(" "),
                Span::styled(format!("{}{}{}", &file.name, avail_marker, dl_marker), name_style),
                Span::styled(format!("  {}", file.size_human), Style::default().fg(DIM)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let no_peer_msg = if app.selected_peer().is_none() {
        vec![ListItem::new(Line::from(Span::styled(
            "  Select a peer →",
            Style::default().fg(DIM),
        )))]
    } else if !app.peer_files_loading && app.peer_files.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  No files shared",
            Style::default().fg(DIM),
        )))]
    } else {
        vec![]
    };

    let inner = block.inner(area);
    f.render_widget(block, area);
    let list = List::new(if app.peer_files.is_empty() { no_peer_msg } else { items });
    f.render_widget(list, inner);
}

/// Renders a dedicated panel showing all active downloads (always visible).
fn draw_downloads_panel(f: &mut Frame, downloads: &[DownloadState], area: Rect) {
    let (title, border_color) = if downloads.is_empty() {
        (
            " Downloads ".to_string(),
            DIM,
        )
    } else {
        (
            format!(" Downloads ({}) ", downloads.len()),
            DOWNLOAD_COLOR,
        )
    };

    let block = Block::default()
        .title(Span::styled(title, Style::default().fg(border_color)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if downloads.is_empty() {
        let hint = Paragraph::new(Line::from(Span::styled(
            "  No active downloads",
            Style::default().fg(DIM),
        )));
        f.render_widget(hint, inner);
        return;
    }

    // Each download gets 3 lines: name, bar, stats
    let mut y = inner.y;
    for dl in downloads {
        if y + 3 > inner.y + inner.height {
            break;
        }
        let row = Rect { y, height: 3, ..inner };
        draw_download_progress(f, dl, row);
        y += 3;
    }
}

fn draw_download_progress(f: &mut Frame, dl: &DownloadState, area: Rect) {
    let pct = if dl.done {
        1.0
    } else if dl.total > 0 {
        (dl.bytes_done as f64 / dl.total as f64).min(1.0)
    } else {
        0.0
    };
    let bar_width = area.width.saturating_sub(2) as usize;
    let filled = (pct * bar_width as f64) as usize;
    let bar_color = if dl.done { SUCCESS } else { DOWNLOAD_COLOR };
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_width - filled);
    let pct_str = format!("{:.0}%", pct * 100.0);
    let right_label = if dl.done {
        "done".to_string()
    } else {
        crate::client::format_speed(dl.speed_bps)
    };
    let icon = if dl.done { "✓" } else { "⬇" };

    let text = vec![
        Line::from(Span::styled(
            format!(" {} {}", icon, dl.name),
            Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(bar, Style::default().fg(bar_color))),
        Line::from(vec![
            Span::styled(format!(" {} ", pct_str), Style::default().fg(Color::White)),
            Span::styled(format!("{} ", right_label), Style::default().fg(DIM)),
        ]),
    ];
    f.render_widget(Paragraph::new(text), area);
}

fn draw_my_shares(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::MyShares;
    let shares = app.my_shares();

    let border_style = if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    };

    let block = Block::default()
        .title(Span::styled(
            format!(
                " My Shares ({}) — drag & drop, [m] add path, [x] remove ",
                shares.len()
            ),
            Style::default().fg(if focused { ACCENT } else { Color::White }),
        ))
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if shares.is_empty() {
        let hint = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled("  No files shared yet.", Style::default().fg(DIM))),
            Line::from(Span::styled(
                "  Drag & drop a file or folder into this terminal,",
                Style::default().fg(DIM),
            )),
            Line::from(Span::styled(
                "  or press [m] to type a path.",
                Style::default().fg(DIM),
            )),
        ]);
        f.render_widget(hint, inner);
        return;
    }

    let header_area = Rect { height: 1, ..inner };
    let list_area = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };

    let header = Line::from(vec![
        Span::styled("  ID       Name", Style::default().fg(DIM)),
        Span::styled(
            "                                Size      DLs  Status",
            Style::default().fg(DIM),
        ),
    ]);
    f.render_widget(Paragraph::new(header), header_area);

    let items: Vec<ListItem> = shares
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let selected = i == app.my_shares_state;
            let is_expired = item.is_expired();
            let is_limit = item.is_limit_reached();

            let icon = match item.kind {
                ShareKind::File => "📄",
                ShareKind::Folder => "📁",
                ShareKind::ZippedFolder => "🗜 ",
            };

            // Show expiry countdown if present
            let status = if is_expired {
                Span::styled("expired", Style::default().fg(WARN))
            } else if is_limit {
                Span::styled("limit", Style::default().fg(WARN))
            } else if let Some(countdown) = item.expiry_countdown() {
                Span::styled(countdown, Style::default().fg(Color::Yellow))
            } else {
                Span::styled("live", Style::default().fg(SUCCESS))
            };

            let name_style = if selected && focused {
                Style::default()
                    .bg(SELECTED_BG)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let marker = if selected { "▶ " } else { "  " };
            let name_trunc = truncate(&item.name, 30);

            let line = Line::from(vec![
                Span::raw(marker),
                Span::styled(format!("[{}] ", item.id), Style::default().fg(ACCENT)),
                Span::raw(icon),
                Span::raw(" "),
                Span::styled(format!("{:<30}", name_trunc), name_style),
                Span::styled(format!("  {:>8}", item.size_human()), Style::default().fg(DIM)),
                Span::styled(format!("  {:>3}x  ", item.download_count), Style::default().fg(DIM)),
                status,
            ]);
            ListItem::new(line)
        })
        .collect();

    let mut list_state = ListState::default();
    if focused {
        list_state.select(Some(app.my_shares_state));
    }
    let list = List::new(items);
    f.render_stateful_widget(list, list_area, &mut list_state);
}

fn draw_log(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(Span::styled(" Activity ", Style::default().fg(DIM)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let visible_lines = inner.height as usize;
    let start = app.log.len().saturating_sub(visible_lines);
    let recent = &app.log[start..];

    let lines: Vec<Line> = recent
        .iter()
        .map(|entry| {
            let time = entry.timestamp.format("%H:%M:%S").to_string();
            let (icon, color) = match entry.kind {
                LogKind::Info => ("·", DIM),
                LogKind::Success => ("✓", SUCCESS),
                LogKind::Warning => ("!", WARN),
                LogKind::Download => ("⬇", DOWNLOAD_COLOR),
            };
            Line::from(vec![
                Span::styled(format!(" {} ", time), Style::default().fg(DIM)),
                Span::styled(format!("{} ", icon), Style::default().fg(color)),
                Span::styled(&entry.message, Style::default().fg(Color::White)),
            ])
        })
        .collect();

    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let dl_dir = app.config.download_dir.display().to_string();

    let context_hints = match app.focus {
        Focus::PeerList => {
            let peers = app.peer_list();
            let is_manual = peers
                .get(app.peer_list_state)
                .map(|p| p.manual)
                .unwrap_or(false);
            if is_manual {
                vec![
                    Span::styled("[↑↓/jk] navigate  ", Style::default().fg(DIM)),
                    Span::styled("[Enter] browse files  ", Style::default().fg(DIM)),
                    Span::styled("[x] remove manual peer  ", Style::default().fg(Color::Yellow)),
                    Span::styled("[m] add peer  ", Style::default().fg(DIM)),
                ]
            } else {
                vec![
                    Span::styled("[↑↓/jk] navigate  ", Style::default().fg(DIM)),
                    Span::styled("[Enter] browse files  ", Style::default().fg(DIM)),
                    Span::styled("[m] add peer manually  ", Style::default().fg(DIM)),
                ]
            }
        }
        Focus::PeerFiles => vec![
            Span::styled("[↑↓/jk] navigate  ", Style::default().fg(DIM)),
            Span::styled("[Enter/d] download  ", Style::default().fg(DIM)),
            Span::styled("[←] back to peers  ", Style::default().fg(DIM)),
        ],
        Focus::MyShares => vec![
            Span::styled("[↑↓/jk] navigate  ", Style::default().fg(DIM)),
            Span::styled("[x/Del] remove share  ", Style::default().fg(Color::Yellow)),
            Span::styled("[m] add path  ", Style::default().fg(DIM)),
            Span::styled("drag & drop to add  ", Style::default().fg(DIM)),
        ],
    };

    let mut spans = vec![Span::styled(" [Tab] switch panel  ", Style::default().fg(DIM))];
    spans.extend(context_hints);
    spans.push(Span::styled("[?] help  ", Style::default().fg(DIM)));
    spans.push(Span::styled("[r] qr code  ", Style::default().fg(DIM)));
    spans.push(Span::styled("[q] quit  ", Style::default().fg(DIM)));
    spans.push(Span::styled(format!("  DL→ {}", dl_dir), Style::default().fg(DIM)));

    let status = Line::from(spans);
    let bar = Paragraph::new(status).style(Style::default().bg(Color::Rgb(15, 20, 30)));
    f.render_widget(bar, area);
}

fn draw_help_overlay(f: &mut Frame, area: Rect) {
    let w = 54u16;
    let h = 24u16;
    let x = area.width.saturating_sub(w) / 2;
    let y = area.height.saturating_sub(h) / 2;
    let popup = Rect::new(x, y, w.min(area.width), h.min(area.height));

    f.render_widget(Clear, popup);

    let text = vec![
        Line::from(Span::styled(
            " Keyboard Shortcuts",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            " Navigation",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled("  Tab / Shift+Tab   Switch panel", Style::default().fg(DIM))),
        Line::from(Span::styled("  ↑/↓  or  j/k      Move selection", Style::default().fg(DIM))),
        Line::from(Span::styled("  ←/→               Switch panels", Style::default().fg(DIM))),
        Line::from(""),
        Line::from(Span::styled(
            " Peers",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled("  m                 Add peer manually", Style::default().fg(DIM))),
        Line::from(Span::styled("  x / Del           Remove manual peer", Style::default().fg(DIM))),
        Line::from(Span::styled("  Enter             Browse peer's files", Style::default().fg(DIM))),
        Line::from(""),
        Line::from(Span::styled(
            " Downloads",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled("  Enter / d         Download selected", Style::default().fg(DIM))),
        Line::from(Span::styled(
            "  Multiple files can download simultaneously",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            "  SHA256 is verified automatically on completion",
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            " Sharing",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled("  x / Del           Remove your share", Style::default().fg(DIM))),
        Line::from(Span::styled("  Drag & drop file  Share it instantly", Style::default().fg(DIM))),
        Line::from(Span::styled(
            "  Drag & drop folder  → zip dialog",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            "  m (in My Shares)  Type a path manually",
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(Span::styled("  ?                 Toggle this help", Style::default().fg(DIM))),
        Line::from(Span::styled("  r                 Show QR code for web UI", Style::default().fg(DIM))),
        Line::from(Span::styled("  q / Ctrl+C        Quit", Style::default().fg(DIM))),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(Color::Rgb(10, 15, 25)));

    let para = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
    f.render_widget(para, popup);
}

fn draw_manual_ip_overlay(f: &mut Frame, app: &App, area: Rect) {
    let w = 44u16;
    let h = 5u16;
    let x = area.width.saturating_sub(w) / 2;
    let y = area.height.saturating_sub(h) / 2;
    let popup = Rect::new(x, y, w.min(area.width), h.min(area.height));
    f.render_widget(Clear, popup);

    let input = app.manual_ip_input.as_deref().unwrap_or("");
    let text = vec![
        Line::from(Span::styled(
            " Enter peer IP (e.g. 192.168.1.5 or :7778)",
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" > ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(input, Style::default().fg(Color::White)),
            Span::styled("█", Style::default().fg(ACCENT)),
        ]),
    ];

    let block = Block::default()
        .title(" Add Peer Manually ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(Color::Rgb(10, 15, 25)));

    f.render_widget(Paragraph::new(text).block(block), popup);
}

fn draw_manual_path_overlay(f: &mut Frame, app: &App, area: Rect) {
    let w = (area.width).min(70).max(52);
    let h = 7u16;
    let x = area.width.saturating_sub(w) / 2;
    let y = area.height.saturating_sub(h) / 2;
    let popup = Rect::new(x, y, w, h.min(area.height));
    f.render_widget(Clear, popup);

    let input = app.manual_path_input.as_deref().unwrap_or("");
    let inner_w = (w as usize).saturating_sub(5);
    let display_input = if input.len() > inner_w {
        &input[input.len() - inner_w..]
    } else {
        input
    };

    let text = vec![
        Line::from(Span::styled(
            " Enter file or folder path:",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            " Windows: C:\\Users\\Tim\\Downloads\\file.txt",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            " Unix:    /home/tim/downloads/file.txt",
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" > ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(display_input, Style::default().fg(Color::White)),
            Span::styled("█", Style::default().fg(ACCENT)),
        ]),
    ];

    let block = Block::default()
        .title(Span::styled(
            " 📂 Add Path Manually ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(Color::Rgb(10, 15, 25)));

    f.render_widget(Paragraph::new(text).block(block), popup);
}

fn draw_zip_confirm_overlay(f: &mut Frame, req: &ZipConfirmRequest, area: Rect) {
    use crate::shares::human_size;

    let w = 56u16;
    let h = 11u16;
    let x = area.width.saturating_sub(w) / 2;
    let y = area.height.saturating_sub(h) / 2;
    let popup = Rect::new(x, y, w.min(area.width), h.min(area.height));
    f.render_widget(Clear, popup);

    let size_str = human_size(req.total_size);
    let zip_hint = if req.would_zip {
        "Recommended: Yes (large folder)"
    } else {
        "Optional (small folder)"
    };

    let text = vec![
        Line::from(vec![
            Span::styled(" Folder: ", Style::default().fg(DIM)),
            Span::styled(
                &req.folder_name,
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(" Size:   ", Style::default().fg(DIM)),
            Span::styled(&size_str, Style::default().fg(Color::White)),
            Span::styled(format!("  ({} files)", req.file_count), Style::default().fg(DIM)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(" Zip before sharing?  ", Style::default().fg(DIM)),
            Span::styled(
                zip_hint,
                Style::default().fg(if req.would_zip { WARN } else { DIM }),
            ),
        ]),
        Line::from(Span::styled(
            " Zipping saves bandwidth but takes time for large folders.",
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  [y] Zip & share    [n] Share as-is    [Esc] Cancel",
            Style::default().fg(Color::White),
        )),
    ];

    let block = Block::default()
        .title(Span::styled(
            " 📁 Share Folder ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(Color::Rgb(10, 15, 25)));

    f.render_widget(Paragraph::new(text).block(block), popup);
}

/// Best-effort: find the first non-loopback IPv4 address on this machine.
/// Falls back to "127.0.0.1" so callers never get an empty string.
fn local_ipv4() -> String {
    // Walk every network interface, pick the first non-loopback IPv4 addr.
    // We use std::net::UdpSocket trick: connect to a public IP (no packet
    // is actually sent) and read back which local address the OS chose.
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                let ip = addr.ip().to_string();
                if ip != "0.0.0.0" {
                    return ip;
                }
            }
        }
    }
    "127.0.0.1".to_string()
}

fn draw_qr_overlay(f: &mut Frame, app: &App, area: Rect) {
    let url = format!("http://{}:{}/", local_ipv4(), app.config.port);

    // Generate QR matrix
    let code = match QrCode::new(url.as_bytes()) {
        Ok(c) => c,
        Err(_) => return,
    };

    let matrix = code.to_colors(); // Vec<qrcode::Color> row-major
    let qr_size = code.width();    // modules per side

    // Render using half-block chars: each char covers 2 vertical modules.
    // Upper module = top half (▀), lower = bottom half, both = █, neither = space.
    // We add a 1-module quiet zone padding on all sides.
    let pad = 1usize;
    let padded = qr_size + pad * 2;
    let char_rows = (padded + 1) / 2; // ceiling div — last row may be half

    // Build lines
    let mut lines: Vec<Line> = Vec::with_capacity(char_rows + 4);

    // Header
    lines.push(Line::from(Span::styled(
        " Scan to open web UI ",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        format!(" {}", url),
        Style::default().fg(Color::White),
    )));
    lines.push(Line::from(""));

    let is_dark = |row: isize, col: isize| -> bool {
        // out-of-bounds (padding) = light
        let r = row - pad as isize;
        let c = col - pad as isize;
        if r < 0 || c < 0 || r >= qr_size as isize || c >= qr_size as isize {
            return false;
        }
        matrix[r as usize * qr_size + c as usize] == qrcode::Color::Dark
    };

    for char_row in 0..char_rows {
        let top_mod = (char_row * 2) as isize;
        let bot_mod = top_mod + 1;

        let mut spans: Vec<Span> = vec![Span::raw(" ")]; // left margin
        for col in 0..padded {
            let c = col as isize;
            let top = is_dark(top_mod, c);
            let bot = is_dark(bot_mod, c);
            // Half-block trick: each terminal cell represents 2 vertical QR modules.
            // We use a white-on-black palette: dark module = white, light = black bg.
            // Top module → foreground, Bottom module → background (via lower-half block).
            let (fg, bg, ch) = match (top, bot) {
                (true,  true)  => (Color::White, Color::Black, "█"), // both dark
                (true,  false) => (Color::White, Color::Black, "▀"), // top dark, bottom light
                (false, true)  => (Color::Black, Color::White, "▀"), // top light, bottom dark (inverted)
                (false, false) => (Color::Black, Color::Black, " "), // both light
            };
            spans.push(Span::styled(ch, Style::default().fg(fg).bg(bg)));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " [r / Esc] close ",
        Style::default().fg(DIM),
    )));

    // Size the popup to the QR + chrome
    let popup_w = (padded as u16 + 3).min(area.width);  // +3 = margins
    let popup_h = (lines.len() as u16 + 2).min(area.height); // +2 = border
    let x = area.width.saturating_sub(popup_w) / 2;
    let y = area.height.saturating_sub(popup_h) / 2;
    let popup = Rect::new(x, y, popup_w, popup_h);

    f.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(
            " 📱 QR Code ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(Color::Rgb(10, 15, 25)));

    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(para, popup);
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}
