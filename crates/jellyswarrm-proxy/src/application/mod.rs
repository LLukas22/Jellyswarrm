//! Application workflows shared by HTTP adapters and future background callers.
//! See [the architecture map](../../ARCHITECTURE.md) for ownership and request paths.
pub(crate) mod accounts;
pub(crate) mod catalog;
pub(crate) mod context;
pub(crate) mod details;
pub(crate) mod error;
pub(crate) mod identity;
pub(crate) mod input;
pub(crate) mod playback;
pub(crate) mod proxy;
pub(crate) mod response;
pub(crate) mod routing;
pub(crate) mod upstream;
