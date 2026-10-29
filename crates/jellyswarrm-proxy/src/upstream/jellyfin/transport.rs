//! JSON wire execution has no application/HTTP-adapter dependencies.
#[derive(thiserror::Error)]
pub(crate) enum JsonTransportError {
    #[error("upstream transport failure")]
    Transport(#[source] reqwest::Error),
    #[error("upstream response failure")]
    Status(hyper::StatusCode),
    #[error("outgoing JSON body preparation failed")]
    RequestBody(#[from] crate::outbound::json::JsonBodyError),
}

// Never render authenticated URLs, body excerpts, or underlying diagnostics.
impl std::fmt::Debug for JsonTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

pub(crate) async fn execute_document(
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<String, JsonTransportError> {
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
