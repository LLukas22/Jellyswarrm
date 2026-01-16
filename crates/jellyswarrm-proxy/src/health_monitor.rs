//! Server health monitoring service
//! Periodically checks server availability and records health history

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use std::{sync::Arc, time::Duration};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::server_storage::{Server, ServerStorageService};

/// Health status for a server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHealth {
    pub server_id: i64,
    pub server_name: String,
    pub server_url: String,
    pub is_online: bool,
    pub response_time_ms: Option<i64>,
    pub server_version: Option<String>,
    pub error_message: Option<String>,
    pub last_checked: DateTime<Utc>,
}

/// Health history entry from database
#[derive(Debug, Clone, FromRow)]
pub struct HealthHistoryEntry {
    pub id: i64,
    pub server_id: i64,
    pub is_online: bool,
    pub response_time_ms: Option<i64>,
    pub server_version: Option<String>,
    pub error_message: Option<String>,
    pub checked_at: DateTime<Utc>,
}

/// Aggregated health statistics for a server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHealthStats {
    pub server_id: i64,
    pub server_name: String,
    pub uptime_percentage: f64,
    pub avg_response_time_ms: Option<f64>,
    pub total_checks: i64,
    pub successful_checks: i64,
    pub current_status: ServerHealth,
}

/// Health monitoring service
#[derive(Clone)]
pub struct HealthMonitorService {
    pool: SqlitePool,
    client: reqwest::Client,
    /// Cache of current health status for all servers
    current_status: Arc<RwLock<Vec<ServerHealth>>>,
}

impl HealthMonitorService {
    pub fn new(pool: SqlitePool) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        Self {
            pool,
            client,
            current_status: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Check health of a single server
    pub async fn check_server_health(&self, server: &Server) -> ServerHealth {
        let start = std::time::Instant::now();
        let url = format!("{}System/Info/Public", server.url.as_str().trim_end_matches('/').to_string() + "/");

        let result = self.client.get(&url).send().await;

        let (is_online, response_time_ms, server_version, error_message) = match result {
            Ok(response) => {
                let elapsed = start.elapsed().as_millis() as i64;

                if response.status().is_success() {
                    // Try to parse server info
                    let version = if let Ok(text) = response.text().await {
                        serde_json::from_str::<serde_json::Value>(&text)
                            .ok()
                            .and_then(|v| v.get("Version").and_then(|v| v.as_str()).map(String::from))
                    } else {
                        None
                    };

                    (true, Some(elapsed), version, None)
                } else {
                    (
                        false,
                        Some(elapsed),
                        None,
                        Some(format!("HTTP {}", response.status())),
                    )
                }
            }
            Err(e) => {
                let error_msg = if e.is_timeout() {
                    "Connection timeout".to_string()
                } else if e.is_connect() {
                    "Connection refused".to_string()
                } else {
                    format!("Error: {}", e)
                };

                (false, None, None, Some(error_msg))
            }
        };

        ServerHealth {
            server_id: server.id,
            server_name: server.name.clone(),
            server_url: server.url.to_string(),
            is_online,
            response_time_ms,
            server_version,
            error_message,
            last_checked: Utc::now(),
        }
    }

    /// Check health of all servers and record results
    pub async fn check_all_servers(&self, server_storage: &ServerStorageService) {
        let servers = match server_storage.list_servers().await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to list servers for health check: {}", e);
                return;
            }
        };

        let mut health_results = Vec::new();

        for server in &servers {
            let health = self.check_server_health(server).await;

            // Record to database
            if let Err(e) = self.record_health(&health).await {
                error!("Failed to record health for server {}: {}", server.name, e);
            }

            if health.is_online {
                debug!(
                    "Server {} is online ({}ms, version: {:?})",
                    server.name,
                    health.response_time_ms.unwrap_or(0),
                    health.server_version
                );
            } else {
                warn!(
                    "Server {} is offline: {:?}",
                    server.name, health.error_message
                );
            }

            health_results.push(health);
        }

