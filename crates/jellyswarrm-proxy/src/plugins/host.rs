//! The proxy side of [`Host`]: one instance per plugin, over the same services
//! the core handlers use.
use axum::http::request::Parts;
use jellyswarrm_plugin_api::{Caller, CredentialKind, Host, HostError, SecretRevision, UserSecret};
use sqlx::SqlitePool;

use crate::{
    application::media_services::MediaServices,
    auth::service::CallerCredential,
    domain::security::{EncryptedPassword, MappingEncryptionKey},
    http::incoming,
};

pub(crate) struct ProxyHost {
    plugin_id: &'static str,
    media: MediaServices,
    settings: Option<serde_json::Value>,
    secrets: SecretStore,
}

impl ProxyHost {
    pub(crate) fn new(
        plugin_id: &'static str,
        media: MediaServices,
        settings: Option<serde_json::Value>,
        session_key: &[u8],
    ) -> Self {
        let secrets = SecretStore::new(
            plugin_id,
            media.user_authorization.pool.clone(),
            session_key,
        );
        Self {
            plugin_id,
            media,
            settings,
            secrets,
        }
    }
}

#[async_trait::async_trait]
impl Host for ProxyHost {
    async fn server_name(&self) -> String {
        self.media.config.read().await.server_name.clone()
    }

    fn url_prefix(&self) -> Option<&str> {
        self.media.url_prefix()
    }

    fn settings(&self) -> Option<&serde_json::Value> {
        self.settings.as_ref()
    }

    async fn caller(&self, parts: &Parts) -> Result<Option<Caller>, HostError> {
        let identity = incoming::resolve_request_identity(
            &parts.headers,
            &parts.uri,
            &self.media.user_authorization,
        )
        .await
        .map_err(|error| {
            tracing::error!(plugin = self.plugin_id, "Failed to resolve caller: {error}");
            HostError::new(error.to_string())
        })?;
        Ok(identity
            .user
            .zip(identity.credential)
            .map(|(user, credential)| Caller {
                user_id: user.id.as_str().to_owned(),
                user_name: user.original_username,
                credential: match credential {
                    CallerCredential::Login(_) => CredentialKind::Login,
                    CallerCredential::ApplicationKey(_) => CredentialKind::ReadOnlyApiKey,
                },
            }))
    }

    async fn user_secret(
        &self,
        caller: &Caller,
        key: &str,
    ) -> Result<Option<UserSecret>, HostError> {
        self.secrets.get(&caller.user_id, key).await
    }

    async fn set_user_secret(
        &self,
        caller: &Caller,
        key: &str,
        value: &str,
    ) -> Result<(), HostError> {
        self.secrets.set(&caller.user_id, key, value).await
    }

    async fn take_user_secret(
        &self,
        caller: &Caller,
        key: &str,
    ) -> Result<Option<UserSecret>, HostError> {
        self.secrets.take(&caller.user_id, key).await
    }

    async fn replace_user_secret(
        &self,
        caller: &Caller,
        key: &str,
        expected: &SecretRevision,
        value: Option<&str>,
    ) -> Result<bool, HostError> {
        self.secrets
            .replace(&caller.user_id, key, expected, value)
            .await
    }
}

/// `plugin_user_secrets` rows of one plugin. The stored ciphertext doubles as
/// a version: every write uses a fresh nonce, so a conditional update on the
/// ciphertext that was read applies only if nothing changed in between.
struct SecretStore {
    plugin_id: &'static str,
    pool: SqlitePool,
    /// `None` when the session key is unusable; every access then fails.
    key: Option<MappingEncryptionKey>,
}

impl SecretStore {
    fn new(plugin_id: &'static str, pool: SqlitePool, session_key: &[u8]) -> Self {
        let key = MappingEncryptionKey::for_plugin(session_key, plugin_id)
            .inspect_err(|error| {
                tracing::error!(
                    plugin = plugin_id,
                    "Plugin secrets are unavailable: {error}"
                );
            })
            .ok();
        Self {
            plugin_id,
            pool,
            key,
        }
    }

    fn key(&self) -> Result<&MappingEncryptionKey, HostError> {
        self.key
            .as_ref()
            .ok_or_else(|| HostError::new("plugin secrets are unavailable"))
    }

    fn storage_error(&self, error: sqlx::Error) -> HostError {
        tracing::error!(
            plugin = self.plugin_id,
            "Plugin secret storage failed: {error}"
        );
        HostError::new(error.to_string())
    }

