use crate::media::MediaFile;
use crate::media::PaginatedMedia;
use crate::{ListParams, SortDirection, SortField};
use askama::Template;

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    pub paginated: PaginatedMedia,
    pub query: ListParams,
}

#[derive(Template)]
#[template(path = "playlist.html")]
pub struct PlaylistTemplate {
    pub playlist_json: String,
    pub current_idx: usize,
    pub video: MediaFile,
    pub sort: SortField,
    pub dir: SortDirection,
    pub search: String,
}
