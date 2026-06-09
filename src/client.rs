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

#[derive(Debug, Clone, PartialEq)]
pub enum DownloadControl {
    Running,
    Paused,
    Cancelled,
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
    /// True when the download was cancelled by the user (not an error).
    pub cancelled: bool,
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
    mut pause_rx: tokio::sync::watch::Receiver<DownloadControl>,
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
                return Ok(DownloadResult {
                    path: dest,
                    checksum_ok: None,
                    cancelled: false,
                });
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
                    .and_then(|s| s.split('/').next_back())
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
                    } else {
                        0.0
                    };

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
            // Check for cancel or pause after each chunk
            let ctrl = pause_rx.borrow().clone();
            match ctrl {
                DownloadControl::Cancelled => {
                    drop(file);
                    tokio::fs::remove_file(&tmp_path).await.ok();
                    return Ok(DownloadResult {
                        path: tmp_path,
                        checksum_ok: None,
                        cancelled: true,
                    });
                }
                DownloadControl::Paused => {
                    // Hold connection until resumed or cancelled
                    loop {
                        match pause_rx.changed().await {
                            Err(_) => {
                                // Sender dropped without sending Cancelled — treat as cancel
                                drop(file);
                                tokio::fs::remove_file(&tmp_path).await.ok();
                                return Ok(DownloadResult {
                                    path: tmp_path,
                                    checksum_ok: None,
                                    cancelled: true,
                                });
                            }
                            Ok(_) => {
                                let new_ctrl = pause_rx.borrow().clone();
                                if new_ctrl == DownloadControl::Cancelled {
                                    drop(file);
                                    tokio::fs::remove_file(&tmp_path).await.ok();
                                    return Ok(DownloadResult {
                                        path: tmp_path,
                                        checksum_ok: None,
                                        cancelled: true,
                                    });
                                }
                                if new_ctrl == DownloadControl::Running {
                                    break; // resumed
                                }
                                // still Paused — keep waiting
                            }
                        }
                    }
                    // Reset timing to avoid bogus speed spike on resume
                    last_time = Instant::now();
                    last_bytes = downloaded;
                    last_update = Instant::now();
                }
                DownloadControl::Running => {}
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
                if expected.starts_with("dir:") {
                    true
                } else {
                    actual == expected
                }
            });

            return Ok(DownloadResult {
                path: dest,
                checksum_ok,
                cancelled: false,
            });
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

