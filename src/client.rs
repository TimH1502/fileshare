use anyhow::Result;
use futures::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;

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
        .danger_accept_invalid_certs(true)
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

/// Maximum number of resume attempts before giving up.
const MAX_RETRIES: u32 = 5;
/// How long to wait between retries (doubles each time, capped at 16s).
const RETRY_BASE_MS: u64 = 500;

pub async fn download_file(
    base_url: &str,
    share_id: &str,
    share_name: &str,
    download_dir: &PathBuf,
    progress_tx: tokio::sync::mpsc::Sender<DownloadProgress>,
    retry_tx: tokio::sync::mpsc::Sender<u32>, // sends attempt number on each retry
    mut pause_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<DownloadResult> {
    tokio::fs::create_dir_all(download_dir).await?;

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;

    // Determine filename from a HEAD-like approach: just use the share name for
    // now and correct it from Content-Disposition on the first successful response.
    // We keep a stable temp path so partial data survives retries.
    let tmp_path = download_dir.join(format!(".dl_tmp_{}", share_id));
    let mut final_path: Option<PathBuf> = None;
    let mut expected_checksum: Option<String> = None;
    let mut total: u64 = 0;

    let mut attempt = 0u32;

    loop {
        // How many bytes do we already have on disk?
        let resume_from: u64 = tokio::fs::metadata(&tmp_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        // Build request — add Range header when resuming
        let mut req = client.get(format!("{}/download/{}", base_url, share_id));
        if resume_from > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={}-", resume_from));
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                attempt += 1;
                if attempt > MAX_RETRIES {
                    // Clean up partial file and bail
                    tokio::fs::remove_file(&tmp_path).await.ok();
                    anyhow::bail!("Download failed after {} retries: {}", MAX_RETRIES, e);
                }
                let wait = std::cmp::min(RETRY_BASE_MS * (1 << (attempt - 1)), 16_000);
                tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                continue;
            }
        };

        match resp.status() {
            // 200 OK — server doesn't support range requests (e.g. live-zip folder)
            // or this is a fresh start. Truncate any partial file and start over.
            s if s == reqwest::StatusCode::OK => {
                tokio::fs::remove_file(&tmp_path).await.ok();
            }
            // 206 Partial Content — server accepted our Range, we can append
            s if s == reqwest::StatusCode::PARTIAL_CONTENT => { /* keep existing bytes */ }
            // 416 Range Not Satisfiable — we already have the whole file
            s if s == reqwest::StatusCode::RANGE_NOT_SATISFIABLE => {
                // rename tmp → final and finish
                let dest = final_path
                    .clone()
                    .unwrap_or_else(|| download_dir.join(share_name));
                tokio::fs::rename(&tmp_path, &dest).await?;
                return Ok(DownloadResult { path: dest, checksum_ok: None });
            }
            s => {
                tokio::fs::remove_file(&tmp_path).await.ok();
                anyhow::bail!("Server returned {}", s);
            }
        }

        // On first successful response, capture metadata
        if expected_checksum.is_none() {
            expected_checksum = resp
                .headers()
                .get("x-checksum-sha256")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_lowercase());
        }
        if total == 0 {
            // For 206, Content-Length is just the remaining bytes; get total from
            // Content-Range: bytes START-END/TOTAL
            if resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
                total = resp
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.split('/').last())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            } else {
                total = resp.content_length().unwrap_or(0);
            }
        }
        if final_path.is_none() {
            let filename = resp
                .headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.split("filename=").nth(1))
                .map(|f| f.trim_matches('"').to_string())
                .unwrap_or_else(|| share_name.to_string());
            final_path = Some(download_dir.join(filename));
        }

        // Open temp file for writing — append if resuming, create fresh if not
        let resume_from_now = tokio::fs::metadata(&tmp_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(resume_from_now > 0)
            .write(true)
            .open(&tmp_path)
            .await?;

        // We need to compute the checksum over the WHOLE file, so if we're
        // resuming we replay the already-written bytes through the hasher first.
        let mut hasher = Sha256::new();
        if resume_from_now > 0 {
            let existing = tokio::fs::read(&tmp_path).await?;
            hasher.update(&existing);
        }

        let mut downloaded = resume_from_now;
        let mut last_bytes = downloaded;
        let mut last_time = Instant::now();
        let mut last_update = Instant::now();
        let mut smoothed_speed = 0.0f64;
        let alpha = 0.15;

        let mut stream = resp.bytes_stream();
        let mut stream_error: Option<anyhow::Error> = None;

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    hasher.update(&chunk);
                    if let Err(e) = file.write_all(&chunk).await {
                        stream_error = Some(e.into());
                        break;
                    }
                    downloaded += chunk.len() as u64;

                    let now = Instant::now();
                    let elapsed = now.duration_since(last_time).as_secs_f64();
                    let new_speed = if elapsed > 0.0 {
                        (downloaded - last_bytes) as f64 / elapsed
                    } else { 0.0 };

                    let (eta_seconds, new_smooth) =
                        calc_eta_seconds(smoothed_speed, new_speed, alpha, total, downloaded);
                    smoothed_speed = new_smooth;

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
                Err(e) => {
                    stream_error = Some(e.into());
                    break;
                }
            }
            // Pause: hold the open connection until resumed (or sender dropped)
            if *pause_rx.borrow() {
                // Wait until value changes (false=resume) or sender is gone
                while *pause_rx.borrow() {
                    if pause_rx.changed().await.is_err() {
                        break; // sender dropped (download finished/cancelled)
                    }
                }
                // Reset timing so we don't get a bogus speed spike on resume
                last_time = Instant::now();
                last_bytes = downloaded;
                last_update = Instant::now();
            }
        }

        // If we finished cleanly (no error, all bytes received), we're done
        if stream_error.is_none() && (total == 0 || downloaded >= total) {
            file.flush().await?;
            drop(file);

            let dest = final_path
                .clone()
                .unwrap_or_else(|| download_dir.join(share_name));
            tokio::fs::rename(&tmp_path, &dest).await?;

            let checksum_ok = expected_checksum.map(|expected| {
                let actual = hex::encode(hasher.finalize());
                if expected.starts_with("dir:") { true } else { actual == expected }
            });

            return Ok(DownloadResult { path: dest, checksum_ok });
        }

        // Stream broke mid-transfer — flush what we have and retry
        file.flush().await.ok();
        drop(file);

        attempt += 1;
        if attempt > MAX_RETRIES {
            tokio::fs::remove_file(&tmp_path).await.ok();
            let err = stream_error.unwrap_or_else(|| anyhow::anyhow!("Incomplete download"));
            anyhow::bail!("Download failed after {} retries: {}", MAX_RETRIES, err);
        }

        // Notify TUI so it can show "retrying" state
        let _ = retry_tx.try_send(attempt);
        // Exponential backoff: 500ms, 1s, 2s, 4s, 8s, capped at 16s
        let wait = std::cmp::min(RETRY_BASE_MS * (1 << (attempt - 1)), 16_000);
        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
        // Loop back — will read tmp_path size and send Range header
    }
}

pub fn calc_eta_seconds(mut smoothed_speed: f64, new_speed: f64, alpha: f64, total: u64, downloaded: u64) -> (f64, f64) {
    smoothed_speed = if smoothed_speed == 0.0 {
        new_speed
    } else {
        alpha * new_speed + (1.0 - alpha) * smoothed_speed
    };
    let eta_seconds = if smoothed_speed > 0.0 {
        total.saturating_sub(downloaded) as f64 / smoothed_speed
    } else { 0.0 };
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
