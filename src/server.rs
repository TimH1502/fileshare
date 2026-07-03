use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, Multipart, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
    Router,
};
use nanoid::nanoid;
use serde::Serialize;
use std::io::SeekFrom;
use std::sync::Arc;
use tokio::io::AsyncSeekExt;
use tokio::sync::broadcast;

use crate::shares::{ShareKind, ShareRegistry, SharedItem};

#[derive(Clone)]
pub struct AppState {
    pub shares: ShareRegistry,
    pub username: String,
    pub event_tx: broadcast::Sender<ServerEvent>,
    pub download_dir: std::path::PathBuf,
}

#[derive(Debug, Clone)]
pub enum ServerEvent {
    Downloaded {
        item_name: String,
        by_addr: String,
    },
    Uploaded {
        item_name: String,
        by_addr: String,
    },
    UploadProgress {
        item_id: String,
        bytes_sent: u64,
        total: u64,
    },
    UploadDone {
        item_id: String,
    },
    Deleted {
        item_name: String,
    },
    /// A browser/web-UI upload has just started streaming in.
    WebUploadStarted {
        transfer_id: String,
        filename: String,
        total: u64,
        by_addr: String,
    },
    /// Periodic progress while the web-UI upload is streaming in.
    WebUploadProgress {
        transfer_id: String,
        bytes_received: u64,
        total: u64,
    },
    /// The web-UI upload finished (file written, share registered).
    WebUploadFinished {
        transfer_id: String,
    },
    /// The web-UI upload failed mid-stream.
    WebUploadFailed {
        transfer_id: String,
    },
}

#[derive(Serialize)]
struct ShareInfo {
    id: String,
    name: String,
    kind: String,
    size: u64,
    size_human: String,
    checksum: String,
    added_at: String,
    download_count: u32,
    available: bool,
}

impl From<&SharedItem> for ShareInfo {
    fn from(item: &SharedItem) -> Self {
        let kind = match item.kind {
            ShareKind::File => "file",
            ShareKind::Folder => "folder",
            ShareKind::ZippedFolder => "zipped_folder",
        };
        Self {
            id: item.id.clone(),
            name: item.name.clone(),
            kind: kind.to_string(),
            size: item.size,
            size_human: item.size_human(),
            checksum: item.checksum.clone(),
            added_at: item.added_at.to_rfc3339(),
            download_count: item.download_count,
            available: item.is_available(),
        }
    }
}

#[derive(Serialize)]
struct ListResponse {
    username: String,
    items: Vec<ShareInfo>,
}

async fn list_shares(State(state): State<Arc<AppState>>) -> Json<ListResponse> {
    let items = state
        .shares
        .list_available()
        .iter()
        .map(ShareInfo::from)
        .collect();
    Json(ListResponse {
        username: state.username.clone(),
        items,
    })
}

/// Wraps a byte stream and fires ServerEvent::UploadProgress as chunks flow through.
/// Used in download_file so the server can track how much it has sent to each peer.
struct ProgressStream {
    inner: tokio_util::io::ReaderStream<tokio::fs::File>,
    event_tx: broadcast::Sender<ServerEvent>,
    item_id: String,
    bytes_sent: u64,
    total: u64,
    last_report: u64,
}

impl futures::Stream for ProgressStream {
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::pin::Pin;
        let result = Pin::new(&mut self.inner).poll_next(cx);
        if let std::task::Poll::Ready(Some(Ok(ref chunk))) = result {
            self.bytes_sent += chunk.len() as u64;
            // Report every 64 KB to avoid flooding the broadcast channel
            if self.bytes_sent - self.last_report >= 65_536 || self.bytes_sent == self.total {
                self.last_report = self.bytes_sent;
                self.event_tx
                    .send(ServerEvent::UploadProgress {
                        item_id: self.item_id.clone(),
                        bytes_sent: self.bytes_sent,
                        total: self.total,
                    })
                    .ok();
            }
        }
        if let std::task::Poll::Ready(None) = result {
            self.event_tx
                .send(ServerEvent::UploadDone {
                    item_id: self.item_id.clone(),
                })
                .ok();
        }
        result
    }
}

