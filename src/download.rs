use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_core::Stream;
use serde::Deserialize;
use tokio::process::{Child, ChildStdout};
use tokio_util::io::ReaderStream;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::media::resolve_media_path;
use crate::AppState;

/// Query params accepted on the download routes.
#[derive(Deserialize)]
pub(crate) struct DownloadParams {
    /// When `mp4`, the file is transcoded to MP4 (H.264 + AAC) on the fly.
    #[serde(default)]
    pub(crate) format: Option<String>,
}

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

/// Swap the extension of a filename for `.mp4` (e.g. `funny.webm` → `funny.mp4`).
fn mp4_filename(name: &str) -> String {
    let path = Path::new(name);
    path.with_extension("mp4")
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{}.mp4", name))
}

/// Streaming wrapper over an ffmpeg stdout pipe. Kills ffmpeg if the response
/// body is dropped (client cancelled, connection reset, …) so no orphaned
/// transcode keeps burning CPU.
struct FfmpegStream {
    inner: ReaderStream<ChildStdout>,
    child: Option<Child>,
}

impl Stream for FfmpegStream {
    type Item = Result<axum::body::Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl Drop for FfmpegStream {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}

/// Arguments for a browser-friendly WebM→MP4 transcode. Fragmented MP4
/// (`empty_moov`) is what makes it possible to stream the output over a pipe:
/// without it ffmpeg buffers everything and only writes the `moov` atom last.
fn ffmpeg_mp4_command(input: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(input)
        .args([
            "-c:v", "libx264",
            "-preset", "veryfast",
            "-crf", "23",
            "-pix_fmt", "yuv420p",
            "-c:a", "aac",
            "-b:a", "128k",
            "-ac", "2",
            "-movflags", "frag_keyframe+empty_moov+faststart",
            "-f", "mp4",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    cmd
}

/// Transcode the file to MP4 with ffmpeg and return it as a streaming
/// attachment. No temp files: output goes straight into the response body.
pub(crate) async fn convert_attachment(path: PathBuf) -> Response {
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("download");

    let mut child = match ffmpeg_mp4_command(&path).spawn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to start ffmpeg: {e}"),
            )
                .into_response();
        }
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "ffmpeg produced no output".to_string(),
            )
                .into_response();
        }
    };

    let stream = FfmpegStream {
        inner: ReaderStream::new(stdout),
        child: Some(child),
    };

    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
    headers.insert(
        header::CONTENT_DISPOSITION,
        attachment_disposition(&mp4_filename(filename)),
    );
    response
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
/// button on the watch page. With `?format=mp4` it transcodes to MP4 instead.
pub(crate) async fn download_media(
    AxumPath(rel): AxumPath<String>,
    State(state): State<AppState>,
    query: Query<DownloadParams>,
    request: Request<Body>,
) -> Response {
    let path = match resolve_media_path(&state.media_root, &rel) {
        Ok(p) => p,
        Err(e) => return (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    };
    if query.format.as_deref() == Some("mp4") {
        return convert_attachment(path).await;
    }
    serve_attachment(path, request).await
}
