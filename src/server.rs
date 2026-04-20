use axum::{
    extract::{Multipart, Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use serde::Serialize;
use std::sync::Arc;
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
    Downloaded { item_name: String, by_addr: String },
    Uploaded { item_name: String, by_addr: String },
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

async fn download_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> Response {
    let item = match state.shares.get(&id) {
        Some(i) if i.is_available() => i,
        Some(_) => return (StatusCode::GONE, "Share is no longer available").into_response(),
        None => return (StatusCode::NOT_FOUND, "Share not found").into_response(),
    };

    state.shares.increment_downloads(&id);
    state
        .event_tx
        .send(ServerEvent::Downloaded {
            item_name: item.name.clone(),
            by_addr: addr.ip().to_string(),
        })
        .ok();

    // FIX: plain unzipped Folder — path is a directory, can't File::open it.
    // Zip it into a temp file on a blocking thread, then stream and clean up.
    if matches!(item.kind, ShareKind::Folder) {
        let folder_path = item.path.clone();
        let folder_name = item.name.clone();
        let checksum = item.checksum.clone();

        let zip_result = tokio::task::spawn_blocking(move || {
            let tmp = std::env::temp_dir()
                .join(format!("fileshare_tmp_{}.zip", folder_name));
            crate::shares::zip_folder_pub(&folder_path, &tmp)?;
            Ok::<_, anyhow::Error>(tmp)
        })
        .await;

        return match zip_result {
            Ok(Ok(tmp_path)) => match tokio::fs::File::open(&tmp_path).await {
                Ok(file) => {
                    let size = tokio::fs::metadata(&tmp_path).await.map(|m| m.len()).unwrap_or(0);
                    let stream = tokio_util::io::ReaderStream::new(file);
                    let body = axum::body::Body::from_stream(stream);
                    // Cleanup: the temp file is unlinked immediately; the OS keeps it
                    // accessible through the open file handle until the stream finishes.
                    tokio::fs::remove_file(&tmp_path).await.ok();
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "application/zip")
                        .header(header::CONTENT_DISPOSITION,
                            format!("attachment; filename=\"{}.zip\"", item.name))
                        .header(header::CONTENT_LENGTH, size)
                        .header("X-Checksum-SHA256", &checksum)
                        .body(body)
                        .unwrap()
                }
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to open temp zip").into_response(),
            },
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to zip folder").into_response(),
        };
    }

    match tokio::fs::File::open(&item.path).await {
        Ok(file) => {
            let stream = tokio_util::io::ReaderStream::new(file);
            let body = axum::body::Body::from_stream(stream);

            let (content_type, filename) = if matches!(item.kind, ShareKind::ZippedFolder) {
                ("application/zip".to_string(), format!("{}.zip", item.name))
            } else {
                (mime_guess::from_path(&item.path).first_or_octet_stream().to_string(),
                 item.name.clone())
            };

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}\"", filename))
                .header(header::CONTENT_LENGTH, item.size)
                .header("X-Checksum-SHA256", &item.checksum)
                .body(body)
                .unwrap()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to open file").into_response(),
    }
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
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    mut multipart: Multipart,
) -> Response {
    while let Ok(Some(field)) = multipart.next_field().await {
        // Accept the first field named "file" (or any field with a filename)
        let filename = match field.file_name() {
            Some(n) if !n.is_empty() => sanitize_filename(n),
            _ => continue,
        };

        // Stream to a temp file first, then move it into the download dir
        let tmp_path = state.download_dir.join(format!(".upload_tmp_{}", filename));
        let dest_path = state.download_dir.join(&filename);

        // Ensure download dir exists
        if let Err(e) = tokio::fs::create_dir_all(&state.download_dir).await {
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
                return Json(UploadResponse {
                    ok: false,
                    name: filename,
                    id: String::new(),
                    size_human: String::new(),
                    error: Some(format!("Failed to create temp file: {e}")),
                })
                .into_response()
            }
        };

        use tokio::io::AsyncWriteExt;
        let mut stream = field;
        loop {
            match stream.chunk().await {
                Ok(Some(chunk)) => {
                    if let Err(e) = f.write_all(&chunk).await {
                        tokio::fs::remove_file(&tmp_path).await.ok();
                        return Json(UploadResponse {
                            ok: false,
                            name: filename,
                            id: String::new(),
                            size_human: String::new(),
                            error: Some(format!("Write error: {e}")),
                        })
                        .into_response();
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tokio::fs::remove_file(&tmp_path).await.ok();
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
            shares_c.add(dest_c, None, None, |_| {})
        })
        .await;

        return match result {
            Ok(Ok(item)) => {
                let name = item.name.clone();
                let id = item.id.clone();
                let size_human = item.size_human();
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
            Ok(Err(e)) => Json(UploadResponse {
                ok: false,
                name: filename,
                id: String::new(),
                size_human: String::new(),
                error: Some(e.to_string()),
            })
            .into_response(),
            Err(e) => Json(UploadResponse {
                ok: false,
                name: filename,
                id: String::new(),
                size_human: String::new(),
                error: Some(e.to_string()),
            })
            .into_response(),
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
        // FIX: item.id is now escaped even though nanoid produces safe chars
        rows.push_str(&format!(
            r#"<tr>
                <td>{} {}</td>
                <td>{}</td>
                <td>{}</td>
                <td><a href="/download/{}" download>Download</a></td>
               </tr>"#,
            kind_icon,
            html_escape(&item.name),
            item.size_human(),
            item.download_count,
            html_escape(&item.id),  // FIX: escape id in href
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
    <thead><tr><th>Name</th><th>Size</th><th>Downloads</th><th></th></tr></thead>
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
      if (!items.length) return '<tr><td colspan="4" style="color:#8b949e">No files shared yet.</td></tr>';
      return items.map(it => `
        <tr>
          <td>${{iconFor(it.kind)}} ${{esc(it.name)}}</td>
          <td>${{esc(it.size_human)}}</td>
          <td>${{it.download_count}}</td>
          <td><a href="/download/${{esc(it.id)}}" download>Download</a></td>
        </tr>`).join('');
    }}
    function setStatus(ok, txt) {{ status.className = ok ? 'ok' : 'err'; status.textContent = txt; }}

    async function poll() {{
      if (document.hidden) return;
      try {{
        const r = await fetch('/shares');
        if (!r.ok) throw new Error('HTTP ' + r.status);
        const d = await r.json();
        const j = JSON.stringify(d.items);
        if (j !== lastJson) {{ lastJson = j; tbody.innerHTML = renderRows(d.items); }}
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

pub fn build_router_with_connect_info(
    state: Arc<AppState>,
) -> axum::extract::connect_info::IntoMakeServiceWithConnectInfo<Router, std::net::SocketAddr> {
    Router::new()
        .route("/", get(serve_browser_ui))
        .route("/shares", get(list_shares))
        .route("/download/{id}", get(download_file))
        .route("/upload", post(upload_file))
        .with_state(state)
        .into_make_service_with_connect_info::<std::net::SocketAddr>()
}
