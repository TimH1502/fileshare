# fileshare

A fast, zero-config local network file sharing CLI. Share files and folders with anyone on your LAN — with a live TUI showing peers, their files, and your active shares.

## Features

- **Live TUI** — split screen: peers on the left, your shares on the right, activity log at the bottom
- **Auto-discovery** — peers appear automatically via mDNS (no manual IP needed)
- **Manual peer** — add a peer by IP if mDNS is blocked (`m` in the Peers panel), remove manual peers with `x`
- **Drag & drop** — drag a file or folder into the terminal to share it instantly
- **Manual path entry** — press `m` in the My Shares panel to type a path directly (Windows and Unix paths both supported)
- **Zip confirmation dialog** — when sharing a folder, a popup shows the folder size and file count and asks whether to zip before sharing; zipping is recommended for large folders to save bandwidth
- **Browser access** — anyone can open `http://<your-ip>:7777` to download without the app
- **Download notifications** — see when someone downloads your file
- **Remove shares** — `x` removes a share from the list (the real file is never touched)
- **Context-aware UI** — the status bar shows only the shortcuts relevant to the currently focused panel
- **Optional expiry & download limits** — via CLI flags in `send` mode
- **SHA256 checksums** — shown per file for integrity verification
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
| `?` | Any | Toggle help overlay |
| `q` / `Ctrl+C` | Any | Quit |

> `x` is context-aware: in the Peers panel it removes a manual peer; in the My Shares panel it removes a share.  
> `m` is context-aware: in the Peers panel it opens the IP entry dialog; in the My Shares panel it opens the path entry dialog.

### Sharing files

**Drag & drop:** Drag a file or folder from your file manager into the terminal window. Most modern terminals (Windows Terminal, iTerm2, GNOME Terminal, etc.) will paste the path as text — fileshare detects this and registers the share automatically.

**Manual path entry:** Tab to the **My Shares** panel and press `m`. A dialog opens where you can type the full path to a file or folder and press `Enter`. Both Windows and Unix paths are accepted:

```
Windows:  C:\Users\Tim\Downloads\report.pdf
Unix:     /home/tim/downloads/report.pdf
```

**Folder zip dialog:** Whenever a folder is added (by drag & drop or manual path), a popup appears showing the folder's total size and file count and asks whether to zip it before sharing:

```
╔══════ 📁 Share Folder ══════════════╗
║ Folder: my_project                  ║
║ Size:   142.3 MB  (847 files)       ║
║                                     ║
║ Zip before sharing?                 ║
║ Recommended: Yes (large folder)     ║
║                                     ║
║  [y] Zip & share  [n] Share as-is  ║
║  [Esc] Cancel                       ║
╚═════════════════════════════════════╝
```

Zipping is recommended for large or deeply nested folders because it significantly reduces transfer time. The zip is created in a local cache directory and the original folder is never modified.

### Managing peers

Peers on the same network are discovered automatically. If auto-discovery doesn't work (corporate Wi-Fi, VMs, VPNs), add a peer manually:

1. Press `Tab` until the **Peers** panel is focused
2. Press `m` and enter the peer's IP address (e.g. `192.168.1.42` or `192.168.1.42:7778`)
3. Press `Enter`

Manually added peers are marked with a `[m]` tag in the list. To remove one, select it and press `x`.

### Config

Saved at:
- Linux/macOS: `~/.config/fileshare/config.toml`
- Windows: `%APPDATA%\fileshare\config.toml`

```toml
username = "alice"
port = 7777
download_dir = "/home/alice/Downloads/fileshare"
```

Reset with:
```bash
./fileshare reset
```

## Network requirements

- Port `7777` (TCP) — file serving & browser UI  
- Port `7778` (UDP) — peer discovery (mDNS multicast `224.0.0.251`)

If multicast is blocked, use `m` in the Peers panel to add peers manually by IP.

## Architecture

```
src/
├── main.rs         Entry point, CLI, task orchestration
├── config.rs       Load/save config.toml
├── shares.rs       In-memory share registry, folder analysis, zip logic
├── discovery.rs    mDNS announce + listen
├── server.rs       axum HTTP server (file serving + browser UI)
├── client.rs       HTTP download with progress reporting
└── tui/
    ├── mod.rs      Terminal setup, event loop, drag-and-drop path detection
    ├── app.rs      App state machine, key handling, dialog state
    └── ui.rs       ratatui layout, widgets, and overlays
```

## Security note

This is a **LAN-only tool** for trusted networks. The server binds to all interfaces (`0.0.0.0`) but is designed for local use. Do not use on public networks without a firewall.
