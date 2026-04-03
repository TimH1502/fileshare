use anyhow::Result;
use futures::StreamExt;
use serde::Deserialize;
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

pub async fn download_file(
    base_url: &str,
    share_id: &str,
    share_name: &str,
    download_dir: &PathBuf,
    progress_tx: tokio::sync::mpsc::Sender<DownloadProgress>,
) -> Result<PathBuf> {
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

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
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
    Ok(dest_path)
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
