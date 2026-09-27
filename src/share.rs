use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use askama::Template;
use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

use crate::download::{convert_attachment, DownloadParams};
use crate::media::resolve_media_path;
use crate::templates::{ShareTemplate, SharesTemplate};
use crate::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Share {
    pub uuid: String,
    pub name: String,
    pub rel: String,
    pub created_at: u64,
    pub expires_at: Option<u64>,
    pub view_count: u64,
    pub max_views: Option<u64>,
}

/// Flat, JSON/template-friendly view of a share including its public URL.
#[derive(Debug, Clone, Serialize)]
pub struct ShareView {
    pub uuid: String,
    pub name: String,
    pub rel: String,
    pub created_at: u64,
    pub expires_at: Option<u64>,
    pub view_count: u64,
    pub max_views: Option<u64>,
    pub url: String,
}

impl Share {
    pub fn view(&self, public_base_url: &str) -> ShareView {
        ShareView {
            uuid: self.uuid.clone(),
            name: self.name.clone(),
            rel: self.rel.clone(),
            created_at: self.created_at,
            expires_at: self.expires_at,
            view_count: self.view_count,
            max_views: self.max_views,
            url: share_url(public_base_url, &self.uuid),
        }
    }
}

pub fn share_url(public_base_url: &str, uuid: &str) -> String {
    format!(
        "{}/share/{}",
        public_base_url.trim_end_matches('/'),
        uuid
    )
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn expired(expires_at: Option<u64>) -> bool {
    expires_at.is_some_and(|t| now_unix() >= t)
}

pub fn thumb_name(uuid: &str) -> String {
    format!("{}.jpg", uuid)
}

pub async fn remove_thumbnail(thumb_dir: &Path, uuid: &str) {
    let _ = tokio::fs::remove_file(thumb_dir.join(thumb_name(uuid))).await;
}

pub(crate) async fn generate_thumbnail(video: &Path, thumb: &Path) -> std::io::Result<()> {
    // Try a frame at 1s first; fall back to the first frame for very short clips.
    for ss in ["1", "0"] {
        let status = tokio::process::Command::new("ffmpeg")
            .arg("-y")
            .arg("-loglevel")
            .arg("error")
            .arg("-ss")
            .arg(ss)
            .arg("-i")
            .arg(video)
            .arg("-frames:v")
            .arg("1")
            .arg("-vf")
            .arg("scale='min(1280,iw)':-2")
            .arg("-q:v")
            .arg("3")
            .arg(thumb)
            .status()
            .await?;

        if status.success()
            && tokio::fs::metadata(thumb).await.is_ok_and(|m| m.len() > 0)
        {
            return Ok(());
        }
    }
    Err(std::io::Error::other("ffmpeg failed to produce a thumbnail"))
}

#[derive(Clone)]
pub struct ShareStore {
    conn: Arc<Mutex<Connection>>,
}

impl ShareStore {
    pub async fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS shares (
                 uuid        TEXT PRIMARY KEY,
                 name        TEXT NOT NULL,
                 rel         TEXT NOT NULL,
                 created_at  INTEGER NOT NULL,
                 expires_at  INTEGER,
                 view_count  INTEGER NOT NULL DEFAULT 0,
                 max_views   INTEGER
             );
             CREATE INDEX IF NOT EXISTS idx_shares_expires_at ON shares(expires_at);",
        )?;
        let columns = {
            let mut statement = conn.prepare("PRAGMA table_info(shares)")?;
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?
        };
        if !columns.iter().any(|column| column == "view_count") {
            conn.execute("ALTER TABLE shares ADD COLUMN view_count INTEGER NOT NULL DEFAULT 0", [])?;
        }
        if !columns.iter().any(|column| column == "max_views") {
            conn.execute("ALTER TABLE shares ADD COLUMN max_views INTEGER", [])?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS share_views (
                 share_uuid TEXT NOT NULL,
                 token      TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 PRIMARY KEY (share_uuid, token)
             );
             CREATE INDEX IF NOT EXISTS idx_share_views_uuid ON share_views(share_uuid);",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub async fn create(
        &self,
        rel: &str,
        name: &str,
        expires_at: Option<u64>,
        max_views: Option<u64>,
    ) -> Result<Share, rusqlite::Error> {
        let share = Share {
            uuid: Uuid::new_v4().to_string(),
            name: name.to_string(),
            rel: rel.to_string(),
            created_at: now_unix(),
            expires_at,
            view_count: 0,
            max_views,
        };

        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO shares (uuid, name, rel, created_at, expires_at, max_views)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                share.uuid,
                share.name,
                share.rel,
                share.created_at as i64,
                share.expires_at.map(|v| v as i64),
                share.max_views.map(|v| v as i64),
            ],
        )?;
        Ok(share)
    }

    pub async fn get(&self, uuid: &str) -> Result<Option<Share>, rusqlite::Error> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT uuid, name, rel, created_at, expires_at, view_count, max_views FROM shares WHERE uuid = ?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![uuid], |row| {
            Ok(Share {
                uuid: row.get(0)?,
                name: row.get(1)?,
                rel: row.get(2)?,
                created_at: row.get::<_, i64>(3)? as u64,
                expires_at: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                view_count: row.get::<_, i64>(5)? as u64,
                max_views: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
            })
        })?;
        rows.next().transpose()
    }

    pub async fn list(&self) -> Result<Vec<Share>, rusqlite::Error> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT uuid, name, rel, created_at, expires_at, view_count, max_views FROM shares ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Share {
                uuid: row.get(0)?,
                name: row.get(1)?,
                rel: row.get(2)?,
                created_at: row.get::<_, i64>(3)? as u64,
                expires_at: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                view_count: row.get::<_, i64>(5)? as u64,
                max_views: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
            })
        })?;
        rows.collect()
    }

    pub async fn delete(&self, uuid: &str) -> Result<bool, rusqlite::Error> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM share_views WHERE share_uuid = ?1", rusqlite::params![uuid])?;
        let n = tx.execute("DELETE FROM shares WHERE uuid = ?1", rusqlite::params![uuid])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Count one browser visit per unguessable session cookie. Existing sessions
    /// remain valid after the limit is reached so the first viewer can finish
    /// streaming or downloading the file.
    pub async fn register_view(&self, uuid: &str, token: &str) -> Result<bool, rusqlite::Error> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM share_views WHERE share_uuid = ?1 AND token = ?2)",
            rusqlite::params![uuid, token],
            |row| row.get(0),
        )?;
        if existing {
            tx.commit()?;
            return Ok(true);
        }
        let updated = tx.execute(
            "UPDATE shares SET view_count = view_count + 1
             WHERE uuid = ?1 AND (max_views IS NULL OR view_count < max_views)",
            rusqlite::params![uuid],
        )?;
        if updated == 0 {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO share_views (share_uuid, token, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![uuid, token, now_unix() as i64],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub async fn has_view(&self, uuid: &str, token: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock().await;
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM share_views WHERE share_uuid = ?1 AND token = ?2)",
            rusqlite::params![uuid, token],
            |row| row.get(0),
        )
    }

    pub async fn regenerate(&self, uuid: &str) -> Result<Option<Share>, rusqlite::Error> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let previous = tx.query_row(
            "SELECT uuid, name, rel, expires_at, max_views FROM shares WHERE uuid = ?1",
            rusqlite::params![uuid],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                    row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                ))
            },
        ).optional()?;
        let Some((_, name, rel, expires_at, max_views)) = previous else {
            tx.commit()?;
            return Ok(None);
        };
        let share = Share {
            uuid: Uuid::new_v4().to_string(),
            name,
            rel,
            created_at: now_unix(),
            expires_at,
            view_count: 0,
            max_views,
        };
        tx.execute(
            "INSERT INTO shares (uuid, name, rel, created_at, expires_at, max_views)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                share.uuid,
                share.name,
                share.rel,
                share.created_at as i64,
                share.expires_at.map(|v| v as i64),
                share.max_views.map(|v| v as i64),
            ],
        )?;
        tx.execute("DELETE FROM share_views WHERE share_uuid = ?1", rusqlite::params![uuid])?;
        tx.execute("DELETE FROM shares WHERE uuid = ?1", rusqlite::params![uuid])?;
        tx.commit()?;
        Ok(Some(share))
    }

    pub async fn purge_expired(&self) -> Result<Vec<String>, rusqlite::Error> {
        let conn = self.conn.lock().await;
        let now = now_unix() as i64;
        let uuids: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT uuid FROM shares WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            )?;
            let rows = stmt.query_map(rusqlite::params![now], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let tx = conn.unchecked_transaction()?;
        for uuid in &uuids {
            tx.execute("DELETE FROM share_views WHERE share_uuid = ?1", rusqlite::params![uuid])?;
        }
        tx.execute(
            "DELETE FROM shares WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            rusqlite::params![now],
        )?;
        tx.commit()?;
        Ok(uuids)
    }
}

