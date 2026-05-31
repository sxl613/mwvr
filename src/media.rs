use std::cmp::Reverse;
use std::path::Path;
use std::time::SystemTime;

use serde::Serialize;
use walkdir::WalkDir;

use crate::{ListParams, SortDirection, SortField};

#[derive(Debug, Clone)]
pub struct MediaFile {
    pub name: String,
    pub path: String, // relative path
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
            e: f.extension.clone(),
            s: f.size,
        })
        .collect();

    (items, current_idx)
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
