//! Own authorization-service configuration and compose focused user, API-key, mapping, and session operations.
use sqlx::{Row, SqlitePool};
use tracing::{debug, info, warn};

use crate::domain::ids::ServerId;
use crate::domain::security::{
    decrypt_password, decrypt_password_with_key_material, encrypt_password, is_legacy_plaintext,
    EncryptedPassword, HashedPassword, MappingEncryptionKey, Password,
};
use crate::servers::storage::Server;
#[cfg(test)]
use crate::servers::url::ServerUrl;
use crate::{domain::security::generate_token, protocol::authorization::Authorization};

#[cfg(test)]
use super::credentials::password_work;
pub(crate) use super::credentials::LocalCredential;
pub(crate) use super::models::{
    AuthorizationSession, CredentialFormat, MappingAuth, ServerMapping, User, VirtualUserApiKey,
};
use crate::protocol::device::{is_android_tv_client, normalize_device, Device};
#[derive(Debug, Clone)]
pub(crate) struct UserAuthorizationService {
    pool: SqlitePool,
    mapping_key: Option<MappingEncryptionKey>,
    session_key: Option<MappingEncryptionKey>,
    legacy_admin_key: Option<HashedPassword>,
}

#[cfg(test)]
pub(crate) enum ServerReference<'a> {
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
pub(crate) use server_mappings::MappingCredentialError;
mod sessions;
mod users;

impl UserAuthorizationService {
    /// Production construction, shared with full HTTP fixtures: mapping and
    /// session encryption are both keyed from the configured session secret.
    pub(crate) async fn from_config(
        pool: SqlitePool,
        config: &crate::config::AppConfig,
    ) -> anyhow::Result<Self> {
        let mapping_key = MappingEncryptionKey::from_session_key(&config.session_key)?;
        let mut authorization =
            Self::with_mapping_key(pool, mapping_key, (&config.password).into());
        authorization
            .enable_session_encryption(&config.session_key)
            .await?;
        Ok(authorization)
    }

    /// Use the persisted session secret in production. `new` remains available
    /// for legacy-format test fixtures that exercise old database rows.
    pub(crate) fn with_mapping_key(
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
    pub(crate) fn new(pool: SqlitePool) -> Self {
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
