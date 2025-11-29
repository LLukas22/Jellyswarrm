use std::sync::Arc;

use serde::Deserialize;
use tracing::{error, info, warn};

use crate::{
    encryption::decrypt_password, server_storage::ServerStorageService,
    user_authorization_service::UserAuthorizationService, AppState,
};

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
    reqwest_client: reqwest::Client,
    config: Arc<tokio::sync::RwLock<crate::config::AppConfig>>,
}

#[derive(Deserialize)]
struct NewUserResponse {
    #[serde(rename = "Id")]
    id: String,
}

impl FederatedUserService {
    pub fn new(state: &AppState) -> Self {
        Self {
            server_storage: state.server_storage.clone(),
            user_authorization: state.user_authorization.clone(),
            reqwest_client: state.reqwest_client.clone(),
            config: state.config.clone(),
        }
    }

    pub fn new_from_components(
        server_storage: Arc<ServerStorageService>,
        user_authorization: Arc<UserAuthorizationService>,
        reqwest_client: reqwest::Client,
        config: Arc<tokio::sync::RwLock<crate::config::AppConfig>>,
    ) -> Self {
        Self {
            server_storage,
            user_authorization,
            reqwest_client,
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
        let admin_password = &config.password;

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

                // Authenticate as admin to get token
                let auth_token = match self
                    .authenticate_as_admin(
                        server.url.as_ref(),
                        &admin.username,
                        &decrypted_admin_password,
                    )
                    .await
                {
                    Ok(token) => token,
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

                // Check if user exists, if not create
                match self
                    .create_user_on_server(server.url.as_ref(), &auth_token, username, password)
                    .await
                {
                    Ok((remote_user_id, created)) => {
                        let (status, should_map) = if created {
                            (SyncStatus::Created, true)
                        } else {
                            // User exists. Check if password matches.
                            match self
                                .check_user_password(server.url.as_ref(), username, password)
                                .await
                            {
                                Ok(true) => (SyncStatus::AlreadyExists, true),
                                Ok(false) => (SyncStatus::ExistsWithDifferentPassword, false),
                                Err(e) => {
                                    warn!(
                                        "Failed to check password for existing user {} on {}: {}",
                                        username, server.name, e
                                    );
                                    // Assume mismatch or failure, don't map to be safe
                                    (SyncStatus::ExistsWithDifferentPassword, false)
                                }
                            }
                        };

                        info!(
                            "Synced user {} to server {} (Remote ID: {}, Status: {:?})",
                            username, server.name, remote_user_id, status
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

                let auth_token = match self
                    .authenticate_as_admin(
                        server.url.as_ref(),
                        &admin.username,
                        &decrypted_admin_password,
                    )
                    .await
                {
                    Ok(token) => token,
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

                match self
                    .delete_user_on_server(server.url.as_ref(), &auth_token, username)
                    .await
                {
                    Ok(deleted) => {
                        let status = if deleted {
                            SyncStatus::Deleted
                        } else {
                            SyncStatus::NotFound
                        };
                        info!(
                            "Deleted user {} from server {} (Deleted: {})",
                            username, server.name, deleted
                        );
                        results.push(ServerSyncResult {
                            server_name: server.name.clone(),
                            status,
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
                    status: SyncStatus::Skipped,
                    message: Some("No admin credentials".to_string()),
                });
            }
        }

        results
    }

    async fn check_user_password(
        &self,
        server_url: &str,
        username: &str,
        password: &str,
    ) -> Result<bool, anyhow::Error> {
        let auth_url = format!(
            "{}/Users/AuthenticateByName",
            server_url.trim_end_matches('/')
        );

        let auth_header = format!(
            "MediaBrowser Client=\"Jellyswarrm Proxy\", Device=\"Server\", DeviceId=\"jellyswarrm-proxy\", Version=\"{}\"",
            env!("CARGO_PKG_VERSION")
        );

        let response = self
            .reqwest_client
            .post(&auth_url)
            .header("Authorization", auth_header)
            .json(&serde_json::json!({
                "Username": username,
                "Pw": password
            }))
            .send()
            .await?;

        if response.status().is_success() {
            Ok(true)
        } else if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            Ok(false)
        } else {
            Err(anyhow::anyhow!(
                "Authentication check failed: {}",
                response.status()
            ))
        }
    }

    async fn authenticate_as_admin(
        &self,
        server_url: &str,
        username: &str,
        password: &str,
    ) -> Result<String, anyhow::Error> {
        let auth_url = format!(
            "{}/Users/AuthenticateByName",
            server_url.trim_end_matches('/')
        );

        let auth_header = format!(
            "MediaBrowser Client=\"Jellyswarrm Proxy\", Device=\"Server\", DeviceId=\"jellyswarrm-proxy\", Version=\"{}\"",
            env!("CARGO_PKG_VERSION")
        );

        let response = self
            .reqwest_client
            .post(&auth_url)
            .header("Authorization", auth_header)
            .json(&serde_json::json!({
                "Username": username,
                "Pw": password
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Authentication failed: {}",
                response.status()
            ));
        }

        #[derive(Deserialize)]
        struct AuthResponse {
            #[serde(rename = "AccessToken")]
            access_token: String,
        }

        let auth_response: AuthResponse = response.json().await?;
        Ok(auth_response.access_token)
    }

    async fn create_user_on_server(
        &self,
        server_url: &str,
        token: &str,
        username: &str,
        password: &str,
    ) -> Result<(String, bool), anyhow::Error> {
        let base_url = server_url.trim_end_matches('/');

        // 1. Check if user exists
        let users_url = format!("{}/Users", base_url);
        let auth_header = format!(
            "MediaBrowser Client=\"Jellyswarrm Proxy\", Device=\"Server\", DeviceId=\"jellyswarrm-proxy\", Version=\"{}\", Token=\"{}\"",
            env!("CARGO_PKG_VERSION"),
            token
        );

        let response = self
            .reqwest_client
            .get(&users_url)
            .header("Authorization", &auth_header)
            .send()
            .await?;

        if response.status().is_success() {
            let users: Vec<serde_json::Value> = response.json().await?;
            if let Some(existing) = users.iter().find(|u| {
                u.get("Name")
                    .and_then(|n| n.as_str())
                    .map(|n| n.eq_ignore_ascii_case(username))
                    .unwrap_or(false)
            }) {
                // User exists, return their ID
                return Ok((
                    existing
                        .get("Id")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    false,
                ));
            }
        }

        // 2. Create user
        let create_url = format!("{}/Users/New", base_url);
        let response = self
            .reqwest_client
            .post(&create_url)
            .header("Authorization", &auth_header)
            .json(&serde_json::json!({
                "Name": username,
                "Password": password
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Failed to create user: {} - {}",
                status,
                text
            ));
        }

        let new_user: NewUserResponse = response.json().await?;

        // Password should be set by the New endpoint if provided.

        Ok((new_user.id, true))
    }

    async fn delete_user_on_server(
        &self,
        server_url: &str,
        token: &str,
        username: &str,
    ) -> Result<bool, anyhow::Error> {
        let base_url = server_url.trim_end_matches('/');

        // 1. Find user ID
        let users_url = format!("{}/Users", base_url);
        let auth_header = format!(
            "MediaBrowser Client=\"Jellyswarrm Proxy\", Device=\"Server\", DeviceId=\"jellyswarrm-proxy\", Version=\"{}\", Token=\"{}\"",
            env!("CARGO_PKG_VERSION"),
            token
        );

        let response = self
            .reqwest_client
            .get(&users_url)
            .header("Authorization", &auth_header)
            .send()
            .await?;

        let user_id = if response.status().is_success() {
            let users: Vec<serde_json::Value> = response.json().await?;
            users
                .iter()
                .find(|u| {
                    u.get("Name")
                        .and_then(|n| n.as_str())
                        .map(|n| n.eq_ignore_ascii_case(username))
                        .unwrap_or(false)
                })
                .and_then(|u| u.get("Id").and_then(|i| i.as_str()).map(|s| s.to_string()))
        } else {
            return Err(anyhow::anyhow!(
                "Failed to list users: {}",
                response.status()
            ));
        };

        if let Some(id) = user_id {
            // 2. Delete user
            let delete_url = format!("{}/Users/{}", base_url, id);
            let response = self
                .reqwest_client
                .delete(&delete_url)
                .header("Authorization", &auth_header)
                .send()
                .await?;

            if !response.status().is_success() {
                return Err(anyhow::anyhow!(
                    "Failed to delete user: {}",
                    response.status()
                ));
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }
}
