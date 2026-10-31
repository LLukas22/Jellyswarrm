//! Application workflows shared by HTTP adapters and future background callers.
//! See [the architecture map](../../ARCHITECTURE.md) for ownership and request paths.
pub(crate) mod accounts;
pub(crate) mod admin;
pub(crate) mod catalog;
pub(crate) mod details;
pub(crate) mod error;
pub(crate) mod fallback;
pub(crate) mod playback;
pub(crate) mod quick_connect;
pub(crate) mod request;
pub(crate) mod services;
pub(crate) mod sessions;
pub(crate) mod syncplay;
