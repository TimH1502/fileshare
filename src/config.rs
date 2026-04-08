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

impl Default for Config {
    fn default() -> Self {
        // Try system download folder first
        let mut download_dir = dirs::download_dir().unwrap_or_else(|| {
            // fallback: current directory + "downloads"
            let mut cwd = std::env::current_dir().expect("Cannot determine current directory");
            cwd.push("downloads");
            cwd
        });

        // Use a subfolder for your app
        download_dir.push("fileshare");

        // Make sure it exists
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

    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if path.exists() {
            let content = fs::read_to_string(&path)?;
            let config: Self = toml::from_str(&content)?;
            Ok(config)
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        fs::write(&path, content)?;
        Ok(())
    }
}
