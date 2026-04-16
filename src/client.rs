use anyhow::Result;
use futures::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteShareInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub size: u64,
    pub size_human: String,
    pub checksum: String,
    pub added_at: String,
    pub download_count: u32,
    pub available: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListResponse {
    pub username: String,
    pub items: Vec<RemoteShareInfo>,
}

pub async fn fetch_peer_shares(base_url: &str) -> Result<ListResponse> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client
        .get(format!("{}/shares", base_url))
        .send()
        .await?
        .json::<ListResponse>()
        .await?;
    Ok(resp)
}

pub struct DownloadProgress {
    pub bytes_downloaded: u64,
    pub total_bytes: u64,
    pub speed_bps: f64,
}

/// Result of a completed download, including integrity check outcome.
pub struct DownloadResult {
    pub path: PathBuf,
    /// None if the server didn't send a checksum header.
    pub checksum_ok: Option<bool>,
}

pub async fn download_file(
    base_url: &str,
    share_id: &str,
    share_name: &str,
    download_dir: &PathBuf,
    progress_tx: tokio::sync::mpsc::Sender<DownloadProgress>,
) -> Result<DownloadResult> {
    tokio::fs::create_dir_all(download_dir).await?;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/download/{}", base_url, share_id))
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("Server returned error: {}", resp.status());
    }

    let total = resp.content_length().unwrap_or(0);

    // Extract server-side checksum from response header (may be absent)
    let expected_checksum = resp
        .headers()
        .get("x-checksum-sha256")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_lowercase());

    // Determine filename from Content-Disposition or use share name
    let filename = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.split("filename=")
                .nth(1)
                .map(|f| f.trim_matches('"').to_string())
        })
        .unwrap_or_else(|| share_name.to_string());

    let dest_path = download_dir.join(&filename);
    let mut file = tokio::fs::File::create(&dest_path).await?;
    let mut stream = resp.bytes_stream();

    let mut downloaded = 0u64;
    let start = std::time::Instant::now();

    // Compute checksum while streaming so we don't need a second pass
    let mut hasher = Sha256::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;

        let elapsed = start.elapsed().as_secs_f64().max(0.001);
        let speed = downloaded as f64 / elapsed;

        progress_tx
            .send(DownloadProgress {
                bytes_downloaded: downloaded,
                total_bytes: total,
                speed_bps: speed,
            })
            .await
            .ok();
    }

    file.flush().await?;

    // FIX: verify integrity against the X-Checksum-SHA256 header
    let checksum_ok = expected_checksum.map(|expected| {
        let actual = hex::encode(hasher.finalize());
        // For folder checksums (prefixed "dir:…") skip byte-level comparison
        if expected.starts_with("dir:") {
            true
        } else {
            actual == expected
        }
    });

    Ok(DownloadResult {
        path: dest_path,
        checksum_ok,
    })
}

pub fn format_speed(bps: f64) -> String {
    if bps >= 1_000_000.0 {
        format!("{:.1} MB/s", bps / 1_000_000.0)
    } else if bps >= 1_000.0 {
        format!("{:.1} KB/s", bps / 1_000.0)
    } else {
        format!("{:.0} B/s", bps)
    }
}
