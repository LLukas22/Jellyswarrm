use axum::extract::{FromRequest, Request};
use hyper::StatusCode;
use tracing::error;

use crate::{
    request_preprocessing::{
        preprocess_request, resolve_request_identity_from_headers_uri, PreprocessedRequest,
    },
    user_authorization_service::{AuthorizationSession, User},
    AppState,
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

pub struct Preprocessed(pub PreprocessedRequest);

impl FromRequest<AppState> for Preprocessed {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        preprocess_request(req, state).await.map(Self).map_err(|e| {
            error!("Failed to preprocess request: {}", e);
            StatusCode::BAD_REQUEST
        })
    }
}

pub struct RequireUser {
    pub preprocessed: PreprocessedRequest,
    pub user: User,
}

impl FromRequest<AppState> for RequireUser {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let Preprocessed(preprocessed) = Preprocessed::from_request(req, state).await?;
        let user = preprocessed.user.clone().ok_or_else(|| {
            error!("User not found in request preprocessing");
            StatusCode::UNAUTHORIZED
        })?;

        Ok(Self { preprocessed, user })
    }
}

pub struct RequireSession {
    pub preprocessed: PreprocessedRequest,
    pub session: AuthorizationSession,
}

impl FromRequest<AppState> for RequireSession {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let Preprocessed(preprocessed) = Preprocessed::from_request(req, state).await?;
        let session = preprocessed.session.clone().ok_or_else(|| {
            error!("Session not found in request preprocessing");
            StatusCode::UNAUTHORIZED
        })?;

        Ok(Self {
            preprocessed,
            session,
        })
    }
}

pub struct RequireUserSession {
    pub preprocessed: PreprocessedRequest,
    pub user: User,
    pub session: AuthorizationSession,
}

impl FromRequest<AppState> for RequireUserSession {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let RequireUser { preprocessed, user } = RequireUser::from_request(req, state).await?;
        let session = preprocessed.session.clone().ok_or_else(|| {
            error!("Session not found in request preprocessing");
            StatusCode::UNAUTHORIZED
        })?;

        Ok(Self {
            preprocessed,
            user,
            session,
        })
    }
}
