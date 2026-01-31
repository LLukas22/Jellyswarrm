use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Row, SqlitePool};
use tokio::sync::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info};
use url::Url;
use futures_util::StreamExt;

use jellyfin_api::{client::{ClientInfo, JellyfinClient}, models::PublicSystemInfo};

use crate::encryption::EncryptedPassword;

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct Server {
    pub id: i64,
    pub name: String,
    pub url: Url,
    pub priority: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ServerAdmin {
    pub id: i64,
    pub server_id: i64,
    pub username: String,
    pub password: EncryptedPassword,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerHealthStatus {
    /// Server is healthy
    Healthy(PublicSystemInfo),
    /// Server is unhealthy with reason
    Unhealthy(String),
}

impl ServerHealthStatus {
    pub fn is_healthy(&self) -> bool {
        matches!(self, ServerHealthStatus::Healthy(_))
    }
}

#[derive(Debug, Clone)]
pub struct ServerStorageService {
    pool: SqlitePool,
    health_status: Arc<RwLock<HashMap<i64, ServerHealthStatus>>>,
    http_client: reqwest::Client,
    client_info: ClientInfo,
}

impl ServerStorageService {
    pub fn new(pool: SqlitePool) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap();
        Self {
            pool,
            health_status: Arc::new(RwLock::new(HashMap::new())),
            http_client,
            client_info: ClientInfo::default(),
        }
    }

    pub async fn add_server(
        &self,
        name: &str,
        url: &str,
        priority: i32,
    ) -> Result<i64, sqlx::Error> {
        // Validate URL
        if Url::parse(url).is_err() {
            return Err(sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Invalid URL format",
            )));
        }

        let now = chrono::Utc::now();

        let result = sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(name)
        .bind(url)
        .bind(priority)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let server_id = result.last_insert_rowid();
        info!(
            "Added server: {} ({}) with priority {}",
            name, url, priority
        );
        Ok(server_id)
    }

    pub async fn get_server_by_name(&self, name: &str) -> Result<Option<Server>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT id, name, url, priority, created_at, updated_at
            FROM servers 
            WHERE name = ?
            "#,
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = row {
            Ok(Some(self.row_to_server(row)))
        } else {
            Ok(None)
        }
    }

    pub async fn get_server_by_id(&self, id: i64) -> Result<Option<Server>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT id, name, url, priority, created_at, updated_at
            FROM servers 
            WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = row {
            Ok(Some(self.row_to_server(row)))
        } else {
            Ok(None)
        }
    }

    pub async fn list_servers(&self) -> Result<Vec<Server>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT id, name, url, priority, created_at, updated_at
            FROM servers 
            ORDER BY priority DESC, name ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let servers = rows
            .into_iter()
            .map(|row| self.row_to_server(row))
            .collect();
        Ok(servers)
    }

    pub async fn update_server_priority(
        &self,
        server_id: i64,
        new_priority: i32,
    ) -> Result<bool, sqlx::Error> {
        let now = chrono::Utc::now();

        let result = sqlx::query(
            r#"
            UPDATE servers 
            SET priority = ?, updated_at = ?
            WHERE id = ?
            "#,
        )
        .bind(new_priority)
        .bind(now)
        .bind(server_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn delete_server(&self, server_id: i64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"
            DELETE FROM servers 
            WHERE id = ?
            "#,
        )
        .bind(server_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn delete_server_by_name(&self, name: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"
            DELETE FROM servers 
            WHERE name = ?
            "#,
        )
        .bind(name)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub fn start_health_check_loop(&self, wait_time_secs: u64) {
        let service = self.clone();
        tokio::spawn(async move {
            info!("Starting server health check loop");
            loop {
                service.check_servers_health().await;
                tokio::time::sleep(tokio::time::Duration::from_secs(wait_time_secs)).await;
            }
        });
    }

    async fn check_servers_health(&self) {
        let servers = match self.list_servers().await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to list servers for health check: {}", e);
                return;
            }
        };

        let statuses: Vec<(i64, ServerHealthStatus)>  = futures_util::stream::iter(servers.into_iter().map(|server| {
            let http_client = self.http_client.clone();
            let client_info = self.client_info.clone();
            async move {
                let client = match JellyfinClient::new_with_client(server.url.as_str(), client_info, http_client) {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to create client for server {}: {}", server.name, e);
                        return (server.id, ServerHealthStatus::Unhealthy(e.to_string()));
                    }
                };

                let status = match client.get_public_system_info().await {
                    Ok(info) => ServerHealthStatus::Healthy(info),
                    Err(e) => ServerHealthStatus::Unhealthy(e.to_string()),
                };

                (server.id, status)
            }
        }))
        .buffer_unordered(5)
        .collect()
        .await;

        let mut lock = self.health_status.write().await;
        for (server_id, status) in statuses {
            if let Some(old_status) = lock.get(&server_id) {
                if *old_status == status {
                    continue; // No change
                }else {
                    info!("Server ID {} health status changed: {:?} -> {:?}", server_id, old_status, status);
                }
            }
            lock.insert(server_id, status);
        }
    }

    pub async fn server_status(&self, server_id: i64) -> ServerHealthStatus {
        let health = self.health_status.read().await;
        health.get(&server_id).unwrap_or(&ServerHealthStatus::Unhealthy("Unknown Server Status".to_string())).clone()
    }

    /// Get the best available server (highest priority, healthy, active)
    pub async fn get_best_server(&self) -> Result<Option<Server>, sqlx::Error> {
        let servers = self.list_servers().await?;

        if servers.is_empty() {
            return Ok(None);
        }
        
        let mut best_healthy = None;
        for server in &servers {
            if self.server_status(server.id).await.is_healthy() {
                best_healthy = Some(server);
                break;
            }
        }
        
        if let Some(server) = best_healthy {
             Ok(Some(server.clone()))
        } else {
             // Fallback to first if exists
             error!("No healthy servers found, falling back to first available server");
             Ok(servers.into_iter().next())
        }
    }

    /// Internal method to convert database row to Server struct
    fn row_to_server(&self, row: sqlx::sqlite::SqliteRow) -> Server {
        Server {
            id: row.get("id"),
            name: row.get("name"),
            url: Url::parse(row.get("url")).unwrap(),
            priority: row.get("priority"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }
    }

    pub async fn add_server_admin(
        &self,
        server_id: i64,
        username: &str,
        password: &EncryptedPassword,
    ) -> Result<i64, sqlx::Error> {
        let now = chrono::Utc::now();

        let result = sqlx::query(
            r#"
            INSERT OR REPLACE INTO server_admins (server_id, username, password, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(server_id)
        .bind(username)
        .bind(password)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let admin_id = result.last_insert_rowid();
        info!("Added admin for server ID: {}", server_id);
        Ok(admin_id)
    }

    pub async fn get_server_admin(
        &self,
        server_id: i64,
    ) -> Result<Option<ServerAdmin>, sqlx::Error> {
        let row = sqlx::query_as::<_, ServerAdmin>(
            r#"
            SELECT id, server_id, username, password, created_at, updated_at
            FROM server_admins 
            WHERE server_id = ?
            "#,
        )
        .bind(server_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    pub async fn delete_server_admin(&self, server_id: i64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"
            DELETE FROM server_admins 
            WHERE server_id = ?
            "#,
        )
        .bind(server_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use crate::config::MIGRATOR;

    use super::*;

    #[tokio::test]
    async fn test_server_storage_service() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();

        MIGRATOR.run(&pool).await.unwrap();
        let service = ServerStorageService::new(pool);

        // Test adding a server
        let server_id = service
            .add_server("test-server", "http://localhost:8096", 100)
            .await
            .unwrap();

        // Test getting the server
        let server = service.get_server_by_id(server_id).await.unwrap();
        assert!(server.is_some());

        let server = server.unwrap();
        assert_eq!(server.name, "test-server");
        assert_eq!(server.url, Url::parse("http://localhost:8096").unwrap());
        assert_eq!(server.priority, 100);

        // Test listing servers
        let servers = service.list_servers().await.unwrap();
        assert_eq!(servers.len(), 1);

        // Test updating priority
        let updated = service
            .update_server_priority(server_id, 200)
            .await
            .unwrap();
        assert!(updated);
    }
}
