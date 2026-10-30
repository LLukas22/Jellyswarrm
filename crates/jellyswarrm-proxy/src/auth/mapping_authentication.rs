//! Authenticate saved backend mappings and select credentials for new client sessions.
use crate::{
    auth::service::{MappingAuth, ServerMapping, User as LocalUser},
    models::User,
    servers::storage::Server,
};
use jellyfin_api::{ClientInfo, JellyfinClient};
use std::sync::Arc;

#[derive(thiserror::Error)]
pub(crate) enum MappingAuthenticationError {
    #[error("Mapping does not belong to this user and server")]
    WrongOwner,
    #[error("Not a Quick Connect mapping")]
    WrongKind,
    #[error("Backend account changed; reconnect with Quick Connect")]
    AccountChanged,
    #[error("Mapped server no longer exists")]
    MissingServer,
    #[error("Saved credential could not be decrypted")]
    Credential(#[from] crate::auth::service::MappingCredentialError),
    #[error("Backend authentication failed")]
    Backend(#[from] jellyfin_api::error::Error),
    #[error("Mapping storage failed")]
    Storage(#[from] sqlx::Error),
}
impl std::fmt::Debug for MappingAuthenticationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

pub(crate) struct MappingLogin {
    pub(crate) response: crate::models::AuthenticateResponse,
    pub(crate) username: String,
    pub(crate) password: Option<crate::domain::security::Password>,
}

/// Independent authenticated identity over the shared transport. Each operation
/// gets a fresh client: credentials are never shared by username or mutated in
/// a process-global cache, and dropping this handle never revokes a durable
/// Quick Connect credential. Logout is an explicit session-lifecycle action.
pub(crate) struct AuthenticatedMapping {
    pub(crate) client: Arc<JellyfinClient>,
    pub(crate) user: jellyfin_api::models::User,
    pub(crate) system_info: jellyfin_api::models::PublicSystemInfo,
    credential_owner: CredentialOwner,
}

enum CredentialOwner {
    TemporarySession,
    SavedMapping,
}

impl AuthenticatedMapping {
    /// Finish temporary work explicitly. Durable mapping credentials are not
    /// owned by this operation and must never be revoked here.
    pub(crate) async fn close(self) {
        if matches!(self.credential_owner, CredentialOwner::TemporarySession) {
            if let Err(error) = self.client.logout().await {
                tracing::debug!(%error, "Could not close temporary backend session");
            }
        }
    }
}

/// Owned handles, built once at startup; clones share the same services.
#[derive(Clone)]
pub(crate) struct MappingAuthenticator {
    authorization: Arc<crate::auth::UserAuthorizationService>,
    transport: reqwest::Client,
    admin_password: crate::domain::security::Password,
}

impl MappingAuthenticator {
    fn mapped_password(
        &self,
        user: &LocalUser,
        mapping: &ServerMapping,
        verified_password: Option<&crate::domain::security::Password>,
    ) -> Result<crate::domain::security::Password, MappingAuthenticationError> {
        Ok(self.authorization.decrypt_server_mapping_password(
            mapping,
            &user.local_credential.mapping_key(),
            &(&self.admin_password).into(),
            verified_password,
            Some(&self.admin_password),
        )?)
    }

    /// Validate ambiguous legacy credentials remotely before the local password
    /// transaction. Temporary validation sessions are closed even if the later
    /// mapping upgrade fails. Callers must first verify the local password.
    pub(crate) async fn prepare_password_change(
        &self,
        user: &LocalUser,
        servers: &crate::servers::ServerStorageService,
        verified_password: &crate::domain::security::Password,
    ) -> Result<(), MappingAuthenticationError> {
        let mappings = self
            .authorization
            .upgrade_mappings_with_verified_password(
                &user.id,
                verified_password,
                Some(&self.admin_password),
            )
            .await?;
        for mapping in mappings {
            if mapping.credential_format != crate::auth::service::CredentialFormat::Legacy {
                continue;
            }
            let server = servers
                .get_server_by_id(mapping.server_id)
                .await?
                .ok_or(MappingAuthenticationError::MissingServer)?;
            self.validate_owner(user, &server, &mapping)?;
            let password = self.mapped_password(user, &mapping, Some(verified_password))?;
            let client = JellyfinClient::new_with_client(
                server.url.as_str(),
                ClientInfo {
                    device_id: format!("jellyswarrm-validation-{}", uuid::Uuid::new_v4()),
                    ..crate::config::CLIENT_INFO.clone()
                },
                self.transport.clone(),
            )?;
            client
                .authenticate_by_name(mapping.auth.username(), password.as_str())
                .await?;
            if let Err(error) = client.logout().await {
                tracing::debug!(%error, "Could not close temporary credential-validation session");
            }
            self.authorization
                .upgrade_validated_password_mapping(&mapping, &password)
                .await?;
        }
        Ok(())
    }
    pub(crate) async fn authenticate_password(
        &self,
        server: &Server,
        username: &str,
        password: &crate::domain::security::Password,
        client_info: ClientInfo,
    ) -> Result<MappingLogin, MappingAuthenticationError> {
        let client = JellyfinClient::new_with_client(
            server.url.as_str(),
            client_info,
            self.transport.clone(),
        )?;
        let response = client
            .authenticate_by_name_typed(username, password.as_str())
            .await?;
        Ok(MappingLogin {
            response,
            username: username.to_owned(),
            password: Some(password.clone()),
        })
    }
    pub(crate) fn new(
        authorization: Arc<crate::auth::UserAuthorizationService>,
        transport: reqwest::Client,
        admin_password: crate::domain::security::Password,
    ) -> Self {
        Self {
            authorization,
            transport,
            admin_password,
        }
    }

    fn validate_owner(
        &self,
        user: &LocalUser,
        server: &Server,
        mapping: &ServerMapping,
    ) -> Result<(), MappingAuthenticationError> {
        if mapping.user_id != user.id || mapping.server_id != server.id {
            return Err(MappingAuthenticationError::WrongOwner);
        }
        Ok(())
    }

    async fn validated_token(
        &self,
        user: &LocalUser,
        server: &Server,
        mapping: &ServerMapping,
    ) -> Result<(String, User), MappingAuthenticationError> {
        self.validate_owner(user, server, mapping)?;
        let MappingAuth::QuickConnect {
            backend_user_id, ..
        } = &mapping.auth
        else {
            return Err(MappingAuthenticationError::WrongKind);
        };
        let token = self
            .authorization
            .decrypt_mapping_token(
                mapping,
                &user.local_credential.mapping_key(),
                &(&self.admin_password).into(),
            )
            .map_err(MappingAuthenticationError::Credential)?;
        let client = JellyfinClient::new_with_client(
            server.url.as_str(),
            ClientInfo {
                device_id: mapping_device_id(server, &user.id),
                ..crate::config::CLIENT_INFO.clone()
            },
            self.transport.clone(),
        )
        .map_err(MappingAuthenticationError::Backend)?;
        client.with_token(token.clone()).await;
        let remote: User = client
            .get_me_typed()
            .await
            .map_err(MappingAuthenticationError::Backend)?;
        if remote.id != *backend_user_id {
            return Err(MappingAuthenticationError::AccountChanged);
        }
        Ok((token, remote))
    }

    /// Remote authentication and credential migration only. Registration of a
    /// local authorization session and client-facing decoration are separate
    /// application stages. Never logs out a durable Quick Connect credential.
    pub(crate) async fn authenticate_for_client(
        &self,
        user: &LocalUser,
        server: &Server,
        mapping: &ServerMapping,
        client_info: ClientInfo,
    ) -> Result<crate::models::AuthenticateResponse, MappingAuthenticationError> {
        let login = self
            .authenticate_login(user, server, mapping, None, client_info)
            .await?;
        if let Some(password) = login.password {
            self.authorization
                .add_server_mapping(
                    &user.id,
                    server,
                    &login.username,
                    &password,
                    Some(&user.local_credential.mapping_key()),
                )
                .await?;
        }
        Ok(login.response)
    }

    /// The caller must have verified the local credential before supplying its
    /// plaintext as a legacy decryption candidate. This operation authenticates
    /// remotely but leaves local session registration to the application.
    pub(crate) async fn authenticate_login(
        &self,
        user: &LocalUser,
        server: &Server,
        mapping: &ServerMapping,
        verified_password: Option<&crate::domain::security::Password>,
        client_info: ClientInfo,
    ) -> Result<MappingLogin, MappingAuthenticationError> {
        self.validate_owner(user, server, mapping)?;
        if matches!(mapping.auth, MappingAuth::QuickConnect { .. }) {
            let (token, remote) = self.validated_token(user, server, mapping).await?;
            return Ok(MappingLogin {
                username: mapping.auth.username().to_owned(),
                password: None,
                response: crate::models::AuthenticateResponse {
                    server_id: remote.server_id.clone(),
                    session_info: crate::models::SessionInfo {
                        user_id: remote.id.clone(),
                        user_name: remote.name.clone(),
                        server_id: remote.server_id.clone(),
                        extra: Default::default(),
                    },
                    user: remote,
                    access_token: token,
                },
            });
        }
        let password = self.mapped_password(user, mapping, verified_password)?;
        self.authenticate_password(server, mapping.auth.username(), &password, client_info)
            .await
    }

    pub(crate) async fn authenticate(
        &self,
        user: &LocalUser,
        server: &Server,
        mapping: &ServerMapping,
    ) -> Result<AuthenticatedMapping, MappingAuthenticationError> {
        self.validate_owner(user, server, mapping)?;
        let client = Arc::new(
            JellyfinClient::new_with_client(
                server.url.as_str(),
                ClientInfo {
                    device_id: match mapping.auth {
                        MappingAuth::Password { .. } => format!(
                            "{}-{}",
                            mapping_device_id(server, &user.id),
                            uuid::Uuid::new_v4()
                        ),
                        MappingAuth::QuickConnect { .. } => mapping_device_id(server, &user.id),
                    },
                    ..crate::config::CLIENT_INFO.clone()
                },
                self.transport.clone(),
            )
            .map_err(MappingAuthenticationError::Backend)?,
        );
        let system_info = client
            .get_public_system_info()
            .await
            .map_err(MappingAuthenticationError::Backend)?;
        let remote_user = match &mapping.auth {
            MappingAuth::QuickConnect { .. } => {
                let (token, remote_user) = self.validated_token(user, server, mapping).await?;
                client.with_token(token).await;
                jellyfin_api::models::User {
                    id: remote_user.id,
                    name: remote_user.name,
                    server_id: Some(remote_user.server_id),
                    policy: None,
                }
            }
            MappingAuth::Password { .. } => {
                let password = self.mapped_password(user, mapping, None)?;
                let remote_user = client
                    .authenticate_by_name(mapping.auth.username(), password.as_str())
                    .await
                    .map_err(MappingAuthenticationError::Backend)?;
                if mapping.credential_format == crate::auth::service::CredentialFormat::Legacy {
                    if let Err(error) = self
                        .authorization
                        .upgrade_validated_password_mapping(mapping, &password)
                        .await
                    {
                        let _ = client.logout().await;
                        return Err(MappingAuthenticationError::Storage(error));
                    }
                }
                remote_user
            }
        };
        Ok(AuthenticatedMapping {
            client,
            user: remote_user,
            system_info,
            credential_owner: match mapping.auth {
                MappingAuth::Password { .. } => CredentialOwner::TemporarySession,
                MappingAuth::QuickConnect { .. } => CredentialOwner::SavedMapping,
            },
        })
    }
}

pub(crate) fn mapping_device_id(server: &Server, user_id: &str) -> String {
    format!("jellyswarrm-{}-{}", server.id, user_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::service::UserAuthorizationService, config::AppConfig, persistence::MIGRATOR,
    };
    use std::sync::Arc;
    use wiremock::{
        matchers::{header_regex, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    #[tokio::test]
    async fn client_login_accepts_verified_legacy_password_without_registering_a_session() {
        use crate::{
            auth::service::CredentialFormat,
            domain::security::{encrypt_password, HashedPassword},
        };
        use wiremock::matchers::body_json;
        let backend = MockServer::start().await;
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let authorization = UserAuthorizationService::new(pool.clone());
        let local_password = "local-password".into();
        let user = authorization
            .create_user("local", &local_password)
            .await
            .unwrap();
        let server = Server::from_row(
            sqlx::query(
                "INSERT INTO servers (name, url, priority) VALUES ('Backend', ?, 100) RETURNING *",
            )
            .bind(backend.uri())
            .fetch_one(&pool)
            .await
            .unwrap(),
        )
        .unwrap();
        // Historic mappings could use raw local-password key material, rather
        // than the stored verifier. Only a verified login may supply that key.
        let mapping = ServerMapping {
            id: 1,
            user_id: user.id.clone(),
            server_id: server.id,
            server_url: backend.uri(),
            auth: MappingAuth::Password {
                username: "Remote".into(),
                password: encrypt_password(
                    &"remote-password".into(),
                    &HashedPassword::from_hashed("local-password".into()),
                )
                .unwrap(),
            },
            credential_format: CredentialFormat::Legacy,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        Mock::given(method("POST")).and(path("/Users/AuthenticateByName"))
            .and(body_json(serde_json::json!({"Username":"Remote", "Pw":"remote-password"})))
            .and(header_regex("authorization", "DeviceId=\\\"client-device\\\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "User":{"Id":"remote-id", "Name":"Remote", "ServerId":"backend", "Policy":{"IsAdministrator":false,"SyncPlayAccess":"None"}},
                "AccessToken":"new-client-token", "ServerId":"backend", "SessionInfo":{"UserId":"remote-id","UserName":"Remote","ServerId":"backend"}
            }))).expect(1).mount(&backend).await;
        let authenticator = MappingAuthenticator::new(
            Arc::new(authorization.clone()),
            reqwest::Client::new(),
            "admin".into(),
        );
        let login = authenticator
            .authenticate_login(
                &user,
                &server,
                &mapping,
                Some(&local_password),
                ClientInfo {
                    device_id: "client-device".into(),
                    ..ClientInfo::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(login.response.access_token, "new-client-token");
        assert_eq!(login.username, "Remote");
        assert_eq!(login.password.unwrap().as_str(), "remote-password");
        assert!(authorization
            .list_server_mappings(&user.id)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(backend.received_requests().await.unwrap().len(), 1);
        backend.verify().await;
    }

    #[test]
    fn authentication_errors_keep_credential_causes_without_exposing_them() {
        use std::error::Error;
        let error = MappingAuthenticationError::Credential(
            crate::auth::service::MappingCredentialError::Encryption(
                crate::domain::security::EncryptionError::PasswordDecryptionFailed,
            ),
        );
        let cause = error.source().unwrap();
        assert!(cause
            .downcast_ref::<crate::auth::service::MappingCredentialError>()
            .is_some());
        assert!(cause
            .source()
            .unwrap()
            .downcast_ref::<crate::domain::security::EncryptionError>()
            .is_some());
        assert_eq!(error.to_string(), "Saved credential could not be decrypted");
    }

    #[tokio::test]
    async fn validates_token_and_original_backend_account() {
        let upstream = MockServer::start().await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let config = AppConfig::default();
        let mapping_key =
            crate::domain::security::MappingEncryptionKey::from_session_key(&config.session_key)
                .unwrap();
        let authorization = UserAuthorizationService::with_mapping_key(
            pool.clone(),
            mapping_key,
            (&config.password).into(),
        );
        let authenticator = MappingAuthenticator::new(
            Arc::new(authorization.clone()),
            reqwest::Client::new(),
            config.password,
        );
        let server = Server::from_row(
            sqlx::query(
                "INSERT INTO servers (name, url, priority) VALUES ('Backend', ?, 100) RETURNING *",
            )
            .bind(upstream.uri())
            .fetch_one(&pool)
            .await
            .unwrap(),
        )
        .unwrap();
        let local = authorization
            .create_user("local", &"password".into())
            .await
            .unwrap();
        authorization
            .add_quick_connect_mapping(
                &local.id,
                &server,
                "Remote",
                "backend-id",
                "saved-token",
                &local.local_credential.mapping_key(),
            )
            .await
            .unwrap();
        let mapping = authorization
            .get_server_mapping(&local.id, &server)
            .await
            .unwrap()
            .unwrap();

        Mock::given(method("GET"))
            .and(path("/Users/Me"))
            .and(header_regex("authorization", "Token=\\\"saved-token\\\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "Id": "backend-id", "Name": "Remote", "ServerId": "upstream",
                "Policy": { "IsAdministrator": false, "SyncPlayAccess": "None" }
            })))
            .mount(&upstream)
            .await;
        let (token, remote) = authenticator
            .validated_token(&local, &server, &mapping)
            .await
            .unwrap();
        assert_eq!(token, "saved-token");
        assert_eq!(remote.id, "backend-id");

        Mock::given(method("GET"))
            .and(path("/System/Info/Public"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&upstream)
            .await;
        let first = authenticator
            .authenticate(&local, &server, &mapping)
            .await
            .unwrap();
        let second = authenticator
            .authenticate(&local, &server, &mapping)
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&first.client, &second.client));
        first.client.with_token("changed-token".into()).await;
        assert_eq!(
            second.client.get_token().await.as_deref(),
            Some("saved-token")
        );
        first.close().await;
        second.close().await;
        assert!(upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path() != "/Sessions/Logout"));
        let mut wrong_owner = local.clone();
        wrong_owner.id = "another-local-user".into();
        let before = upstream.received_requests().await.unwrap().len();
        assert!(authenticator
            .authenticate(&wrong_owner, &server, &mapping)
            .await
            .is_err());
        assert_eq!(upstream.received_requests().await.unwrap().len(), before);

        upstream.reset().await;
        Mock::given(method("GET"))
            .and(path("/Users/Me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "Id": "different-id", "Name": "Different", "ServerId": "upstream",
                "Policy": { "IsAdministrator": false, "SyncPlayAccess": "None" }
            })))
            .mount(&upstream)
            .await;
        assert!(authenticator
            .validated_token(&local, &server, &mapping)
            .await
            .is_err());

        upstream.reset().await;
        authorization
            .add_server_mapping(
                &local.id,
                &server,
                "Remote",
                &"password".into(),
                Some(&local.local_credential.mapping_key()),
            )
            .await
            .unwrap();
        let mapping = authorization
            .get_server_mapping(&local.id, &server)
            .await
            .unwrap()
            .unwrap();
        Mock::given(method("GET"))
            .and(path("/System/Info/Public"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&upstream)
            .await;
        Mock::given(method("POST")).and(path("/Users/AuthenticateByName"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"AccessToken":"temporary", "User":{"Id":"backend-id", "Name":"Remote"}})))
            .mount(&upstream).await;
        Mock::given(method("POST"))
            .and(path("/Sessions/Logout"))
            .respond_with(ResponseTemplate::new(204))
            .expect(2)
            .mount(&upstream)
            .await;
        let first = authenticator
            .authenticate(&local, &server, &mapping)
            .await
            .unwrap();
        let second = authenticator
            .authenticate(&local, &server, &mapping)
            .await
            .unwrap();
        let requests = upstream.received_requests().await.unwrap();
        let logins: Vec<_> = requests
            .iter()
            .filter(|request| request.url.path() == "/Users/AuthenticateByName")
            .collect();
        assert_eq!(logins.len(), 2);
        assert_ne!(
            logins[0].headers["authorization"],
            logins[1].headers["authorization"]
        );
        first.close().await;
        second.close().await;
    }
}
