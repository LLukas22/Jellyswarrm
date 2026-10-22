//! Proxy-owned SyncPlay coordination and wire models, sharing client-session WebSocket transport.
mod commands;
pub(crate) mod models;
pub(crate) mod service;
mod transitions;
pub(crate) use service::SyncPlayService;