async fn download_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Response {
    // Parse Range header: "bytes=START-" or "bytes=START-END"
    let range_start: Option<u64> = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("bytes="))
        .and_then(|s| s.split('-').next())
        .and_then(|s| s.parse().ok());

    let item = match state.shares.get(&id) {
        Some(i) if i.is_available() => i,
        Some(_) => return (StatusCode::GONE, "Share is no longer available").into_response(),
        None => return (StatusCode::NOT_FOUND, "Share not found").into_response(),
    };

    // Only count and log fresh downloads, not range-resume continuations
    if range_start.is_none() {
        state.shares.increment_downloads(&id);
        state
            .event_tx
            .send(ServerEvent::Downloaded {
                item_name: item.name.clone(),
                by_addr: addr.ip().to_string(),
            })
            .ok();
    }

    // Plain raw Folder shares are never zipped — not pre-zipped, not zipped
    // on demand. They're downloaded file-by-file via the manifest endpoint
    // (see folder_manifest / download_folder_file below). Hitting the
    // single-file /download/{id} route for a folder share is a client error.
    if matches!(item.kind, ShareKind::Folder) {
        return (
            StatusCode::BAD_REQUEST,
            "This share is a folder \u{2014} fetch /shares/{id}/manifest and download each \
             file via /download/{id}/file?path=<relative_path> instead of /download/{id}",
        )
            .into_response();
    }

    let (content_type, filename) = if matches!(item.kind, ShareKind::ZippedFolder) {
        ("application/zip".to_string(), format!("{}.zip", item.name))
    } else {
        (
            mime_guess::from_path(&item.path)
                .first_or_octet_stream()
                .to_string(),
            item.name.clone(),
        )
    };

    stream_file_response(
        &state,
        &item.path,
        item.size,
        &item.id,
        &item.checksum,
        &content_type,
        &filename,
        range_start,
    )
    .await
}

/// Stream a single on-disk file as an HTTP response, honouring Range
/// requests (for pause/resume) and firing the same progress events used
/// everywhere else in the app. Shared by the single-file `/download/{id}`
/// route and the per-file `/download/{id}/file` route used for raw folders.
#[allow(clippy::too_many_arguments)]
async fn stream_file_response(
    state: &AppState,
    path: &std::path::Path,
    total: u64,
    item_id: &str,
    checksum: &str,
    content_type: &str,
    filename: &str,
    range_start: Option<u64>,
) -> Response {
    match tokio::fs::File::open(path).await {
        Ok(mut file) => {
            // Honour Range requests so browser pause/resume works
            let (status, start, content_length) = match range_start {
                Some(start) if start < total => {
                    if file.seek(SeekFrom::Start(start)).await.is_err() {
                        return (StatusCode::RANGE_NOT_SATISFIABLE, "Seek failed").into_response();
                    }
                    (StatusCode::PARTIAL_CONTENT, start, total - start)
                }
                Some(start) if start >= total => {
                    return Response::builder()
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(header::CONTENT_RANGE, format!("bytes */{}", total))
                        .body(axum::body::Body::empty())
                        .unwrap();
                }
                _ => (StatusCode::OK, 0, total),
            };

            let progress_stream = ProgressStream {
                inner: tokio_util::io::ReaderStream::new(file),
                event_tx: state.event_tx.clone(),
                item_id: item_id.to_string(),
                bytes_sent: start, // progress already accounts for resumed offset
                total,
                last_report: start,
            };
            let body = axum::body::Body::from_stream(progress_stream);

            let mut builder = Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, content_type)
                .header(
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}\"", filename),
                )
                .header(header::CONTENT_LENGTH, content_length)
                .header(header::ACCEPT_RANGES, "bytes")
                .header("X-Checksum-SHA256", checksum);
            if status == StatusCode::PARTIAL_CONTENT {
                builder = builder.header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, total - 1, total),
                );
            }
            builder.body(body).unwrap()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to open file").into_response(),
    }
}

#[derive(Serialize)]
struct ManifestEntryResponse {
    path: String,
    size: u64,
}

#[derive(Serialize)]
struct ManifestResponse {
    folder_name: String,
    files: Vec<ManifestEntryResponse>,
}