#[derive(Debug)]
pub struct ApiError(StatusCode, String);

impl ApiError {
    fn internal<E: std::fmt::Display>(e: E) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

#[derive(Deserialize)]
pub struct CreateShareRequest {
    pub rel: String,
    pub expires_in: Option<u64>,
    pub expires_at: Option<u64>,
    pub max_views: Option<u64>,
}

pub async fn create_share(
    State(state): State<AppState>,
    Json(req): Json<CreateShareRequest>,
) -> Result<Json<ShareView>, ApiError> {
    let abs = resolve_media_path(&state.media_root, &req.rel)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;

    let name = abs
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("shared")
        .to_string();

    if req.max_views == Some(0) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "max_views must be at least 1".into()));
    }
    let expires_at = if let Some(timestamp) = req.expires_at {
        if timestamp <= now_unix() {
            return Err(ApiError(StatusCode::BAD_REQUEST, "expiry must be in the future".into()));
        }
        Some(timestamp)
    } else {
        req.expires_in
            .filter(|ttl| *ttl > 0)
            .map(|ttl| now_unix().saturating_add(ttl))
    };

    let share = state
        .share_store
        .create(&req.rel, &name, expires_at, req.max_views)
        .await
        .map_err(ApiError::internal)?;

    // Pre-generate the thumbnail in the background so link previews (WhatsApp,
    // Telegram, etc.) don't race against on-demand generation.
    let thumb_dir = state.thumb_dir.clone();
    let uuid = share.uuid.clone();
    tokio::spawn(async move {
        let thumb = thumb_dir.join(thumb_name(&uuid));
        let already_cached = tokio::fs::metadata(&thumb)
            .await
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        if !already_cached {
            let _ = generate_thumbnail(&abs, &thumb).await;
        }
    });

    Ok(Json(share.view(&state.public_base_url)))
}

