//! Own authorization-service configuration and compose focused user, API-key, mapping, and session operations.
use sqlx::{Row, SqlitePool};
use tracing::{debug, info, warn};

use crate::auth::encryption::{
    decrypt_password, decrypt_password_with_key_material, encrypt_password, is_legacy_plaintext,
    EncryptedPassword, HashedPassword, MappingEncryptionKey, Password,
};
use crate::models::{generate_token, Authorization};
use crate::servers::id::ServerId;
use crate::servers::storage::Server;
#[cfg(test)]
use crate::servers::url::ServerUrl;

#[cfg(test)]
use super::credentials::password_work;
pub(crate) use super::credentials::LocalCredential;
use super::models::is_android_tv_client;
pub(crate) use super::models::{
    normalize_device, AuthorizationSession, CredentialFormat, Device, MappingAuth, ServerMapping,
    User, VirtualUserApiKey,
};
#[derive(Debug, Clone)]
pub struct UserAuthorizationService {
    pool: SqlitePool,
    mapping_key: Option<MappingEncryptionKey>,
    session_key: Option<MappingEncryptionKey>,
    legacy_admin_key: Option<HashedPassword>,
}

#[cfg(test)]
pub enum ServerReference<'a> {
    Server(&'a Server),
    Url(&'a str),
}

#[cfg(test)]
impl<'a> From<&'a Server> for ServerReference<'a> {
    fn from(server: &'a Server) -> Self {
        Self::Server(server)
    }
}

#[cfg(test)]
impl<'a> From<&'a str> for ServerReference<'a> {
    fn from(server_url: &'a str) -> Self {
        Self::Url(server_url)
    }
}

#[cfg(test)]
impl<'a> From<&'a &'a str> for ServerReference<'a> {
    fn from(server_url: &'a &'a str) -> Self {
        Self::Url(server_url)
    }
}

mod api_keys;
mod server_mappings;
mod sessions;
mod users;

impl UserAuthorizationService {
    /// Use the persisted session secret in production. `new` remains available
    /// for legacy-format test fixtures that exercise old database rows.
    pub fn with_mapping_key(
        pool: SqlitePool,
        mapping_key: MappingEncryptionKey,
        legacy_admin_key: HashedPassword,
    ) -> Self {
        Self {
            pool,
            mapping_key: Some(mapping_key),
            session_key: None,
            legacy_admin_key: Some(legacy_admin_key),
        }
    }

    #[cfg(test)]
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            mapping_key: None,
            session_key: None,
            legacy_admin_key: None,
        }
    }
}

#[cfg(test)]
#[path = "../../unit_tests/auth.rs"]
mod tests;