        // Update cached status
        *self.current_status.write().await = health_results;
    }

    /// Record health check result to database
    async fn record_health(&self, health: &ServerHealth) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO server_health_history (server_id, is_online, response_time_ms, server_version, error_message, checked_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(health.server_id)
        .bind(health.is_online)
        .bind(health.response_time_ms)
        .bind(&health.server_version)
        .bind(&health.error_message)
        .bind(health.last_checked)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get current health status for all servers
    pub async fn get_current_status(&self) -> Vec<ServerHealth> {
        self.current_status.read().await.clone()
    }

    /// Get health history for a server
    pub async fn get_health_history(
        &self,
        server_id: i64,
        limit: i64,
    ) -> Result<Vec<HealthHistoryEntry>, sqlx::Error> {
        let entries = sqlx::query_as::<_, HealthHistoryEntry>(
            r#"
            SELECT id, server_id, is_online, response_time_ms, server_version, error_message, checked_at
            FROM server_health_history
            WHERE server_id = ?
            ORDER BY checked_at DESC
            LIMIT ?
            "#,
        )
        .bind(server_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(entries)
    }

    /// Get health statistics for all servers
    pub async fn get_health_stats(&self) -> Result<Vec<ServerHealthStats>, sqlx::Error> {
        let current = self.current_status.read().await.clone();

        let mut stats = Vec::new();

        for health in &current {
            // Get aggregated stats from last 24 hours
            let row = sqlx::query(
                r#"
                SELECT
                    COUNT(*) as total_checks,
                    SUM(CASE WHEN is_online = 1 THEN 1 ELSE 0 END) as successful_checks,
                    AVG(CASE WHEN is_online = 1 THEN response_time_ms ELSE NULL END) as avg_response_time
                FROM server_health_history
                WHERE server_id = ? AND checked_at > datetime('now', '-24 hours')
                "#,
            )
            .bind(health.server_id)
            .fetch_one(&self.pool)
            .await?;

            let total_checks: i64 = row.get("total_checks");
            let successful_checks: i64 = row.get("successful_checks");
            let avg_response_time: Option<f64> = row.get("avg_response_time");

            let uptime_percentage = if total_checks > 0 {
                (successful_checks as f64 / total_checks as f64) * 100.0
            } else {
                0.0
            };

            stats.push(ServerHealthStats {
                server_id: health.server_id,
                server_name: health.server_name.clone(),
                uptime_percentage,
                avg_response_time_ms: avg_response_time,
                total_checks,
                successful_checks,
                current_status: health.clone(),
            });
        }

        Ok(stats)
    }

    /// Cleanup old health history entries
    pub async fn cleanup_old_history(&self, days_to_keep: i64) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM server_health_history WHERE checked_at < datetime('now', ? || ' days')",
        )
        .bind(-days_to_keep)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() > 0 {
            info!(
                "Cleaned up {} old health history entries (older than {} days)",
                result.rows_affected(),
                days_to_keep
            );
        }

        Ok(result.rows_affected())
    }
}

/// Start the background health monitoring task
pub fn start_health_monitor(
    health_service: HealthMonitorService,
    server_storage: Arc<ServerStorageService>,
    check_interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(check_interval_secs));

        // Run initial check immediately
        health_service.check_all_servers(&server_storage).await;

        loop {
            interval.tick().await;
            health_service.check_all_servers(&server_storage).await;
        }
    })
}

use sqlx::Row;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_check_offline_server() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::config::MIGRATOR.run(&pool).await.unwrap();

        let service = HealthMonitorService::new(pool);

        let server = Server {
            id: 1,
            name: "Test Server".to_string(),
            url: url::Url::parse("http://127.0.0.1:59999").unwrap(), // Non-existent port
            priority: 100,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let health = service.check_server_health(&server).await;

        assert!(!health.is_online);
        assert!(health.error_message.is_some());
    }
}
