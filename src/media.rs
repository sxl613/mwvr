use std::cmp::Reverse;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde::Deserialize;
use tokio::process::Command;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use walkdir::WalkDir;

use crate::{AppState, ListParams, SortDirection, SortField};

#[derive(Debug, Clone, Serialize)]
pub struct MediaMetadata {
    pub duration_seconds: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CachedMediaMetadata {
    pub size: u64,
    pub modified_ns: Option<u128>,
    pub info: MediaMetadata,
}

#[derive(Deserialize)]
struct ProbeOutput {
    streams: Option<Vec<ProbeStream>>,
    format: Option<ProbeFormat>,
}

#[derive(Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

async fn probe_metadata(path: &Path) -> std::io::Result<MediaMetadata> {
    let output = Command::new("ffprobe")
        .args([
            "-v", "error", "-show_entries",
            "format=duration:stream=codec_type,codec_name,width,height",
            "-of", "json",
        ])
        .arg(path)
        .output()
        .await?;
    if !output.status.success() {
        return Err(std::io::Error::other("ffprobe could not read this media file"));
    }

    let probe: ProbeOutput = serde_json::from_slice(&output.stdout)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let streams = probe.streams.unwrap_or_default();
    let video = streams.iter().find(|s| s.codec_type.as_deref() == Some("video"));
    let audio = streams.iter().find(|s| s.codec_type.as_deref() == Some("audio"));
    let duration_seconds = probe
        .format
        .and_then(|f| f.duration)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0);

    Ok(MediaMetadata {
        duration_seconds,
        width: video.and_then(|s| s.width),
        height: video.and_then(|s| s.height),
        video_codec: video.and_then(|s| s.codec_name.clone()),
        audio_codec: audio.and_then(|s| s.codec_name.clone()),
    })
}

fn file_signature(metadata: &std::fs::Metadata) -> (u64, Option<u128>) {
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    (metadata.len(), modified_ns)
}

pub async fn media_info(
    AxumPath(rel): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<MediaMetadata>, (StatusCode, String)> {
    let path = super::media::resolve_media_path(&state.media_root, &rel)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let (size, modified_ns) = file_signature(&metadata);

    if let Some(cached) = state.media_metadata.read().await.get(&rel)
        && cached.size == size && cached.modified_ns == modified_ns
    {
        return Ok(Json(cached.info.clone()));
    }

    let info = probe_metadata(&path)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    state.media_metadata.write().await.insert(
        rel,
        CachedMediaMetadata { size, modified_ns, info: info.clone() },
    );
    Ok(Json(info))
}

pub async fn media_thumbnail(
    AxumPath(rel): AxumPath<String>,
    State(state): State<AppState>,
    request: axum::http::Request<Body>,
) -> Response {
    let path = match resolve_media_path(&state.media_root, &rel) {
        Ok(path) => path,
        Err(error) => return (StatusCode::NOT_FOUND, error.to_string()).into_response(),
    };
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) => return (StatusCode::NOT_FOUND, error.to_string()).into_response(),
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rel.hash(&mut hasher);
    let (size, modified_ns) = file_signature(&metadata);
    size.hash(&mut hasher);
    modified_ns.hash(&mut hasher);
    let thumb = state.thumb_dir.join(format!("media-{:016x}.jpg", hasher.finish()));
    let fresh = match tokio::fs::metadata(&thumb).await {
        Ok(cached) if cached.len() > 0 => match (metadata.modified(), cached.modified()) {
            (Ok(source), Ok(cached)) => cached >= source,
            _ => true,
        },
        _ => false,
    };

    if !fresh {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            crate::share::generate_thumbnail(&path, &thumb),
        ).await;
        if !matches!(result, Ok(Ok(()))) {
            return (StatusCode::BAD_GATEWAY, "thumbnail generation failed").into_response();
        }
    }

    match ServeFile::new(thumb).oneshot(request).await {
        Ok(mut response) => {
            response.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("public, max-age=86400"),
            );
            response.map(Body::new)
        }
        Err(never) => match never {},
    }
}

#[derive(Debug, Clone)]
pub struct MediaFile {
    pub name: String,
    pub path: String, // URL path, e.g. /media/clips/funny.webm
    pub rel: String,  // path relative to MEDIA_PATH, e.g. clips/funny.webm
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    pub extension: String,
}

#[derive(Debug, Clone)]
pub struct PaginatedMedia {
    pub total: usize,
    pub total_pages: usize,
    pub page: usize,
    pub files: Vec<MediaFile>,
}

