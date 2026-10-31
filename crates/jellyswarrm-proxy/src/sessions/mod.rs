//! Proxy-owned presence, playback metadata and bounded connection delivery.
//! Authentication orchestration belongs to application; incoming adapters to HTTP.
pub(crate) mod connections;
pub(crate) mod models;
mod service;

pub(crate) use service::{ClientSession, ClientSessionService};

/// Authenticated caller data, independent of its HTTP extractor.
#[derive(Clone)]
pub(crate) struct SessionCaller {
    pub(crate) user: crate::auth::service::LocalUser,
    pub(crate) session_id: String,
}
