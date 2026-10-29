//! Proxy-owned presence, playback metadata and bounded connection delivery.
//! Authentication orchestration belongs to application; incoming adapters to HTTP.
pub(crate) mod models;
mod service;
pub(crate) mod transport;

pub(crate) use service::{ClientSession, ClientSessionService};

/// Authenticated caller data, independent of its HTTP extractor.
#[derive(Clone)]
pub(crate) struct SessionContext {
    pub(crate) user: crate::auth::service::User,
    pub(crate) session_id: String,
}
