//! Unselected operation input shared by every client entry point (catalog, detail,
//! playback, live, fallback). Collecting caller context never constructs a backend
//! request; workflows select and encode one.

use crate::{
    application::request::remap_authorization,
    application::{
        error::ApplicationError,
        request::{
            context::{AuthorizedTarget, RequestContext, TargetError},
            selection::UrlSelection,
        },
    },
    processors::request_analyzer::RequestBodyAnalysisResult,
};

pub(crate) struct OperationRequest {
    pub(crate) original_request: reqwest::Request,
    pub(crate) context: RequestContext,
    pub(crate) body_analysis: Option<RequestBodyAnalysisResult>,
}

pub(crate) struct ResolvedTarget {
    pub(crate) target: AuthorizedTarget,
    pub(crate) server_matched_request: bool,
}

impl OperationRequest {
    /// Validate client-selected media in `url` against the full mapped scope, not
    /// the set of healthy sessions, so an offline selected ID cannot bypass access
    /// validation. A scoped caller also needs at least one usable backend session.
    pub(crate) async fn validate_access(
        &self,
        url: &url::Url,
        selection: &UrlSelection,
    ) -> Result<(), ApplicationError> {
        self.context
            .validate_media_ids(url, self.body_analysis.as_ref(), selection)
            .await?;
        if let Some(scope) = self.context.access_scope() {
            if self
                .context
                .backends()
                .sessions
                .as_ref()
                .is_none_or(Vec::is_empty)
            {
                return Err(if scope.server_ids().is_empty() {
                    ApplicationError::InvalidInput("empty authorization scope")
                } else {
                    ApplicationError::UnavailableOwner
                });
            }
        }
        Ok(())
    }

    pub(crate) async fn resolve_target(
        &self,
        state: &crate::application::services::MediaServices,
    ) -> Result<ResolvedTarget, ApplicationError> {
        let (server, session, server_matched_request) = super::routing::resolve_server(
            &self.context.backends().sessions,
            &self.body_analysis,
            state,
            &self.original_request,
            self.context.access_scope(),
        )
        .await
        .map_err(ApplicationError::from)?;
        state
            .processors
            .url_processor
            .validate_container_request(
                self.original_request.url(),
                session.as_ref(),
                self.context.access_scope(),
                server.id,
                self.body_analysis
                    .as_ref()
                    .map(|analysis| analysis.found_ids.as_slice())
                    .unwrap_or_default(),
            )
            .await
            .map_err(routing_error)?;
        let auth = remap_authorization(self.context.authorization(), session.as_ref());
        let target =
            AuthorizedTarget::new(server, session, auth, self.context.access_scope().cloned())
                .map_err(|error| match error {
                    TargetError::AccessDenied | TargetError::ViewerMismatch => {
                        ApplicationError::AccessDenied
                    }
                    TargetError::UnavailableOwner => ApplicationError::UnavailableOwner,
                })?;
        Ok(ResolvedTarget {
            target,
            server_matched_request,
        })
    }
}

pub(crate) fn routing_error(error: anyhow::Error) -> ApplicationError {
    if error.is::<sqlx::Error>() {
        ApplicationError::Storage(
            error
                .downcast::<sqlx::Error>()
                .expect("checked storage cause"),
        )
    } else if error.is::<super::routing::RoutingError>() {
        error
            .downcast::<super::routing::RoutingError>()
            .expect("checked routing cause")
            .into()
    } else if error.is::<TargetError>() {
        ApplicationError::TargetSelection(
            error
                .downcast::<TargetError>()
                .expect("checked target cause"),
        )
    } else if crate::processors::ProcessingErrors::contains_cause::<sqlx::Error>(error.as_ref()) {
        // Keep the entire aggregate/context tree, not just its first database
        // cause. A validation failure in an earlier field must not hide storage
        // failure in a later one.
        ApplicationError::Internal(error)
    } else {
        ApplicationError::InvalidRouting(error)
    }
}

pub(crate) fn request_body_error(error: anyhow::Error) -> ApplicationError {
    match routing_error(error) {
        ApplicationError::InvalidRouting(cause) => ApplicationError::RequestBody(cause),
        error => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processors::ProcessingErrors;

    fn mixed_failures() -> anyhow::Error {
        anyhow::Error::new(ProcessingErrors {
            causes: vec![
                anyhow::anyhow!("invalid selection"),
                anyhow::Error::new(ProcessingErrors {
                    causes: vec![
                        anyhow::Error::new(sqlx::Error::PoolClosed).context("field lookup")
                    ],
                })
                .context("nested fields"),
            ],
        })
        .context("request analysis")
    }

    #[test]
    fn storage_in_later_nested_fields_keeps_the_entire_aggregate() {
        for classify in [routing_error, request_body_error] {
            let ApplicationError::Internal(error) = classify(mixed_failures()) else {
                panic!("storage failure must not be classified as invalid input");
            };
            let fields = error.downcast_ref::<ProcessingErrors>().unwrap();
            assert_eq!(fields.causes.len(), 2);
            assert!(crate::processors::ProcessingErrors::contains_cause::<
                sqlx::Error,
            >(error.as_ref()));
        }
    }

    #[test]
    fn validation_only_fields_retain_their_input_failure_type() {
        let invalid = || {
            anyhow::Error::new(ProcessingErrors {
                causes: vec![anyhow::anyhow!("invalid selection")],
            })
        };
        assert!(matches!(
            routing_error(invalid()),
            ApplicationError::InvalidRouting(_)
        ));
        assert!(matches!(
            request_body_error(invalid()),
            ApplicationError::RequestBody(_)
        ));
    }
}
