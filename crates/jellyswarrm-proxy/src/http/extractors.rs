//! Extract authenticated, access-scoped application inputs from incoming Axum requests.
use axum::extract::{FromRef, FromRequest, Request};
use hyper::StatusCode;
use tracing::error;

use crate::{
    application::{
        error::ApplicationError,
        request::input::{routing_error, OperationRequest},
        services::MediaServices,
    },
    auth::service::LocalUser,
    http::preprocessing::{extract_request_infos, resolve_request_identity},
};

pub(crate) struct RequirePrimaryUser(pub(crate) LocalUser);

impl<S: Send + Sync> FromRequest<S> for RequirePrimaryUser
where
    std::sync::Arc<crate::auth::UserAuthorizationService>: FromRef<S>,
{
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let authorization =
            std::sync::Arc::<crate::auth::UserAuthorizationService>::from_ref(state);
        let identity = resolve_request_identity(req.headers(), req.uri(), &authorization)
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

/// Collect caller identity, body analysis, and backend availability without
/// selecting or encoding a backend request. The extractors below differ only in
/// which access validation runs before a workflow sees the request.
async fn collect(
    req: Request,
    state: &MediaServices,
) -> Result<OperationRequest, ApplicationError> {
    extract_request_infos(req, state, None)
        .await
        .map_err(routing_error)
}

/// A caller-scoped operation; target workflows perform their own access checks.
pub(crate) struct OperationInput(pub(crate) OperationRequest);

impl<S: Send + Sync> FromRequest<S> for OperationInput
where
    MediaServices: FromRef<S>,
{
    type Rejection = ApplicationError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(collect(req, &MediaServices::from_ref(state)).await?))
    }
}

/// A catalog operation whose selected media and backend availability were validated.
pub(crate) struct CatalogInput(pub(crate) OperationRequest);

impl<S: Send + Sync> FromRequest<S> for CatalogInput
where
    MediaServices: FromRef<S>,
{
    type Rejection = ApplicationError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let state = MediaServices::from_ref(state);
        let request = collect(req, &state).await?;
        tracing::debug!(principal = ?request.context.principal(), device_present = request.context.device().is_some(), "Collected catalog context");
        request.validate_catalog(&state).await?;
        Ok(Self(request))
    }
}

/// A live-stream operation whose selected media and backend availability were validated.
pub(crate) struct LiveInput(pub(crate) OperationRequest);

impl<S: Send + Sync> FromRequest<S> for LiveInput
where
    MediaServices: FromRef<S>,
{
    type Rejection = ApplicationError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let state = MediaServices::from_ref(state);
        let request = collect(req, &state).await?;
        request
            .validate_access(request.original_request.url(), &state.url_selection)
            .await?;
        Ok(Self(request))
    }
}