pub async fn regenerate_share(
    AxumPath(uuid): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<ShareView>, ApiError> {
    let existing = state.share_store.get(&uuid).await.map_err(ApiError::internal)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "share not found".into()))?;
    if expired(existing.expires_at) {
        return Err(ApiError(StatusCode::GONE, "share expired".into()));
    }
    let abs = resolve_media_path(&state.media_root, &existing.rel)
        .map_err(|e| ApiError(StatusCode::NOT_FOUND, e.to_string()))?;
    let share = state.share_store.regenerate(&uuid).await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "share not found".into()))?;
    remove_thumbnail(&state.thumb_dir, &uuid).await;

    let thumb_dir = state.thumb_dir.clone();
    let thumbnail_uuid = share.uuid.clone();
    tokio::spawn(async move {
        let thumb = thumb_dir.join(thumb_name(&thumbnail_uuid));
        let _ = generate_thumbnail(&abs, &thumb).await;
    });

    Ok(Json(share.view(&state.public_base_url)))
}

pub async fn list_shares(State(state): State<AppState>) -> Result<Json<Vec<ShareView>>, ApiError> {
    let mut views = Vec::new();

    for share in state.share_store.list().await.map_err(ApiError::internal)? {
        if expired(share.expires_at) {
            let _ = state.share_store.delete(&share.uuid).await;
            remove_thumbnail(&state.thumb_dir, &share.uuid).await;
            continue;
        }
        views.push(share.view(&state.public_base_url));
    }

    Ok(Json(views))
}

