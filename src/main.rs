mod client;
mod config;
mod discovery;
mod server;
mod shares;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;

use config::Config;
use discovery::PeerRegistry;
use server::{AppState, ServerEvent};
use shares::ShareRegistry;

#[derive(Parser)]
#[command(name = "fileshare", about = "Local network file sharing", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Your display name (overrides saved config)
    #[arg(short, long)]
    username: Option<String>,

    /// Port to listen on (default: 7777)
    #[arg(short, long)]
    port: Option<u16>,
}

#[derive(Subcommand)]
enum Commands {
    /// Share a file or folder immediately (non-interactive)
    Send {
        /// Path to share
        path: PathBuf,
        /// Limit downloads
        #[arg(short, long)]
        limit: Option<u32>,
        /// Expire after N minutes
        #[arg(short, long)]
        expires: Option<u64>,
    },
    /// Clear saved config
    Reset,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Reset) => {
            let path = Config::config_path();
            if path.exists() {
                std::fs::remove_file(&path)?;
                println!("Config cleared: {}", path.display());
            } else {
                println!("No config found.");
            }
            return Ok(());
        }
        Some(Commands::Send { path, limit, expires }) => {
            return run_send(cli.username, cli.port, path, limit, expires).await;
        }
        None => {}
    }

    run_tui(cli.username, cli.port).await
}

async fn setup(username_override: Option<String>, port_override: Option<u16>) -> Result<Config> {
    let mut config = Config::load().unwrap_or_default();

    if let Some(u) = username_override {
        config.username = u;
    }
    if let Some(p) = port_override {
        config.port = p;
    }

    if config.username.is_empty() {
        config.username = prompt_username()?;
        config.save().ok();
    }

    Ok(config)
}

fn prompt_username() -> Result<String> {
    println!("\n  📡  fileshare — local network file sharing\n");
    print!("  Enter your display name: ");
    use std::io::Write;
    std::io::stdout().flush()?;
    let mut name = String::new();
    std::io::stdin().read_line(&mut name)?;
    let name = name.trim().to_string();
    if name.is_empty() {
        anyhow::bail!("Username cannot be empty");
    }
    println!("  Hello, {}! Starting…\n", name);
    Ok(name)
}

async fn run_tui(username_override: Option<String>, port_override: Option<u16>) -> Result<()> {
    let config = setup(username_override, port_override).await?;

    let cache_dir = config
        .download_dir
        .parent()
        .unwrap_or(&config.download_dir)
        .join(".fileshare_zip_cache");

    let shares = ShareRegistry::new(cache_dir);
    let peers = PeerRegistry::new();

    let (event_tx, event_rx) = broadcast::channel::<ServerEvent>(64);

    let state = Arc::new(AppState {
        shares: shares.clone(),
        username: config.username.clone(),
        event_tx: event_tx.clone(),
    });

    // Start HTTP server
    let addr: SocketAddr = format!("0.0.0.0:{}", config.port).parse()?;
    let router = server::build_router_with_connect_info(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    // Start UDP discovery announcer
    {
        let username = config.username.clone();
        let port = config.port;
        let shares_clone = shares.clone();
        tokio::spawn(async move {
            discovery::run_announcer(username, port, shares_clone)
                .await
                .ok();
        });
    }

    // Start UDP discovery listener
    {
        let peers_clone = peers.clone();
        let port = config.port;
        tokio::spawn(async move {
            discovery::run_listener(peers_clone, port).await.ok();
        });
    }

    // Run TUI
    tui::run(config, peers, shares, event_rx).await?;

    Ok(())
}

async fn run_send(
    username_override: Option<String>,
    port_override: Option<u16>,
    path: PathBuf,
    limit: Option<u32>,
    expires: Option<u64>,
) -> Result<()> {
    let config = setup(username_override, port_override).await?;

    let cache_dir = config.download_dir.parent().unwrap_or(&config.download_dir).join(".fileshare_zip_cache");
    let shares = ShareRegistry::new(cache_dir);
    let item = shares.add(path, limit, expires, |name| {
            println!("Zipping '{}' — this may take a moment…", name);
        })?;
    println!(
        "Sharing '{}' ({}) — ID: {}",
        item.name,
        item.size_human(),
        item.id
    );
    println!(
        "Browse at: http://0.0.0.0:{}/",
        config.port
    );
    println!("SHA256: {}", item.checksum);
    println!("Press Ctrl+C to stop sharing.");

    let (event_tx, _) = broadcast::channel::<ServerEvent>(64);
    let state = Arc::new(AppState {
        shares,
        username: config.username.clone(),
        event_tx,
    });

    let addr: SocketAddr = format!("0.0.0.0:{}", config.port).parse()?;
    let router = server::build_router_with_connect_info(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}
