use std::{collections::HashSet, sync::Arc};

use tracing::{error, info, warn};

use crate::{
    encryption::{decrypt_password, HashedPassword, Password},
    server_storage::ServerStorageService,
    user_authorization_service::{LocalCredential, UserAuthorizationService},
    AppState,
};
use jellyfin_api::JellyfinClient;

use crate::server_id::ServerId;

#[derive(Debug, Clone)]
pub enum SyncStatus {
    Created,
    AlreadyExists,
    ExistsWithDifferentPassword,
    Failed,
    Skipped,
    Deleted,
    NotFound,
}

#[derive(Debug, Clone)]
pub struct ServerSyncResult {
    pub server_name: String,
    pub status: SyncStatus,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserSyncStatus {
    Created,
    MappedExisting,
    AlreadyMapped,
    ExistsWithDifferentPassword,
    NoReusableCredentials,
    Failed,
}

#[derive(Debug, Clone)]
pub struct UserSyncResult {
    pub username: String,
    pub status: UserSyncStatus,
    pub message: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum UserSyncError {
    #[error("Server not found")]
    ServerNotFound,
    #[error("This server does not have managed admin credentials")]
    NoAdminCredentials,
    #[error("Could not load the server configuration")]
    ServerStorage,
    #[error("Could not decrypt the configured server administrator password")]
    AdminPasswordDecryption,
    #[error("Could not connect to the server")]
    Client,
    #[error("The configured server administrator credentials are no longer valid")]
    AdminAuthentication,
    #[error("Could not load users from the server")]
    RemoteUsers,
    #[error("Could not load Jellyswarrm users")]
    LocalUsers,
}

#[derive(Clone)]
pub struct FederatedUserService {
    server_storage: Arc<ServerStorageService>,
    user_authorization: Arc<UserAuthorizationService>,
    config: Arc<tokio::sync::RwLock<crate::config::AppConfig>>,
}

impl FederatedUserService {
    pub fn new(state: &AppState) -> Self {
        Self {
            server_storage: state.server_storage.clone(),
            user_authorization: state.user_authorization.clone(),
            config: state.config.clone(),
        }
    }

    pub fn new_from_components(
        server_storage: Arc<ServerStorageService>,
        user_authorization: Arc<UserAuthorizationService>,
        config: Arc<tokio::sync::RwLock<crate::config::AppConfig>>,
    ) -> Self {
        Self {
            server_storage,
            user_authorization,
            config,
        }
    }

