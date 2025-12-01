use std::sync::Arc;

use tracing::{error, info, warn};

use crate::{
    encryption::decrypt_password, server_storage::ServerStorageService,
    user_authorization_service::UserAuthorizationService, AppState,
};
use jellyfin_api::JellyfinClient;

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

    /// Syncs a user to all configured servers where an admin account is available.
    /// If the user does not exist on a server, it is created.
    /// If the user exists, we assume it's fine (we don't update passwords for existing users here to avoid conflicts).
    pub async fn sync_user_to_all_servers(
        &self,
        username: &str,
        password: &str,
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
        let admin_password = config.password.clone();
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
                    .authenticate_by_name(&admin.username, &decrypted_admin_password)
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

                    let (status, should_map) =
                        match user_client.authenticate_by_name(username, password).await {
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
                                server.url.as_str(),
                                username,
                                password,
                                Some(password), // Encrypt with their own password so they can use it
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
                    match client.create_user(username, Some(password)).await {
                        Ok(new_user) => {
                            info!(
                                "Synced user {} to server {} (Remote ID: {}, Status: Created)",
                                username, server.name, new_user.id
                            );

                            if let Err(e) = self
                                .user_authorization
                                .add_server_mapping(
                                    user_id,
                                    server.url.as_str(),
                                    username,
                                    password,
                                    Some(password), // Encrypt with their own password so they can use it
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
                    match decrypt_password(&admin.password, admin_password) {
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
                    .authenticate_by_name(&admin.username, &decrypted_admin_password)
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
