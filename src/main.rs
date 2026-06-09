mod client;
mod config;
mod discovery;
mod server;
mod shares;
mod tls;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::{env, net::SocketAddr};
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
    env::set_var("RUST_BACKTRACE", "1");
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
        Some(Commands::Send {
            path,
            limit,
            expires,
        }) => {
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

    crate::config::debug_log(&format!(
        "run_tui: config.download_dir = {:?}",
        config.download_dir
    ));

    let cache_dir = config
        .download_dir
        .parent()
        .unwrap_or(&config.download_dir)
        .join(".fileshare_zip_cache");

    crate::config::debug_log(&format!("run_tui: cache_dir = {:?}", cache_dir));

    let index_path = Config::config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("shares.index.json");

    crate::config::debug_log(&format!("run_tui: index_path = {:?}", index_path));

    let shares = ShareRegistry::new(cache_dir, index_path);
    let restored = shares.restore_from_index();

    let peers = PeerRegistry::new();

    let (event_tx, event_rx) = broadcast::channel::<ServerEvent>(512);

    let state = Arc::new(AppState {
        shares: shares.clone(),
        username: config.username.clone(),
        event_tx: event_tx.clone(),
        download_dir: config.download_dir.clone(),
    });

    // Start HTTPS server
    let addr: SocketAddr = format!("0.0.0.0:{}", config.port).parse()?;
    let router = server::build_router_with_connect_info(state);
    let tls_config = tls::load_or_generate().await?;

    tokio::spawn(async move {
        axum_server::bind_rustls(addr, tls_config)
            .serve(router)
            .await
            .ok();
    });

    // Start mDNS
    {
        let username = config.username.clone();
        let port = config.port;
        let shares_clone = shares.clone();
        let peers_clone = peers.clone();

        tokio::spawn(async move {
            discovery::run_mdns(username, port, shares_clone, peers_clone)
                .await
                .ok();
        });
    }

    // Background index flush task (debounces download counter writes)
    {
        let shares_clone = shares.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                shares_clone.flush_if_dirty();
            }
        });
    }

    // Background expiry pruner
    {
        let shares_clone = shares.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                shares_clone.prune_expired();
            }
        });
    }

    tui::run(config, peers, shares, event_rx, restored).await?;

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

    let cache_dir = config
        .download_dir
        .parent()
        .unwrap_or(&config.download_dir)
        .join(".fileshare_zip_cache");
    let index_path = Config::config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("shares.index.json");
    let shares = ShareRegistry::new(cache_dir, index_path);
    let item = shares.add(path, limit, expires, |name, done, total| {
        let pct = (done * 100).checked_div(total).unwrap_or(0);
        eprint!(
            "\rZipping '{}' ... {}/{} files ({}%)   ",
            name, done, total, pct
        );
    })?;
    eprintln!();
    println!(
        "Sharing '{}' ({}) — ID: {}",
        item.name,
        item.size_human(),
        item.id
    );
    println!("Browse at: https://0.0.0.0:{}/", config.port);
    println!("SHA256: {}", item.checksum);
    println!("Press Ctrl+C to stop sharing.");

    let (event_tx, _) = broadcast::channel::<ServerEvent>(64);
    let state = Arc::new(AppState {
        shares,
        username: config.username.clone(),
        event_tx,
        download_dir: config.download_dir.clone(),
    });

    let addr: SocketAddr = format!("0.0.0.0:{}", config.port).parse()?;
    let router = server::build_router_with_connect_info(state);
    let tls_config = tls::load_or_generate().await?;
    axum_server::bind_rustls(addr, tls_config)
        .serve(router)
        .await?;
    Ok(())
}
