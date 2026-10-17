//! Unselected operation input shared by migrated detail/playback entry points.
//! Collecting caller context never constructs a backend request.
use hyper::StatusCode;

use crate::{
    application::{
        context::{AuthorizedTarget, RequestContext, TargetError},
        error::ApplicationError,
    },
    processors::request_analyzer::RequestBodyAnalysisResult,
    upstream::jellyfin::request::remap_authorization,
    AppState,
};

pub(crate) struct OperationRequest {
    pub original_request: reqwest::Request,
    pub context: RequestContext,
    pub body_analysis: Option<RequestBodyAnalysisResult>,
}

pub(crate) struct ResolvedTarget {
    pub target: AuthorizedTarget,
    pub server_matched_request: bool,
}

impl OperationRequest {
    /// Retain endpoint status contracts while routing moves to application
    /// policy. No URL, header, body or credential translation occurs here.
    pub(crate) async fn resolve_target(
        &self,
        state: &AppState,
    ) -> Result<ResolvedTarget, StatusCode> {
        self.resolve_target_typed(state)
            .await
            .map_err(|error| error.detail_status())
    }

    pub(crate) async fn resolve_target_typed(
        &self,
        state: &AppState,
    ) -> Result<ResolvedTarget, ApplicationError> {
        self.resolve_target_using(&state.routing_services()).await
    }

    pub(crate) async fn resolve_target_using(
        &self,
        state: &crate::application::routing::RoutingServices<'_>,
    ) -> Result<ResolvedTarget, ApplicationError> {
        let (server, session, server_matched_request) =
            crate::application::routing::resolve_server(
                &self.context.backends().sessions,
                &self.body_analysis,
                state,
                &self.original_request,
                self.context.access_scope().as_ref(),
            )
            .await
            .map_err(routing_error)?;
        state
            .urls
            .validate_container_request(
                self.original_request.url(),
                &session,
                self.context.access_scope().as_ref(),
                server.id,
                self.body_analysis
                    .as_ref()
                    .map(|analysis| analysis.found_ids.as_slice())
                    .unwrap_or_default(),
            )
            .await
            .map_err(routing_error)?;
        let auth = remap_authorization(&self.context.authorization().cloned(), &session);
        let target =
            AuthorizedTarget::new(server, session, auth, self.context.access_scope().clone())
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

fn routing_error(error: anyhow::Error) -> ApplicationError {
    if error.is::<crate::http::preprocessing::MediaAccessDenied>() {
        ApplicationError::AccessDenied
    } else if error.is::<crate::application::playback::reports::NoAvailableBackend>() {
        ApplicationError::UnavailableOwner
    } else {
        ApplicationError::InvalidRouting(error)
    }
}

/// HTTP adapters retain the established pre-operation rejection contract.
pub(crate) fn input_status(error: anyhow::Error) -> StatusCode {
    routing_error(error).detail_status()
}
