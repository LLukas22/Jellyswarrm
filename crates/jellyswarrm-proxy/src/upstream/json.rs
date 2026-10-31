//! Execute Jellyfin JSON requests and normalize their responses.

use crate::processors::{payload_from_request, set_json_body, JsonBodyError};
#[cfg(test)]
use hyper::StatusCode;
use serde_json::Value;

#[derive(thiserror::Error)]
pub(crate) enum JsonTransportError {
    #[error("upstream transport failure")]
    Transport(#[source] reqwest::Error),
    #[error("upstream response failure")]
    Status(hyper::StatusCode),
    #[error("outgoing JSON body preparation failed")]
    RequestBody(#[from] crate::processors::JsonBodyError),
}

// Never render authenticated URLs, body excerpts, or underlying diagnostics.
impl std::fmt::Debug for JsonTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

/// Keep the document intact until decoding, including duplicate DTO fields.
/// Typed decoding and workflow failure policy belong to the application.
pub(crate) async fn execute(
    client: &reqwest::Client,
    mut request: reqwest::Request,
) -> Result<String, JsonTransportError> {
    sync_posted_guide_body(&mut request)?;
    let response = client
        .execute(request)
        .await
        .map_err(|error| JsonTransportError::Transport(error.without_url()))?;
    if !response.status().is_success() {
        return Err(JsonTransportError::Status(response.status()));
    }
    response
        .text()
        .await
        .map_err(|error| JsonTransportError::Transport(error.without_url()))
}

pub(crate) async fn execute_status(
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<http::StatusCode, JsonTransportError> {
    Ok(client
        .execute(request)
        .await
        .map_err(|error| JsonTransportError::Transport(error.without_url()))?
        .status())
}

/// Keep the POST guide body aligned with the translated query, including paging.
fn sync_posted_guide_body(request: &mut reqwest::Request) -> Result<(), JsonBodyError> {
    if request.method() != reqwest::Method::POST
        || !request
            .url()
            .path()
            .trim_end_matches('/')
            .to_ascii_lowercase()
            .ends_with("/livetv/programs")
    {
        return Ok(());
    }
    let mut payload: Value = payload_from_request(request)?;
    let fields = payload
        .as_object_mut()
        .ok_or(JsonBodyError::InvalidInput("guide body must be an object"))?;
    for (key, value) in request.url().query_pairs() {
        let pagination =
            key.eq_ignore_ascii_case("StartIndex") || key.eq_ignore_ascii_case("Limit");
        if let Some((_, field)) = fields
            .iter_mut()
            .find(|(field, _)| field.eq_ignore_ascii_case(&key))
        {
            *field = if pagination {
                serde_json::from_str(&value).map_err(JsonBodyError::Decode)?
            } else {
                match field {
                    Value::Array(_) => Value::Array(
                        value
                            .split(',')
                            .filter(|id| !id.is_empty())
                            .map(|id| Value::String(id.into()))
                            .collect(),
                    ),
                    Value::Bool(_) | Value::Number(_) => {
                        serde_json::from_str(&value).map_err(JsonBodyError::Decode)?
                    }
                    _ => Value::String(value.into_owned()),
                }
            };
        } else if pagination {
            fields.insert(
                key.into_owned(),
                serde_json::from_str(&value).map_err(JsonBodyError::Decode)?,
            );
        }
    }
    set_json_body(request, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
    use serde_json::json;
    use std::error::Error;
    use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn typed_guide_normalization_preserves_invalid_request_json_cause() {
        let mut request = reqwest::Request::new(
            reqwest::Method::POST,
            "http://proxy/LiveTv/Programs?Limit=not-a-number"
                .parse()
                .unwrap(),
        );
        *request.body_mut() = Some(reqwest::Body::from("{}"));
        let error = execute(&reqwest::Client::new(), request).await.unwrap_err();
        assert!(matches!(
            error,
            JsonTransportError::RequestBody(JsonBodyError::Decode(_))
        ));
        assert!(error
            .source()
            .unwrap()
            .source()
            .unwrap()
            .is::<serde_json::Error>());
        assert!(!format!("{error:?}").contains("not-a-number"));
    }

    #[tokio::test]
    async fn execution_retains_undecoded_document() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"Id":"one","Id":"two"}"#))
            .mount(&upstream)
            .await;
        let client = reqwest::Client::new();
        let request = client.get(upstream.uri()).build().unwrap();
        let document = execute(&client, request).await.unwrap();
        assert_eq!(document, r#"{"Id":"one","Id":"two"}"#);
    }

    #[tokio::test]
    async fn execution_preserves_transport_cause_without_authenticated_url() {
        let client = reqwest::Client::new();
        // Unsupported scheme fails locally and deterministically, without traffic.
        let request = reqwest::Request::new(
            reqwest::Method::GET,
            "ftp://localhost/Items?api_key=private-token"
                .parse()
                .unwrap(),
        );
        let error = execute(&client, request).await.unwrap_err();

        assert!(matches!(error, JsonTransportError::Transport(_)));
        let cause = error
            .source()
            .unwrap()
            .downcast_ref::<reqwest::Error>()
            .unwrap();
        assert!(cause.url().is_none());
        assert!(!format!("{error:?}").contains("private-token"));
    }

    #[tokio::test]
    async fn execution_retains_source_status() {
        for status in [401, 403, 404, 400, 503] {
            let upstream = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&upstream)
                .await;
            let client = reqwest::Client::new();
            let request = client.get(upstream.uri()).build().unwrap();
            let error = execute(&client, request).await.unwrap_err();

            assert!(
                matches!(error, JsonTransportError::Status(actual) if actual.as_u16() == status)
            );
        }
    }

    #[tokio::test]
    async fn execute_json_request_preserves_upstream_not_found_status() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&upstream)
            .await;
        let client = reqwest::Client::new();
        let request = client.get(upstream.uri()).build().unwrap();

        let result = execute(&client, request).await;

        assert!(matches!(
            result.unwrap_err(),
            JsonTransportError::Status(StatusCode::NOT_FOUND)
        ));
    }

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

    #[tokio::test]
    async fn execution_retains_invalid_json_for_application_decoding() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not JSON"))
            .mount(&upstream)
            .await;
        let client = reqwest::Client::new();
        let request = client.get(upstream.uri()).build().unwrap();

        assert_eq!(execute(&client, request).await.unwrap(), "not JSON");
    }
}