    /// Synchronize every existing Jellyswarrm user to one managed Jellyfin server.
    ///
    /// Reusable passwords come only from decryptable, same-username mappings. Local password
    /// hashes are never reversed or used as remote passwords.
    pub async fn sync_all_users_to_server(
        &self,
        server_id: ServerId,
    ) -> Result<Vec<UserSyncResult>, UserSyncError> {
        let server = self
            .server_storage
            .get_server_by_id(server_id)
            .await
            .map_err(|error| {
                error!(
                    "Failed to load server {} for user sync: {}",
                    server_id, error
                );
                UserSyncError::ServerStorage
            })?
            .ok_or(UserSyncError::ServerNotFound)?;

        let admin = self
            .server_storage
            .get_server_admin(server_id)
            .await
            .map_err(|error| {
                error!(
                    "Failed to load admin credentials for server {}: {}",
                    server.name, error
                );
                UserSyncError::ServerStorage
            })?
            .ok_or(UserSyncError::NoAdminCredentials)?;

        let (admin_master, admin_master_plain) = {
            let config = self.config.read().await;
            (
                HashedPassword::from(&config.password),
                config.password.clone(),
            )
        };
        let server_admin_password =
            decrypt_password(&admin.password, &admin_master).map_err(|error| {
                error!(
                    "Failed to decrypt admin password for server {}: {}",
                    server.name, error
                );
                UserSyncError::AdminPasswordDecryption
            })?;

        let client = JellyfinClient::new(server.url.as_str(), crate::config::CLIENT_INFO.clone())
            .map_err(|error| {
            error!(
                "Failed to create Jellyfin client for server {}: {}",
                server.name, error
            );
            UserSyncError::Client
        })?;

        client
            .authenticate_by_name(&admin.username, server_admin_password.as_str())
            .await
            .map_err(|_| {
                error!(
                    "Failed to authenticate configured admin on server {}",
                    server.name
                );
                UserSyncError::AdminAuthentication
            })?;

        let remote_users = client.get_users().await.map_err(|_| {
            error!(
                "Failed to list users on server {} during user sync",
                server.name
            );
            UserSyncError::RemoteUsers
        })?;
        let local_users = self
            .user_authorization
            .list_users()
            .await
            .map_err(|error| {
                error!("Failed to list local users for user sync: {}", error);
                UserSyncError::LocalUsers
            })?;

        let remote_usernames: HashSet<String> = remote_users
            .iter()
            .map(|user| user.name.to_ascii_lowercase())
            .collect();
        let mut results = Vec::with_capacity(local_users.len());

        for user in local_users {
            let username = user.original_username.clone();
            match self
                .user_authorization
                .get_server_mapping_by_server_id(&user.id, server_id)
                .await
            {
                Ok(Some(_)) => {
                    results.push(UserSyncResult {
                        username,
                        status: UserSyncStatus::AlreadyMapped,
                        message: None,
                    });
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    error!(
                        "Failed to check target mapping for user {} on server {}: {}",
                        user.original_username, server.name, error
                    );
                    results.push(UserSyncResult {
                        username,
                        status: UserSyncStatus::Failed,
                        message: Some("Could not check existing mappings".to_string()),
                    });
                    continue;
                }
            }

            let candidate = if matches!(user.local_credential, LocalCredential::Passwordless) {
                Ok(Password::from(""))
            } else {
                self.reusable_password_for_user(&user, &admin_master, &admin_master_plain)
                    .await
            };

            let candidate = match candidate {
                Ok(password) => password,
                Err(message) => {
                    results.push(UserSyncResult {
                        username,
                        status: UserSyncStatus::NoReusableCredentials,
                        message: Some(message),
                    });
                    continue;
                }
            };

            if remote_usernames.contains(&user.original_username.to_ascii_lowercase()) {
                let user_client = match JellyfinClient::new(
                    server.url.as_str(),
                    crate::config::CLIENT_INFO.clone(),
                ) {
                    Ok(client) => client,
                    Err(error) => {
                        error!(
                            "Failed to create validation client for user {} on server {}: {}",
                            user.original_username, server.name, error
                        );
                        results.push(UserSyncResult {
                            username,
                            status: UserSyncStatus::Failed,
                            message: Some(
                                "Could not connect to validate the remote user".to_string(),
                            ),
                        });
                        continue;
                    }
                };

                match user_client
                    .authenticate_by_name(&user.original_username, candidate.as_str())
                    .await
                {
                    Ok(_) => {}
                    Err(jellyfin_api::error::Error::AuthenticationFailed(_)) => {
                        results.push(UserSyncResult {
                            username,
                            status: UserSyncStatus::ExistsWithDifferentPassword,
                            message: Some(
                                "Remote user exists, but the reusable credentials did not match"
                                    .to_string(),
                            ),
                        });
                        continue;
                    }
                    Err(_) => {
                        error!(
                            "Failed to validate user {} on server {}",
                            user.original_username, server.name
                        );
                        results.push(UserSyncResult {
                            username,
                            status: UserSyncStatus::Failed,
                            message: Some(
                                "Could not validate the existing remote user".to_string(),
                            ),
                        });
                        continue;
                    }
                }

                let status = UserSyncStatus::MappedExisting;
                if let Err(error) = self
                    .user_authorization
                    .add_server_mapping(
                        &user.id,
                        &server,
                        &user.original_username,
                        &candidate,
                        Some(&user.local_credential.mapping_key()),
                    )
                    .await
                {
                    error!(
                        "Failed to save mapping for user {} on server {}: {}",
                        user.original_username, server.name, error
                    );
                    results.push(UserSyncResult {
                        username,
                        status: UserSyncStatus::Failed,
                        message: Some(
                            "Remote user matched, but the mapping could not be saved".to_string(),
                        ),
                    });
                } else {
                    results.push(UserSyncResult {
                        username,
                        status,
                        message: None,
                    });
                }
                continue;
            }

            let remote_password = (!candidate.as_str().is_empty()).then_some(candidate.as_str());
            if client
                .create_user(&user.original_username, remote_password)
                .await
                .is_err()
            {
                warn!(
                    "Failed to create user {} on server {}",
                    user.original_username, server.name
                );
                results.push(UserSyncResult {
                    username,
                    status: UserSyncStatus::Failed,
                    message: Some("Could not create the remote user".to_string()),
                });
                continue;
            }

            if let Err(error) = self
                .user_authorization
                .add_server_mapping(
                    &user.id,
                    &server,
                    &user.original_username,
                    &candidate,
                    Some(&user.local_credential.mapping_key()),
                )
                .await
            {
                error!(
                    "Created user {} on server {}, but failed to save its mapping: {}",
                    user.original_username, server.name, error
                );
                results.push(UserSyncResult {
                    username,
                    status: UserSyncStatus::Failed,
                    message: Some(
                        "Remote user was created, but the mapping could not be saved".to_string(),
                    ),
                });
            } else {
                results.push(UserSyncResult {
                    username,
                    status: UserSyncStatus::Created,
                    message: None,
                });
            }
        }

        Ok(results)
    }

