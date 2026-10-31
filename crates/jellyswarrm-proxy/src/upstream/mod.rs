//! Target-bound Jellyfin upstream transport: JSON execution, streamed media
//! delivery, HLS playlist adaptation, and hop-by-hop header filtering.
pub(crate) mod delivery;
pub(crate) mod headers;
pub(crate) mod hls;
pub(crate) mod json;
