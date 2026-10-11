//! Shareable playlists: an ordered list of media files behind a public,
//! UUID-gated `/playlist/{uuid}` URL. Viewing is open to anyone holding the
//! link; every mutation (create, rename, reorder, add, remove, delete) needs
//! the `PLAYLIST_PASSWORD` passphrase in the `X-Playlist-Password` header.

use std::cmp::Reverse;
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
use uuid::Uuid;

use crate::download::{convert_attachment, serve_attachment, serve_playable, DownloadParams};
use crate::media::{resolve_media_path, serve_media_thumbnail, MediaFile};
use crate::templates::{PlaylistShareTemplate, PlaylistsTemplate};
use crate::AppState;

const PASSWORD_HEADER: &str = "x-playlist-password";
const MAX_NAME_LEN: usize = 200;
const MAX_ITEMS: usize = 5000;

#[derive(Debug, Clone, Serialize)]
pub struct Playlist {
    pub uuid: String,
    pub name: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub items: Vec<String>, // rels, in play order
}

/// JSON view for the management page / API.
#[derive(Debug, Clone, Serialize)]
pub struct PlaylistView {
    pub uuid: String,
    pub name: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub count: usize,
    pub items: Vec<String>,
    pub url: String,
}

/// One playlist entry as embedded in the public page (short keys mirror
/// `media::PlaylistItem`).
#[derive(Debug, Clone, Serialize)]
pub struct PlaylistEntry {
    pub n: String, // name
    pub r: String, // rel
    pub e: String, // extension
    pub s: u64,    // size
    pub m: bool,   // missing from the library
}

impl Playlist {
    pub fn view(&self, public_base_url: &str) -> PlaylistView {
        PlaylistView {
            uuid: self.uuid.clone(),
            name: self.name.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            count: self.items.len(),
            items: self.items.clone(),
            url: playlist_url(public_base_url, &self.uuid),
        }
    }
}

pub fn playlist_url(public_base_url: &str, uuid: &str) -> String {
    format!("{}/playlist/{}", public_base_url.trim_end_matches('/'), uuid)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone)]
pub struct PlaylistStore {
    conn: Arc<Mutex<Connection>>,
}

