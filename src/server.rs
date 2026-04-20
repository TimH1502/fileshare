use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::get,
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
}

#[derive(Debug, Clone)]
pub enum ServerEvent {
    Downloaded { item_name: String, by_addr: String },
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
    body {{ font-family: monospace; max-width: 800px; margin: 40px auto; padding: 0 20px; background: #0d1117; color: #c9d1d9; }}
    h1 {{ color: #58a6ff; }}
    table {{ width: 100%; border-collapse: collapse; }}
    th {{ text-align: left; border-bottom: 1px solid #30363d; padding: 8px; color: #8b949e; }}
    td {{ padding: 8px; border-bottom: 1px solid #21262d; }}
    a {{ color: #58a6ff; text-decoration: none; }}
    a:hover {{ text-decoration: underline; }}
    #status {{ font-size: 0.75em; color: #8b949e; float: right; margin-top: 6px; }}
    #status.ok::before {{ content: "● "; color: #3fb950; }}
    #status.err::before {{ content: "● "; color: #f85149; }}
  </style>
</head>
<body>
  <h1>📡 {username}'s files <span id="status" class="ok">live</span></h1>
  <p style="color:#8b949e">Install <code>fileshare</code> for a better experience. Or download directly:</p>
  <table>
    <thead><tr><th>Name</th><th>Size</th><th>Downloads</th><th></th></tr></thead>
    <tbody id="shares-body">{rows}</tbody>
  </table>
  <script>
    const INTERVAL_MS = 4000;
    const tbody = document.getElementById('shares-body');
    const status = document.getElementById('status');
    let lastJson = '';
    let timer = null;

    function iconFor(kind) {{
      if (kind === 'file') return '📄';
      if (kind === 'folder') return '📁';
      return '🗜️';
    }}

    function escHtml(s) {{
      return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
    }}

    function renderRows(items) {{
      if (!items.length) {{
        return '<tr><td colspan="4" style="color:#8b949e">No files shared yet.</td></tr>';
      }}
      return items.map(item => `
        <tr>
          <td>${{iconFor(item.kind)}} ${{escHtml(item.name)}}</td>
          <td>${{escHtml(item.size_human)}}</td>
          <td>${{item.download_count}}</td>
          <td><a href="/download/${{escHtml(item.id)}}" download>Download</a></td>
        </tr>`).join('');
    }}

    function setStatus(ok, text) {{
      status.className = ok ? 'ok' : 'err';
      status.textContent = text;
    }}

    async function poll() {{
      if (document.hidden) return;
      try {{
        const res = await fetch('/shares');
        if (!res.ok) throw new Error('HTTP ' + res.status);
        const data = await res.json();
        const json = JSON.stringify(data.items);
        if (json !== lastJson) {{
          lastJson = json;
          tbody.innerHTML = renderRows(data.items);
        }}
        const now = new Date().toLocaleTimeString();
        setStatus(true, 'live · ' + now);
      }} catch (e) {{
        setStatus(false, 'offline');
      }}
    }}

    // Pause when tab is hidden, resume immediately on focus
    document.addEventListener('visibilitychange', () => {{
      if (!document.hidden) poll();
    }});

    // Start polling
    poll();
    timer = setInterval(poll, INTERVAL_MS);
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
        .with_state(state)
        .into_make_service_with_connect_info::<std::net::SocketAddr>()
}