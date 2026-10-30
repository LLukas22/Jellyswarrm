//! Extract authenticated, access-scoped application inputs from incoming Axum requests.
use axum::extract::{FromRef, FromRequest, Request};
use hyper::StatusCode;
use tracing::error;

use crate::{
    application::{
        catalog::input::CatalogRequest,
        context::RequestContext,
        error::ApplicationError,
        input::{routing_error, CollectedRequest, OperationRequest},
        playback::live::LiveRequest,
    },
    auth::service::User,
    http::{
        preprocessing::{extract_request_infos, resolve_request_identity_from_headers_uri},
        state::MediaState,
    },
};

pub(crate) struct RequirePrimaryUser(pub(crate) User);

impl<S: Send + Sync> FromRequest<S> for RequirePrimaryUser
where
    std::sync::Arc<crate::auth::UserAuthorizationService>: FromRef<S>,
{
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let authorization =
            std::sync::Arc::<crate::auth::UserAuthorizationService>::from_ref(state);
        let identity =
            resolve_request_identity_from_headers_uri(req.headers(), req.uri(), &authorization)
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
pub(crate) struct OperationInput(pub(crate) OperationRequest);

impl<S: Send + Sync> FromRequest<S> for OperationInput
where
    MediaState: FromRef<S>,
{
    type Rejection = ApplicationError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let state = MediaState::from_ref(state);
        let CollectedRequest {
            original_request,
            auth,
            user,
            sessions,
            body_analysis,
            access_scope,
        } = extract_request_infos(req, &state, None)
            .await
            .map_err(routing_error)?;
        let context = RequestContext::new(&original_request, auth, user, sessions, access_scope);
        Ok(Self(OperationRequest {
            original_request,
            context,
            body_analysis,
        }))
    }
}

/// Catalog authentication and scope collection without selecting or rewriting a backend.
pub(crate) struct CatalogInput(pub(crate) CatalogRequest);

impl<S: Send + Sync> FromRequest<S> for CatalogInput
where
    MediaState: FromRef<S>,
{
    type Rejection = ApplicationError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let state = MediaState::from_ref(state);
        let CollectedRequest {
            original_request,
            auth,
            user,
            sessions,
            body_analysis,
            access_scope,
        } = extract_request_infos(req, &state, None)
            .await
            .map_err(routing_error)?;
        let context = RequestContext::new(&original_request, auth, user, sessions, access_scope);
        tracing::debug!(principal = ?context.principal(), device_present = context.device().is_some(), "Collected catalog context");
        let request = CatalogRequest::new(original_request, context, body_analysis);
        request.validate(&state.catalog_service()).await?;
        Ok(Self(request))
    }
}

pub(crate) struct LiveInput(pub(crate) LiveRequest);

impl<S: Send + Sync> FromRequest<S> for LiveInput
where
    MediaState: FromRef<S>,
{
    type Rejection = ApplicationError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let state = MediaState::from_ref(state);
        let CollectedRequest {
            original_request,
            auth,
            user,
            sessions,
            body_analysis,
            access_scope,
        } = extract_request_infos(req, &state, None)
            .await
            .map_err(routing_error)?;
        let context = RequestContext::new(&original_request, auth, user, sessions, access_scope);
        context
            .validate_media_ids(
                original_request.url(),
                body_analysis.as_ref(),
                &state.url_selection,
            )
            .await?;
        if let Some(scope) = context.access_scope() {
            if context
                .backends()
                .sessions
                .as_ref()
                .is_none_or(Vec::is_empty)
            {
                return Err(if scope.server_ids().is_empty() {
                    ApplicationError::InvalidInput("empty live authorization scope")
                } else {
                    ApplicationError::UnavailableOwner
                });
            }
        }
        Ok(Self(LiveRequest {
            original_request,
            context,
        }))
    }
}
