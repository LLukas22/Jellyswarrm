//! Unselected catalog input. Authentication and authorization do not pick a backend.
use crate::application::error::ApplicationError;
use hyper::StatusCode;

use super::services::CatalogService;
use crate::{
    application::{context::RequestContext, input::OperationRequest},
    processors::request_analyzer::RequestBodyAnalysisResult,
    request_preprocessing,
};

pub(crate) struct CatalogRequest {
    pub original_request: reqwest::Request,
    pub context: RequestContext,
    pub body_analysis: Option<RequestBodyAnalysisResult>,
}

pub(crate) fn input_error(error: anyhow::Error) -> StatusCode {
    input_error_typed(error).detail_status()
}

pub(super) fn input_error_typed(error: anyhow::Error) -> ApplicationError {
    if error.is::<request_preprocessing::NoAvailableBackend>() {
        ApplicationError::UnavailableOwner
    } else if error.is::<request_preprocessing::MediaAccessDenied>() {
        ApplicationError::AccessDenied
    } else {
        ApplicationError::InvalidRouting(error)
    }
}

impl CatalogRequest {
    pub(crate) fn new(
        original_request: reqwest::Request,
        context: RequestContext,
        body_analysis: Option<RequestBodyAnalysisResult>,
    ) -> Self {
        Self {
            original_request,
            context,
            body_analysis,
        }
    }

    /// The preserved wire request is authoritative. Do not retain a second,
    /// independently mutable interpretation that can diverge after adaptation.
    pub(crate) fn query(&self) -> super::query::CatalogQuery {
        super::query::CatalogQuery::from_url(self.original_request.url())
    }

    /// Validate using the full mapped scope, not the set of healthy sessions.
    /// In particular, an offline selected ID cannot bypass access validation.
    pub(crate) async fn validate(&self, state: &CatalogService) -> Result<(), ApplicationError> {
        let mut access_url = self.original_request.url().clone();
        if access_url
            .path()
            .trim_end_matches('/')
            .to_ascii_lowercase()
            .ends_with("/items")
        {
            // Batch lookup authorizes/partitions each Id in RequestedItems.
            // Its established contract returns authorized results and omits
            // forbidden entries, rather than rejecting the entire batch.
            let pairs = access_url
                .query_pairs()
                .filter(|(key, _)| !key.eq_ignore_ascii_case("Ids"))
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect::<Vec<_>>();
            access_url.query_pairs_mut().clear().extend_pairs(pairs);
        }
        self.context
            .validate_media_ids(
                &access_url,
                self.body_analysis.as_ref(),
                &state.processors.url_processor,
            )
            .await
            .map_err(input_error_typed)?;
        if let Some(scope) = self.context.access_scope().as_ref() {
            if self
                .context
                .backends()
                .sessions
                .as_ref()
                .is_none_or(Vec::is_empty)
            {
                return Err(if scope.server_ids().is_empty() {
                    ApplicationError::InvalidInput("empty catalog authorization scope")
                } else {
                    ApplicationError::UnavailableOwner
                });
            }
        }
        Ok(())
    }
}

impl From<CatalogRequest> for OperationRequest {
    fn from(request: CatalogRequest) -> Self {
        Self {
            original_request: request.original_request,
            context: request.context,
            body_analysis: request.body_analysis,
        }
    }
}

// Existing handler unit fixtures intentionally keep their legacy construction.
// Production catalog extraction never creates a PreprocessedRequest.
#[cfg(test)]
impl From<request_preprocessing::PreprocessedRequest> for CatalogRequest {
    fn from(request: request_preprocessing::PreprocessedRequest) -> Self {
        let context = RequestContext::new(
            &request.original_request,
            request.auth,
            request.user,
            request.sessions,
            request.access_scope,
        );
        Self::new(request.original_request, context, None)
    }
}
