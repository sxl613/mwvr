use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::media::resolve_media_path;
use crate::AppState;

/// Build a Content-Disposition header that forces a download. Provides an
/// ASCII fallback name plus an RFC 5987 UTF-8-encoded name so non-ASCII
/// filenames survive as well.
fn attachment_disposition(filename: &str) -> HeaderValue {
    let ascii: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii() && !matches!(c, '"' | '\\' | '\r' | '\n') {
                c
            } else {
                '_'
            }
        })
        .collect();

    let utf8: String = filename
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{:02X}", b)
            }
        })
        .collect();

    HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        ascii, utf8
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// Stream a file with `Content-Disposition: attachment` so the browser
/// downloads it (with a progress UI) instead of streaming it inline.
/// Works on desktop and mobile Safari/Chrome alike — unlike the HTML
/// `download` attribute, which iOS ignores.
pub(crate) async fn serve_attachment(path: PathBuf, request: Request<Body>) -> Response {
    match ServeFile::new(path.clone()).oneshot(request).await {
        Ok(mut response) => {
            let filename = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("download");
            response.headers_mut().insert(
                header::CONTENT_DISPOSITION,
                attachment_disposition(filename),
            );
            response.map(Body::new)
        }
        Err(never) => match never {},
    }
}

/// Private download route: `/download/{*rest}` serves the file at `rel`
/// (path relative to MEDIA_PATH) as an attachment, e.g. the "download"
/// button on the watch page.
pub(crate) async fn download_media(
    AxumPath(rel): AxumPath<String>,
    State(state): State<AppState>,
    request: Request<Body>,
) -> Response {
    let path = match resolve_media_path(&state.media_root, &rel) {
        Ok(p) => p,
        Err(e) => return (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    };
    serve_attachment(path, request).await
}