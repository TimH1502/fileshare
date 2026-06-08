# fileshare

A fast, zero-config local network file sharing CLI. Share files and folders with anyone on your LAN — with a live TUI showing peers, their files, and your active shares. Also accessible from any browser on the network with full upload, download, and delete support.

## Features

- **Live TUI** — split screen: peers on the left, your shares on the right, transfers and activity log at the bottom
- **Auto-discovery** — peers appear automatically via mDNS (no manual IP needed)
- **Manual peer** — add a peer by IP if mDNS is blocked (`m` in the Peers panel), remove with `x`
- **Drag & drop** — drag a file or folder into the terminal to share it instantly (Windows, Linux, macOS)
- **Manual path entry** — press `m` in the My Shares panel to type a path directly
- **Folder zip dialog** — when sharing a folder, a popup shows size and file count and asks whether to zip; zipping saves bandwidth for large folders
- **Parallel zipping** — folders are compressed using all CPU cores (rayon + zlib-ng), reaching speeds comparable to 7-Zip; a live progress bar in the log updates in-place without spamming new lines
- **HTTPS browser UI** — open `https://<your-ip>:7777` from any device on the network; plain HTTP on port 7778 redirects automatically to HTTPS
- **Web upload** — drag files onto the browser UI or use the file picker to upload directly to the sharing host
- **Web delete** — remove a share from the browser UI with the Delete button (file on disk is untouched)
- **Live auto-refresh** — the browser UI polls every 4 seconds and updates the file list without a page reload; a live indicator shows connection status
- **Transfer panel** — active uploads (⬆ orange → green when done) and downloads (⬇ magenta → green when done) shown simultaneously with speed and progress bar; entries fade after 5 seconds
- **QR code overlay** — press `r` in the TUI to show a QR code for the browser URL; scan with a phone to open instantly
- **Download notifications** — the TUI activity log shows when someone downloads or uploads a file
- **Remove shares** — `x` removes a share from the list (the real file is never touched)
- **SHA256 checksums** — verified automatically on download; shown per file
- **Session persistence** — shares are restored from an index file on restart
- **Optional expiry & download limits** — via CLI flags in `send` mode
- **Self-signed TLS** — cert generated on first run, stored in config dir, reused on subsequent runs; browser shows a one-time warning
- **Single binary** — no runtime dependencies, works on Windows, Linux, macOS

## Build

```bash
# Prerequisites: Rust (https://rustup.rs)
cargo build --release

# Binary will be at:
# Linux/macOS: ./target/release/fileshare
# Windows:     ./target/release/fileshare.exe
```

## Usage

### Interactive TUI (recommended)

```bash
./fileshare
```

On first launch you'll be asked for a display name (saved to config).

### Non-interactive send

```bash
# Share a single file
./fileshare send ./photo.jpg

# Share with download limit
./fileshare send ./report.pdf --limit 3

# Share folder, expires in 30 minutes
./fileshare send ./project/ --expires 30
```

When sharing a folder via `send`, zipping progress is printed to stderr and refreshes in-place:

```
Zipping 'project' ... 2341/5700 files (41%)
```

### Keyboard shortcuts (TUI)

The status bar at the bottom always shows the shortcuts available in the currently focused panel. Full reference:

| Key | Panel | Action |
|-----|-------|--------|
| `Tab` / `Shift+Tab` | Any | Switch panel |
| `↑↓` or `j/k` | Any | Navigate list |
| `Enter` | Peers | Browse peer's files |
| `m` | Peers | Add peer manually by IP |
| `x` / `Delete` | Peers | Remove a manually added peer |
| `Enter` / `d` | Peer Files | Download selected file |
| `←` | Peer Files | Go back to Peers |
| `m` | My Shares | Enter a file/folder path manually |
| `x` / `Delete` | My Shares | Remove share (file untouched) |
| `r` | Any | Toggle QR code overlay for browser URL |
| `u` | Any | Toggle download speed unit: MB/s ↔ Mb/s |
| `?` | Any | Toggle help overlay |
| `q` / `Ctrl+C` | Any | Quit |

> `x` is context-aware: in the Peers panel it removes a manual peer; in the My Shares panel it removes a share.  
> `m` is context-aware: in the Peers panel it opens the IP entry dialog; in the My Shares panel it opens the path entry dialog.

### Sharing files

**Drag & drop:** Drag a file or folder from your file manager into the terminal window. fileshare detects the dropped path and registers the share automatically. Works with paths on any drive (C:, D:, etc. on Windows).

