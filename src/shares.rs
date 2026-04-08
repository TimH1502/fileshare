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

// ---------------------------------------------------------------------------
// On-disk index
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default)]
struct ShareIndex {
    items: Vec<SharedItem>,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ShareRegistry {
    inner: Arc<RwLock<HashMap<String, SharedItem>>>,
    pub zip_cache_dir: PathBuf,
    index_path: PathBuf,
}

impl ShareRegistry {
    pub fn new(zip_cache_dir: PathBuf, index_path: PathBuf) -> Self {
        fs::create_dir_all(&zip_cache_dir).ok();
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            zip_cache_dir,
            index_path,
        }
    }

    // -----------------------------------------------------------------------
    // Index persistence
    // -----------------------------------------------------------------------

    /// Serialize all in-memory items to the index file atomically.
    fn save_index(&self) {
        let store = self.inner.read().unwrap();
        let index = ShareIndex {
            items: store.values().cloned().collect(),
        };
        drop(store);

        if let Some(parent) = self.index_path.parent() {
            fs::create_dir_all(parent).ok();
        }
        if let Ok(json) = serde_json::to_string_pretty(&index) {
            // Write to a temp file first then rename for atomicity
            let tmp = self.index_path.with_extension("tmp");
            if fs::write(&tmp, &json).is_ok() {
                fs::rename(&tmp, &self.index_path).ok();
            }
        }
    }

    /// Restore shares from the index on startup.
    /// - Validates that each path still exists on disk.
    /// - Drops expired / limit-reached entries.
    /// - Deletes orphaned zip files (zips in the cache dir not referenced by any index entry).
    /// Returns the number of shares successfully restored.
    pub fn restore_from_index(&self) -> usize {
        // Read index file
        let raw = match fs::read_to_string(&self.index_path) {
            Ok(s) => s,
            Err(_) => {
                // First run — no index yet. Still clean up any stray zips.
                self.prune_orphan_zips(&[]);
                return 0;
            }
        };
        let index: ShareIndex = match serde_json::from_str(&raw) {
            Ok(i) => i,
            Err(_) => {
                self.prune_orphan_zips(&[]);
                return 0;
            }
        };

        let mut store = self.inner.write().unwrap();
        let mut restored = 0usize;
        let mut valid_zip_paths: Vec<PathBuf> = Vec::new();

        for item in index.items {
            // Drop if time- or count-expired
            if item.is_expired() || item.is_limit_reached() {
                continue;
            }
            // Drop if the path on disk is gone
            if !item.path.exists() {
                continue;
            }
            if item.kind == ShareKind::ZippedFolder {
                valid_zip_paths.push(item.path.clone());
            }
            store.insert(item.id.clone(), item);
            restored += 1;
        }
        drop(store);

        // Remove any zip files not in the validated list
        self.prune_orphan_zips(&valid_zip_paths);

        restored
    }

    /// Delete `.zip` files inside the cache directory that are not listed in `valid_zips`.
    fn prune_orphan_zips(&self, valid_zips: &[PathBuf]) {
        let Ok(entries) = fs::read_dir(&self.zip_cache_dir) else {
            return;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("zip")
                && !valid_zips.contains(&path)
            {
                fs::remove_file(&path).ok();
            }
        }
    }

