//! Execute a JSON attempt, then apply the operation's response projection.
//! Transport only executes/decodes; this layer owns application processing.

use crate::{
    application::error::ApplicationError,
    processors::response_processor::ResponseProcessingProfile, server_storage::Server,
    upstream::jellyfin::json::execute_json_request_typed, AppState,
};

pub(crate) async fn execute_processed_json_request_typed(
    state: &AppState,
    request: reqwest::Request,
    server: &Server,
    profile: ResponseProcessingProfile,
    proxy_api_key: Option<&str>,
) -> Result<serde_json::Value, ApplicationError> {
    let mut response = execute_json_request_typed(&state.reqwest_client, request).await?;
    state
        .process_response_json_typed(&mut response, server, profile, proxy_api_key)
        .await?;
    Ok(response)
}