impl PlaylistStore {
    /// Opens (a second connection to) the share database; WAL mode lets both
    /// stores coexist on the same file.
    pub async fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS playlists (
                 uuid        TEXT PRIMARY KEY,
                 name        TEXT NOT NULL,
                 created_at  INTEGER NOT NULL,
                 updated_at  INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS playlist_items (
                 playlist_uuid TEXT NOT NULL,
                 position      INTEGER NOT NULL,
                 rel           TEXT NOT NULL,
                 PRIMARY KEY (playlist_uuid, rel)
             );
             CREATE INDEX IF NOT EXISTS idx_playlist_items_pos
                 ON playlist_items(playlist_uuid, position);",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn load(conn: &Connection, uuid: &str) -> Result<Option<Playlist>, rusqlite::Error> {
        let head = conn
            .query_row(
                "SELECT uuid, name, created_at, updated_at FROM playlists WHERE uuid = ?1",
                rusqlite::params![uuid],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)? as u64,
                        row.get::<_, i64>(3)? as u64,
                    ))
                },
            )
            .optional()?;
        let Some((uuid, name, created_at, updated_at)) = head else {
            return Ok(None);
        };
        let mut stmt = conn.prepare(
            "SELECT rel FROM playlist_items WHERE playlist_uuid = ?1 ORDER BY position",
        )?;
        let items = stmt
            .query_map(rusqlite::params![uuid], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(Playlist {
            uuid,
            name,
            created_at,
            updated_at,
            items,
        }))
    }

    fn write_items(tx: &rusqlite::Transaction, uuid: &str, items: &[String]) -> Result<(), rusqlite::Error> {
        tx.execute(
            "DELETE FROM playlist_items WHERE playlist_uuid = ?1",
            rusqlite::params![uuid],
        )?;
        let mut stmt = tx.prepare(
            "INSERT INTO playlist_items (playlist_uuid, position, rel) VALUES (?1, ?2, ?3)",
        )?;
        for (position, rel) in items.iter().enumerate() {
            stmt.execute(rusqlite::params![uuid, position as i64, rel])?;
        }
        Ok(())
    }

    pub async fn create(&self, name: &str, items: &[String]) -> Result<Playlist, rusqlite::Error> {
        let now = now_unix();
        let playlist = Playlist {
            uuid: Uuid::new_v4().to_string(),
            name: name.to_string(),
            created_at: now,
            updated_at: now,
            items: items.to_vec(),
        };
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO playlists (uuid, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![playlist.uuid, playlist.name, now as i64, now as i64],
        )?;
        Self::write_items(&tx, &playlist.uuid, &playlist.items)?;
        tx.commit()?;
        Ok(playlist)
    }

    pub async fn get(&self, uuid: &str) -> Result<Option<Playlist>, rusqlite::Error> {
        let conn = self.conn.lock().await;
        Self::load(&conn, uuid)
    }

    pub async fn list(&self) -> Result<Vec<Playlist>, rusqlite::Error> {
        let conn = self.conn.lock().await;
        let uuids = {
            let mut stmt = conn.prepare("SELECT uuid FROM playlists ORDER BY updated_at DESC")?;
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut playlists = Vec::with_capacity(uuids.len());
        for uuid in uuids {
            if let Some(playlist) = Self::load(&conn, &uuid)? {
                playlists.push(playlist);
            }
        }
        Ok(playlists)
    }

    pub async fn contains(&self, uuid: &str, rel: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock().await;
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlist_items WHERE playlist_uuid = ?1 AND rel = ?2)",
            rusqlite::params![uuid, rel],
            |row| row.get(0),
        )
    }

    /// Apply an edit; `None` fields are left untouched. Returns `None` when
    /// the playlist does not exist.
    pub async fn update(
        &self,
        uuid: &str,
        name: Option<&str>,
        items: Option<&[String]>,
    ) -> Result<Option<Playlist>, rusqlite::Error> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE playlists SET name = COALESCE(?2, name), updated_at = ?3 WHERE uuid = ?1",
            rusqlite::params![uuid, name, now_unix() as i64],
        )?;
        if updated == 0 {
            return Ok(None);
        }
        if let Some(items) = items {
            Self::write_items(&tx, uuid, items)?;
        }
        tx.commit()?;
        Self::load(&conn, uuid)
    }

    pub async fn delete(&self, uuid: &str) -> Result<bool, rusqlite::Error> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM playlist_items WHERE playlist_uuid = ?1",
            rusqlite::params![uuid],
        )?;
        let n = tx.execute("DELETE FROM playlists WHERE uuid = ?1", rusqlite::params![uuid])?;
        tx.commit()?;
        Ok(n > 0)
    }
}

#[derive(Debug)]
pub struct ApiError(StatusCode, String);

impl ApiError {
    fn internal<E: std::fmt::Display>(e: E) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
    fn not_found() -> Self {
        ApiError(StatusCode::NOT_FOUND, "playlist not found".into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Gate for every playlist mutation. Editing is disabled outright when no
/// passphrase is configured; wrong guesses are slowed down.
async fn require_password(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = state.playlist_password.as_deref() else {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "playlist editing is disabled (PLAYLIST_PASSWORD is not set)".into(),
        ));
    };
    let given = headers
        .get(PASSWORD_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if constant_time_eq(given.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        tokio::time::sleep(Duration::from_millis(750)).await;
        Err(ApiError(StatusCode::UNAUTHORIZED, "wrong password".into()))
    }
}

fn clean_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "name must not be empty".into()));
    }
    Ok(name.chars().take(MAX_NAME_LEN).collect())
}

/// Validate and dedupe the requested rels against the media root, keeping the
/// first occurrence's position.
fn clean_items(state: &AppState, rels: &[String]) -> Result<Vec<String>, ApiError> {
    if rels.len() > MAX_ITEMS {
        return Err(ApiError(StatusCode::BAD_REQUEST, "too many items".into()));
    }
    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::with_capacity(rels.len());
    for rel in rels {
        if !seen.insert(rel.as_str()) {
            continue;
        }
        resolve_media_path(&state.media_root, rel)
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{rel}: {e}")))?;
        items.push(rel.clone());
    }
    Ok(items)
}

