use anyhow::Result;
use dirs::config_dir;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub username: String,
    pub port: u16,
    pub download_dir: PathBuf,
}

/// Append a timestamped line to ~/.config/fileshare/debug.log.
/// Safe to call at any point — silently ignores write errors.
#[cfg(debug_assertions)]
pub fn debug_log(msg: &str) {
    let log_path = config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("fileshare")
        .join("debug.log");
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).ok();
    }
    use std::io::Write;
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        writeln!(f, "[{now}] {msg}").ok();
    }
}
#[cfg(not(debug_assertions))]
pub fn debug_log(_msg: &str) {
    // no-op in release
}

impl Default for Config {
    fn default() -> Self {
        let download_dir = Self::default_download_dir();
        debug_log(&format!("Default::default() → download_dir = {:?}", download_dir));
        if let Err(e) = std::fs::create_dir_all(&download_dir) {
            panic!("Failed to create download directory {:?}: {}", download_dir, e);
        }
        Self {
            username: String::new(),
            port: 7777,
            download_dir,
        }
    }
}

impl Config {
    pub fn config_path() -> PathBuf {
        config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("fileshare")
            .join("config.toml")
    }

    /// Returns the canonical default download directory for this platform.
    /// Downloads/fileshare on Windows/Mac/Linux desktop, ./downloads/fileshare as fallback.
    pub fn default_download_dir() -> PathBuf {
        let dirs_result = dirs::download_dir();
        debug_log(&format!("dirs::download_dir() = {:?}", dirs_result));
        let dir = dirs_result.unwrap_or_else(|| {
            let cwd = std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."));
            debug_log(&format!("download_dir fallback → cwd = {:?}", cwd));
            let mut p = cwd;
            p.push("downloads");
            p
        });
        // dir.push("fileshare");
        debug_log(&format!("default_download_dir() = {:?}", dir));
        dir
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        debug_log(&format!("Config::load() — config path = {:?}", path));

        if !path.exists() {
            debug_log("config.toml not found, using Default");
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&path)?;
        let mut config: Self = toml::from_str(&content)?;
        debug_log(&format!("Loaded config, download_dir = {:?}", config.download_dir));

        // Validate the saved download_dir: if it doesn't exist and can't be created,
        // or if it is a relative path, reset to the platform default.
        let dir_ok = fs::create_dir_all(&config.download_dir).is_ok()
            && config.download_dir.is_absolute();

        debug_log(&format!("download_dir valid = {}", dir_ok));

        if !dir_ok {
            let new_dir = Self::default_download_dir();
            debug_log(&format!("Resetting download_dir to {:?}", new_dir));
            config.download_dir = new_dir;
            fs::create_dir_all(&config.download_dir).ok();
            config.save().ok();
        }

        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        fs::write(&path, content)?;
        debug_log(&format!("Config saved, download_dir = {:?}", self.download_dir));
        Ok(())
    }
}
