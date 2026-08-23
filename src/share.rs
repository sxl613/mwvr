use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use askama::Template;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

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
}

/// Flat, JSON/template-friendly view of a share including its public URL.
#[derive(Debug, Clone, Serialize)]
pub struct ShareView {
    pub uuid: String,
    pub name: String,
    pub rel: String,
    pub created_at: u64,
    pub expires_at: Option<u64>,
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
                 expires_at  INTEGER
             );
             CREATE INDEX IF NOT EXISTS idx_shares_expires_at ON shares(expires_at);",
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
    ) -> Result<Share, rusqlite::Error> {
        let share = Share {
            uuid: Uuid::new_v4().to_string(),
            name: name.to_string(),
            rel: rel.to_string(),
            created_at: now_unix(),
            expires_at,
        };

        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO shares (uuid, name, rel, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                share.uuid,
                share.name,
                share.rel,
                share.created_at as i64,
                share.expires_at.map(|v| v as i64),
            ],
        )?;
        Ok(share)
    }

    pub async fn get(&self, uuid: &str) -> Result<Option<Share>, rusqlite::Error> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT uuid, name, rel, created_at, expires_at FROM shares WHERE uuid = ?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![uuid], |row| {
            Ok(Share {
                uuid: row.get(0)?,
                name: row.get(1)?,
                rel: row.get(2)?,
                created_at: row.get::<_, i64>(3)? as u64,
                expires_at: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
            })
        })?;
        rows.next().transpose()
    }

    pub async fn list(&self) -> Result<Vec<Share>, rusqlite::Error> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT uuid, name, rel, created_at, expires_at FROM shares ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Share {
                uuid: row.get(0)?,
                name: row.get(1)?,
                rel: row.get(2)?,
                created_at: row.get::<_, i64>(3)? as u64,
                expires_at: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
            })
        })?;
        rows.collect()
    }

    pub async fn delete(&self, uuid: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock().await;
        let n = conn.execute("DELETE FROM shares WHERE uuid = ?1", rusqlite::params![uuid])?;
        Ok(n > 0)
    }

    pub async fn purge_expired(&self) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "DELETE FROM shares WHERE expires_at IS NOT NULL AND expires_at < ?1",
            rusqlite::params![now_unix() as i64],
        )?;
        Ok(n)
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

    let expires_at = req
        .expires_in
        .filter(|ttl| *ttl > 0)
        .map(|ttl| now_unix().saturating_add(ttl));

    let share = state
        .share_store
        .create(&req.rel, &name, expires_at)
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(share.view(&state.public_base_url)))
}

pub async fn list_shares(State(state): State<AppState>) -> Result<Json<Vec<ShareView>>, ApiError> {
    let mut views = Vec::new();

    for share in state.share_store.list().await.map_err(ApiError::internal)? {
        if expired(share.expires_at) {
            let _ = state.share_store.delete(&share.uuid).await;
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
            continue;
        }
        views.push(share.view(&state.public_base_url));
    }

    let shares_json =
        serde_json::to_string(&views).map_err(|e| ApiError::internal(e.to_string()))?;

    SharesTemplate { shares_json }
        .render()
        .map(Html)
        .map_err(ApiError::internal)
}

pub async fn share_page(
    AxumPath(uuid): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Response, ApiError> {
    let share = state
        .share_store
        .get(&uuid)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "share not found".into()))?;

    if expired(share.expires_at) {
        let _ = state.share_store.delete(&uuid).await;
        return Err(ApiError(StatusCode::NOT_FOUND, "share expired".into()));
    }

    let extension = share
        .name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_lowercase())
        .unwrap_or_else(|| "mp4".into());

    let html = ShareTemplate {
        uuid: share.uuid,
        name: share.name,
        extension,
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
            return (StatusCode::NOT_FOUND, "share expired".to_string()).into_response();
        }
        Ok(None) => {
            return (StatusCode::NOT_FOUND, "share not found".to_string()).into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

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
