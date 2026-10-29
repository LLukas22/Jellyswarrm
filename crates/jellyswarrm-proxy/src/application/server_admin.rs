//! Registry mutations and their dependent effects. SQL commits first; later
//! requests resolve fresh server settings and cannot reuse earlier catalog keys.
use crate::{
    application::catalog::snapshots::CatalogSnapshots,
    config::MediaStreamingMode,
    domain::security::{encrypt_password, EncryptionError, HashedPassword, Password},
    domain::server_id::ServerId,
    playback::sessions::SessionStorage,
    servers::ServerStorageService,
};

/// Narrow credential-administration workflow, independent of templates, incoming
/// HTTP status policy, playback storage, and the global application container.
pub(crate) struct ServerCredentials<'a> {
    registry: &'a ServerStorageService,
    transport: reqwest::Client,
    encryption_key: HashedPassword,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ServerCredentialError {
    #[error("Server not found")]
    NotFound,
    #[error("User is not an administrator on this server")]
    NotAdministrator,
    #[error("Could not construct backend client")]
    Client(#[source] jellyfin_api::error::Error),
    #[error("Backend authentication failed")]
    Authentication(#[source] jellyfin_api::error::Error),
    #[error("Credential encryption failed")]
    Encryption(#[from] EncryptionError),
    #[error("Credential storage failed")]
    Storage(#[from] sqlx::Error),
}

impl<'a> ServerCredentials<'a> {
    pub(crate) fn new(
        registry: &'a ServerStorageService,
        transport: reqwest::Client,
        encryption_key: HashedPassword,
    ) -> Self {
        Self {
            registry,
            transport,
            encryption_key,
        }
    }

    pub(crate) async fn save(
        &self,
        server_id: ServerId,
        username: &str,
        password: &Password,
    ) -> Result<(), ServerCredentialError> {
        let server = self
            .registry
            .get_server_by_id(server_id)
            .await?
            .ok_or(ServerCredentialError::NotFound)?;
        let client = jellyfin_api::JellyfinClient::new_with_client(
            server.url.as_str(),
            jellyfin_api::ClientInfo {
                device_id: format!("jellyswarrm-admin-check-{}", uuid::Uuid::new_v4()),
                ..crate::config::CLIENT_INFO.clone()
            },
            self.transport.clone(),
        )
        .map_err(ServerCredentialError::Client)?;
        let user = client
            .authenticate_by_name(username, password.as_str())
            .await
            .map_err(ServerCredentialError::Authentication)?;
        // This login is only proof of credentials/role, not a saved credential.
        // Close it on both role rejection and later encryption/storage failure.
        if let Err(error) = client.logout().await {
            tracing::debug!(%error, "Could not close administrator verification session");
        }
        if !user.policy.is_some_and(|policy| policy.is_administrator) {
            return Err(ServerCredentialError::NotAdministrator);
        }
        let encrypted = encrypt_password(password, &self.encryption_key)?;
        self.registry
            .add_server_admin(server_id, username, &encrypted)
            .await?;
        Ok(())
    }

    pub(crate) async fn delete(&self, server_id: ServerId) -> Result<bool, ServerCredentialError> {
        Ok(self.registry.delete_server_admin(server_id).await?)
    }
}

pub(crate) struct ServerAdministration<'a> {
    registry: &'a ServerStorageService,
    playback: &'a SessionStorage,
    catalogs: &'a CatalogSnapshots,
}

impl<'a> ServerAdministration<'a> {
    pub(crate) fn new(
        registry: &'a ServerStorageService,
        playback: &'a SessionStorage,
        catalogs: &'a CatalogSnapshots,
    ) -> Self {
        Self {
            registry,
            playback,
            catalogs,
        }
    }

    pub(crate) async fn add(
        &self,
        name: &str,
        url: &str,
        priority: i32,
        mode: MediaStreamingMode,
    ) -> Result<ServerId, sqlx::Error> {
        let id = self.registry.add_server(name, url, priority, mode).await?;
        self.catalogs.invalidate_all();
        self.registry.check_servers_health().await;
        Ok(id)
    }

    pub(crate) async fn set_priority(
        &self,
        id: ServerId,
        priority: i32,
    ) -> Result<bool, sqlx::Error> {
        let changed = self.registry.update_server_priority(id, priority).await?;
        if changed {
            self.catalogs.invalidate_all();
        }
        Ok(changed)
    }

    pub(crate) async fn set_streaming_mode(
        &self,
        id: ServerId,
        mode: MediaStreamingMode,
    ) -> Result<bool, sqlx::Error> {
        let changed = self
            .registry
            .update_server_media_streaming_mode(id, mode)
            .await?;
        if changed {
            self.catalogs.invalidate_all();
        }
        Ok(changed)
    }

    pub(crate) async fn delete(&self, id: ServerId) -> Result<bool, sqlx::Error> {
        let deleted = self.registry.delete_server(id).await?;
        if deleted {
            self.catalogs.invalidate_all();
            self.playback.remove_sessions_for_server(id).await;
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        media::MediaStorageService, persistence::MIGRATOR, playback::sessions::PlaybackSession,
    };

    #[tokio::test]
    async fn administrator_credentials_are_verified_encrypted_and_failure_atomic() {
        use crate::domain::security::decrypt_password;
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let upstream = MockServer::start().await;
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let registry = ServerStorageService::new(pool);
        let id = registry
            .add_server("Backend", &upstream.uri(), 100, MediaStreamingMode::Proxy)
            .await
            .unwrap();
        let key: HashedPassword = (&Password::from("master-password")).into();
        let operation = ServerCredentials::new(&registry, reqwest::Client::new(), key.clone());
        Mock::given(method("POST")).and(path("/Users/AuthenticateByName"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "AccessToken":"temporary-admin-token", "User":{"Id":"admin", "Name":"Admin", "Policy":{"IsAdministrator":true}}
            }))).expect(1).mount(&upstream).await;
        Mock::given(method("POST"))
            .and(path("/Sessions/Logout"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&upstream)
            .await;
        operation
            .save(id, "admin", &"saved-password".into())
            .await
            .unwrap();
        let saved = registry.get_server_admin(id).await.unwrap().unwrap();
        assert_eq!(saved.username, "admin");
        assert_eq!(
            decrypt_password(&saved.password, &key).unwrap(),
            Password::from("saved-password")
        );
        upstream.verify().await;

        for policy in [
            serde_json::json!({"IsAdministrator":false}),
            serde_json::Value::Null,
        ] {
            upstream.reset().await;
            Mock::given(method("POST")).and(path("/Users/AuthenticateByName"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "AccessToken":"unprivileged-token", "User":{"Id":"ordinary", "Name":"User", "Policy":policy}
                }))).expect(1).mount(&upstream).await;
            Mock::given(method("POST"))
                .and(path("/Sessions/Logout"))
                .respond_with(ResponseTemplate::new(204))
                .expect(1)
                .mount(&upstream)
                .await;
            assert!(matches!(
                operation.save(id, "ordinary", &"wrong".into()).await,
                Err(ServerCredentialError::NotAdministrator)
            ));
            let retained = registry.get_server_admin(id).await.unwrap().unwrap();
            assert_eq!(retained.username, "admin");
            assert_eq!(retained.password, saved.password);
            upstream.verify().await;
        }
        upstream.reset().await;
        Mock::given(method("POST"))
            .and(path("/Users/AuthenticateByName"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&upstream)
            .await;
        let error = operation
            .save(id, "invalid", &"wrong".into())
            .await
            .unwrap_err();
        assert!(matches!(error, ServerCredentialError::Authentication(_)));
        assert!(std::error::Error::source(&error)
            .unwrap()
            .is::<jellyfin_api::error::Error>());
        assert_eq!(
            registry
                .get_server_admin(id)
                .await
                .unwrap()
                .unwrap()
                .password,
            saved.password
        );
        assert!(operation.delete(id).await.unwrap());
        assert!(!operation.delete(id).await.unwrap());
        assert!(registry.get_server_admin(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn missing_server_is_rejected_before_any_authentication() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let registry = ServerStorageService::new(pool);
        let operation = ServerCredentials::new(
            &registry,
            reqwest::Client::new(),
            (&Password::from("master")).into(),
        );
        assert!(matches!(
            operation
                .save(ServerId::new(999), "admin", &"password".into())
                .await,
            Err(ServerCredentialError::NotFound)
        ));
    }

    #[tokio::test]
    async fn registry_changes_are_visible_to_mapping_reads_and_deletion_releases_playback() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let registry = ServerStorageService::new(pool.clone());
        let media = MediaStorageService::new(pool);
        let playback = SessionStorage::new();
        let catalogs = CatalogSnapshots::default();
        let id = registry
            .add_server(
                "Backend",
                "http://backend.example",
                100,
                MediaStreamingMode::Proxy,
            )
            .await
            .unwrap();
        let server = registry.get_server_by_id(id).await.unwrap().unwrap();
        let mapping = media
            .get_or_create_media_mapping("item", &server)
            .await
            .unwrap();
        assert_eq!(
            media
                .get_media_mapping_with_server(&mapping.virtual_media_id)
                .await
                .unwrap()
                .unwrap()
                .1
                .priority,
            100
        );
        playback
            .add_session(PlaybackSession {
                session_id: "play".into(),
                item_id: mapping.virtual_media_id.clone(),
                user_id: "viewer".into(),
                server_id: id,
                original_item_id: Some("item".into()),
            })
            .await;
        let admin = ServerAdministration::new(&registry, &playback, &catalogs);
        assert!(admin.set_priority(id, 999).await.unwrap());
        assert!(admin
            .set_streaming_mode(id, MediaStreamingMode::Redirect)
            .await
            .unwrap());
        let resolved = media
            .get_media_mapping_with_server(&mapping.virtual_media_id)
            .await
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(resolved.priority, 999);
        assert_eq!(resolved.media_streaming_mode, MediaStreamingMode::Redirect);
        assert!(admin.delete(id).await.unwrap());
        assert!(media
            .get_media_mapping_with_server(&mapping.virtual_media_id)
            .await
            .unwrap()
            .is_none());
        assert!(playback.get_session("play").await.is_none());
        assert!(!admin.delete(id).await.unwrap());
    }
}
