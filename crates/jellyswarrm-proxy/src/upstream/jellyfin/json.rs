//! Jellyfin JSON wire encoding, execution, and response translation.

use hyper::StatusCode;
use reqwest::header::{HeaderValue, CONTENT_LENGTH, TRANSFER_ENCODING};
use serde::Serialize;

use crate::application::error::ApplicationError;

#[cfg(test)]
pub fn payload_from_request<T>(request: &reqwest::Request) -> Result<T, StatusCode>
where
    T: serde::de::DeserializeOwned,
{
    payload_from_request_typed(request).map_err(|error| error.detail_status())
}

pub(crate) fn payload_from_request_typed<T>(
    request: &reqwest::Request,
) -> Result<T, ApplicationError>
where
    T: serde::de::DeserializeOwned,
{
    let bytes = request
        .body()
        .ok_or(ApplicationError::InvalidInput("missing JSON body"))?
        .as_bytes()
        .ok_or(ApplicationError::InvalidInput("unbuffered JSON body"))?;
    serde_json::from_slice::<T>(bytes).map_err(ApplicationError::InvalidRequestJson)
}

pub fn set_json_body<T>(request: &mut reqwest::Request, payload: &T) -> Result<(), StatusCode>
where
    T: Serialize,
{
    set_json_body_typed(request, payload).map_err(|error| error.detail_status())
}

pub(crate) fn set_json_body_typed<T>(
    request: &mut reqwest::Request,
    payload: &T,
) -> Result<(), ApplicationError>
where
    T: Serialize,
{
    let json = serde_json::to_vec(payload).map_err(ApplicationError::Serialization)?;

    let len = json.len();
    *request.body_mut() = Some(reqwest::Body::from(json));
    request.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&len.to_string())
            .map_err(|error| ApplicationError::Internal(error.into()))?,
    );
    request.headers_mut().remove(TRANSFER_ENCODING);

    Ok(())
}

/// Compatibility entry point for route families not yet using application errors.
pub async fn execute_json_request<T>(
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<T, StatusCode>
where
    T: serde::de::DeserializeOwned,
{
    execute_json_request_typed(client, request)
        .await
        .map_err(|error| error.detail_status())
}

/// Execute without logging authenticated URLs or dumping upstream payloads.
/// Preserve transport and JSON causes instead of reducing them to HTTP status.
pub(crate) async fn execute_json_request_typed<T>(
    client: &reqwest::Client,
    mut request: reqwest::Request,
) -> Result<T, ApplicationError>
where
    T: serde::de::DeserializeOwned,
{
    super::compatibility::sync_posted_guide_body(&mut request)?;
    let response = client
        .execute(request)
        .await
        .map_err(|error| ApplicationError::UpstreamTransport(error.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(ApplicationError::UpstreamStatus(status));
    }

    let response_text = response
        .text()
        .await
        .map_err(|error| ApplicationError::UpstreamTransport(error.without_url()))?;
    serde_json::from_str::<T>(&response_text).map_err(ApplicationError::InvalidUpstreamJson)
}

pub(crate) fn response_json_to_payload_typed<T>(
    payload: serde_json::Value,
) -> Result<T, ApplicationError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(payload).map_err(ApplicationError::InvalidProcessedJson)
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let error =
            execute_json_request_typed::<serde_json::Value>(&reqwest::Client::new(), request)
                .await
                .unwrap_err();
        assert!(matches!(error, ApplicationError::InvalidRequestJson(_)));
        assert_eq!(error.detail_status(), StatusCode::BAD_REQUEST);
        assert!(error.source().unwrap().is::<serde_json::Error>());
        assert!(!format!("{error:?}").contains("not-a-number"));
    }

    #[tokio::test]
    async fn typed_execution_preserves_json_cause_without_payload_diagnostics() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not JSON"))
            .mount(&upstream)
            .await;
        let client = reqwest::Client::new();
        let request = client.get(upstream.uri()).build().unwrap();
        let error = execute_json_request_typed::<serde_json::Value>(&client, request)
            .await
            .unwrap_err();

        assert!(matches!(error, ApplicationError::InvalidUpstreamJson(_)));
        assert!(error.source().unwrap().is::<serde_json::Error>());
        assert_eq!(error.detail_status(), StatusCode::BAD_GATEWAY);
        assert!(!format!("{error:?}").contains("not JSON"));
    }

    #[tokio::test]
    async fn typed_execution_preserves_transport_cause_without_authenticated_url() {
        let client = reqwest::Client::new();
        // Unsupported scheme fails locally and deterministically, without traffic.
        let request = reqwest::Request::new(
            reqwest::Method::GET,
            "ftp://localhost/Items?api_key=private-token"
                .parse()
                .unwrap(),
        );
        let error = execute_json_request_typed::<serde_json::Value>(&client, request)
            .await
            .unwrap_err();

        assert!(matches!(error, ApplicationError::UpstreamTransport(_)));
        let cause = error
            .source()
            .unwrap()
            .downcast_ref::<reqwest::Error>()
            .unwrap();
        assert!(cause.url().is_none());
        assert_eq!(error.detail_status(), StatusCode::BAD_GATEWAY);
        assert!(!format!("{error:?}").contains("private-token"));
    }

    #[tokio::test]
    async fn typed_execution_retains_source_status_and_fallback_eligibility() {
        for (status, eligible) in [
            (401, true),
            (403, true),
            (404, true),
            (400, false),
            (503, true),
        ] {
            let upstream = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&upstream)
                .await;
            let client = reqwest::Client::new();
            let request = client.get(upstream.uri()).build().unwrap();
            let error = execute_json_request_typed::<serde_json::Value>(&client, request)
                .await
                .unwrap_err();

            assert!(matches!(error, ApplicationError::UpstreamStatus(_)));
            assert_eq!(error.detail_status().as_u16(), status);
            assert_eq!(error.allows_detail_fallback(), eligible);
        }
    }

    #[test]
    fn processed_payload_validation_preserves_cause_and_existing_internal_error_contract() {
        let error = response_json_to_payload_typed::<Vec<String>>(json!({})).unwrap_err();
        assert!(matches!(error, ApplicationError::InvalidProcessedJson(_)));
        assert!(error.source().unwrap().is::<serde_json::Error>());
        assert_eq!(error.detail_status(), StatusCode::INTERNAL_SERVER_ERROR);
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

        let result = execute_json_request::<serde_json::Value>(&client, request).await;

        assert_eq!(result.unwrap_err(), StatusCode::NOT_FOUND);
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
    fn missing_or_invalid_request_json_is_bad_request() {
        let client = reqwest::Client::new();
        let missing = client.post("http://localhost/Items").build().unwrap();
        let invalid = client
            .post("http://localhost/Items")
            .body("{")
            .build()
            .unwrap();
        for request in [missing, invalid] {
            assert_eq!(
                payload_from_request::<serde_json::Value>(&request).unwrap_err(),
                StatusCode::BAD_REQUEST
            );
        }
    }

    #[tokio::test]
    async fn invalid_upstream_json_is_bad_gateway() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not JSON"))
            .mount(&upstream)
            .await;
        let client = reqwest::Client::new();
        let request = client.get(upstream.uri()).build().unwrap();

        assert_eq!(
            execute_json_request::<serde_json::Value>(&client, request)
                .await
                .unwrap_err(),
            StatusCode::BAD_GATEWAY
        );
    }
}