    /// The stored ciphertext and its plaintext. A value that no longer decrypts
    /// (e.g. after a session key change) reads as absent.
    async fn get(&self, user_id: &str, key: &str) -> Result<Option<UserSecret>, HostError> {
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT value FROM plugin_user_secrets WHERE plugin_id = ? AND user_id = ? AND key = ?",
        )
        .bind(self.plugin_id)
        .bind(user_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| self.storage_error(error))?;
        self.decrypt(stored)
    }

    async fn take(&self, user_id: &str, key: &str) -> Result<Option<UserSecret>, HostError> {
        let stored: Option<String> = sqlx::query_scalar(
            "DELETE FROM plugin_user_secrets WHERE plugin_id = ? AND user_id = ? AND key = ? RETURNING value",
        )
        .bind(self.plugin_id)
        .bind(user_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| self.storage_error(error))?;
        self.decrypt(stored)
    }

    fn decrypt(&self, stored: Option<String>) -> Result<Option<UserSecret>, HostError> {
        let Some(ciphertext) = stored.map(EncryptedPassword::from_raw) else {
            return Ok(None);
        };
        match self.key()?.decrypt(&ciphertext) {
            Ok(plain) => Ok(Some(UserSecret {
                value: plain.into_inner(),
                revision: SecretRevision::new(ciphertext.into_inner()),
            })),
            Err(error) => {
                tracing::warn!(
                    plugin = self.plugin_id,
                    "Ignoring a plugin secret that no longer decrypts: {error}"
                );
                Ok(None)
            }
        }
    }

    fn encrypt(&self, value: &str) -> Result<String, HostError> {
        self.key()?
            .encrypt(&value.into())
            .map(EncryptedPassword::into_inner)
            .map_err(|error| HostError::new(error.to_string()))
    }

    async fn set(&self, user_id: &str, key: &str, value: &str) -> Result<(), HostError> {
        sqlx::query(
            "INSERT INTO plugin_user_secrets (plugin_id, user_id, key, value, updated_at) \
                 VALUES (?, ?, ?, ?, ?) \
                 ON CONFLICT (plugin_id, user_id, key) \
                 DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(self.plugin_id)
        .bind(user_id)
        .bind(key)
        .bind(self.encrypt(value)?)
        .bind(chrono::Utc::now())
        .execute(&self.pool)
        .await
        .map_err(|error| self.storage_error(error))?;
        Ok(())
    }

    async fn replace(
        &self,
        user_id: &str,
        key: &str,
        expected: &SecretRevision,
        value: Option<&str>,
    ) -> Result<bool, HostError> {
        let query = match value {
            Some(value) => sqlx::query(
                "UPDATE plugin_user_secrets SET value = ?, updated_at = ? \
                 WHERE plugin_id = ? AND user_id = ? AND key = ? AND value = ?",
            )
            .bind(self.encrypt(value)?)
            .bind(chrono::Utc::now()),
            None => sqlx::query(
                "DELETE FROM plugin_user_secrets \
                 WHERE plugin_id = ? AND user_id = ? AND key = ? AND value = ?",
            ),
        };
        let result = query
            .bind(self.plugin_id)
            .bind(user_id)
            .bind(key)
            .bind(expected.as_str())
            .execute(&self.pool)
            .await
            .map_err(|error| self.storage_error(error))?;
        Ok(result.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MIGRATOR;

    const SESSION_KEY: [u8; 64] = [7; 64];

    async fn fixture() -> (SqlitePool, SecretStore, String) {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let config = crate::config::AppConfig::default();
        let users = crate::auth::UserAuthorizationService::from_config(pool.clone(), &config)
            .await
            .unwrap();
        let user = users
            .fixture_user("alice", &"password".into())
            .await
            .unwrap();
        let store = SecretStore::new("test", pool.clone(), &SESSION_KEY);
        (pool, store, user.id.as_str().to_owned())
    }

    #[tokio::test]
    async fn secrets_are_encrypted_scoped_and_replaced_only_when_unchanged() {
        let (pool, store, user) = fixture().await;
        assert!(store.get(&user, "session").await.unwrap().is_none());
        store.set(&user, "session", "first").await.unwrap();
        let first = store.get(&user, "session").await.unwrap().unwrap();
        assert_eq!(first.value, "first");
        let raw: String = sqlx::query_scalar("SELECT value FROM plugin_user_secrets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!raw.contains("first"));
        let other = SecretStore::new("other", pool.clone(), &SESSION_KEY);
        assert!(other.get(&user, "session").await.unwrap().is_none());
        assert!(store
            .replace(&user, "session", &first.revision, Some("first"))
            .await
            .unwrap());
        assert!(!store
            .replace(&user, "session", &first.revision, None)
            .await
            .unwrap());
        let current = store.get(&user, "session").await.unwrap().unwrap();
        assert!(store
            .replace(&user, "session", &current.revision, None)
            .await
            .unwrap());
        assert!(store.get(&user, "session").await.unwrap().is_none());
        store.set(&user, "session", "logout").await.unwrap();
        assert!(other.take(&user, "session").await.unwrap().is_none());
        assert_eq!(
            store.take(&user, "session").await.unwrap().unwrap().value,
            "logout"
        );
        assert!(store.take(&user, "session").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn secrets_are_removed_with_their_user_and_survive_only_their_key() {
        let (pool, store, user) = fixture().await;
        store.set(&user, "session", "value").await.unwrap();

        let rotated = SecretStore::new("test", pool.clone(), &[8; 64]);
        assert!(rotated.get(&user, "session").await.unwrap().is_none());

        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(&user)
            .execute(&pool)
            .await
            .unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plugin_user_secrets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }
}
