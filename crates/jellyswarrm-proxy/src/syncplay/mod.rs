//! Proxy-owned SyncPlay coordination and wire models, sharing client-session WebSocket transport.
pub(crate) mod models;
pub(crate) mod service;
pub(crate) use service::SyncPlayService;