#[derive(Deserialize)]
pub struct CreatePlaylistRequest {
    pub name: String,
    #[serde(default)]
    pub items: Vec<String>,
}

pub async fn create_playlist(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreatePlaylistRequest>,
) -> Result<Json<PlaylistView>, ApiError> {
    require_password(&state, &headers).await?;
    let name = clean_name(&req.name)?;
    let items = clean_items(&state, &req.items)?;
    let playlist = state
        .playlist_store
        .create(&name, &items)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(playlist.view(&state.public_base_url)))
}

pub async fn list_playlists(
    State(state): State<AppState>,
) -> Result<Json<Vec<PlaylistView>>, ApiError> {
    let playlists = state.playlist_store.list().await.map_err(ApiError::internal)?;
    Ok(Json(
        playlists
            .iter()
            .map(|p| p.view(&state.public_base_url))
            .collect(),
    ))
}

pub async fn playlists_page(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    let views: Vec<PlaylistView> = state
        .playlist_store
        .list()
        .await
        .map_err(ApiError::internal)?
        .iter()
        .map(|p| p.view(&state.public_base_url))
        .collect();
    PlaylistsTemplate {
        playlists_json: script_json(&views)?,
        editing_enabled: state.playlist_password.is_some(),
        analytics_tag: state.analytics_tag.clone(),
    }
    .render()
    .map(Html)
    .map_err(ApiError::internal)
}

#[derive(Deserialize)]
pub struct UpdatePlaylistRequest {
    pub name: Option<String>,
    /// Full replacement list, in play order (covers reorder and remove).
    pub items: Option<Vec<String>>,
    /// Appended to the end, skipping rels already present.
    #[serde(default)]
    pub add: Vec<String>,
    /// Removed if present.
    #[serde(default)]
    pub remove: Vec<String>,
}

pub async fn update_playlist(
    AxumPath(uuid): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpdatePlaylistRequest>,
) -> Result<Json<PlaylistView>, ApiError> {
    require_password(&state, &headers).await?;
    let name = req.name.as_deref().map(clean_name).transpose()?;

    let items = if req.items.is_some() || !req.add.is_empty() || !req.remove.is_empty() {
        let existing = state
            .playlist_store
            .get(&uuid)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(ApiError::not_found)?
            .items;
        let mut items = req.items.unwrap_or_else(|| existing.clone());
        items.extend(req.add);
        items.retain(|rel| !req.remove.contains(rel));
        // Only newly added rels are checked against disk: an existing entry
        // whose file has since vanished must not block editing the rest.
        let fresh: Vec<String> = items
            .iter()
            .filter(|rel| !existing.contains(rel))
            .cloned()
            .collect();
        clean_items(&state, &fresh)?;
        let mut seen = std::collections::HashSet::new();
        items.retain(|rel| seen.insert(rel.clone()));
        if items.len() > MAX_ITEMS {
            return Err(ApiError(StatusCode::BAD_REQUEST, "too many items".into()));
        }
        Some(items)
    } else {
        None
    };

    let playlist = state
        .playlist_store
        .update(&uuid, name.as_deref(), items.as_deref())
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(playlist.view(&state.public_base_url)))
}

