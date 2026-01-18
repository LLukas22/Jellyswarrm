//! Audit logging service for tracking admin actions

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use tracing::info;

/// Actor type for audit logs
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ActorType {
    Admin,
    User,
    System,
}

impl std::fmt::Display for ActorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActorType::Admin => write!(f, "admin"),
            ActorType::User => write!(f, "user"),
            ActorType::System => write!(f, "system"),
        }
    }
}

impl From<&str> for ActorType {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "admin" => ActorType::Admin,
            "user" => ActorType::User,
            "system" => ActorType::System,
            _ => ActorType::System,
        }
    }
}

/// Action types for audit logs
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AuditAction {
    Create,
    Update,
    Delete,
    Login,
    Logout,
    PasswordChange,
    SessionReset,
}

impl std::fmt::Display for AuditAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuditAction::Create => write!(f, "create"),
            AuditAction::Update => write!(f, "update"),
            AuditAction::Delete => write!(f, "delete"),
            AuditAction::Login => write!(f, "login"),
            AuditAction::Logout => write!(f, "logout"),
            AuditAction::PasswordChange => write!(f, "password_change"),
            AuditAction::SessionReset => write!(f, "session_reset"),
        }
    }
}

impl From<&str> for AuditAction {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "create" => AuditAction::Create,
            "update" => AuditAction::Update,
            "delete" => AuditAction::Delete,
            "login" => AuditAction::Login,
            "logout" => AuditAction::Logout,
            "password_change" => AuditAction::PasswordChange,
            "session_reset" => AuditAction::SessionReset,
            _ => AuditAction::Update,
        }
    }
}

/// Resource types for audit logs
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ResourceType {
    User,
    Server,
    Mapping,
    Admin,
    Settings,
    Session,
}

impl std::fmt::Display for ResourceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResourceType::User => write!(f, "user"),
            ResourceType::Server => write!(f, "server"),
            ResourceType::Mapping => write!(f, "mapping"),
            ResourceType::Admin => write!(f, "admin"),
            ResourceType::Settings => write!(f, "settings"),
            ResourceType::Session => write!(f, "session"),
        }
    }
}

impl From<&str> for ResourceType {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "user" => ResourceType::User,
            "server" => ResourceType::Server,
            "mapping" => ResourceType::Mapping,
            "admin" => ResourceType::Admin,
            "settings" => ResourceType::Settings,
            "session" => ResourceType::Session,
            _ => ResourceType::User,
        }
    }
}