    async fn reusable_password_for_user(
        &self,
        user: &crate::user_authorization_service::User,
        admin_master: &HashedPassword,
        admin_password_plain: &Password,
    ) -> Result<Password, String> {
        let mappings = self
            .user_authorization
            .list_server_mappings(&user.id)
            .await
            .map_err(|error| {
                error!(
                    "Failed to load mappings for user {} during user sync: {}",
                    user.original_username, error
                );
                "Could not load existing mappings".to_string()
            })?;
        let user_master = user.local_credential.mapping_key();
        let mut candidates = HashSet::new();

        for mapping in mappings.iter().filter(|mapping| {
            mapping
                .mapped_username
                .eq_ignore_ascii_case(&user.original_username)
        }) {
            if let Some(password) = self.user_authorization.try_decrypt_server_mapping_password(
                mapping,
                &user_master,
                admin_master,
                None,
                Some(admin_password_plain),
            ) {
                candidates.insert(password);
            }
        }

        match candidates.len() {
            0 => Err("No safely reusable credentials were found".to_string()),
            1 => Ok(candidates.into_iter().next().expect("candidate exists")),
            _ => Err("Existing mappings contain conflicting credentials".to_string()),
        }
    }

    /// Syncs a user to all configured servers where an admin account is available.
    /// If the user does not exist on a server, it is created.
    /// If the user exists, we assume it's fine (we don't update passwords for existing users here to avoid conflicts).
    pub async fn sync_user_to_all_servers(
        &self,
        username: &str,
        password: &Password,
        user_id: &str,
    ) -> Vec<ServerSyncResult> {
        let mut results = Vec::new();
        let servers = match self.server_storage.list_servers().await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to list servers for sync: {}", e);
                return results;
            }
        };

        let config = self.config.read().await;
        let admin_password: HashedPassword = config.password.clone().into();
        let mapping_key = LocalCredential::from_password(password).mapping_key();

        drop(config);

        for server in servers {
            // Check if we have admin credentials for this server
            if let Some(admin) = match self.server_storage.get_server_admin(server.id).await {
                Ok(a) => a,
                Err(e) => {
                    results.push(ServerSyncResult {
                        server_name: server.name.clone(),
                        status: SyncStatus::Failed,
                        message: Some(format!("Failed to get admin creds: {}", e)),
                    });
                    continue;
                }
            } {
                // Decrypt admin password
                let decrypted_admin_password =
                    match decrypt_password(&admin.password, &admin_password) {
                        Ok(p) => p,
                        Err(e) => {
                            error!(
                                "Failed to decrypt admin password for server {}: {}",
                                server.name, e
                            );
                            results.push(ServerSyncResult {
                                server_name: server.name.clone(),
                                status: SyncStatus::Failed,
                                message: Some("Failed to decrypt admin password".to_string()),
                            });
                            continue;
                        }
                    };

                let client_info = crate::config::CLIENT_INFO.clone();

                let client = match JellyfinClient::new(server.url.as_str(), client_info.clone()) {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to create jellyfin client: {}", e);
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status: SyncStatus::Failed,
                            message: Some(format!("Client error: {}", e)),
                        });
                        continue;
                    }
                };

