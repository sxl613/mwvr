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

#[derive(Template)]
#[template(path = "share.html")]
pub struct ShareTemplate {
    pub uuid: String,
    pub name: String,
    pub extension: String,
    pub page_url: String,
    pub thumbnail_url: String,
    pub mime: String,
}

#[derive(Template)]
#[template(path = "shares.html")]
pub struct SharesTemplate {
    pub shares_json: String,
}