/// GET /shares/{id}/manifest — lists every file inside a raw (unzipped)
/// folder share, so a client can download them one by one and reconstruct
/// the folder locally, with no zipping at any point.
async fn folder_manifest(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let item = match state.shares.get(&id) {
        Some(i) if i.is_available() => i,
        Some(_) => return (StatusCode::GONE, "Share is no longer available").into_response(),
        None => return (StatusCode::NOT_FOUND, "Share not found").into_response(),
    };

    if !matches!(item.kind, ShareKind::Folder) {
        return (
            StatusCode::BAD_REQUEST,
            "This share is not a raw folder; use /download/{id} instead",
        )
            .into_response();
    }

    let folder_path = item.path.clone();
    let entries =
        tokio::task::spawn_blocking(move || crate::shares::list_folder_manifest(&folder_path))
            .await
            .unwrap_or_default();

    Json(ManifestResponse {
        folder_name: item.name.clone(),
        files: entries
            .into_iter()
            .map(|e| ManifestEntryResponse {
                path: e.rel_path,
                size: e.size,
            })
            .collect(),
    })
    .into_response()
}

#[derive(serde::Deserialize)]
struct FolderFileQuery {
    path: String,
}

/// GET /download/{id}/file?path=<relative_path> — streams a single file out
/// of a raw folder share. Supports Range requests exactly like the regular
/// single-file download route, so pause/resume works the same way.
async fn download_folder_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<FolderFileQuery>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let range_start: Option<u64> = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("bytes="))
        .and_then(|s| s.split('-').next())
        .and_then(|s| s.parse().ok());

    let item = match state.shares.get(&id) {
        Some(i) if i.is_available() => i,
        Some(_) => return (StatusCode::GONE, "Share is no longer available").into_response(),
        None => return (StatusCode::NOT_FOUND, "Share not found").into_response(),
    };

    if !matches!(item.kind, ShareKind::Folder) {
        return (
            StatusCode::BAD_REQUEST,
            "This share is not a raw folder; use /download/{id} instead",
        )
            .into_response();
    }

    let folder_root = item.path.clone();
    let rel_path = query.path.clone();
    let resolved = tokio::task::spawn_blocking(move || {
        crate::shares::resolve_folder_member(&folder_root, &rel_path)
    })
    .await
    .ok()
    .flatten();

    let file_path = match resolved {
        Some(p) => p,
        None => return (StatusCode::NOT_FOUND, "File not found in folder").into_response(),
    };

    let size = match tokio::fs::metadata(&file_path).await {
        Ok(m) => m.len(),
        Err(_) => return (StatusCode::NOT_FOUND, "File not found in folder").into_response(),
    };

    // Only count/log a fresh download once per file (not range-resume continuations).
    if range_start.is_none() {
        state
            .event_tx
            .send(ServerEvent::Downloaded {
                item_name: format!("{}/{}", item.name, query.path),
                by_addr: addr.ip().to_string(),
            })
            .ok();
    }

    let content_type = mime_guess::from_path(&file_path)
        .first_or_octet_stream()
        .to_string();
    let filename = file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());

    // Per-file checksum isn't precomputed for raw folders (only the cheap
    // folder-level fingerprint is), so no X-Checksum-SHA256 header here —
    // the client's overall folder download can verify sizes instead.
    stream_file_response(
        &state,
        &file_path,
        size,
        &item.id,
        "",
        &content_type,
        &filename,
        range_start,
    )
    .await
}

