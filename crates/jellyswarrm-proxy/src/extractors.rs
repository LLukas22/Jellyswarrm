use axum::extract::{FromRequest, Request};
use hyper::StatusCode;
use tracing::error;

use crate::{
    request_preprocessing::{preprocess_request, PreprocessedRequest},
    user_authorization_service::{AuthorizationSession, User},
    AppState,
};

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
