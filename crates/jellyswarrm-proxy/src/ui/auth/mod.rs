use std::sync::Arc;

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use axum_login::{AuthUser, AuthnBackend, UserId};
use serde::{Deserialize, Serialize};
use tokio::{sync::RwLock, task};
use tracing::info;

use crate::{
    admin_storage::AdminStorageService,
    config::AppConfig, encryption::{HashedPassword, Password},
    user_authorization_service::UserAuthorizationService,
};

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
    pub password_hash: HashedPassword,
    pub role: UserRole,
    pub is_super_admin: bool,
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
        self.password_hash.as_str().as_bytes() // We use the password hash as the auth
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
    admin_storage: Arc<AdminStorageService>,
}

impl Backend {
    pub fn new(
        config: Arc<RwLock<AppConfig>>,
        user_auth: Arc<UserAuthorizationService>,
        admin_storage: Arc<AdminStorageService>,
    ) -> Self {
        Self { config, user_auth, admin_storage }
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
        info!("Authenticating user: {}", creds.username);

        // First, try to authenticate against admin_users table (multi-admin support)
        if let Some(admin) = self
            .admin_storage
            .authenticate(&creds.username, &creds.password)
            .await?
        {
            info!("Admin authentication successful via database: {}", admin.username);
            let user = User {
                id: format!("admin-{}", admin.id),
                username: admin.username,
                password_hash: HashedPassword::from_hashed(admin.password_hash),
                role: UserRole::Admin,
                is_super_admin: admin.is_super_admin,
            };
            return Ok(Some(user));
        }

        // Fallback to config-based admin authentication for backwards compatibility
        let password: Password = creds.password.clone().into();
        let password_hashed: HashedPassword = password.clone().into();
        let config = self.config.read().await;
        if creds.username == config.username && password_hashed == config.password.clone().into() {
            info!("Admin authentication successful via config");
            let user = User {
                id: "admin".to_string(),
                username: creds.username,
                password_hash: password_hashed,
                role: UserRole::Admin,
                is_super_admin: true, // Config admin is always super admin
            };
            return Ok(Some(user));
        }
        drop(config);

        // Try regular user authentication
        if let Some(user) = self
            .user_auth
            .get_user_by_credentials(&creds.username, &password)
            .await?
        {
            info!("User authentication successful: {}", user.original_username);
            let user = User {
                id: user.id,
                username: user.original_username,
                password_hash: user.original_password_hash,
                role: UserRole::User,
                is_super_admin: false,
            };
            return Ok(Some(user));
        }

        info!("Authentication failed for user: {}", creds.username);
        Ok(None)
    }

    async fn get_user(&self, user_id: &UserId<Self>) -> Result<Option<Self::User>, Self::Error> {
        // Check if this is a database admin (format: "admin-{id}")
        if let Some(admin_id_str) = user_id.strip_prefix("admin-") {
            if let Ok(admin_id) = admin_id_str.parse::<i64>() {
                if let Some(admin) = self.admin_storage.get_admin_by_id(admin_id).await? {
                    return Ok(Some(User {
                        id: format!("admin-{}", admin.id),
                        username: admin.username,
                        password_hash: HashedPassword::from_hashed(admin.password_hash),
                        role: UserRole::Admin,
                        is_super_admin: admin.is_super_admin,
                    }));
                }
            }
        }

        // Config-based admin fallback
        if user_id == "admin" {
            let config = self.config.read().await;
            return Ok(Some(User {
                id: "admin".to_string(),
                username: config.username.clone(),
                password_hash: config.password.clone().into(),
                role: UserRole::Admin,
                is_super_admin: true,
            }));
        }

        // Regular user lookup
        if let Some(user) = self.user_auth.get_user_by_id(user_id).await? {
            let user = User {
                id: user.id,
                username: user.original_username,
                password_hash: user.original_password_hash,
                role: UserRole::User,
                is_super_admin: false,
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
