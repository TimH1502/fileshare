# fileshare

A fast, zero-config local network file sharing CLI. Share files and folders with anyone on your LAN — with a live TUI showing peers, their files, and your active shares.

## Features

- **Live TUI** — split screen: peers on the left, your shares on the right, activity log at the bottom
- **Auto-discovery** — peers appear automatically via UDP multicast (no manual IP needed)
- **Manual peer** — add a peer by IP if multicast is blocked (`m` key)
- **Drag & drop** — drag a file/folder into the terminal to share it instantly
- **Smart folder sharing** — folders with >20 files or >5 levels deep are auto-zipped for download
- **Browser access** — anyone can open `http://<your-ip>:7777` to download without the app
- **Download notifications** — see when someone downloads your file
- **Remove shares** — `x` removes from the share list, never touches the real file
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

| Key | Action |
|-----|--------|
| `Tab` / `Shift+Tab` | Switch panel |
| `↑↓` or `j/k` | Navigate |
| `Enter` / `d` | Download selected file |
| `x` / `Delete` | Remove your share (file untouched) |
| `m` | Add peer manually by IP |
| `?` | Toggle help overlay |
| `q` / `Ctrl+C` | Quit |

### Sharing files

**Drag & drop:** Drag a file or folder from your file manager into the terminal window. Most modern terminals (Windows Terminal, iTerm2, GNOME Terminal, etc.) will paste the path as text — fileshare detects this and registers the share.

**Type a path:** In the "My Shares" panel, type the full path to a file and press Enter.

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
- Port `7778` (UDP) — peer discovery (multicast `239.255.42.99`)

If multicast is blocked (some corporate WiFi, VMs), use `m` to add peers manually by IP.

## Architecture

```
src/
├── main.rs         Entry point, CLI, task orchestration
├── config.rs       Load/save ~/.config/fileshare/config.toml
├── shares.rs       In-memory share registry, zip heuristic
├── discovery.rs    UDP multicast announce + listen
├── server.rs       axum HTTP server (file serving + browser UI)
├── client.rs       HTTP download with progress
└── tui/
    ├── mod.rs      Terminal setup, event loop
    ├── app.rs      App state machine, key handling
    └── ui.rs       ratatui layout & widgets
```

## Security note

This is a **LAN-only tool** for trusted networks. The server binds to all interfaces (`0.0.0.0`) but is designed for local use. Do not use on public networks without a firewall.