/// Represents an audit log entry
#[derive(Debug, Clone, FromRow)]
pub struct AuditLogEntry {
    pub id: i64,
    pub actor_type: String,
    pub actor_id: String,
    pub actor_name: String,
    pub action: String,
    pub resource_type: String,
    pub resource_id: Option<String>,
    pub resource_name: Option<String>,
    pub details: Option<String>,
    pub ip_address: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Filter options for querying audit logs
#[derive(Debug, Clone, Default)]
pub struct AuditLogFilter {
    pub actor_type: Option<String>,
    pub actor_id: Option<String>,
    pub action: Option<String>,
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    pub from_date: Option<DateTime<Utc>>,
    pub to_date: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Service for audit logging
#[derive(Debug, Clone)]
pub struct AuditService {
    pool: SqlitePool,
}

impl AuditService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Log an action
    pub async fn log(
        &self,
        actor_type: ActorType,
        actor_id: &str,
        actor_name: &str,
        action: AuditAction,
        resource_type: ResourceType,
        resource_id: Option<&str>,
        resource_name: Option<&str>,
        details: Option<&str>,
        ip_address: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            INSERT INTO audit_logs (actor_type, actor_id, actor_name, action, resource_type, resource_id, resource_name, details, ip_address, created_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(actor_type.to_string())
        .bind(actor_id)
        .bind(actor_name)
        .bind(action.to_string())
        .bind(resource_type.to_string())
        .bind(resource_id)
        .bind(resource_name)
        .bind(details)
        .bind(ip_address)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;

        let log_id = result.last_insert_rowid();
        info!(
            "Audit: {} {} {} {} (resource: {:?})",
            actor_name, action, resource_type, resource_id.unwrap_or("-"), resource_name
        );
        Ok(log_id)
    }

    /// Helper to log admin actions
    pub async fn log_admin_action(
        &self,
        admin_id: &str,
        admin_name: &str,
        action: AuditAction,
        resource_type: ResourceType,
        resource_id: Option<&str>,
        resource_name: Option<&str>,
        details: Option<&str>,
        ip_address: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        self.log(
            ActorType::Admin,
            admin_id,
            admin_name,
            action,
            resource_type,
            resource_id,
            resource_name,
            details,
            ip_address,
        )
        .await
    }

    /// Query audit logs with filters
    pub async fn query(&self, filter: AuditLogFilter) -> Result<Vec<AuditLogEntry>, sqlx::Error> {
        let mut query = String::from(
            r#"
            SELECT id, actor_type, actor_id, actor_name, action, resource_type, resource_id, resource_name, details, ip_address, created_at
            FROM audit_logs
            WHERE 1=1
            "#,
        );

        let mut params: Vec<String> = Vec::new();

        if let Some(ref actor_type) = filter.actor_type {
            query.push_str(" AND actor_type = ?");
            params.push(actor_type.clone());
        }

        if let Some(ref actor_id) = filter.actor_id {
            query.push_str(" AND actor_id = ?");
            params.push(actor_id.clone());
        }

        if let Some(ref action) = filter.action {
            query.push_str(" AND action = ?");
            params.push(action.clone());
        }

        if let Some(ref resource_type) = filter.resource_type {
            query.push_str(" AND resource_type = ?");
            params.push(resource_type.clone());
        }

        if let Some(ref resource_id) = filter.resource_id {
            query.push_str(" AND resource_id = ?");
            params.push(resource_id.clone());
        }

        query.push_str(" ORDER BY created_at DESC");

        if let Some(limit) = filter.limit {
            query.push_str(&format!(" LIMIT {}", limit));
        } else {
            query.push_str(" LIMIT 100"); // Default limit
        }

        if let Some(offset) = filter.offset {
            query.push_str(&format!(" OFFSET {}", offset));
        }

        // Build the query dynamically
        let mut sqlx_query = sqlx::query_as::<_, AuditLogEntry>(&query);

        for param in params {
            sqlx_query = sqlx_query.bind(param);
        }

        let logs = sqlx_query.fetch_all(&self.pool).await?;

        Ok(logs)
    }

    /// Get recent audit logs (last N entries)
    pub async fn get_recent(&self, limit: i64) -> Result<Vec<AuditLogEntry>, sqlx::Error> {
        let logs = sqlx::query_as::<_, AuditLogEntry>(
            r#"
            SELECT id, actor_type, actor_id, actor_name, action, resource_type, resource_id, resource_name, details, ip_address, created_at
            FROM audit_logs
            ORDER BY created_at DESC
            LIMIT ?
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(logs)
    }

    /// Count audit logs
    pub async fn count(&self) -> Result<i64, sqlx::Error> {
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_logs")
            .fetch_one(&self.pool)
            .await?;
        Ok(count.0)
    }

    /// Get audit logs for a specific resource
    pub async fn get_for_resource(
        &self,
        resource_type: ResourceType,
        resource_id: &str,
    ) -> Result<Vec<AuditLogEntry>, sqlx::Error> {
        let logs = sqlx::query_as::<_, AuditLogEntry>(
            r#"
            SELECT id, actor_type, actor_id, actor_name, action, resource_type, resource_id, resource_name, details, ip_address, created_at
            FROM audit_logs
            WHERE resource_type = ? AND resource_id = ?
            ORDER BY created_at DESC
            LIMIT 50
            "#,
        )
        .bind(resource_type.to_string())
        .bind(resource_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(logs)
    }

    /// Cleanup old audit logs (keep last N days)
    pub async fn cleanup(&self, days_to_keep: i64) -> Result<u64, sqlx::Error> {
        let cutoff = Utc::now() - chrono::Duration::days(days_to_keep);

        let result = sqlx::query("DELETE FROM audit_logs WHERE created_at < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() > 0 {
            info!(
                "Cleaned up {} old audit log entries (older than {} days)",
                result.rows_affected(),
                days_to_keep
            );
        }

        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MIGRATOR;

    #[tokio::test]
    async fn test_audit_logging() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();

        let service = AuditService::new(pool);

        // Log an action
        let log_id = service
            .log(
                ActorType::Admin,
                "admin-1",
                "testadmin",
                AuditAction::Create,
                ResourceType::User,
                Some("user-123"),
                Some("testuser"),
                Some("Created new user"),
                Some("127.0.0.1"),
            )
            .await
            .unwrap();

        assert!(log_id > 0);

        // Get recent logs
        let logs = service.get_recent(10).await.unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].actor_name, "testadmin");
        assert_eq!(logs[0].action, "create");

        // Count logs
        let count = service.count().await.unwrap();
        assert_eq!(count, 1);
    }
}