pub async fn delete_share(
    AxumPath(uuid): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    let deleted = state
        .share_store
        .delete(&uuid)
        .await
        .map_err(ApiError::internal)?;

    if deleted {
        remove_thumbnail(&state.thumb_dir, &uuid).await;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(StatusCode::NOT_FOUND, "share not found".into()))
    }
}

pub async fn shares_page(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    let mut views = Vec::new();

    for share in state.share_store.list().await.map_err(ApiError::internal)? {
        if expired(share.expires_at) {
            let _ = state.share_store.delete(&share.uuid).await;
            remove_thumbnail(&state.thumb_dir, &share.uuid).await;
            continue;
        }
        views.push(share.view(&state.public_base_url));
    }

    let shares_json =
        serde_json::to_string(&views).map_err(|e| ApiError::internal(e.to_string()))?;

    SharesTemplate {
        shares_json,
        analytics_tag: state.analytics_tag.clone(),
    }
    .render()
    .map(Html)
    .map_err(ApiError::internal)
}

fn view_cookie_name(uuid: &str) -> String {
    format!("mwvr_view_{uuid}")
}

fn view_cookie_token(headers: &HeaderMap, uuid: &str) -> Option<String> {
    let name = view_cookie_name(uuid);
    headers
        .get(header::COOKIE)?
        .to_str().ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| {
            if key == name && Uuid::parse_str(value).is_ok() {
                Some(value.to_string())
            } else {
                None
            }
        })
}

async fn session_allowed(share: &Share, headers: &HeaderMap, state: &AppState) -> bool {
    if share.max_views.is_none() {
        return true;
    }
    let Some(token) = view_cookie_token(headers, &share.uuid) else { return false; };
    state.share_store.has_view(&share.uuid, &token).await.unwrap_or(false)
}

pub async fn share_page(
    AxumPath(uuid): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let share = state
        .share_store
        .get(&uuid)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "share not found".into()))?;

    if expired(share.expires_at) {
        let _ = state.share_store.delete(&uuid).await;
        remove_thumbnail(&state.thumb_dir, &uuid).await;
        return Err(ApiError(StatusCode::NOT_FOUND, "share expired".into()));
    }

    let token = view_cookie_token(&headers, &uuid).unwrap_or_else(|| Uuid::new_v4().to_string());
    let allowed = state.share_store.register_view(&uuid, &token).await
        .map_err(ApiError::internal)?;
    if !allowed {
        return Err(ApiError(StatusCode::GONE, "share view limit reached".into()));
    }

    let extension = share
        .name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_lowercase())
        .unwrap_or_else(|| "mp4".into());

    // Filename for the "convert to mp4" download (extension swapped, case-safe).
    let mp4_name = share
        .name
        .rsplit_once('.')
        .map(|(stem, _)| format!("{}.mp4", stem))
        .unwrap_or_else(|| format!("{}.mp4", share.name));

    let page_url = share_url(&state.public_base_url, &share.uuid);
    let thumbnail_url = format!("{}/thumbnail", page_url);

    let html = ShareTemplate {
        uuid: share.uuid,
        name: share.name,
        extension: extension.clone(),
        mp4_name,
        page_url,
        thumbnail_url,
        mime: format!("video/{}", extension),
        analytics_tag: state.analytics_tag.clone(),
    }
    .render()
    .map_err(ApiError::internal)?;

    // Keep share URLs out of caches, referrer headers, and search indexes.
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(
        HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static("noindex, nofollow"),
    );
    headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{}={}; HttpOnly; SameSite=Lax; Path=/share/{}",
            view_cookie_name(&uuid), token, uuid
        )).unwrap_or_else(|_| HeaderValue::from_static("")),
    );

    Ok((headers, Html(html)).into_response())
}

