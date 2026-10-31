//! Incoming HTTP adapters: routing, extraction, middleware, assets, and fallback forwarding.
pub(crate) mod assets;
pub(crate) mod auth_keys;
pub(crate) mod branding;
pub(crate) mod client_routes;
pub(crate) mod error;
pub(crate) mod extractors;
pub(crate) mod fallback;
pub(crate) mod health;
pub(crate) mod middleware;
pub(crate) mod preprocessing;
pub(crate) mod quick_connect;
pub(crate) mod router;
pub(crate) mod sessions;
pub(crate) mod state;
pub(crate) mod syncplay;
