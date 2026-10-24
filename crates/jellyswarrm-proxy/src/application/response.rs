//! Shared response processing: project upstream media into client identities and
//! record returned media for client-session observation. Callers choose when to
//! observe; projection alone never publishes a playback binding.

use crate::{
    application::error::ApplicationError,
    processors::response_processor::{
        ResponseProcessingContext, ResponseProcessingProfile, ResponseProcessor,
    },
    servers::storage::Server,
};

pub(crate) async fn project_media_json(
    processor: &ResponseProcessor,
    config: &crate::config::SettingsView,
    payload: &mut serde_json::Value,
    server: &Server,
    profile: ResponseProcessingProfile,
    proxy_api_key: Option<&str>,
) -> Result<bool, ApplicationError> {
    let context = ResponseProcessingContext {
        server: server.clone(),
        proxy_server_id: config.read().await.server_id.clone(),
        proxy_api_key: proxy_api_key.map(str::to_owned),
        profile,
    };
    let response = crate::processors::process_json(payload, processor, &context)
        .await
        .map_err(ApplicationError::Internal)?;
    Ok(response.was_modified)
}

pub(crate) async fn media_response(
    sessions: &crate::sessions::ClientSessionService,
    token: Option<&str>,
    payload: &serde_json::Value,
) {
    if let Some(token) = token {
        sessions.cache_media_response(token, payload).await;
    }
}
