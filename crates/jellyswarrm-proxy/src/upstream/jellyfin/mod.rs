//! Outgoing Jellyfin request construction, JSON transport, streaming delivery, and HLS adaptation.
pub mod delivery;
pub mod hls;
pub mod json;
pub(crate) mod request;
pub(crate) mod transport;
