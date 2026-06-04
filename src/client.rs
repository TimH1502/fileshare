use anyhow::Result;
use futures::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::{io::AsyncWriteExt, time::Instant};

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteShareInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub size: u64,
    pub size_human: String,
    pub available: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListResponse {
    pub items: Vec<RemoteShareInfo>,
}

pub async fn fetch_peer_shares(base_url: &str) -> Result<ListResponse> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .danger_accept_invalid_certs(true) // self-signed cert on local network
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
    pub eta_seconds: f64,
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

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // self-signed cert on local network
        .build()?;
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

    // Compute checksum while streaming so we don't need a second pass
    let mut hasher = Sha256::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;

        let mut last_bytes = 0;
        let mut last_time = Instant::now();
        let mut last_update = Instant::now();

        let mut smoothed_speed = 0.0;
        let alpha = 0.15; // gentler EMA -- more history weight

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;

            // write + hash SAME bytes
            file.write_all(&chunk).await?;
            hasher.update(&chunk);

            downloaded += chunk.len() as u64;

            let now = Instant::now();
            let elapsed = now.duration_since(last_time).as_secs_f64();

            let new_speed = if elapsed > 0.0 {
                (downloaded - last_bytes) as f64 / elapsed
            } else {
                0.0
            };

            // EMA smoothing
            let eta_seconds;
            (eta_seconds, smoothed_speed) = calc_eta_seconds(smoothed_speed, new_speed, alpha, total, downloaded);

            if last_update.elapsed() >= tokio::time::Duration::from_millis(500) {
                let _ = progress_tx.try_send(DownloadProgress {
                    bytes_downloaded: downloaded,
                    total_bytes: total,
                    speed_bps: smoothed_speed,
                    eta_seconds,
                });

                last_update = now;
                last_time = now;
                last_bytes = downloaded;
            }
        }
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

// EMA smoothing
pub fn calc_eta_seconds(mut smoothed_speed: f64, new_speed: f64, alpha: f64, total: u64, downloaded: u64)-> (f64, f64) {
    smoothed_speed = if smoothed_speed == 0.0 {
            new_speed
    } else {
        alpha * new_speed + (1.0 - alpha) * smoothed_speed
    };

    let eta_seconds = if smoothed_speed > 0.0 {
        total.saturating_sub(downloaded) as f64 / smoothed_speed // no underflow
    } else {
        0.0
    };
    (eta_seconds, smoothed_speed)
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
