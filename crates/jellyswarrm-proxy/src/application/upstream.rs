//! Typed workflow decoding of upstream JSON documents. The execution contract
//! belongs to upstream; it must never depend on this module.
use super::error::ApplicationError;
use crate::upstream::jellyfin::json::{JellyfinJsonUpstream, JsonUpstream};

pub(crate) fn response_json_to_payload<T: serde::de::DeserializeOwned>(
    payload: serde_json::Value,
) -> Result<T, ApplicationError> {
    serde_json::from_value(payload).map_err(ApplicationError::InvalidProcessedJson)
}

pub(crate) async fn execute_payload<T: serde::de::DeserializeOwned>(
    upstream: &dyn JsonUpstream,
    request: reqwest::Request,
) -> Result<T, ApplicationError> {
    serde_json::from_str(&upstream.execute(request).await?)
        .map_err(ApplicationError::InvalidUpstreamJson)
}

/// Concrete execution for workflows that do not keep a reusable capability.
/// Decode the original document here, never through a JSON Value intermediary.
pub(crate) async fn execute_json_request<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<T, ApplicationError> {
    execute_payload(
        &JellyfinJsonUpstream {
            client: client.clone(),
        },
        request,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::jellyfin::json::JsonTransportError;
    use std::error::Error;
    use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn concrete_execution_preserves_json_cause_and_rejects_duplicate_dto_fields() {
        #[derive(Debug, serde::Deserialize)]
        struct Item {
            #[serde(rename = "Id")]
            _id: String,
        }
        for document in ["not JSON", r#"{"Id":"one","Id":"two"}"#] {
            let upstream = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_string(document))
                .mount(&upstream)
                .await;
            let client = reqwest::Client::new();
            let request = client.get(upstream.uri()).build().unwrap();
            let error = execute_json_request::<Item>(&client, request)
                .await
                .unwrap_err();
            assert!(matches!(error, ApplicationError::InvalidUpstreamJson(_)));
            let cause = error
                .source()
                .unwrap()
                .downcast_ref::<serde_json::Error>()
                .unwrap();
            if document.starts_with('{') {
                assert!(cause.to_string().contains("duplicate field"));
            }
            assert!(!format!("{error:?}").contains(document));
        }
    }

    #[test]
    fn processed_payload_validation_preserves_json_cause() {
        let error = response_json_to_payload::<Vec<String>>(serde_json::json!({})).unwrap_err();
        assert!(matches!(error, ApplicationError::InvalidProcessedJson(_)));
        assert!(error.source().unwrap().is::<serde_json::Error>());
    }

    #[test]
    fn transport_status_conversion_retains_application_fallback_policy() {
        for (status, eligible) in [
            (401, true),
            (403, true),
            (404, true),
            (400, false),
            (503, true),
        ] {
            let error = ApplicationError::from(JsonTransportError::Status(
                http::StatusCode::from_u16(status).unwrap(),
            ));
            assert!(
                matches!(error, ApplicationError::UpstreamStatus(actual) if actual.as_u16() == status)
            );
            assert_eq!(error.allows_detail_fallback(), eligible);
        }
    }

    struct FakeUpstream(&'static str);
    #[async_trait::async_trait]
    impl JsonUpstream for FakeUpstream {
        async fn execute(&self, request: reqwest::Request) -> Result<String, JsonTransportError> {
            assert_eq!(request.url().path(), "/opaque-item");
            assert_eq!(request.headers()["x-extension"], "retained");
            Ok(self.0.to_owned())
        }
        async fn execute_status(
            &self,
            _: reqwest::Request,
        ) -> Result<hyper::StatusCode, JsonTransportError> {
            Ok(hyper::StatusCode::NO_CONTENT)
        }
    }

    #[tokio::test]
    async fn workflow_can_use_a_fake_capability_and_retains_payload_error_cause() {
        let mut request = reqwest::Request::new(
            reqwest::Method::GET,
            "http://unused.invalid/opaque-item".parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("x-extension", "retained".parse().unwrap());
        let error =
            execute_payload::<Vec<String>>(&FakeUpstream("{\"unexpected\": true}"), request)
                .await
                .unwrap_err();
        assert!(matches!(error, ApplicationError::InvalidUpstreamJson(_)));
        assert!(error.source().unwrap().is::<serde_json::Error>());
    }

    #[tokio::test]
    async fn typed_execution_still_rejects_duplicate_dto_fields() {
        #[derive(Debug, serde::Deserialize)]
        struct Item {
            #[serde(rename = "Id")]
            _id: String,
        }
        let mut request = reqwest::Request::new(
            reqwest::Method::GET,
            "http://unused.invalid/opaque-item".parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("x-extension", "retained".parse().unwrap());
        let error =
            execute_payload::<Item>(&FakeUpstream("{\"Id\":\"one\",\"Id\":\"two\"}"), request)
                .await
                .unwrap_err();
        assert!(matches!(error, ApplicationError::InvalidUpstreamJson(_)));
        assert!(error
            .source()
            .unwrap()
            .downcast_ref::<serde_json::Error>()
            .unwrap()
            .to_string()
            .contains("duplicate field"));
    }
}
