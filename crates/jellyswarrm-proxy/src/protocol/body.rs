//! Buffered JSON request bodies: decode the outgoing payload and re-encode it with
//! consistent framing headers. Encoding updates framing, never workflow policy.

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
        reqwest::header::CONTENT_LENGTH,
        reqwest::header::HeaderValue::from_str(&len.to_string()).map_err(JsonBodyError::Header)?,
    );
    request
        .headers_mut()
        .remove(reqwest::header::TRANSFER_ENCODING);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
    use serde_json::json;

    #[test]
    fn set_json_body_replaces_length_and_removes_transfer_encoding() {
        let mut request = reqwest::Client::new()
            .post("http://localhost/Items")
            .header(CONTENT_LENGTH, "999")
            .header(TRANSFER_ENCODING, "chunked")
            .body("old body")
            .build()
            .unwrap();
        let payload = json!({"UnknownField": {"preserved": true}});

        set_json_body(&mut request, &payload).unwrap();

        let body = request.body().unwrap().as_bytes().unwrap();
        assert_eq!(
            request.headers()[CONTENT_LENGTH].to_str().unwrap(),
            body.len().to_string()
        );
        assert!(!request.headers().contains_key(TRANSFER_ENCODING));
        assert_eq!(
            payload_from_request::<serde_json::Value>(&request).unwrap(),
            payload
        );
    }

    #[test]
    fn missing_or_invalid_request_json_retains_input_causes() {
        let client = reqwest::Client::new();
        let missing = client.post("http://localhost/Items").build().unwrap();
        let invalid = client
            .post("http://localhost/Items")
            .body("{")
            .build()
            .unwrap();
        for request in [missing, invalid] {
            assert!(matches!(
                payload_from_request::<serde_json::Value>(&request).unwrap_err(),
                JsonBodyError::InvalidInput(_) | JsonBodyError::Decode(_)
            ));
        }
    }
}
