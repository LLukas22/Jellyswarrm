//! Application-owned JSON execution capability. Opaque requests preserve proxy
//! extensions; target selection/construction remains a separate preceding stage.
use super::error::ApplicationError;

#[async_trait::async_trait]
pub(crate) trait JsonUpstream: Send + Sync {
    /// Preserve the document until typed decoding. A Value intermediary would
    /// accept duplicate DTO fields and change numeric decoding behavior.
    async fn execute(&self, request: reqwest::Request) -> Result<String, ApplicationError>;
    async fn execute_status(
        &self,
        request: reqwest::Request,
    ) -> Result<hyper::StatusCode, ApplicationError>;
}

pub(crate) async fn execute_payload<T: serde::de::DeserializeOwned>(
    upstream: &dyn JsonUpstream,
    request: reqwest::Request,
) -> Result<T, ApplicationError> {
    serde_json::from_str(&upstream.execute(request).await?)
        .map_err(ApplicationError::InvalidUpstreamJson)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    struct FakeUpstream(&'static str);
    #[async_trait::async_trait]
    impl JsonUpstream for FakeUpstream {
        async fn execute(&self, request: reqwest::Request) -> Result<String, ApplicationError> {
            assert_eq!(request.url().path(), "/opaque-item");
            assert_eq!(request.headers()["x-extension"], "retained");
            Ok(self.0.to_owned())
        }
        async fn execute_status(
            &self,
            _: reqwest::Request,
        ) -> Result<hyper::StatusCode, ApplicationError> {
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
