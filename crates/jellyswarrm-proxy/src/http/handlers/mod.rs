//! Incoming Jellyfin HTTP adapters with their own request/response shapes. Client
//! library/playback adapters live next to their routes in `http::client_routes`.
pub(crate) mod auth_keys;
pub(crate) mod branding;
pub(crate) mod quick_connect;
pub(crate) mod syncplay;
