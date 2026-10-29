//! Translation failures retain causes; application owns their routing/input policy.
#[derive(thiserror::Error)]
pub(crate) enum ProcessingError {
    #[error("request body translation failed")]
    RequestBody(#[source] anyhow::Error),
    #[error("response translation failed")]
    Response(#[source] anyhow::Error),
    #[error("playlist response translation failed")]
    Playlist(#[source] super::response_processor::PlaylistResponseError),
    #[error("JSON body encoding failed")]
    JsonBody(#[from] crate::outbound::json::JsonBodyError),
}

impl std::fmt::Debug for ProcessingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