pub async fn delete_playlist(
    AxumPath(uuid): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    require_password(&state, &headers).await?;
    if state.playlist_store.delete(&uuid).await.map_err(ApiError::internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

/// Lets the page verify a passphrase before entering edit mode.
pub async fn unlock_playlist(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    require_password(&state, &headers).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct LibraryParams {
    #[serde(default)]
    pub query: String,
}

#[derive(Serialize)]
pub struct LibraryItem {
    pub n: String,
    pub r: String,
    pub e: String,
    pub s: u64,
}

/// Library search for the "add videos" panel on the public playlist page.
/// Exposes file names, so it sits behind the edit password.
pub async fn playlist_library(
    AxumPath(uuid): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<LibraryParams>,
) -> Result<Json<Vec<LibraryItem>>, ApiError> {
    require_password(&state, &headers).await?;
    state
        .playlist_store
        .get(&uuid)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(ApiError::not_found)?;

    let query = params.query.to_lowercase();
    let files = state.cache.read().await;
    let mut matches: Vec<&MediaFile> = files
        .iter()
        .filter(|f| query.is_empty() || f.name.to_lowercase().contains(&query))
        .collect();
    matches.sort_by_key(|f| Reverse(f.modified));
    Ok(Json(
        matches
            .into_iter()
            .take(100)
            .map(|f| LibraryItem {
                n: f.name.clone(),
                r: f.rel.clone(),
                e: f.extension.clone(),
                s: f.size,
            })
            .collect(),
    ))
}

/// Serialize for embedding inside `<script type="application/json">`; `<` is
/// escaped so a file name can never close the script element.
fn script_json<T: Serialize>(value: &T) -> Result<String, ApiError> {
    serde_json::to_string(value)
        .map(|s| s.replace('<', "\\u003c"))
        .map_err(ApiError::internal)
}

fn private_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(
        HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static("noindex, nofollow"),
    );
    headers
}

fn encode_path(rel: &str) -> String {
    rel.split('/')
        .map(|segment| {
            segment
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                        (b as char).to_string()
                    } else {
                        format!("%{:02X}", b)
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub async fn playlist_page(
    AxumPath(uuid): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Response, ApiError> {
    let playlist = state
        .playlist_store
        .get(&uuid)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(ApiError::not_found)?;

    let entries: Vec<PlaylistEntry> = {
        let files = state.cache.read().await;
        playlist
            .items
            .iter()
            .map(|rel| {
                let indexed = files.iter().find(|f| &f.rel == rel);
                let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
                let extension = name
                    .rsplit_once('.')
                    .map(|(_, ext)| ext.to_lowercase())
                    .unwrap_or_default();
                PlaylistEntry {
                    m: indexed.is_none() && resolve_media_path(&state.media_root, rel).is_err(),
                    s: indexed.map(|f| f.size).unwrap_or(0),
                    n: name,
                    e: extension,
                    r: rel.clone(),
                }
            })
            .collect()
    };

    let page_url = playlist_url(&state.public_base_url, &playlist.uuid);
    let thumbnail_url = entries
        .iter()
        .find(|e| !e.m)
        .map(|e| format!("{}/thumbnail/{}", page_url, encode_path(&e.r)))
        .unwrap_or_default();

    let html = PlaylistShareTemplate {
        uuid: playlist.uuid.clone(),
        name: playlist.name.clone(),
        count: entries.len(),
        page_url,
        thumbnail_url,
        items_json: script_json(&entries)?,
        editing_enabled: state.playlist_password.is_some(),
        analytics_tag: state.analytics_tag.clone(),
    }
    .render()
    .map_err(ApiError::internal)?;

    Ok((private_headers(), Html(html)).into_response())
}

/// Resolve `rel` only if it is part of the playlist, so the public routes can
/// never be used to reach arbitrary library files.
async fn member_path(
    state: &AppState,
    uuid: &str,
    rel: &str,
) -> Result<std::path::PathBuf, Response> {
    match state.playlist_store.contains(uuid, rel).await {
        Ok(true) => {}
        Ok(false) => return Err((StatusCode::NOT_FOUND, "not in playlist").into_response()),
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()),
    }
    resolve_media_path(&state.media_root, rel)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()).into_response())
}

pub async fn playlist_stream(
    AxumPath((uuid, rel)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    request: Request<Body>,
) -> Response {
    let path = match member_path(&state, &uuid, &rel).await {
        Ok(path) => path,
        Err(response) => return response,
    };
    let mut response = serve_playable(path, request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub async fn playlist_download(
    AxumPath((uuid, rel)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    query: Query<DownloadParams>,
    request: Request<Body>,
) -> Response {
    let path = match member_path(&state, &uuid, &rel).await {
        Ok(path) => path,
        Err(response) => return response,
    };
    if query.format.as_deref() == Some("mp4") {
        return convert_attachment(path).await;
    }
    serve_attachment(path, request).await
}

pub async fn playlist_thumbnail(
    AxumPath((uuid, rel)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    request: Request<Body>,
) -> Response {
    if let Err(response) = member_path(&state, &uuid, &rel).await {
        return response;
    }
    serve_media_thumbnail(&state, &rel, request).await
}
