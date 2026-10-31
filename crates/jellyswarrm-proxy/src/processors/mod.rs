//! Translate request/response JSON fields and URLs through the shared proxy processor pipeline.
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

mod pipeline;
pub(crate) use pipeline::ProxyProcessors;

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
    JsonBody(#[from] JsonBodyError),
}

impl std::fmt::Debug for ProcessingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

#[derive(thiserror::Error)]
pub(crate) enum JsonBodyError {
    #[error("invalid outgoing JSON input")]
    InvalidInput(&'static str),
    #[error("invalid request JSON")]
    Decode(#[source] serde_json::Error),
    #[error("JSON serialization failed")]
    Encode(#[source] serde_json::Error),
    #[error("JSON framing failed")]
    Header(#[source] reqwest::header::InvalidHeaderValue),
}

impl std::fmt::Debug for JsonBodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

pub(crate) fn payload_from_request<T: serde::de::DeserializeOwned>(
    request: &reqwest::Request,
) -> Result<T, JsonBodyError> {
    let bytes = request
        .body()
        .ok_or(JsonBodyError::InvalidInput("missing JSON body"))?
        .as_bytes()
        .ok_or(JsonBodyError::InvalidInput("unbuffered JSON body"))?;
    serde_json::from_slice(bytes).map_err(JsonBodyError::Decode)
}

/// Buffered JSON request bodies. Encoding updates framing, never workflow policy.
pub(crate) fn set_json_body<T: serde::Serialize>(
    request: &mut reqwest::Request,
    payload: &T,
) -> Result<(), JsonBodyError> {
    let json = serde_json::to_vec(payload).map_err(JsonBodyError::Encode)?;
    let len = json.len();
    *request.body_mut() = Some(reqwest::Body::from(json));
    request.headers_mut().insert(
        reqwest::header::CONTENT_LENGTH,
        reqwest::header::HeaderValue::from_str(&len.to_string()).map_err(JsonBodyError::Header)?,
    );
    request
        .headers_mut()
        .remove(reqwest::header::TRANSFER_ENCODING);
    Ok(())
}
