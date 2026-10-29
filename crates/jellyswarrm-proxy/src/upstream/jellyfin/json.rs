//! Jellyfin JSON wire encoding, execution, and response translation.

#[cfg(test)]
use hyper::StatusCode;
use reqwest::header::{HeaderValue, CONTENT_LENGTH, TRANSFER_ENCODING};
use serde::Serialize;

use crate::application::error::ApplicationError;
use crate::application::upstream::JsonUpstream;
use serde_json::Value;

/// Concrete adapter for application workflows. Wire normalization, execution,
/// and error conversion are kept together here; transport owns HTTP I/O.
pub(crate) struct JellyfinJsonUpstream {
    pub(crate) client: reqwest::Client,
}

#[async_trait::async_trait]
impl JsonUpstream for JellyfinJsonUpstream {
    async fn execute_status(
        &self,
        request: reqwest::Request,
    ) -> Result<hyper::StatusCode, ApplicationError> {
        Ok(self
            .client
            .execute(request)
            .await
            .map_err(|error| ApplicationError::UpstreamTransport(error.without_url()))?
            .status())
    }

    async fn execute(&self, mut request: reqwest::Request) -> Result<String, ApplicationError> {
        sync_posted_guide_body(&mut request)?;
        super::transport::execute_document(&self.client, request)
            .await
            .map_err(Into::into)
    }
}

impl From<super::transport::JsonTransportError> for ApplicationError {
    fn from(error: super::transport::JsonTransportError) -> Self {
        match error {
            super::transport::JsonTransportError::Transport(error) => {
                Self::UpstreamTransport(error)
            }
            super::transport::JsonTransportError::Status(status) => Self::UpstreamStatus(status),
            super::transport::JsonTransportError::Json(error) => Self::InvalidUpstreamJson(error),
        }
    }
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

/// Execute without logging authenticated URLs or dumping upstream payloads.
/// Preserve transport and JSON causes instead of reducing them to HTTP status.
pub(crate) async fn execute_json_request_typed<T>(
    client: &reqwest::Client,
    mut request: reqwest::Request,
) -> Result<T, ApplicationError>
where
    T: serde::de::DeserializeOwned,
{
    sync_posted_guide_body(&mut request)?;
    super::transport::execute(client, request)
        .await
        .map_err(Into::into)
}

pub(crate) fn response_json_to_payload_typed<T>(
    payload: serde_json::Value,
) -> Result<T, ApplicationError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(payload).map_err(ApplicationError::InvalidProcessedJson)
}

/// Keep the POST guide body aligned with the translated query, including paging.
fn sync_posted_guide_body(request: &mut reqwest::Request) -> Result<(), ApplicationError> {
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
    let mut payload: Value = payload_from_request_typed(request)?;
    let fields = payload
        .as_object_mut()
        .ok_or(ApplicationError::InvalidInput(
            "guide body must be an object",
        ))?;
    for (key, value) in request.url().query_pairs() {
        let pagination =
            key.eq_ignore_ascii_case("StartIndex") || key.eq_ignore_ascii_case("Limit");
        if let Some((_, field)) = fields
            .iter_mut()
            .find(|(field, _)| field.eq_ignore_ascii_case(&key))
        {
            *field = if pagination {
                serde_json::from_str(&value).map_err(ApplicationError::InvalidRequestJson)?
            } else {
                match field {
                    Value::Array(_) => Value::Array(
                        value
                            .split(',')
                            .filter(|id| !id.is_empty())
                            .map(|id| Value::String(id.into()))
                            .collect(),
                    ),
                    Value::Bool(_) | Value::Number(_) => serde_json::from_str(&value)
                        .map_err(ApplicationError::InvalidRequestJson)?,
                    _ => Value::String(value.into_owned()),
                }
            };
        } else if pagination {
            fields.insert(
                key.into_owned(),
                serde_json::from_str(&value).map_err(ApplicationError::InvalidRequestJson)?,
            );
        }
    }
    set_json_body_typed(request, &payload)
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

            assert!(
                matches!(error, ApplicationError::UpstreamStatus(actual) if actual.as_u16() == status)
            );
            assert_eq!(error.allows_detail_fallback(), eligible);
        }
    }

    #[test]
    fn processed_payload_validation_preserves_json_cause() {
        let error = response_json_to_payload_typed::<Vec<String>>(json!({})).unwrap_err();
        assert!(matches!(error, ApplicationError::InvalidProcessedJson(_)));
        assert!(error.source().unwrap().is::<serde_json::Error>());
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

        let result = execute_json_request_typed::<serde_json::Value>(&client, request).await;

        assert!(matches!(
            result.unwrap_err(),
            ApplicationError::UpstreamStatus(StatusCode::NOT_FOUND)
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

        set_json_body_typed(&mut request, &payload).unwrap();

        let body = request.body().unwrap().as_bytes().unwrap();
        assert_eq!(
            request.headers()[CONTENT_LENGTH].to_str().unwrap(),
            body.len().to_string()
        );
        assert!(!request.headers().contains_key(TRANSFER_ENCODING));
        assert_eq!(
            payload_from_request_typed::<serde_json::Value>(&request).unwrap(),
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
                payload_from_request_typed::<serde_json::Value>(&request).unwrap_err(),
                ApplicationError::InvalidInput(_) | ApplicationError::InvalidRequestJson(_)
            ));
        }
    }

    #[tokio::test]
    async fn invalid_upstream_json_retains_json_cause() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not JSON"))
            .mount(&upstream)
            .await;
        let client = reqwest::Client::new();
        let request = client.get(upstream.uri()).build().unwrap();

        assert!(matches!(
            execute_json_request_typed::<serde_json::Value>(&client, request)
                .await
                .unwrap_err(),
            ApplicationError::InvalidUpstreamJson(_)
        ));
    }
}
