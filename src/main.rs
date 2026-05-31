mod media;
mod templates;
use askama::Template;
use axum::extract::{Path as AxumPath, Query, State};
use axum::middleware;
use axum::response::{IntoResponse, Redirect};
use axum::{Router, http::StatusCode, response::Html, routing::get};
use serde::Deserialize;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use templates::{IndexTemplate, PlaylistTemplate};
use tokio::sync::RwLock;
use tower_http::services::ServeDir;

#[derive(Deserialize)]
struct ListParams {
    #[serde(default = "default_page")]
    page: u32,

    #[serde(default = "default_page_size")]
    page_size: u32,

    #[serde(default)]
    sort: SortField,

    #[serde(default = "default_sort_dir")]
    dir: SortDirection,

    #[serde(default)]
    query: String,
}

fn default_page() -> u32 {
    1
}
fn default_page_size() -> u32 {
    50
}
fn default_sort_dir() -> SortDirection {
    SortDirection::Asc
}

#[derive(Deserialize, Copy, Clone)]
#[serde(rename_all = "lowercase")]
enum SortField {
    Name,
    Size,
    LastModified,
    Created,
}

#[derive(Deserialize, Copy, Clone)]
#[serde(rename_all = "lowercase")]
enum SortDirection {
    Asc,
    Desc,
}

impl SortDirection {
    pub fn is_asc(&self) -> bool {
        matches!(self, SortDirection::Asc)
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            SortDirection::Asc => "asc",
            SortDirection::Desc => "desc",
        }
    }
}

impl SortField {
    pub fn is_name(&self) -> bool {
        matches!(self, SortField::Name)
    }
    pub fn is_size(&self) -> bool {
        matches!(self, SortField::Size)
    }
    pub fn is_last_modified(&self) -> bool {
        matches!(self, SortField::LastModified)
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            SortField::Name => "name",
            SortField::Size => "size",
            SortField::LastModified => "lastmodified",
            SortField::Created => "created",
        }
    }
}

impl Default for SortField {
    fn default() -> SortField {
        SortField::LastModified
    }
}

#[derive(Clone)]
struct AppState {
    cache: Arc<RwLock<Vec<media::MediaFile>>>,
    auth_cookie_name: Option<String>,
    auth_cookie_value: Option<String>,
}

#[tokio::main]
async fn main() {
    // Load .env file
    dotenvy::dotenv().ok();

    // Read config from environment
    let addr = std::env::var("BIND_ADDRESS").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let bind_addr = format!("{}:{}", addr, port);
    let media_path = std::env::var("MEDIA_PATH").unwrap_or_else(|_| "./media".to_string());
    let auth_cookie_value = std::env::var("AUTH_COOKIE_VALUE").ok();
    let auth_cookie_name = std::env::var("AUTH_COOKIE_NAME").ok();

    // Build initial file index
    let index = media::build_index(&PathBuf::from(&media_path));
    let cache = Arc::new(RwLock::new(index));

    let path_cl = PathBuf::from(&media_path);
    let cache_cl = Arc::clone(&cache);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let fresh = media::build_index(&path_cl);
            *cache_cl.write().await = fresh;
        }
    });

    // Create application state
    let state = AppState {
        cache: cache.clone(),
        auth_cookie_value: auth_cookie_value,
        auth_cookie_name: auth_cookie_name,
    };

    // Build our application with routes
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/playlist", get(playlist_handler))
        .route("/v/{filename}", get(watch_redirect))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        // Serve static files from media directory
        .nest_service("/media", ServeDir::new(media_path))
        .with_state(state);

    // Parse the bind address
    let addr: SocketAddr = bind_addr.parse().expect("Invalid BIND_ADDRESS");

    // Start the server
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind address");

    axum::serve(listener, app).await.expect("Server error");
}

async fn index_handler(
    State(state): State<AppState>,
    Query(query): Query<ListParams>,
) -> Result<Html<String>, (StatusCode, String)> {
    let files = state.cache.read().await;
    let paginated = media::list_media_files(&files, &query);
    IndexTemplate { paginated, query }
        .render()
        .map(Html)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

#[derive(Deserialize)]
struct WatchParams {
    #[serde(default)]
    sort: SortField,
    #[serde(default = "default_sort_dir")]
    dir: SortDirection,
}

// Redirect /v/{filename}?sort=...&dir=... to /playlist?video={filename}&sort=...&dir=...
async fn watch_redirect(
    AxumPath(filename): AxumPath<String>,
    Query(params): Query<WatchParams>,
) -> Redirect {
    let sort = params.sort.as_str();
    let dir = params.dir.as_str();
    Redirect::to(&format!(
        "/playlist?video={}&sort={}&dir={}",
        filename, sort, dir
    ))
}

#[derive(Deserialize)]
struct PlaylistParams {
    #[serde(default)]
    video: String,

    #[serde(default)]
    sort: SortField,

    #[serde(default = "default_sort_dir")]
    dir: SortDirection,

    #[serde(default)]
    query: String,
}

async fn playlist_handler(
    State(state): State<AppState>,
    Query(params): Query<PlaylistParams>,
) -> Result<Html<String>, (StatusCode, String)> {
    let files = state.cache.read().await;

    let (playlist, current_idx) = media::build_playlist(
        &files,
        &params.query,
        &params.video,
        params.sort,
        params.dir,
    );

    let video = playlist
        .get(current_idx)
        .map(|item| media::MediaFile {
            name: item.n.clone(),
            path: item.p.clone(),
            size: item.s,
            modified: None,
            created: None,
            extension: item.e.clone(),
        })
        .unwrap_or_else(|| media::MediaFile {
            name: "Unknown".into(),
            path: "".into(),
            size: 0,
            modified: None,
            created: None,
            extension: "mp4".into(),
        });

    let playlist_json = serde_json::to_string(&playlist)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    PlaylistTemplate {
        playlist_json,
        current_idx,
        video,
        sort: params.sort,
        dir: params.dir,
        search: params.query,
    }
    .render()
    .map(Html)
    .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

async fn auth_middleware(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    match (&state.auth_cookie_name, &state.auth_cookie_value) {
        (Some(name), Some(pwd)) if !name.trim().is_empty() && !pwd.trim().is_empty() => {
            if let Some(cookie_header) = request.headers().get("cookie") {
                if let Ok(cookie_str) = cookie_header.to_str() {
                    for cookie in cookie_str.split(";") {
                        let cookie = cookie.trim();
                        if let Some((key, value)) = cookie.split_once("=") {
                            if key == name && value == pwd {
                                return next.run(request).await;
                            }
                        }
                    }
                }
            }
            return StatusCode::UNAUTHORIZED.into_response();
        }
        _ => {
            return next.run(request).await;
        }
    }
}
