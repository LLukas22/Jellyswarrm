//! Translate request/response JSON fields, identifiers and URLs between the virtual
//! server and a selected backend. JSON body framing lives in `protocol::body`.
mod delivery_urls;
pub(crate) mod field_matcher;
mod json_processor;
pub(crate) mod request_analyzer;
pub(crate) mod request_processor;
pub(crate) mod response_processor;
pub(crate) mod url_processor;
mod user_ids;

pub(crate) use json_processor::ProcessingErrors;
pub(crate) use json_processor::{analyze_json, process_json};
pub(crate) use user_ids::UserIdError;

mod pipeline;
pub(crate) use pipeline::ProxyProcessors;

/// A selection the target backend cannot serve because part of it lives on
/// another backend. The message is safe and actionable for clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.message())]
pub(crate) enum MixedServerSelection {
    Playlist,
    Collection,
}

impl MixedServerSelection {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Playlist => "Playlist track or entry is unavailable on the selected server; mixed-server playlists are unsupported",
            Self::Collection => "Collection item is unavailable on the selected server; mixed-server collections are unsupported",
        }
    }
}

/// Translation failures retain causes; application owns their routing/input policy.
#[derive(thiserror::Error)]
pub(crate) enum ProcessingError {
    #[error("request body translation failed")]
    RequestBody(#[source] anyhow::Error),
    #[error("response translation failed")]
    Response(#[source] anyhow::Error),
    #[error("playlist response translation failed")]
    Playlist(#[source] response_processor::PlaylistResponseError),
    #[error("JSON body encoding failed")]
    JsonBody(#[from] crate::protocol::body::JsonBodyError),
}

impl std::fmt::Debug for ProcessingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