    // -----------------------------------------------------------------------
    // Adding shares
    // -----------------------------------------------------------------------

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
        drop(store);
        self.save_index();
        Ok(item)
    }

    fn add_file(
        &self,
        path: PathBuf,
        download_limit: Option<u32>,
        expires_in_mins: Option<u64>,
    ) -> Result<SharedItem> {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let size = fs::metadata(&path)?.len();
        let checksum = compute_checksum(&path)?;
        let expires_at =
            expires_in_mins.map(|m| Utc::now() + chrono::Duration::minutes(m as i64));

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

    fn add_folder(
        &self,
        path: PathBuf,
        download_limit: Option<u32>,
        expires_in_mins: Option<u64>,
        on_zipping: impl FnOnce(&str),
    ) -> Result<SharedItem> {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let expires_at =
            expires_in_mins.map(|m| Utc::now() + chrono::Duration::minutes(m as i64));

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

    /// Folder-only variant where the caller decides whether to zip.
    pub fn add_with_zip_choice(
        &self,
        path: PathBuf,
        download_limit: Option<u32>,
        expires_in_mins: Option<u64>,
        should_zip: bool,
        on_zipping: impl FnOnce(&str) + Send + 'static,
    ) -> Result<SharedItem> {
        let path = path.canonicalize()?;
        if path.is_file() {
            let item = self.add_file(path, download_limit, expires_in_mins)?;
            let mut store = self.inner.write().unwrap();
            store.insert(item.id.clone(), item.clone());
            drop(store);
            self.save_index();
            return Ok(item);
        }
        if !path.is_dir() {
            anyhow::bail!("Path is neither a file nor a directory: {:?}", path);
        }

        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let expires_at =
            expires_in_mins.map(|m| Utc::now() + chrono::Duration::minutes(m as i64));
        let (file_count, _, total_size) = analyse_folder_full(&path);

        let (final_path, kind, size, checksum) = if should_zip {
            on_zipping(&name);
            let zip_path = self.zip_cache_dir.join(format!("{}.zip", name));
            zip_folder(&path, &zip_path)?;
            let size = fs::metadata(&zip_path)?.len();
            let checksum = compute_checksum(&zip_path)?;
            (zip_path, ShareKind::ZippedFolder, size, checksum)
        } else {
            let checksum = format!("dir:{}", file_count);
            (path, ShareKind::Folder, total_size, checksum)
        };

        let item = SharedItem {
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
        };
        let mut store = self.inner.write().unwrap();
        store.insert(item.id.clone(), item.clone());
        drop(store);
        self.save_index();
        Ok(item)
    }

    // -----------------------------------------------------------------------
    // Removing shares
    // -----------------------------------------------------------------------

    /// Remove a share by id. If it was a ZippedFolder, delete the zip from cache.
    /// Returns the removed item so the caller can log its name.
    pub fn remove(&self, id: &str) -> Option<SharedItem> {
        let mut store = self.inner.write().unwrap();
        let item = store.remove(id);
        drop(store);

        if let Some(ref it) = item {
            if it.kind == ShareKind::ZippedFolder && it.path.exists() {
                fs::remove_file(&it.path).ok();
            }
            self.save_index();
        }
        item
    }

    // -----------------------------------------------------------------------
    // Queries & mutations
    // -----------------------------------------------------------------------

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
        drop(store);
        self.save_index();
    }

    pub fn prune_expired(&self) {
        let mut store = self.inner.write().unwrap();
        // Collect zips belonging to expired/limit-reached items so we can delete them
        let to_delete: Vec<PathBuf> = store
            .values()
            .filter(|v| v.is_expired() || v.is_limit_reached())
            .filter(|v| v.kind == ShareKind::ZippedFolder)
            .map(|v| v.path.clone())
            .collect();
        store.retain(|_, v| !v.is_expired() && !v.is_limit_reached());
        drop(store);

        for path in to_delete {
            if path.exists() {
                fs::remove_file(&path).ok();
            }
        }
        self.save_index();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn compute_checksum(path: &Path) -> Result<String> {
    let data = fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(hex::encode(hasher.finalize()))
}

pub fn analyse_folder(path: &Path) -> (usize, usize) {
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

/// Returns (file_count, max_depth, total_size_bytes)
pub fn analyse_folder_full(path: &Path) -> (usize, usize, u64) {
    let mut file_count = 0;
    let mut max_depth = 0;
    let mut total_size: u64 = 0;
    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            file_count += 1;
            if let Ok(meta) = entry.metadata() {
                total_size += meta.len();
            }
        }
        let depth = entry.depth();
        if depth > max_depth {
            max_depth = depth;
        }
    }
    (file_count, max_depth, total_size)
}

pub fn folder_size(path: &Path) -> u64 {
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