**Manual path entry:** Tab to the **My Shares** panel and press `m`. A dialog opens where you can type the full path and press `Enter`:

```
Windows:  C:\Users\Tim\Downloads\report.pdf
          D:\Projects\myapp\
Unix:     /home/tim/downloads/report.pdf
```

**Folder zip dialog:** Whenever a folder is added, a popup appears and asks whether to zip it before sharing:

```
╔══════ 📁 Share Folder ══════════════════════════════╗
║ Folder: my_project                                   ║
║ Size:   142.3 MB  (847 files)                        ║
║                                                      ║
║ Zip before sharing?  Recommended: Yes (large folder) ║
║ Zipping saves bandwidth but takes time for large     ║
║ folders.                                             ║
║                                                      ║
║  [y] Zip & share    [n] Share as-is    [Esc] Cancel  ║
╚══════════════════════════════════════════════════════╝
```

Zipping is recommended for large or deeply nested folders. The zip is created in a local cache directory and the original folder is never modified. Compression runs in parallel across all CPU cores, so even large folders (1 GB+, 5000+ files) finish in seconds.

While zipping, the log panel shows a live progress bar that refreshes in-place:

```
📦 Zipping 'my_project' [████████░░░░░░░░░░░░] 423/847 files (50%)
```

Zipped folders appear in your shares list with a 📦 icon.

### Browser UI

Open `https://<your-ip>:7777` from any browser on the network. On the first visit your browser will show a security warning about the self-signed certificate — click **Advanced → Proceed** (or equivalent). The browser remembers the exception for 10 years.

**From the browser you can:**
- Download any shared file with one click
- Upload files by dragging them onto the upload zone or using the file picker — per-file progress bars show upload speed; the file list updates automatically on completion
- Delete a share with the **✕ Delete** button (removes from the share list only; file on disk is untouched)

### QR code

Press `r` in the TUI to open a QR code overlay showing the HTTPS URL for the browser UI. Scan it with a phone to open the page without typing the IP address. Press `r` or `Esc` to close.

### Managing peers

Peers on the same network are discovered automatically. If auto-discovery doesn't work (corporate Wi-Fi, VMs, VPNs), add a peer manually:

1. Press `Tab` until the **Peers** panel is focused
2. Press `m` and enter the peer's IP address (e.g. `192.168.1.42` or `192.168.1.42:7777`)
3. Press `Enter`

Manually added peers are marked with `[m]` in the list. To remove one, select it and press `x`.

### Config

Saved at:
- Linux: `~/.config/fileshare/config.toml`
- macOS: `~/Library/Application Support/fileshare/config.toml`
- Windows: `%APPDATA%\fileshare\config.toml`

```toml
username = "alice"
port = 7777
download_dir = "/home/alice/Downloads"
```

TLS cert and key are stored alongside the config (`cert.pem`, `key.pem`) and reused on every startup.

A debug log is written to `debug.log` in the same directory during startup — useful for diagnosing path or config issues.

Reset config with:
```bash
./fileshare reset
```

## Network requirements

| Port | Protocol | Purpose |
|------|----------|---------| 
| `7777` | TCP | HTTPS file server and browser UI |
| `7778` | TCP | HTTP redirect to HTTPS |
| `5353` | UDP | mDNS peer discovery (multicast `224.0.0.251`) |

If multicast is blocked, use `m` in the Peers panel to add peers manually by IP.

## Architecture

```
src/
├── main.rs         Entry point, CLI, task orchestration
├── config.rs       Load/save config.toml, debug logging
├── tls.rs          Self-signed cert generation and loading (rcgen + rustls)
├── shares.rs       In-memory share registry, folder analysis, parallel zip
├── discovery.rs    mDNS announce + listen
├── server.rs       axum HTTPS server — file serving, upload, delete, browser UI
├── client.rs       HTTPS download with progress reporting and SHA256 verification
└── tui/
    ├── mod.rs      Terminal setup, event loop, drag-and-drop path detection
    ├── app.rs      App state machine, key handling, dialog state, transfer tracking
    └── ui.rs       ratatui layout, widgets, overlays (QR code, help, zip confirm, transfers)
```

## Security note

This tool is designed for **trusted local networks**. The HTTPS server binds to all interfaces (`0.0.0.0`). The self-signed certificate is accepted by the CLI client unconditionally (both ends are yours), and browsers show a one-time warning on first visit. Do not expose port 7777 to the public internet.
