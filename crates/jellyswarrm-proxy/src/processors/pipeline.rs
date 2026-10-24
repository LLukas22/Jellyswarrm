//! Shared request/response processing with cause-preserving failures. Incoming
//! adapters, not processors, choose client-facing messages and status codes.
use super::{
    request_analyzer::RequestAnalyzer,
    request_processor::{RequestProcessingContext, RequestProcessor},
    response_processor::{ResponseProcessingContext, ResponseProcessingProfile, ResponseProcessor},
    url_processor::UrlProcessor,
};
use crate::upstream::jellyfin::json::set_json_body_typed;
use crate::{application, processors, DataContext};
use tracing::{debug, error};
pub struct ProxyProcessors {
    pub request_processor: RequestProcessor,
    pub request_analyzer: RequestAnalyzer,
    pub response_processor: ResponseProcessor,
    pub url_processor: UrlProcessor,
}

impl ProxyProcessors {
    pub fn new(data_context: DataContext) -> Self {
        Self {
            request_processor: RequestProcessor::new(
                data_context.media_storage.clone(),
                data_context.user_authorization.clone(),
            ),
            request_analyzer: RequestAnalyzer::new(
                data_context.media_storage.clone(),
                data_context.server_storage.clone(),
                data_context.play_sessions.clone(),
                data_context.user_authorization.clone(),
            ),
            response_processor: ResponseProcessor::new(
                data_context.media_storage.clone(),
                data_context.user_authorization.clone(),
            ),
            url_processor: UrlProcessor::from(data_context),
        }
    }

    pub(crate) async fn process_request_body(
        &self,
        request: &mut reqwest::Request,
        context: &RequestProcessingContext,
        request_url: &url::Url,
    ) -> Result<(), application::error::ApplicationError> {
        let Some(mut json_value) = buffered_json(request) else {
            return Ok(());
        };

        let response = processors::process_json(&mut json_value, &self.request_processor, context)
            .await
            .map_err(application::error::ApplicationError::RequestBody)?;

        if response.was_modified {
            debug!("Modified JSON body for request to {}", request_url);
            set_json_body_typed(request, &json_value)?;
        }

        Ok(())
    }

    pub(crate) async fn process_response_json_typed(
        &self,
        payload: &mut serde_json::Value,
        context: &ResponseProcessingContext,
    ) -> Result<bool, application::error::ApplicationError> {
        use application::error::ApplicationError;
        let permissions_modified =
            if context.profile == ResponseProcessingProfile::PlaylistPermissions {
                self.response_processor
                    .remap_playlist_users(payload, &context.server)
                    .await
                    .map_err(|error| {
                        error!("Failed to process playlist sharing response: {error}");
                        ApplicationError::PlaylistResponse(error)
                    })?
            } else {
                false
            };
        let processed = processors::process_json(payload, &self.response_processor, context)
            .await
            .map_err(ApplicationError::Internal)?;

        Ok(permissions_modified || processed.was_modified)
    }
}

/// Non-JSON/unbuffered/malformed bodies are opaque forwarding data, matching the
/// proxy's best-effort body contract. This helper has no incoming HTTP adapter.
fn buffered_json(request: &reqwest::Request) -> Option<serde_json::Value> {
    let content_type = request
        .headers()
        .get(reqwest::header::CONTENT_TYPE)?
        .to_str()
        .ok()?;
    if !content_type.contains("application/json") {
        return None;
    }
    serde_json::from_slice(request.body()?.as_bytes()?).ok()
}
