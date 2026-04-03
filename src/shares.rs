use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use uuid::Uuid;
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ShareKind {
    File,
    Folder,
    ZippedFolder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedItem {
    pub id: String,
    pub name: String,
    pub kind: ShareKind,
    pub size: u64,
    pub path: PathBuf,
    pub checksum: String,
    pub added_at: DateTime<Utc>,
    pub download_count: u32,
    pub download_limit: Option<u32>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl SharedItem {
    pub fn is_expired(&self) -> bool {
        if let Some(expires) = self.expires_at {
            Utc::now() > expires
        } else {
            false
        }
    }

    pub fn is_limit_reached(&self) -> bool {
        if let Some(limit) = self.download_limit {
            self.download_count >= limit
        } else {
            false
        }
    }

    pub fn is_available(&self) -> bool {
        !self.is_expired() && !self.is_limit_reached()
    }

    pub fn size_human(&self) -> String {
        human_size(self.size)
    }
}

pub fn human_size(size: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut s = size as f64;
    let mut unit = 0;
    while s >= 1024.0 && unit < UNITS.len() - 1 {
        s /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", size)
    } else {
        format!("{:.1} {}", s, UNITS[unit])
    }
}

#[derive(Clone)]
pub struct ShareRegistry {
    inner: Arc<RwLock<HashMap<String, SharedItem>>>,
    pub zip_cache_dir: PathBuf,
}

impl ShareRegistry {
    pub fn new(zip_cache_dir: PathBuf) -> Self {
        fs::create_dir_all(&zip_cache_dir).ok();
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            zip_cache_dir,
        }
    }

    pub fn add(
        &self,
        path: PathBuf,
        download_limit: Option<u32>,
        expires_in_mins: Option<u64>,
        on_zipping: impl FnOnce(&str) + Send + 'static,
    ) -> Result<SharedItem> {
        let path = path.canonicalize()?;
        let item = if path.is_file() {
            self.add_file(path, download_limit, expires_in_mins)?
        } else if path.is_dir() {
            self.add_folder(path, download_limit, expires_in_mins, on_zipping)?
        } else {
            anyhow::bail!("Path is neither a file nor a directory: {:?}", path);
        };

        let mut store = self.inner.write().unwrap();
        store.insert(item.id.clone(), item.clone());
        Ok(item)
    }

    fn add_file(&self, path: PathBuf, download_limit: Option<u32>, expires_in_mins: Option<u64>) -> Result<SharedItem> {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let size = fs::metadata(&path)?.len();
        let checksum = compute_checksum(&path)?;
        let expires_at = expires_in_mins.map(|m| Utc::now() + chrono::Duration::minutes(m as i64));

        Ok(SharedItem {
            id: Uuid::new_v4().to_string()[..8].to_string(),
            name,
            kind: ShareKind::File,
            size,
            path,
            checksum,
            added_at: Utc::now(),
            download_count: 0,
            download_limit,
            expires_at,
        })
    }

    fn add_folder(&self, path: PathBuf, download_limit: Option<u32>, expires_in_mins: Option<u64>, on_zipping: impl FnOnce(&str)) -> Result<SharedItem> {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let expires_at = expires_in_mins.map(|m| Utc::now() + chrono::Duration::minutes(m as i64));

        // Heuristic: zip if > 20 files or > 5 levels deep
        let (file_count, max_depth) = analyse_folder(&path);
        let should_zip = file_count > 20 || max_depth > 5;

        let (final_path, kind, size, checksum) = if should_zip {
            on_zipping(&name);
            let zip_path = self.zip_cache_dir.join(format!("{}.zip", name));
            zip_folder(&path, &zip_path)?;
            let size = fs::metadata(&zip_path)?.len();
            let checksum = compute_checksum(&zip_path)?;
            (zip_path, ShareKind::ZippedFolder, size, checksum)
        } else {
            let size = folder_size(&path);
            let checksum = format!("dir:{}", file_count);
            (path, ShareKind::Folder, size, checksum)
        };

        Ok(SharedItem {
            id: Uuid::new_v4().to_string()[..8].to_string(),
            name,
            kind,
            size,
            path: final_path,
            checksum,
            added_at: Utc::now(),
            download_count: 0,
            download_limit,
            expires_at,
        })
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut store = self.inner.write().unwrap();
        store.remove(id).is_some()
    }

    pub fn get(&self, id: &str) -> Option<SharedItem> {
        let store = self.inner.read().unwrap();
        store.get(id).cloned()
    }

    pub fn list(&self) -> Vec<SharedItem> {
        let store = self.inner.read().unwrap();
        let mut items: Vec<_> = store.values().cloned().collect();
        items.sort_by(|a, b| b.added_at.cmp(&a.added_at));
        items
    }

    pub fn list_available(&self) -> Vec<SharedItem> {
        self.list().into_iter().filter(|i| i.is_available()).collect()
    }

    pub fn increment_downloads(&self, id: &str) {
        let mut store = self.inner.write().unwrap();
        if let Some(item) = store.get_mut(id) {
            item.download_count += 1;
        }
    }

    pub fn prune_expired(&self) {
        let mut store = self.inner.write().unwrap();
        store.retain(|_, v| !v.is_expired() && !v.is_limit_reached());
    }
}

fn compute_checksum(path: &Path) -> Result<String> {
    let data = fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(hex::encode(hasher.finalize()))
}

fn analyse_folder(path: &Path) -> (usize, usize) {
    let mut file_count = 0;
    let mut max_depth = 0;
    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            file_count += 1;
        }
        let depth = entry.depth();
        if depth > max_depth {
            max_depth = depth;
        }
    }
    (file_count, max_depth)
}

fn folder_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn zip_folder(src: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::create(dest)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    let base = src.parent().unwrap_or(src);
    for entry in WalkDir::new(src).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        let rel = path.strip_prefix(base)?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            zip.add_directory(&rel_str, options)?;
        } else {
            zip.start_file(&rel_str, options)?;
            let mut f = fs::File::open(path)?;
            std::io::copy(&mut f, &mut zip)?;
        }
    }
    zip.finish()?;
    Ok(())
}