#[derive(Serialize)]
struct UploadResponse {
    ok: bool,
    name: String,
    id: String,
    size_human: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn upload_file(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    // Best-effort total size from Content-Length header (browsers send this for XHR uploads)
    let content_length: u64 = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    while let Ok(Some(field)) = multipart.next_field().await {
        // Accept the first field named "file" (or any field with a filename)
        let filename = match field.file_name() {
            Some(n) if !n.is_empty() => sanitize_filename(n),
            _ => continue,
        };

        // Generate a transfer ID for TUI progress tracking (before the share ID exists)
        let transfer_id = nanoid!(8);

        // Notify TUI that an upload is starting
        state
            .event_tx
            .send(ServerEvent::WebUploadStarted {
                transfer_id: transfer_id.clone(),
                filename: filename.clone(),
                total: content_length,
                by_addr: addr.ip().to_string(),
            })
            .ok();

        // Stream to a temp file first, then move it into the download dir
        let tmp_path = state.download_dir.join(format!(".upload_tmp_{}", filename));
        let dest_path = state.download_dir.join(&filename);

        // Ensure download dir exists
        if let Err(e) = tokio::fs::create_dir_all(&state.download_dir).await {
            state
                .event_tx
                .send(ServerEvent::WebUploadFailed { transfer_id })
                .ok();
            return Json(UploadResponse {
                ok: false,
                name: filename,
                id: String::new(),
                size_human: String::new(),
                error: Some(format!("Failed to create upload dir: {e}")),
            })
            .into_response();
        }

        // Stream body → temp file
        let mut f = match tokio::fs::File::create(&tmp_path).await {
            Ok(f) => f,
            Err(e) => {
                state
                    .event_tx
                    .send(ServerEvent::WebUploadFailed { transfer_id })
                    .ok();
                return Json(UploadResponse {
                    ok: false,
                    name: filename,
                    id: String::new(),
                    size_human: String::new(),
                    error: Some(format!("Failed to create temp file: {e}")),
                })
                .into_response();
            }
        };

        use tokio::io::AsyncWriteExt;
        let mut stream = field;
        let mut bytes_received: u64 = 0;
        let mut last_report: u64 = 0;
        loop {
            match stream.chunk().await {
                Ok(Some(chunk)) => {
                    bytes_received += chunk.len() as u64;
                    if let Err(e) = f.write_all(&chunk).await {
                        tokio::fs::remove_file(&tmp_path).await.ok();
                        state
                            .event_tx
                            .send(ServerEvent::WebUploadFailed { transfer_id })
                            .ok();
                        return Json(UploadResponse {
                            ok: false,
                            name: filename,
                            id: String::new(),
                            size_human: String::new(),
                            error: Some(format!("Write error: {e}")),
                        })
                        .into_response();
                    }
                    // Emit progress every 256 KB
                    if bytes_received - last_report >= 262_144 {
                        last_report = bytes_received;
                        state
                            .event_tx
                            .send(ServerEvent::WebUploadProgress {
                                transfer_id: transfer_id.clone(),
                                bytes_received,
                                total: content_length,
                            })
                            .ok();
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tokio::fs::remove_file(&tmp_path).await.ok();
                    state
                        .event_tx
                        .send(ServerEvent::WebUploadFailed { transfer_id })
                        .ok();
                    return Json(UploadResponse {
                        ok: false,
                        name: filename,
                        id: String::new(),
                        size_human: String::new(),
                        error: Some(format!("Read error: {e}")),
                    })
                    .into_response();
                }
            }
        }
        drop(f);

        // Rename temp → final destination (atomic on same fs)
        if let Err(e) = tokio::fs::rename(&tmp_path, &dest_path).await {
            tokio::fs::remove_file(&tmp_path).await.ok();
            state
                .event_tx
                .send(ServerEvent::WebUploadFailed { transfer_id })
                .ok();
            return Json(UploadResponse {
                ok: false,
                name: filename,
                id: String::new(),
                size_human: String::new(),
                error: Some(format!("Failed to move file: {e}")),
            })
            .into_response();
        }

        // Register in share registry (runs checksum on blocking thread)
        let shares_c = state.shares.clone();
        let dest_c = dest_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            shares_c.add(dest_c, None, None, |_name, _done, _total| {})
        })
        .await;

        return match result {
            Ok(Ok(item)) => {
                let name = item.name.clone();
                let id = item.id.clone();
                let size_human = item.size_human();
                state
                    .event_tx
                    .send(ServerEvent::WebUploadFinished { transfer_id })
                    .ok();
                state
                    .event_tx
                    .send(ServerEvent::Uploaded {
                        item_name: name.clone(),
                        by_addr: addr.ip().to_string(),
                    })
                    .ok();
                Json(UploadResponse {
                    ok: true,
                    name,
                    id,
                    size_human,
                    error: None,
                })
                .into_response()
            }
            Ok(Err(e)) => {
                state
                    .event_tx
                    .send(ServerEvent::WebUploadFailed { transfer_id })
                    .ok();
                Json(UploadResponse {
                    ok: false,
                    name: filename,
                    id: String::new(),
                    size_human: String::new(),
                    error: Some(e.to_string()),
                })
                .into_response()
            }
            Err(e) => {
                state
                    .event_tx
                    .send(ServerEvent::WebUploadFailed { transfer_id })
                    .ok();
                Json(UploadResponse {
                    ok: false,
                    name: filename,
                    id: String::new(),
                    size_human: String::new(),
                    error: Some(e.to_string()),
                })
                .into_response()
            }
        };
    }

    Json(UploadResponse {
        ok: false,
        name: String::new(),
        id: String::new(),
        size_human: String::new(),
        error: Some("No file field found in request".to_string()),
    })
    .into_response()
}

