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
        Some(_) => {
            return (StatusCode::GONE, "Share is no longer available").into_response()
        }
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

    match tokio::fs::File::open(&item.path).await {
        Ok(file) => {
            let stream = tokio_util::io::ReaderStream::new(file);
            let body = axum::body::Body::from_stream(stream);

            let content_type = if matches!(item.kind, ShareKind::ZippedFolder) {
                "application/zip".to_string()
            } else {
                mime_guess::from_path(&item.path)
                    .first_or_octet_stream()
                    .to_string()
            };

            let filename = if matches!(item.kind, ShareKind::ZippedFolder) {
                format!("{}.zip", item.name)
            } else {
                item.name.clone()
            };

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .header(
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}\"", filename),
                )
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
            item.id
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
  </style>
</head>
<body>
  <h1>📡 {username}'s files</h1>
  <p style="color:#8b949e">Install <code>fileshare</code> for a better experience. Or download directly:</p>
  <table>
    <thead><tr><th>Name</th><th>Size</th><th>Downloads</th><th></th></tr></thead>
    <tbody>{rows}</tbody>
  </table>
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

pub fn build_router_with_connect_info(state: Arc<AppState>) -> axum::extract::connect_info::IntoMakeServiceWithConnectInfo<Router, std::net::SocketAddr> {
    Router::new()
        .route("/", get(serve_browser_ui))
        .route("/shares", get(list_shares))
        .route("/download/{id}", get(download_file))
        .with_state(state)
        .into_make_service_with_connect_info::<std::net::SocketAddr>()
}
