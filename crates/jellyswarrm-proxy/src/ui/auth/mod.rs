use std::sync::Arc;

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use axum_login::{AuthUser, AuthnBackend, UserId};
use serde::{Deserialize, Serialize};
use tokio::{sync::RwLock, task};
use tracing::{error, info};

use crate::{config::AppConfig, user_authorization_service::UserAuthorizationService};

mod routes;

pub use routes::router;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum UserRole {
    Admin,
    User,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub password: String,
    pub role: UserRole,
}

pub struct AuthenticatedUser(pub User);

impl<S> FromRequestParts<S> for AuthenticatedUser
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth_session = AuthSession::from_request_parts(parts, state)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        match auth_session.user {
            Some(user) => Ok(AuthenticatedUser(user)),
            None => Err(StatusCode::UNAUTHORIZED),
        }
    }
}

// Here we've implemented `Debug` manually to avoid accidentally logging the
// password hash.
impl std::fmt::Debug for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("User")
            .field("id", &self.id)
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .field("role", &self.role)
            .finish()
    }
}

impl AuthUser for User {
    type Id = String;

    fn id(&self) -> Self::Id {
        self.id.clone()
    }

    fn session_auth_hash(&self) -> &[u8] {
        self.password.as_bytes() // We use the password hash as the auth
                                 // hash--what this means
                                 // is when the user changes their password the
                                 // auth session becomes invalid.
    }
}

// This allows us to extract the authentication fields from forms. We use this
// to authenticate requests with the backend.
#[derive(Debug, Clone, Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
    pub next: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Backend {
    config: Arc<RwLock<AppConfig>>,
    user_auth: Arc<UserAuthorizationService>,
}

impl Backend {
    pub fn new(config: Arc<RwLock<AppConfig>>, user_auth: Arc<UserAuthorizationService>) -> Self {
        Self { config, user_auth }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    TaskJoin(#[from] task::JoinError),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

impl AuthnBackend for Backend {
    type User = User;
    type Credentials = Credentials;
    type Error = Error;

    async fn authenticate(
        &self,
        creds: Self::Credentials,
    ) -> Result<Option<Self::User>, Self::Error> {
        let config = self.config.read().await;
        info!("Authenticating user: {}", creds.username);
        if creds.username == config.username && creds.password == config.password {
            info!("Admin authentication successful");
            // If the password is correct, we return the default user.
            let user = User {
                id: "admin".to_string(),
                username: creds.username,
                password: config.password.clone(),
                role: UserRole::Admin,
            };
            return Ok(Some(user));
        }

        if let Some(user) = self
            .user_auth
            .get_user_by_credentials(&creds.username, &creds.password)
            .await?
        {
            info!("User authentication successful: {}", user.original_username);
            let user = User {
                id: user.id,
                username: user.original_username,
                password: user.original_password_hash,
                role: UserRole::User,
            };
            return Ok(Some(user));
        }

        info!("Authentication failed for user: {}", creds.username);
        Ok(None)
    }

    async fn get_user(&self, user_id: &UserId<Self>) -> Result<Option<Self::User>, Self::Error> {
        if user_id == "admin" {
            let config = self.config.read().await;
            return Ok(Some(User {
                id: "admin".to_string(),
                username: config.username.clone(),
                password: config.password.clone(),
                role: UserRole::Admin,
            }));
        }

        if let Some(user) = self.user_auth.get_user_by_id(user_id).await? {
            let user = User {
                id: user.id,
                username: user.original_username,
                password: user.original_password_hash,
                role: UserRole::User,
            };
            return Ok(Some(user));
        }

        Ok(None)
    }
}

// We use a type alias for convenience.
//
// Note that we've supplied our concrete backend here.
pub type AuthSession = axum_login::AuthSession<Backend>;