pub fn build_index(media_path: &Path) -> Vec<MediaFile> {
    let mut files = Vec::new();
    let valid_extensions = ["mp4", "webm", "mkv", "avi", "mov"];

    for entry in WalkDir::new(media_path)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();

        if !valid_extensions.contains(&extension.as_str()) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let relative_path = path
            .strip_prefix(media_path)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string();

        files.push(MediaFile {
            name,
            path: format!("/media/{}", relative_path),
            rel: relative_path,
            size: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            extension,
        });
    }
    files
}

pub fn list_media_files(all_files: &[MediaFile], params: &ListParams) -> PaginatedMedia {
    let query = if params.query.is_empty() {
        None
    } else {
        Some(params.query.to_lowercase())
    };

    let mut files: Vec<MediaFile> = all_files
        .iter()
        .filter(|f| {
            query
                .as_deref()
                .map_or(true, |q| f.name.to_lowercase().contains(q))
        })
        .cloned()
        .collect();

    match params.sort {
        SortField::Name => files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
        SortField::Size => files.sort_by_key(|f| f.size),
        SortField::Created => files.sort_by(|a, b| a.created.cmp(&b.created)),
        SortField::LastModified => files.sort_by_key(|f| Reverse(f.modified)),
    };
    if matches!(params.dir, SortDirection::Desc) {
        files.reverse();
    }
    let page_size = (params.page_size as usize).max(1);
    let total_pages = (files.len() + page_size - 1) / page_size.max(1);
    let page = (params.page as usize).clamp(1, total_pages.max(1));
    let start = (page - 1) * page_size;
    let end = (start + page_size).min(files.len());
    PaginatedMedia {
        total: files.len(),
        total_pages: total_pages,
        page: page,
        files: files[start..end].to_vec(),
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct PlaylistItem {
    pub n: String, // name
    pub p: String, // path
    pub r: String, // rel (path relative to MEDIA_PATH)
    pub e: String, // extension
    pub s: u64,    // size
}

pub fn build_playlist(
    all_files: &[MediaFile],
    search: &str,
    current: &str,
    sort: SortField,
    dir: SortDirection,
) -> (Vec<PlaylistItem>, usize) {
    let q = if search.is_empty() {
        None
    } else {
        Some(search.to_lowercase())
    };

    let mut files: Vec<&MediaFile> = all_files
        .iter()
        .filter(|f| {
            q.as_deref()
                .map_or(true, |q| f.name.to_lowercase().contains(q))
        })
        .collect();

    match sort {
        SortField::Name => files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
        SortField::Size => files.sort_by_key(|f| f.size),
        SortField::Created => files.sort_by(|a, b| a.created.cmp(&b.created)),
        SortField::LastModified => files.sort_by_key(|f| Reverse(f.modified)),
    }
    if matches!(dir, SortDirection::Desc) {
        files.reverse();
    }

    let current_idx = files.iter().position(|f| f.name == current).unwrap_or(0);

    let items: Vec<PlaylistItem> = files
        .iter()
        .map(|f| PlaylistItem {
            n: f.name.clone(),
            p: f.path.clone(),
            r: f.rel.clone(),
            e: f.extension.clone(),
            s: f.size,
        })
        .collect();

    (items, current_idx)
}

#[derive(Debug)]
pub enum ResolveError {
    Io(std::io::Error),
    Traversal,
    NotAFile,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Io(e) => write!(f, "io error: {e}"),
            ResolveError::Traversal => write!(f, "path traversal detected"),
            ResolveError::NotAFile => write!(f, "not a file"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Resolve a share-relative path against the media root, rejecting anything that
/// escapes the root (path traversal) or is not a regular file.
pub fn resolve_media_path(media_root: &Path, rel: &str) -> Result<PathBuf, ResolveError> {
    let root = media_root.canonicalize().map_err(ResolveError::Io)?;
    let candidate = root.join(rel).canonicalize().map_err(ResolveError::Io)?;
    if !candidate.starts_with(&root) {
        return Err(ResolveError::Traversal);
    }
    if !candidate.is_file() {
        return Err(ResolveError::NotAFile);
    }
    Ok(candidate)
}

pub fn format_size(bytes: &u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut size = bytes.clone() as f64;
    let mut unit_idx = 0;

    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    format!("{:.1} {}", size, UNITS[unit_idx])
}
