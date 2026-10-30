//! Buffered JSON request bodies. Encoding updates framing, never workflow policy.
use reqwest::header::{HeaderValue, CONTENT_LENGTH, TRANSFER_ENCODING};

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

pub(crate) fn set_json_body<T: serde::Serialize>(
    request: &mut reqwest::Request,
    payload: &T,
) -> Result<(), JsonBodyError> {
    let json = serde_json::to_vec(payload).map_err(JsonBodyError::Encode)?;
    let len = json.len();
    *request.body_mut() = Some(reqwest::Body::from(json));
    request.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&len.to_string()).map_err(JsonBodyError::Header)?,
    );
    request.headers_mut().remove(TRANSFER_ENCODING);
    Ok(())
}
