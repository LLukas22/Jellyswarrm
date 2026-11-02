//! Proxy-owned SyncPlay coordination and wire models, sharing client-session WebSocket transport.
//!
#![doc = include_str!("README.md")]
mod commands;
pub(crate) mod models;
pub(crate) mod service;
mod transitions;
pub(crate) use service::SyncPlayService;
