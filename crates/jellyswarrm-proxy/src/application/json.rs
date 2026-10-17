//! Shared media projection after wire execution. Catalog and playback profiles
//! do not include playlist ACL processing.

use crate::{
    application::error::ApplicationError,
    processors::response_processor::{
        ResponseProcessingContext, ResponseProcessingProfile, ResponseProcessor,
    },
    server_storage::Server,
};

pub(crate) async fn project_media_json(
    processor: &ResponseProcessor,
    config: &tokio::sync::RwLock<crate::config::AppConfig>,
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
