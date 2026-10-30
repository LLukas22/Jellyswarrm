//! Shared request/response processing with cause-preserving failures. Incoming
//! adapters, not processors, choose client-facing messages and status codes.
use super::ProcessingError;
use super::{
    request_analyzer::RequestAnalyzer,
    request_processor::{RequestProcessingContext, RequestProcessor},
    response_processor::{ResponseProcessingContext, ResponseProcessingProfile, ResponseProcessor},
    url_processor::UrlProcessor,
};
use crate::processors;
use crate::processors::set_json_body;
use tracing::{debug, error};
pub(crate) struct ProxyProcessors {
    pub(crate) request_processor: RequestProcessor,
    pub(crate) request_analyzer: RequestAnalyzer,
    pub(crate) response_processor: ResponseProcessor,
    pub(crate) url_processor: UrlProcessor,
}

impl ProxyProcessors {
    pub(crate) fn new(
        request_processor: RequestProcessor,
        request_analyzer: RequestAnalyzer,
        response_processor: ResponseProcessor,
        url_processor: UrlProcessor,
    ) -> Self {
        Self {
            request_processor,
            request_analyzer,
            response_processor,
            url_processor,
        }
    }

    pub(crate) async fn process_request_body(
        &self,
        request: &mut reqwest::Request,
        context: &RequestProcessingContext,
        request_url: &url::Url,
    ) -> Result<(), ProcessingError> {
        let Some(mut json_value) = buffered_json(request) else {
            return Ok(());
        };

        let response = processors::process_json(&mut json_value, &self.request_processor, context)
            .await
            .map_err(ProcessingError::RequestBody)?;

        if response.was_modified {
            debug!("Modified JSON body for request to {}", request_url);
            set_json_body(request, &json_value)?;
        }

        Ok(())
    }

    pub(crate) async fn process_response_json(
        &self,
        payload: &mut serde_json::Value,
        context: &ResponseProcessingContext,
    ) -> Result<bool, ProcessingError> {
        let permissions_modified =
            if context.profile == ResponseProcessingProfile::PlaylistPermissions {
                self.response_processor
                    .remap_playlist_users(payload, &context.server)
                    .await
                    .map_err(|error| {
                        error!("Failed to process playlist sharing response: {error}");
                        ProcessingError::Playlist(error)
                    })?
            } else {
                false
            };
        let processed = processors::process_json(payload, &self.response_processor, context)
            .await
            .map_err(ProcessingError::Response)?;

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
