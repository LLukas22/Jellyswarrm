//! Shared request/response processor pipeline, including safe client-facing validation errors.
use super::{
    request_analyzer::RequestAnalyzer,
    request_processor::{RequestProcessingContext, RequestProcessor},
    response_processor::{ResponseProcessingContext, ResponseProcessingProfile, ResponseProcessor},
    url_processor::UrlProcessor,
};
use crate::http::{preprocessing::body_to_json, proxy::client_request_error};
use crate::upstream::jellyfin::json::set_json_body;
use crate::{application, processors, DataContext};
use axum::http::StatusCode;
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
            request_processor: RequestProcessor::new(data_context.clone()),
            request_analyzer: RequestAnalyzer::new(data_context.clone()),
            response_processor: ResponseProcessor::new(data_context.clone()),
            url_processor: UrlProcessor::new(data_context),
        }
    }

    pub async fn process_request_body(
        &self,
        request: &mut reqwest::Request,
        context: &RequestProcessingContext,
        request_url: &url::Url,
    ) -> Result<(), (StatusCode, String)> {
        let Some(mut json_value) = body_to_json(request) else {
            return Ok(());
        };

        let response = processors::process_json(&mut json_value, &self.request_processor, context)
            .await
            .map_err(|e| {
                error!("Failed to process JSON body: {}", e);
                (
                    StatusCode::BAD_REQUEST,
                    client_request_error(&e).to_string(),
                )
            })?;

        if response.was_modified {
            debug!("Modified JSON body for request to {}", request_url);
            set_json_body(request, &response.data)
                .map_err(|status| (status, "Failed to serialize request".to_string()))?;
        }

        Ok(())
    }

    pub async fn process_response_json(
        &self,
        payload: &mut serde_json::Value,
        context: &ResponseProcessingContext,
    ) -> Result<bool, StatusCode> {
        self.process_response_json_typed(payload, context)
            .await
            .map_err(|error| error.detail_status())
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
                        use processors::response_processor::PlaylistResponseError;
                        error!("Failed to process playlist sharing response: {error}");
                        let status = match &error {
                            PlaylistResponseError::IncompleteSharing => StatusCode::CONFLICT,
                            PlaylistResponseError::InvalidSharing => StatusCode::BAD_GATEWAY,
                            PlaylistResponseError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
                        };
                        ApplicationError::Internal(error.into()).with_status(status)
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