pub fn calc_eta_seconds(
    mut smoothed_speed: f64,
    new_speed: f64,
    alpha: f64,
    total: u64,
    downloaded: u64,
) -> (f64, f64) {
    smoothed_speed = if smoothed_speed == 0.0 {
        new_speed
    } else {
        alpha * new_speed + (1.0 - alpha) * smoothed_speed
    };
    let eta_seconds = if smoothed_speed > 0.0 {
        total.saturating_sub(downloaded) as f64 / smoothed_speed
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

/// Format a speed in bytes/s using the chosen unit system.
/// `Bytes` → MB/s / KB/s / B/s (same as `format_speed`)
/// `Bits`  → Mb/s / Kb/s / b/s  (multiply by 8)
pub fn format_speed_unit(bps: f64, unit: crate::tui::app::SpeedUnit) -> String {
    use crate::tui::app::SpeedUnit;
    match unit {
        SpeedUnit::Bytes => format_speed(bps),
        SpeedUnit::Bits => {
            let bits = bps * 8.0;
            if bits >= 1_000_000.0 {
                format!("{:.1} Mb/s", bits / 1_000_000.0)
            } else if bits >= 1_000.0 {
                format!("{:.1} Kb/s", bits / 1_000.0)
            } else {
                format!("{:.0} b/s", bits)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{mpsc, watch};

    // -----------------------------------------------------------------------
    // Pure function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_speed_bytes() {
        assert_eq!(format_speed(0.0), "0 B/s");
        assert_eq!(format_speed(500.0), "500 B/s");
        assert_eq!(format_speed(999.0), "999 B/s");
    }

    #[test]
    fn test_format_speed_kilobytes() {
        assert_eq!(format_speed(1_000.0), "1.0 KB/s");
        assert_eq!(format_speed(512_000.0), "512.0 KB/s");
        assert_eq!(format_speed(999_999.0), "1000.0 KB/s");
    }

    #[test]
    fn test_format_speed_megabytes() {
        assert_eq!(format_speed(1_000_000.0), "1.0 MB/s");
        assert_eq!(format_speed(36_000_000.0), "36.0 MB/s");
        assert_eq!(format_speed(1_250_000_000.0), "1250.0 MB/s");
    }

    #[test]
    fn test_calc_eta_cold_start() {
        // When smoothed_speed is 0, first sample becomes the speed directly
        let (eta, smooth) = calc_eta_seconds(0.0, 10_000_000.0, 0.15, 100_000_000, 0);
        assert_eq!(smooth, 10_000_000.0);
        assert!((eta - 10.0).abs() < 0.001, "eta should be 10s, got {}", eta);
    }

    #[test]
    fn test_calc_eta_ema_smoothing() {
        // EMA should move toward new value but not jump to it
        let (_, smooth1) = calc_eta_seconds(0.0, 10_000_000.0, 0.15, 100_000_000, 0);
        let (_, smooth2) = calc_eta_seconds(smooth1, 20_000_000.0, 0.15, 100_000_000, 0);
        assert!(smooth2 > smooth1, "speed should increase toward new value");
        assert!(
            smooth2 < 20_000_000.0,
            "should not jump fully to new sample"
        );
    }

    #[test]
    fn test_calc_eta_zero_remaining() {
        // When downloaded == total, eta should be 0
        let (eta, _) = calc_eta_seconds(10_000_000.0, 10_000_000.0, 0.15, 100, 100);
        assert_eq!(eta, 0.0);
    }

    #[test]
    fn test_calc_eta_zero_speed() {
        // When speed is 0, eta stays 0 (not infinity/NaN)
        let (eta, _) = calc_eta_seconds(0.0, 0.0, 0.15, 100_000_000, 50_000_000);
        assert_eq!(eta, 0.0);
        assert!(eta.is_finite());
    }

    #[test]
    fn test_calc_eta_decreases_as_progress_increases() {
        let speed = 10_000_000.0_f64; // 10 MB/s constant
        let total = 100_000_000_u64;
        let (eta1, _) = calc_eta_seconds(speed, speed, 0.15, total, 10_000_000);
        let (eta2, _) = calc_eta_seconds(speed, speed, 0.15, total, 50_000_000);
        let (eta3, _) = calc_eta_seconds(speed, speed, 0.15, total, 90_000_000);
        assert!(eta1 > eta2, "eta should decrease as bytes increase");
        assert!(eta2 > eta3);
    }

    // -----------------------------------------------------------------------
    // download_file: cancel while paused — temp file deleted
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_cancel_while_paused_deletes_temp_file() {
        // Spin up a minimal axum server that serves a large-ish static body
        use axum::{routing::get, Router};
        use std::net::SocketAddr;

        let app = Router::new().route(
            "/download/{id}",
            get(|| async {
                // 1 MB of zeros
                let body = vec![0u8; 1_024 * 1_024];
                axum::response::Response::builder()
                    .header("content-length", body.len().to_string())
                    .header("accept-ranges", "bytes")
                    .body(axum::body::Body::from(body))
                    .unwrap()
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        let (prog_tx, _prog_rx) = mpsc::channel(32);
        let (retry_tx, _retry_rx) = mpsc::channel(8);
        let (pause_tx, pause_rx) = watch::channel(DownloadControl::Running);

        // Pause immediately
        pause_tx.send(DownloadControl::Paused).unwrap();

        let base = format!("http://{}", addr);
        let dir2 = dir_path.clone();

        // Run download in background, cancel after a short delay
        let handle = tokio::spawn(async move {
            download_file(
                &base,
                "test",
                "test_file.bin",
                &dir2,
                prog_tx,
                retry_tx,
                pause_rx,
            )
            .await
        });

        // Let the task start and enter pause wait
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        pause_tx.send(DownloadControl::Cancelled).unwrap();

        let result = handle.await.unwrap().unwrap();
        assert!(result.cancelled, "result should be cancelled");

        // Temp file must be gone
        let tmp = dir_path.join(".dl_tmp_test");
        assert!(!tmp.exists(), "temp file should be deleted on cancel");
    }

    // -----------------------------------------------------------------------
    // download_file: cancel while actively downloading — temp file deleted
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_cancel_while_downloading_deletes_temp_file() {
        use axum::{routing::get, Router};
        use std::net::SocketAddr;
        use tokio::time::Duration;

        // Serve body slowly in chunks so cancel can race the stream
        let app = Router::new().route(
            "/download/{id}",
            get(|| async {
                // Use futures::stream::unfold — no extra crate needed
                let stream = futures::stream::unfold(0u32, |i| async move {
                    if i >= 100 {
                        return None;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                    let chunk = bytes::Bytes::from(vec![0u8; 10_240]);
                    Some((Ok::<_, std::convert::Infallible>(chunk), i + 1))
                });
                axum::response::Response::builder()
                    .header("content-length", (100 * 10_240).to_string())
                    .header("accept-ranges", "bytes")
                    .body(axum::body::Body::from_stream(stream))
                    .unwrap()
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        let (prog_tx, _) = mpsc::channel(32);
        let (retry_tx, _) = mpsc::channel(8);
        let (pause_tx, pause_rx) = watch::channel(DownloadControl::Running);

        let base = format!("http://{}", addr);
        let dir2 = dir_path.clone();

        let handle = tokio::spawn(async move {
            download_file(
                &base,
                "test",
                "test_file.bin",
                &dir2,
                prog_tx,
                retry_tx,
                pause_rx,
            )
            .await
        });

        // Let a few chunks land, then cancel
        tokio::time::sleep(Duration::from_millis(80)).await;
        pause_tx.send(DownloadControl::Cancelled).unwrap();

        let result = handle.await.unwrap().unwrap();
        assert!(result.cancelled, "result should be cancelled");

        let tmp = dir_path.join(".dl_tmp_test");
        assert!(!tmp.exists(), "temp file should be deleted on cancel");
        // Final file must not exist either
        let final_file = dir_path.join("test_file.bin");
        assert!(
            !final_file.exists(),
            "final file must not exist after cancel"
        );
    }

    // -----------------------------------------------------------------------
    // download_file: resume from partial — Range header sent
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_resume_sends_range_header() {
        use axum::{extract::Request, routing::get, Router};
        use std::net::SocketAddr;
        use std::sync::{Arc, Mutex};

        let range_seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let range_seen2 = range_seen.clone();

        let app = Router::new().route(
            "/download/{id}",
            get(move |req: Request| {
                let range_seen = range_seen2.clone();
                async move {
                    let range_hdr = req
                        .headers()
                        .get("range")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    *range_seen.lock().unwrap() = range_hdr.clone();

                    if let Some(range) = range_hdr {
                        // Parse "bytes=N-"
                        let start: u64 = range
                            .strip_prefix("bytes=")
                            .unwrap_or("0-")
                            .split('-')
                            .next()
                            .unwrap_or("0")
                            .parse()
                            .unwrap_or(0);
                        let total: u64 = 1024;
                        let remaining = total - start;
                        axum::response::Response::builder()
                            .status(206)
                            .header("content-length", remaining.to_string())
                            .header(
                                "content-range",
                                format!("bytes {}-{}/{}", start, total - 1, total),
                            )
                            .header("accept-ranges", "bytes")
                            .body(axum::body::Body::from(vec![0u8; remaining as usize]))
                            .unwrap()
                    } else {
                        axum::response::Response::builder()
                            .status(200)
                            .header("content-length", "1024")
                            .header("accept-ranges", "bytes")
                            .body(axum::body::Body::from(vec![0u8; 1024]))
                            .unwrap()
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();

        // Pre-create a partial temp file (512 bytes already downloaded)
        let tmp = dir_path.join(".dl_tmp_resume_test");
        tokio::fs::write(&tmp, vec![1u8; 512]).await.unwrap();

        let (prog_tx, _) = mpsc::channel(32);
        let (retry_tx, _) = mpsc::channel(8);
        let (_, pause_rx) = watch::channel(DownloadControl::Running);

        let base = format!("http://{}", addr);
        let result = download_file(
            &base,
            "resume_test",
            "out.bin",
            &dir_path,
            prog_tx,
            retry_tx,
            pause_rx,
        )
        .await
        .unwrap();

        assert!(!result.cancelled);
        // Range header should have been sent with offset 512
        let seen = range_seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            Some("bytes=512-".to_string()),
            "expected Range: bytes=512-, got {:?}",
            seen
        );
    }

    // -----------------------------------------------------------------------
    // download_file: server returns 200 (no range support) — starts fresh
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_full_download_completes() {
        use axum::{routing::get, Router};
        use std::net::SocketAddr;

        let content = b"Hello, fileshare!".to_vec();
        let content2 = content.clone();

        let app = Router::new().route(
            "/download/{id}",
            get(move || {
                let body = content2.clone();
                async move {
                    axum::response::Response::builder()
                        .status(200)
                        .header("content-length", body.len().to_string())
                        .header("accept-ranges", "bytes")
                        .header("content-disposition", "attachment; filename=\"hello.txt\"")
                        .body(axum::body::Body::from(body))
                        .unwrap()
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let (prog_tx, _) = mpsc::channel(32);
        let (retry_tx, _) = mpsc::channel(8);
        let (_, pause_rx) = watch::channel(DownloadControl::Running);

        let result = download_file(
            &format!("http://{}", addr),
            "id1",
            "hello.txt",
            &dir.path().to_path_buf(),
            prog_tx,
            retry_tx,
            pause_rx,
        )
        .await
        .unwrap();

        assert!(!result.cancelled);
        let written = tokio::fs::read(&result.path).await.unwrap();
        assert_eq!(written, content);
    }
}
