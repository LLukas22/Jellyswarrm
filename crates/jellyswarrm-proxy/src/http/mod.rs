//! Incoming HTTP adapters: routing, extraction, middleware, assets, and the catch-all forwarding handler.
pub(crate) mod assets;
pub(crate) mod auth_keys;
pub(crate) mod branding;
pub(crate) mod client_routes;
pub(crate) mod client_sessions;
pub(crate) mod error;
pub(crate) mod extractors;
pub(crate) mod forwarding;
pub(crate) mod health;
pub(crate) mod incoming;
pub(crate) mod middleware;
pub(crate) mod quick_connect;
pub(crate) mod router;
pub(crate) mod state;
pub(crate) mod syncplay;
