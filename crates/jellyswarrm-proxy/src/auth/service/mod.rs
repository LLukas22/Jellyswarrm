//! Own authorization-service configuration and compose focused user, API-key, mapping, and session operations.
use sqlx::{Row, SqlitePool};
use tracing::{debug, info};

use crate::domain::ids::ServerId;
use crate::domain::security::{
    decrypt_password, decrypt_password_with_key_material, encrypt_password, is_legacy_plaintext,
    EncryptedPassword, HashedPassword, MappingEncryptionKey, Password,
};
use crate::servers::storage::Server;
use crate::{domain::security::generate_token, protocol::authorization::Authorization};

pub(crate) use super::credentials::LocalCredential;
pub(crate) use super::models::{
    AuthorizationSession, CredentialFormat, LocalUser, MappingAuth, ServerMapping,
    VirtualUserApiKey,
};
use crate::protocol::device::{normalize_device, Device};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallerCredential {
    Login(i64),
    ApplicationKey(i64),
}

#[derive(Debug, Clone)]
pub(crate) struct UserAuthorizationService {
    pub(crate) session_changes: tokio::sync::watch::Sender<()>,
    pub(crate) pool: SqlitePool,
    preserved_devices: moka::future::Cache<String, std::sync::Arc<tokio::sync::Mutex<()>>>,
    preserve_upstream_device_ids: bool,
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
mod authorization_sessions;
mod users;

impl UserAuthorizationService {
    /// Jellyfin replaces an earlier token when a user authenticates again with
    /// the same upstream DeviceId. Keep the real player metadata on the proxy.
    pub(crate) fn upstream_login_device(&self, client: &Authorization) -> Authorization {
        let mut upstream = client.clone();
        upstream.token = None;
        if !self.preserve_upstream_device_ids
            || !Device::has_known_device_id(&normalize_device(&client.device_id))
        {
            upstream.device_id = generate_token();
        }
        upstream
    }

    /// Jellyfin rotates a preserved device's token immediately. Serialize the
    /// upstream exchange through local publication so competing sign-ins agree
    /// on which token wins, even for two mappings to the same backend account.
    pub(crate) async fn lock_preserved_device(
        &self,
        client: &Authorization,
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        if !self.preserve_upstream_device_ids
            || !Device::has_known_device_id(&normalize_device(&client.device_id))
        {
            return None;
        }
        let lock = self
            .preserved_devices
            .get_with(client.device_id.clone(), async { Default::default() })
            .await;
        Some(lock.lock_owned().await)
    }

    /// Production construction, shared with full HTTP fixtures: mapping and
    /// session encryption are both keyed from the configured session secret.
    pub(crate) async fn from_config(
        pool: SqlitePool,
        config: &crate::config::AppConfig,
    ) -> anyhow::Result<Self> {
        let mapping_key = MappingEncryptionKey::from_session_key(&config.session_key)?;
        let mut authorization =
            Self::with_mapping_key(pool, mapping_key, (&config.password).into());
        authorization.preserve_upstream_device_ids = config.preserve_upstream_device_ids;
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
            session_changes: tokio::sync::watch::channel(()).0,
            preserved_devices: moka::future::Cache::builder()
                .time_to_idle(std::time::Duration::from_secs(3600))
                .build(),
            preserve_upstream_device_ids: false,
            mapping_key: Some(mapping_key),
            session_key: None,
            legacy_admin_key: Some(legacy_admin_key),
        }
    }

    #[cfg(test)]
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            session_changes: tokio::sync::watch::channel(()).0,
            preserved_devices: moka::future::Cache::builder()
                .time_to_idle(std::time::Duration::from_secs(3600))
                .build(),
            preserve_upstream_device_ids: false,
            mapping_key: None,
            session_key: Some(MappingEncryptionKey::for_authorization_sessions(&[7; 64]).unwrap()),
            legacy_admin_key: None,
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod login_migration_tests;
