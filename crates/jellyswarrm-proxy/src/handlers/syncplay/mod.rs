//! SyncPlay implementation owned by Jellyswarrm.
//!
//! This module provides:
//! - HTTP routes under `/SyncPlay/*` and `/GetUtcTime`
//! - websocket integration for `/websocket` and `/socket`
//! - an in-memory SyncPlay coordinator (`SyncPlayService`)
//! - wire-compatible DTOs and websocket payload models

pub(crate) mod models;
pub(crate) mod routes;
pub(crate) mod service;

pub(crate) use routes::*;
pub(crate) use service::SyncPlayService;
