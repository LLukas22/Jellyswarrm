//! Composition adapter implementing the application's JSON execution port.
use crate::application::{error::ApplicationError, upstream::JsonUpstream};

pub(crate) struct JellyfinJsonUpstream {
    pub client: reqwest::Client,
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
        super::compatibility::sync_posted_guide_body(&mut request)?;
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