pub async fn share_stream(
    AxumPath(uuid): AxumPath<String>,
    State(state): State<AppState>,
    request: Request<Body>,
) -> Response {
    let share = match state.share_store.get(&uuid).await {
        Ok(Some(share)) if !expired(share.expires_at) => share,
        Ok(Some(share)) => {
            let _ = state.share_store.delete(&share.uuid).await;
            remove_thumbnail(&state.thumb_dir, &share.uuid).await;
            return (StatusCode::NOT_FOUND, "share expired".to_string()).into_response();
        }
        Ok(None) => {
            return (StatusCode::NOT_FOUND, "share not found".to_string()).into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    if !session_allowed(&share, request.headers(), &state).await {
        return (StatusCode::FORBIDDEN, "open the share page before streaming").into_response();
    }

    let path = match resolve_media_path(&state.media_root, &share.rel) {
        Ok(p) => p,
        Err(e) => return (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    };

    match ServeFile::new(path).oneshot(request).await {
        Ok(mut response) => {
            // Prevent revoked shares from being replayed from caches.
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response.map(Body::new)
        }
        Err(never) => match never {},
    }
}

pub async fn share_download(
    AxumPath(uuid): AxumPath<String>,
    State(state): State<AppState>,
    query: Query<DownloadParams>,
    request: Request<Body>,
) -> Response {
    let share = match state.share_store.get(&uuid).await {
        Ok(Some(share)) if !expired(share.expires_at) => share,
        Ok(Some(share)) => {
            let _ = state.share_store.delete(&share.uuid).await;
            remove_thumbnail(&state.thumb_dir, &share.uuid).await;
            return (StatusCode::NOT_FOUND, "share expired".to_string()).into_response();
        }
        Ok(None) => {
            return (StatusCode::NOT_FOUND, "share not found".to_string()).into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    if !session_allowed(&share, request.headers(), &state).await {
        return (StatusCode::FORBIDDEN, "open the share page before downloading").into_response();
    }

    let path = match resolve_media_path(&state.media_root, &share.rel) {
        Ok(p) => p,
        Err(e) => return (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    };

    // `?format=mp4` → transcode to MP4; otherwise serve the original bytes.
    if query.format.as_deref() == Some("mp4") {
        return convert_attachment(path).await;
    }

    // Content-Disposition: attachment forces a download on desktop and mobile.
    crate::download::serve_attachment(path, request).await
}

pub async fn share_thumbnail(
    AxumPath(uuid): AxumPath<String>,
    State(state): State<AppState>,
    request: Request<Body>,
) -> Response {
    let share = match state.share_store.get(&uuid).await {
        Ok(Some(share)) if !expired(share.expires_at) => share,
        Ok(Some(share)) => {
            let _ = state.share_store.delete(&share.uuid).await;
            remove_thumbnail(&state.thumb_dir, &share.uuid).await;
            return (StatusCode::NOT_FOUND, "share expired".to_string()).into_response();
        }
        Ok(None) => {
            return (StatusCode::NOT_FOUND, "share not found".to_string()).into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    let video = match resolve_media_path(&state.media_root, &share.rel) {
        Ok(p) => p,
        Err(e) => return (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    };

    let thumb = state.thumb_dir.join(thumb_name(&share.uuid));

    let needs_generate = match tokio::fs::metadata(&thumb).await {
        Ok(meta) => meta.len() == 0,
        Err(_) => true,
    };

    if needs_generate {
        match tokio::time::timeout(
            Duration::from_secs(15),
            generate_thumbnail(&video, &thumb),
        )
        .await
        {
            Ok(Ok(())) => {}
            _ => {
                return (StatusCode::NOT_FOUND, "thumbnail unavailable".to_string())
                    .into_response();
            }
        }
    }

    match ServeFile::new(thumb).oneshot(request).await {
        Ok(mut response) => {
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400"),
            );
            response.map(Body::new)
        }
        Err(never) => match never {},
    }
}
