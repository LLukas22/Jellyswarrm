//! Incoming Jellyfin HTTP adapters that delegate to application workflows and feature services.
pub(crate) mod auth_keys;
pub(crate) mod branding;
pub(crate) mod federated;
pub(crate) mod items;
pub(crate) mod livestreams;
mod pipeline_error;
pub(crate) mod quick_connect;
pub(crate) mod syncplay;
pub(crate) mod system;
pub(crate) mod users;
pub(crate) mod videos;