fn sanitize_filename(name: &str) -> String {
    // Strip directory components, replace dangerous chars
    let base = std::path::Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "upload".to_string());
    base.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c => c,
        })
        .collect()
}

async fn serve_browser_ui(State(state): State<Arc<AppState>>) -> axum::response::Html<String> {
    let items = state.shares.list_available();
    let mut rows = String::new();
    for item in &items {
        let kind_icon = match item.kind {
            ShareKind::File => "📄",
            ShareKind::Folder => "📁",
            ShareKind::ZippedFolder => "🗜️",
        };
        let download_cell = if matches!(item.kind, ShareKind::Folder) {
            format!(
                r#"<button onclick="toggleBrowse('{0}', this)" style="background:none;border:1px solid #d29922;color:#d29922;border-radius:4px;padding:2px 8px;cursor:pointer;font-family:monospace;font-size:0.85em;margin-right:4px">&#x1F50D; Browse</button><button onclick="downloadFolder('{0}', '{1}', this)" style="background:none;border:1px solid #58a6ff;color:#58a6ff;border-radius:4px;padding:2px 8px;cursor:pointer;font-family:monospace;font-size:0.85em">&#x2B07; All files</button>"#,
                html_escape(&item.id),
                html_escape(&item.name),
            )
        } else {
            format!(
                r#"<a href="/download/{}" download>Download</a>"#,
                html_escape(&item.id)
            )
        };
        let browse_row = if matches!(item.kind, ShareKind::Folder) {
            format!(
                r#"<tr id="browse-row-{0}" style="display:none"><td colspan="5" style="background:#0d1117;padding:0"><div id="browse-body-{0}" style="padding:8px 16px"></div></td></tr>"#,
                html_escape(&item.id)
            )
        } else {
            String::new()
        };
        // FIX: item.id is now escaped even though nanoid produces safe chars
        rows.push_str(&format!(
            r#"<tr>
                <td>{} {}</td>
                <td>{}</td>
                <td>{}</td>
                <td>{}</td>
                <td><button onclick="deleteShare('{}', this)" style="background:none;border:1px solid #f85149;color:#f85149;border-radius:4px;padding:2px 8px;cursor:pointer;font-family:monospace;font-size:0.85em">&#x2715; Delete</button></td>
               </tr>{}"#,
            kind_icon,
            html_escape(&item.name),
            item.size_human(),
            item.download_count,
            download_cell,
            html_escape(&item.id),
            browse_row,
        ));
    }

    axum::response::Html(format!(
        r#"<!DOCTYPE html>
<html>
<head>
  <title>{username} — fileshare</title>
  <style>
    *, *::before, *::after {{ box-sizing: border-box; }}
    body {{ font-family: monospace; max-width: 860px; margin: 40px auto; padding: 0 20px; background: #0d1117; color: #c9d1d9; }}
    h1 {{ color: #58a6ff; margin-bottom: 4px; }}
    table {{ width: 100%; border-collapse: collapse; margin-top: 16px; }}
    th {{ text-align: left; border-bottom: 1px solid #30363d; padding: 8px; color: #8b949e; }}
    td {{ padding: 8px; border-bottom: 1px solid #21262d; }}
    a {{ color: #58a6ff; text-decoration: none; }}
    a:hover {{ text-decoration: underline; }}
    #status {{ font-size: 0.75em; color: #8b949e; float: right; margin-top: 8px; }}
    #status.ok::before {{ content: "● "; color: #3fb950; }}
    #status.err::before {{ content: "● "; color: #f85149; }}

    /* Upload zone */
    #upload-zone {{
      border: 2px dashed #30363d;
      border-radius: 8px;
      padding: 28px 20px;
      text-align: center;
      color: #8b949e;
      cursor: pointer;
      transition: border-color 0.15s, background 0.15s;
      margin-bottom: 4px;
      position: relative;
    }}
    #upload-zone.drag-over {{
      border-color: #58a6ff;
      background: rgba(88,166,255,0.06);
      color: #c9d1d9;
    }}
    #upload-zone input[type=file] {{
      position: absolute; inset: 0; opacity: 0; cursor: pointer; width: 100%; height: 100%;
    }}
    #upload-btn {{
      display: inline-block; margin-top: 10px; padding: 6px 18px;
      background: #21262d; color: #58a6ff; border: 1px solid #30363d;
      border-radius: 6px; cursor: pointer; font-family: monospace; font-size: 0.9em;
    }}
    #upload-btn:hover {{ background: #30363d; }}

    /* Upload progress list */
    #upload-list {{ margin: 8px 0 0; }}
    .upl-item {{
      display: flex; align-items: center; gap: 10px;
      padding: 6px 8px; background: #161b22; border-radius: 6px; margin-bottom: 4px;
      font-size: 0.85em;
    }}
    .upl-name {{ flex: 1; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }}
    .upl-bar-wrap {{ width: 120px; height: 6px; background: #21262d; border-radius: 3px; flex-shrink: 0; }}
    .upl-bar {{ height: 100%; border-radius: 3px; background: #58a6ff; transition: width 0.1s; }}
    .upl-status {{ flex-shrink: 0; min-width: 60px; text-align: right; color: #8b949e; }}
    .upl-item.done .upl-bar {{ background: #3fb950; }}
    .upl-item.err  .upl-bar {{ background: #f85149; }}
  </style>
</head>
<body>
  <h1>📡 {username}'s files <span id="status" class="ok">live</span></h1>
  <p style="color:#8b949e;margin:0 0 12px">Install <code>fileshare</code> for a better experience, or use the web UI below.</p>

  <div id="upload-zone">
    <input type="file" id="file-input" multiple>
    <div>⬆ Drop files here to upload, or</div>
    <label id="upload-btn" for="file-input">Choose files</label>
  </div>
  <div id="upload-list"></div>

  <table>
    <thead><tr><th>Name</th><th>Size</th><th>Downloads</th><th></th><th></th></tr></thead>
    <tbody id="shares-body">{rows}</tbody>
  </table>

  <script>
    const POLL_MS = 4000;
    const tbody   = document.getElementById('shares-body');
    const status  = document.getElementById('status');
    const zone    = document.getElementById('upload-zone');
    const input   = document.getElementById('file-input');
    const uplList = document.getElementById('upload-list');
    let lastJson  = '';

    // ── Polling ────────────────────────────────────────────────────────────
    function iconFor(kind) {{
      return kind === 'folder' ? '📁' : kind === 'zipped_folder' ? '🗜️' : '📄';
    }}
    function esc(s) {{
      return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
    }}
    function renderRows(items) {{
      if (!items.length) return '<tr><td colspan="5" style="color:#8b949e">No files shared yet.</td></tr>';
      return items.map(it => `
        <tr>
          <td>${{iconFor(it.kind)}} ${{esc(it.name)}}</td>
          <td>${{esc(it.size_human)}}</td>
          <td>${{it.download_count}}</td>
          <td>${{it.kind === 'folder'
              ? `<button onclick="toggleBrowse('${{esc(it.id)}}', this)" style="background:none;border:1px solid #d29922;color:#d29922;border-radius:4px;padding:2px 8px;cursor:pointer;font-family:monospace;font-size:0.85em;margin-right:4px">🔍 Browse</button><button onclick="downloadFolder('${{esc(it.id)}}', '${{esc(it.name)}}', this)" style="background:none;border:1px solid #58a6ff;color:#58a6ff;border-radius:4px;padding:2px 8px;cursor:pointer;font-family:monospace;font-size:0.85em">⬇ All files</button>`
              : `<a href="/download/${{esc(it.id)}}" download>Download</a>`}}</td>
          <td><button onclick="deleteShare('${{esc(it.id)}}', this)" style="background:none;border:1px solid #f85149;color:#f85149;border-radius:4px;padding:2px 8px;cursor:pointer;font-family:monospace;font-size:0.85em">✕ Delete</button></td>
        </tr>
        ${{it.kind === 'folder' ? `<tr id="browse-row-${{esc(it.id)}}" style="display:none"><td colspan="5" style="background:#0d1117;padding:0"><div id="browse-body-${{esc(it.id)}}" style="padding:8px 16px"></div></td></tr>` : ''}}`).join('');
    }}
    function setStatus(ok, txt) {{ status.className = ok ? 'ok' : 'err'; status.textContent = txt; }}

    function fmtSize(bytes) {{
      const units = ['B','KB','MB','GB','TB'];
      let i = 0, n = bytes;
      while (n >= 1024 && i < units.length - 1) {{ n /= 1024; i++; }}
      return (i === 0 ? n : n.toFixed(1)) + ' ' + units[i];
    }}

    // Browsing state survives the 4s poll re-render so an open folder
    // doesn't snap shut while the user is looking at it.
    const openFolders = new Set();

    async function toggleBrowse(id, btn) {{
      const row = document.getElementById('browse-row-' + id);
      const body = document.getElementById('browse-body-' + id);
      if (!row) return;
      const isOpen = row.style.display !== 'none';
      if (isOpen) {{
        row.style.display = 'none';
        openFolders.delete(id);
        return;
      }}
      openFolders.add(id);
      row.style.display = '';
      body.innerHTML = '<span style="color:#8b949e">Loading…</span>';
      await loadBrowseBody(id, body);
    }}

    async function loadBrowseBody(id, body) {{
      try {{
        const r = await fetch('/shares/' + id + '/manifest');
        if (!r.ok) throw new Error('HTTP ' + r.status);
        const manifest = await r.json();
        const files = manifest.files || [];
        if (!files.length) {{
          body.innerHTML = '<span style="color:#8b949e">This folder has no files.</span>';
          return;
        }}
        body.innerHTML = `<table style="width:100%;margin:0"><tbody>` + files.map(f => `
          <tr>
            <td style="border:none;padding:2px 8px 2px 0;font-size:0.9em">📄 ${{esc(f.path)}}</td>
            <td style="border:none;padding:2px 8px;font-size:0.9em;color:#8b949e;white-space:nowrap">${{fmtSize(f.size)}}</td>
            <td style="border:none;padding:2px 0;white-space:nowrap">
              <a href="/download/${{esc(id)}}/file?path=${{encodeURIComponent(f.path)}}" download style="font-size:0.9em">Download</a>
            </td>
          </tr>`).join('') + `</tbody></table>`;
      }} catch (e) {{
        body.innerHTML = '<span style="color:#f85149">Failed to load folder contents: ' + esc(e.message) + '</span>';
      }}
    }}

    async function deleteShare(id, btn) {{
      if (!confirm('Remove this share? The file on disk is NOT deleted.')) return;
      btn.disabled = true;
      btn.textContent = '…';
      try {{
        const r = await fetch('/shares/' + id, {{ method: 'DELETE' }});
        const d = await r.json();
        if (d.ok) {{ poll(); }} else {{ btn.textContent = '✕'; btn.disabled = false; alert(d.error || 'Delete failed'); }}
      }} catch {{ btn.textContent = '✕'; btn.disabled = false; }}
    }}

    // Raw folders are never zipped, so there's no single file to download.
    // Fetch the manifest, then download every file individually (one browser
    // download per file, landing flat in the user's Downloads folder — the
    // browser sandbox doesn't let a webpage create a real subfolder there).
    async function downloadFolder(id, name, btn) {{
      const origLabel = btn.textContent;
      btn.disabled = true;
      try {{
        const r = await fetch('/shares/' + id + '/manifest');
        if (!r.ok) throw new Error('HTTP ' + r.status);
        const manifest = await r.json();
        const files = manifest.files || [];
        if (!files.length) {{ alert('This folder has no files.'); return; }}
        for (let i = 0; i < files.length; i++) {{
          btn.textContent = `⬇ ${{i + 1}}/${{files.length}}…`;
          const url = '/download/' + id + '/file?path=' + encodeURIComponent(files[i].path);
          const a = document.createElement('a');
          a.href = url;
          a.download = files[i].path.split('/').pop();
          document.body.appendChild(a);
          a.click();
          a.remove();
          // Small gap between triggers so the browser doesn't drop rapid-fire
          // downloads or throw up a "multiple downloads" blocker dialog.
          await new Promise(res => setTimeout(res, 400));
        }}
        btn.textContent = `✓ ${{files.length}} files`;
      }} catch (e) {{
        btn.textContent = '✕ failed';
        alert('Folder download failed: ' + e.message);
      }} finally {{
        setTimeout(() => {{ btn.textContent = origLabel; btn.disabled = false; }}, 3000);
      }}
    }}

    async function poll() {{
      if (document.hidden) return;
      try {{
        const r = await fetch('/shares');
        if (!r.ok) throw new Error('HTTP ' + r.status);
        const d = await r.json();
        const j = JSON.stringify(d.items);
        if (j !== lastJson) {{
          lastJson = j;
          tbody.innerHTML = renderRows(d.items);
          // Re-open any folders the user had expanded before this refresh.
          for (const id of openFolders) {{
            const row = document.getElementById('browse-row-' + id);
            const body = document.getElementById('browse-body-' + id);
            if (row && body) {{
              row.style.display = '';
              loadBrowseBody(id, body);
            }}
          }}
        }}
        setStatus(true, 'live · ' + new Date().toLocaleTimeString());
      }} catch {{ setStatus(false, 'offline'); }}
    }}
    document.addEventListener('visibilitychange', () => {{ if (!document.hidden) poll(); }});
    poll();
    setInterval(poll, POLL_MS);

    // ── Drag-over styling ──────────────────────────────────────────────────
    zone.addEventListener('dragover',  e => {{ e.preventDefault(); zone.classList.add('drag-over'); }});
    zone.addEventListener('dragleave', () => zone.classList.remove('drag-over'));
    zone.addEventListener('drop', e => {{
      e.preventDefault();
      zone.classList.remove('drag-over');
      uploadFiles([...e.dataTransfer.files]);
    }});
    input.addEventListener('change', () => {{ uploadFiles([...input.files]); input.value = ''; }});

    // ── Upload logic ───────────────────────────────────────────────────────
    function fmtBytes(b) {{
      if (b >= 1e9) return (b/1e9).toFixed(1) + ' GB';
      if (b >= 1e6) return (b/1e6).toFixed(1) + ' MB';
      if (b >= 1e3) return (b/1e3).toFixed(1) + ' KB';
      return b + ' B';
    }}

    function makeUplItem(file) {{
      const el = document.createElement('div');
      el.className = 'upl-item';
      el.innerHTML = `
        <span class="upl-name">${{esc(file.name)}}</span>
        <div class="upl-bar-wrap"><div class="upl-bar" style="width:0%"></div></div>
        <span class="upl-status">0%</span>`;
      uplList.prepend(el);
      return {{
        bar:    el.querySelector('.upl-bar'),
        lbl:    el.querySelector('.upl-status'),
        finish: (ok, msg) => {{
          el.classList.add(ok ? 'done' : 'err');
          el.querySelector('.upl-bar').style.width = '100%';
          el.querySelector('.upl-status').textContent = msg;
          setTimeout(() => el.remove(), 4000);
        }}
      }};
    }}

    function uploadFiles(files) {{
      files.forEach(file => {{
        const ui  = makeUplItem(file);
        const fd  = new FormData();
        fd.append('file', file, file.name);
        const xhr = new XMLHttpRequest();

        xhr.upload.onprogress = e => {{
          if (!e.lengthComputable) return;
          const pct = Math.round(e.loaded / e.total * 100);
          ui.bar.style.width = pct + '%';
          ui.lbl.textContent = pct + '%';
        }};

        xhr.onload = () => {{
          try {{
            const r = JSON.parse(xhr.responseText);
            if (r.ok) {{
              ui.finish(true, r.size_human || '✓');
              poll(); // refresh table immediately
            }} else {{
              ui.finish(false, r.error || 'error');
            }}
          }} catch {{ ui.finish(false, 'error'); }}
        }};

        xhr.onerror = () => ui.finish(false, 'failed');
        xhr.open('POST', '/upload');
        xhr.send(fd);
      }});
    }}
  </script>
</body>
</html>"#,
        username = html_escape(&state.username),
        rows = rows
    ))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// DELETE /shares/:id  — removes a share from the registry.
async fn delete_share(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.shares.remove(&id) {
        Some(item) => {
            state
                .event_tx
                .send(ServerEvent::Deleted {
                    item_name: item.name,
                })
                .ok();
            (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "ok": false, "error": "Share not found" })),
        )
            .into_response(),
    }
}

pub fn build_router_with_connect_info(
    state: Arc<AppState>,
) -> axum::extract::connect_info::IntoMakeServiceWithConnectInfo<Router, std::net::SocketAddr> {
    Router::new()
        .route("/", get(serve_browser_ui))
        .route("/shares", get(list_shares))
        .route("/shares/{id}", delete(delete_share))
        .route("/download/{id}", get(download_file))
        .route("/download/{id}/file", get(download_folder_file))
        .route("/shares/{id}/manifest", get(folder_manifest))
        .route(
            "/upload",
            post(upload_file).layer(DefaultBodyLimit::disable()),
        )
        .with_state(state)
        .into_make_service_with_connect_info::<std::net::SocketAddr>()
}
