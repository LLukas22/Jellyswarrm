//! Extract authenticated, access-scoped application inputs from incoming Axum requests.
use axum::extract::{FromRequest, Request};
use hyper::StatusCode;
use tracing::error;

use crate::{
    auth::service::User, http::preprocessing::resolve_request_identity_from_headers_uri, AppState,
};

pub struct RequirePrimaryUser(pub User);

impl FromRequest<AppState> for RequirePrimaryUser {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let identity = resolve_request_identity_from_headers_uri(req.headers(), req.uri(), state)
            .await
            .map_err(|error| {
                error!("Failed to resolve API key caller: {error}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        let user = identity.user.ok_or(StatusCode::UNAUTHORIZED)?;
        let token = identity
            .auth
            .as_ref()
            .and_then(|auth| auth.token_ref())
            .ok_or(StatusCode::UNAUTHORIZED)?;

        if token != user.virtual_key {
            return Err(StatusCode::FORBIDDEN);
        }

        Ok(Self(user))
    }
}

/// A caller-scoped operation that has not selected or encoded a backend request.
pub struct OperationInput(pub(crate) crate::application::input::OperationRequest);
pub use OperationInput as PlaybackInput;

impl FromRequest<AppState> for OperationInput {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        use crate::application::{context::RequestContext, input::OperationRequest};
        let crate::application::input::CollectedRequest {
            original_request,
            auth,
            user,
            sessions,
            body_analysis,
            access_scope,
        } = crate::http::preprocessing::extract_request_infos(req, state, None)
            .await
            .map_err(input_status)?;
        let context = RequestContext::new(&original_request, auth, user, sessions, access_scope);
        Ok(Self(OperationRequest {
            original_request,
            context,
            body_analysis,
        }))
    }
}

pub struct LiveInput(pub(crate) crate::application::playback::live::LiveRequest);

/// Catalog authentication and scope collection without selecting or rewriting a backend.
pub struct CatalogInput(pub(crate) crate::application::catalog::input::CatalogRequest);

impl FromRequest<AppState> for CatalogInput {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        use crate::application::{catalog::input::CatalogRequest, context::RequestContext};
        let crate::application::input::CollectedRequest {
            original_request,
            auth,
            user,
            sessions,
            body_analysis,
            access_scope,
        } = crate::http::preprocessing::extract_request_infos(req, state, None)
            .await
            .map_err(input_error)?;
        let context = RequestContext::new(&original_request, auth, user, sessions, access_scope);
        tracing::debug!(principal = ?context.principal(), device_present = context.device().is_some(), "Collected catalog context");
        let request = CatalogRequest::new(original_request, context, body_analysis);
        request
            .validate(&state.catalog_service())
            .await
            .map_err(|error| error.detail_status())?;
        Ok(Self(request))
    }
}

impl FromRequest<AppState> for LiveInput {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        use crate::application::{context::RequestContext, playback::live::LiveRequest};
        let crate::application::input::CollectedRequest {
            original_request,
            auth,
            user,
            sessions,
            body_analysis,
            access_scope,
        } = crate::http::preprocessing::extract_request_infos(req, state, None)
            .await
            .map_err(input_error)?;
        let context = RequestContext::new(&original_request, auth, user, sessions, access_scope);
        context
            .validate_media_ids(
                original_request.url(),
                body_analysis.as_ref(),
                &state.url_selection,
            )
            .await
            .map_err(input_error)?;
        if let Some(scope) = context.access_scope().as_ref() {
            if context
                .backends()
                .sessions
                .as_ref()
                .is_none_or(Vec::is_empty)
            {
                return Err(if scope.server_ids().is_empty() {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                });
            }
        }
        Ok(Self(LiveRequest {
            original_request,
            context,
        }))
    }
}

fn input_status(error: anyhow::Error) -> StatusCode {
    crate::application::input::routing_error(error).detail_status()
}

fn input_error(error: anyhow::Error) -> StatusCode {
    crate::application::catalog::input::input_error_typed(error).detail_status()
}

// Legacy HTTP fixtures adapt their wire-side inputs here, never in application
// code. Production requests use the shared named extraction boundary above.
#[cfg(test)]
impl From<crate::http::preprocessing::PreprocessedRequest>
    for crate::application::catalog::input::CatalogRequest
{
    fn from(request: crate::http::preprocessing::PreprocessedRequest) -> Self {
        let context = crate::application::context::RequestContext::new(
            &request.original_request,
            request.auth,
            request.user,
            request.sessions,
            request.access_scope,
        );
        Self::new(request.original_request, context, None)
    }
}