                // Authenticate as admin to get token
                match client
                    .authenticate_by_name(&admin.username, decrypted_admin_password.as_str())
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        error!(
                            "Failed to authenticate as admin on server {}: {}",
                            server.name, e
                        );
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status: SyncStatus::Failed,
                            message: Some(format!("Admin auth failed: {}", e)),
                        });
                        continue;
                    }
                };

                // Check if user exists
                let users = match client.get_users().await {
                    Ok(u) => u,
                    Err(e) => {
                        error!("Failed to list users on server {}: {}", server.name, e);
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status: SyncStatus::Failed,
                            message: Some(format!("Failed to list users: {}", e)),
                        });
                        continue;
                    }
                };

                let existing_user = users.iter().find(|u| u.name.eq_ignore_ascii_case(username));

                if let Some(remote_user) = existing_user {
                    // User exists. Check if password matches.
                    // We need a new client to check user password
                    let user_client =
                        match JellyfinClient::new(server.url.as_str(), client_info.clone()) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };

                    let (status, should_map) = match user_client
                        .authenticate_by_name(username, password.as_str())
                        .await
                    {
                        Ok(_) => (SyncStatus::AlreadyExists, true),
                        Err(_) => (SyncStatus::ExistsWithDifferentPassword, false),
                    };

                    info!(
                        "Synced user {} to server {} (Remote ID: {}, Status: {:?})",
                        username, server.name, remote_user.id, status
                    );

                    if should_map {
                        if let Err(e) = self
                            .user_authorization
                            .add_server_mapping(
                                user_id,
                                &server,
                                username,
                                password,
                                Some(&mapping_key),
                            )
                            .await
                        {
                            error!(
                                "Failed to create local mapping for synced user on server {}: {}",
                                server.name, e
                            );
                            results.push(ServerSyncResult {
                                server_name: server.name.clone(),
                                status: SyncStatus::Failed,
                                message: Some(format!("Failed to save local mapping: {}", e)),
                            });
                        } else {
                            results.push(ServerSyncResult {
                                server_name: server.name.clone(),
                                status,
                                message: None,
                            });
                        }
                    } else {
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status,
                            message: Some("User exists with different password".to_string()),
                        });
                    }
                } else {
                    // Create user
                    let remote_password = if password.as_str().is_empty() {
                        None
                    } else {
                        Some(password.as_str())
                    };

                    match client.create_user(username, remote_password).await {
                        Ok(new_user) => {
                            info!(
                                "Synced user {} to server {} (Remote ID: {}, Status: Created)",
                                username, server.name, new_user.id
                            );

                            if let Err(e) = self
                                .user_authorization
                                .add_server_mapping(
                                    user_id,
                                    &server,
                                    username,
                                    password,
                                    Some(&mapping_key),
                                )
                                .await
                            {
                                error!(
                                    "Failed to create local mapping for synced user on server {}: {}",
                                    server.name, e
                                );
                                results.push(ServerSyncResult {
                                    server_name: server.name.clone(),
                                    status: SyncStatus::Failed,
                                    message: Some(format!("Failed to save local mapping: {}", e)),
                                });
                            } else {
                                results.push(ServerSyncResult {
                                    server_name: server.name.clone(),
                                    status: SyncStatus::Created,
                                    message: None,
                                });
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Failed to sync user {} to server {}: {}",
                                username, server.name, e
                            );
                            results.push(ServerSyncResult {
                                server_name: server.name.clone(),
                                status: SyncStatus::Failed,
                                message: Some(format!("Sync failed: {}", e)),
                            });
                        }
                    }
                }
            } else {
                warn!(
                    "Skipping sync for server {}: No admin credentials configured",
                    server.name
                );
                results.push(ServerSyncResult {
                    server_name: server.name.clone(),
                    status: SyncStatus::Skipped,
                    message: Some("No admin credentials".to_string()),
                });
            }
        }

        results
    }

    pub async fn delete_user_from_all_servers(&self, username: &str) -> Vec<ServerSyncResult> {
        let mut results = Vec::new();
        let servers = match self.server_storage.list_servers().await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to list servers for delete: {}", e);
                return results;
            }
        };

        let config = self.config.read().await;
        let admin_password = &config.password;

        for server in servers {
            if let Some(admin) = match self.server_storage.get_server_admin(server.id).await {
                Ok(a) => a,
                Err(e) => {
                    results.push(ServerSyncResult {
                        server_name: server.name.clone(),
                        status: SyncStatus::Failed,
                        message: Some(format!("Failed to get admin creds: {}", e)),
                    });
                    continue;
                }
            } {
                let decrypted_admin_password =
                    match decrypt_password(&admin.password, &admin_password.into()) {
                        Ok(p) => p,
                        Err(e) => {
                            error!(
                                "Failed to decrypt admin password for server {}: {}",
                                server.name, e
                            );
                            results.push(ServerSyncResult {
                                server_name: server.name.clone(),
                                status: SyncStatus::Failed,
                                message: Some("Failed to decrypt admin password".to_string()),
                            });
                            continue;
                        }
                    };

                let client_info = crate::config::CLIENT_INFO.clone();

                let client = match JellyfinClient::new(server.url.as_str(), client_info.clone()) {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to create jellyfin client: {}", e);
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status: SyncStatus::Failed,
                            message: Some(format!("Client error: {}", e)),
                        });
                        continue;
                    }
                };

                match client
                    .authenticate_by_name(&admin.username, decrypted_admin_password.as_str())
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        error!(
                            "Failed to authenticate as admin on server {}: {}",
                            server.name, e
                        );
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status: SyncStatus::Failed,
                            message: Some(format!("Admin auth failed: {}", e)),
                        });
                        continue;
                    }
                };

                // Find user ID
                let users = match client.get_users().await {
                    Ok(u) => u,
                    Err(e) => {
                        error!("Failed to list users on server {}: {}", server.name, e);
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status: SyncStatus::Failed,
                            message: Some(format!("Failed to list users: {}", e)),
                        });
                        continue;
                    }
                };

                let user_id = users
                    .iter()
                    .find(|u| u.name.eq_ignore_ascii_case(username))
                    .map(|u| u.id.clone());

                if let Some(id) = user_id {
                    match client.delete_user(&id).await {
                        Ok(_) => {
                            info!(
                                "Deleted user {} from server {} (Deleted: true)",
                                username, server.name
                            );
                            results.push(ServerSyncResult {
                                server_name: server.name.clone(),
                                status: SyncStatus::Deleted,
                                message: None,
                            });
                        }
                        Err(e) => {
                            warn!(
                                "Failed to delete user {} from server {}: {}",
                                username, server.name, e
                            );
                            results.push(ServerSyncResult {
                                server_name: server.name.clone(),
                                status: SyncStatus::Failed,
                                message: Some(format!("Delete failed: {}", e)),
                            });
                        }
                    }
                } else {
                    results.push(ServerSyncResult {
                        server_name: server.name.clone(),
                        status: SyncStatus::NotFound,
                        message: None,
                    });
                }
            } else {
                results.push(ServerSyncResult {
                    server_name: server.name.clone(),
                    status: SyncStatus::Skipped,
                    message: Some("No admin credentials".to_string()),
                });
            }
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{AppConfig, MediaStreamingMode, MIGRATOR},
        encryption::encrypt_password,
        server_storage::Server,
        user_authorization_service::User,
    };
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;
    use tokio::sync::RwLock;
    use wiremock::{
        matchers::{body_partial_json, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    struct TestContext {
        mock: MockServer,
        service: FederatedUserService,
        authorization: Arc<UserAuthorizationService>,
        source: Server,
        target: Server,
    }

    async fn setup(remote_users: serde_json::Value, expected_syncs: u64) -> TestContext {
        let mock = MockServer::start().await;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        MIGRATOR.run(&pool).await.unwrap();

        let storage = Arc::new(ServerStorageService::new(pool.clone()));
        let authorization = Arc::new(UserAuthorizationService::new(pool));
        let config = AppConfig {
            password: Password::from("jellyswarrm-master"),
            ..AppConfig::default()
        };
        let config = Arc::new(RwLock::new(config));

        let source_id = storage
            .add_server(
                "source",
                "http://source.example",
                100,
                MediaStreamingMode::Proxy,
            )
            .await
            .unwrap();
        let target_id = storage
            .add_server("target", &mock.uri(), 100, MediaStreamingMode::Proxy)
            .await
            .unwrap();
        let source = storage.get_server_by_id(source_id).await.unwrap().unwrap();
        let target = storage.get_server_by_id(target_id).await.unwrap().unwrap();

        let admin_password = Password::from("managed-admin-password");
        let admin_master = {
            let config = config.read().await;
            HashedPassword::from(&config.password)
        };
        let encrypted_admin = encrypt_password(&admin_password, &admin_master).unwrap();
        storage
            .add_server_admin(target_id, "managed-admin", &encrypted_admin)
            .await
            .unwrap();

        Mock::given(method("POST"))
            .and(path("/Users/AuthenticateByName"))
            .and(body_partial_json(json!({
                "Username": "managed-admin",
                "Pw": "managed-admin-password"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(auth_response("managed-admin")))
            .expect(expected_syncs)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/Users"))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote_users))
            .expect(expected_syncs)
            .mount(&mock)
            .await;

        let service =
            FederatedUserService::new_from_components(storage, authorization.clone(), config);

        TestContext {
            mock,
            service,
            authorization,
            source,
            target,
        }
    }

    fn remote_user(username: &str) -> serde_json::Value {
        json!({
            "Id": format!("remote-{username}"),
            "Name": username,
            "ServerId": "server",
            "Policy": null
        })
    }

    fn auth_response(username: &str) -> serde_json::Value {
        json!({
            "AccessToken": format!("token-{username}"),
            "User": remote_user(username)
        })
    }

    async fn add_user_with_reusable_mapping(
        context: &TestContext,
        username: &str,
        password: &str,
    ) -> User {
        let password = Password::from(password);
        let user = context
            .authorization
            .create_user(username, &password)
            .await
            .unwrap();
        context
            .authorization
            .add_server_mapping(
                &user.id,
                &context.source,
                username,
                &password,
                Some(&user.local_credential.mapping_key()),
            )
            .await
            .unwrap();
        user
    }

    fn result_status(results: &[UserSyncResult], username: &str) -> UserSyncStatus {
        results
            .iter()
            .find(|result| result.username == username)
            .unwrap()
            .status
    }

    #[tokio::test]
    async fn creates_missing_user_and_second_sync_is_idempotent() {
        let context = setup(json!([]), 2).await;
        let user = add_user_with_reusable_mapping(&context, "alice", "alice-password").await;

        Mock::given(method("POST"))
            .and(path("/Users/New"))
            .and(body_partial_json(json!({
                "Name": "alice",
                "Password": "alice-password"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote_user("alice")))
            .expect(1)
            .mount(&context.mock)
            .await;

        let first = context
            .service
            .sync_all_users_to_server(context.target.id)
            .await
            .unwrap();
        let second = context
            .service
            .sync_all_users_to_server(context.target.id)
            .await
            .unwrap();

        assert_eq!(result_status(&first, "alice"), UserSyncStatus::Created);
        assert_eq!(
            result_status(&second, "alice"),
            UserSyncStatus::AlreadyMapped
        );
        let mappings = context
            .authorization
            .list_server_mappings(&user.id)
            .await
            .unwrap();
        assert_eq!(
            mappings
                .iter()
                .filter(|mapping| mapping.server_id == context.target.id)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn maps_existing_user_when_credentials_match_without_recreating_it() {
        let context = setup(json!([remote_user("bob")]), 1).await;
        let user = add_user_with_reusable_mapping(&context, "bob", "bob-password").await;

        Mock::given(method("POST"))
            .and(path("/Users/AuthenticateByName"))
            .and(body_partial_json(json!({
                "Username": "bob",
                "Pw": "bob-password"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(auth_response("bob")))
            .expect(1)
            .mount(&context.mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/Users/New"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&context.mock)
            .await;

        let results = context
            .service
            .sync_all_users_to_server(context.target.id)
            .await
            .unwrap();

        assert_eq!(
            result_status(&results, "bob"),
            UserSyncStatus::MappedExisting
        );
        assert!(context
            .authorization
            .get_server_mapping_by_server_id(&user.id, context.target.id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn does_not_map_or_reset_existing_user_when_password_differs() {
        let context = setup(json!([remote_user("carol")]), 1).await;
        let user = add_user_with_reusable_mapping(&context, "carol", "known-password").await;

        Mock::given(method("POST"))
            .and(path("/Users/AuthenticateByName"))
            .and(body_partial_json(json!({ "Username": "carol" })))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&context.mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/Users/New"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&context.mock)
            .await;

        let results = context
            .service
            .sync_all_users_to_server(context.target.id)
            .await
            .unwrap();

        assert_eq!(
            result_status(&results, "carol"),
            UserSyncStatus::ExistsWithDifferentPassword
        );
        assert!(context
            .authorization
            .get_server_mapping_by_server_id(&user.id, context.target.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn skips_user_without_reusable_credentials() {
        let context = setup(json!([]), 1).await;
        let user = context
            .authorization
            .create_user("dana", &Password::from("local-only-password"))
            .await
            .unwrap();
        Mock::given(method("POST"))
            .and(path("/Users/New"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&context.mock)
            .await;

        let results = context
            .service
            .sync_all_users_to_server(context.target.id)
            .await
            .unwrap();

        assert_eq!(
            result_status(&results, "dana"),
            UserSyncStatus::NoReusableCredentials
        );
        assert!(context
            .authorization
            .get_server_mapping_by_server_id(&user.id, context.target.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn skips_user_when_matching_mappings_have_conflicting_passwords() {
        let context = setup(json!([]), 1).await;
        let user = add_user_with_reusable_mapping(&context, "frank", "first-password").await;
        let other_source_id = context
            .service
            .server_storage
            .add_server(
                "other-source",
                "http://other-source.example",
                90,
                MediaStreamingMode::Proxy,
            )
            .await
            .unwrap();
        let other_source = context
            .service
            .server_storage
            .get_server_by_id(other_source_id)
            .await
            .unwrap()
            .unwrap();
        context
            .authorization
            .add_server_mapping(
                &user.id,
                &other_source,
                "FRANK",
                &Password::from("second-password"),
                Some(&user.local_credential.mapping_key()),
            )
            .await
            .unwrap();
        Mock::given(method("POST"))
            .and(path("/Users/New"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&context.mock)
            .await;

        let results = context
            .service
            .sync_all_users_to_server(context.target.id)
            .await
            .unwrap();

        assert_eq!(
            result_status(&results, "frank"),
            UserSyncStatus::NoReusableCredentials
        );
        assert!(results[0]
            .message
            .as_deref()
            .is_some_and(|message| message.contains("conflicting")));
        assert!(context
            .authorization
            .get_server_mapping_by_server_id(&user.id, context.target.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn creates_genuinely_passwordless_user_without_a_password() {
        let context = setup(json!([]), 1).await;
        let user = context
            .authorization
            .create_user("erin", &Password::from(""))
            .await
            .unwrap();
        Mock::given(method("POST"))
            .and(path("/Users/New"))
            .and(body_partial_json(json!({
                "Name": "erin",
                "Password": null
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote_user("erin")))
            .expect(1)
            .mount(&context.mock)
            .await;

        let results = context
            .service
            .sync_all_users_to_server(context.target.id)
            .await
            .unwrap();

        assert_eq!(result_status(&results, "erin"), UserSyncStatus::Created);
        assert!(context
            .authorization
            .get_server_mapping_by_server_id(&user.id, context.target.id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn one_user_failure_does_not_stop_the_batch() {
        let context = setup(json!([]), 1).await;
        add_user_with_reusable_mapping(&context, "fails", "fails-password").await;
        let succeeds =
            add_user_with_reusable_mapping(&context, "succeeds", "succeeds-password").await;

        Mock::given(method("POST"))
            .and(path("/Users/New"))
            .and(body_partial_json(json!({ "Name": "fails" })))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&context.mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/Users/New"))
            .and(body_partial_json(json!({ "Name": "succeeds" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote_user("succeeds")))
            .expect(1)
            .mount(&context.mock)
            .await;

        let results = context
            .service
            .sync_all_users_to_server(context.target.id)
            .await
            .unwrap();

        assert_eq!(result_status(&results, "fails"), UserSyncStatus::Failed);
        assert_eq!(result_status(&results, "succeeds"), UserSyncStatus::Created);
        assert!(context
            .authorization
            .get_server_mapping_by_server_id(&succeeds.id, context.target.id)
            .await
            .unwrap()
            .is_some());
    }
}
