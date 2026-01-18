//! Per-user server access permissions

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use tracing::info;

/// User permission entry
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct UserPermission {
    pub id: i64,
    pub user_id: String,
    pub server_id: i64,
    pub permission_type: String, // "allow" or "deny"
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

/// Permission type for a user-server combination
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionType {
    Allow,
    Deny,
}

impl PermissionType {
    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionType::Allow => "allow",
            PermissionType::Deny => "deny",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "allow" => Some(PermissionType::Allow),
            "deny" => Some(PermissionType::Deny),
            _ => None,
        }
    }
}

/// User permission with server details
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct UserPermissionWithServer {
    pub id: i64,
    pub user_id: String,
    pub server_id: i64,
    pub server_name: String,
    pub server_url: String,
    pub permission_type: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

/// User permissions management service
#[derive(Clone)]
pub struct UserPermissionsService {
    pool: SqlitePool,
}

impl UserPermissionsService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Set a permission for a user on a specific server
    pub async fn set_permission(
        &self,
        user_id: &str,
        server_id: i64,
        permission_type: PermissionType,
        created_by: &str,
    ) -> Result<UserPermission, sqlx::Error> {
        let now = Utc::now();

        // Upsert - update if exists, insert if not
        let permission = sqlx::query_as::<_, UserPermission>(
            r#"
            INSERT INTO user_permissions (user_id, server_id, permission_type, created_by, created_at)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(user_id, server_id) DO UPDATE SET
                permission_type = excluded.permission_type,
                created_by = excluded.created_by,
                created_at = excluded.created_at
            RETURNING id, user_id, server_id, permission_type, created_by, created_at
            "#,
        )
        .bind(user_id)
        .bind(server_id)
        .bind(permission_type.as_str())
        .bind(created_by)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;

        info!(
            "Set permission {} for user {} on server {}",
            permission_type.as_str(),
            user_id,
            server_id
        );

        Ok(permission)
    }

    /// Remove a permission for a user on a specific server
    pub async fn remove_permission(&self, user_id: &str, server_id: i64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM user_permissions WHERE user_id = ? AND server_id = ?")
            .bind(user_id)
            .bind(server_id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() > 0 {
            info!(
                "Removed permission for user {} on server {}",
                user_id, server_id
            );
        }

        Ok(result.rows_affected() > 0)
    }

    /// Check if a user is allowed to access a specific server
    /// Returns None if no explicit permission is set (defaults to allow)
    pub async fn check_permission(
        &self,
        user_id: &str,
        server_id: i64,
    ) -> Result<Option<PermissionType>, sqlx::Error> {
        let permission = sqlx::query_as::<_, (String,)>(
            "SELECT permission_type FROM user_permissions WHERE user_id = ? AND server_id = ?",
        )
        .bind(user_id)
        .bind(server_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(permission.and_then(|(pt,)| PermissionType::from_str(&pt)))
    }

    /// Check if a user can access a server (allow by default if no explicit rule)
    pub async fn can_access_server(&self, user_id: &str, server_id: i64) -> Result<bool, sqlx::Error> {
        match self.check_permission(user_id, server_id).await? {
            Some(PermissionType::Deny) => Ok(false),
            Some(PermissionType::Allow) | None => Ok(true),
        }
    }

    /// Get all permissions for a user with server details
    pub async fn get_user_permissions(
        &self,
        user_id: &str,
    ) -> Result<Vec<UserPermissionWithServer>, sqlx::Error> {
        let permissions = sqlx::query_as::<_, UserPermissionWithServer>(
            r#"
            SELECT
                up.id, up.user_id, up.server_id, s.name as server_name, s.url as server_url,
                up.permission_type, up.created_by, up.created_at
            FROM user_permissions up
            JOIN servers s ON up.server_id = s.id
            WHERE up.user_id = ?
            ORDER BY s.name
            "#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(permissions)
    }

    /// Get all permissions for a server
    pub async fn get_server_permissions(
        &self,
        server_id: i64,
    ) -> Result<Vec<UserPermission>, sqlx::Error> {
        let permissions = sqlx::query_as::<_, UserPermission>(
            r#"
            SELECT id, user_id, server_id, permission_type, created_by, created_at
            FROM user_permissions
            WHERE server_id = ?
            ORDER BY user_id
            "#,
        )
        .bind(server_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(permissions)
    }

    /// Get all denied servers for a user (for filtering in federated queries)
    pub async fn get_denied_server_ids(&self, user_id: &str) -> Result<Vec<i64>, sqlx::Error> {
        let ids = sqlx::query_as::<_, (i64,)>(
            "SELECT server_id FROM user_permissions WHERE user_id = ? AND permission_type = 'deny'",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(ids.into_iter().map(|(id,)| id).collect())
    }

    /// Bulk set permissions for a user
    pub async fn bulk_set_permissions(
        &self,
        user_id: &str,
        permissions: Vec<(i64, PermissionType)>,
        created_by: &str,
    ) -> Result<(), sqlx::Error> {
        for (server_id, permission_type) in permissions {
            self.set_permission(user_id, server_id, permission_type, created_by)
                .await?;
        }
        Ok(())
    }

    /// Remove all permissions for a user
    pub async fn remove_all_user_permissions(&self, user_id: &str) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM user_permissions WHERE user_id = ?")
            .bind(user_id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() > 0 {
            info!(
                "Removed {} permissions for user {}",
                result.rows_affected(),
                user_id
            );
        }

        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permission_type_conversion() {
        assert_eq!(PermissionType::Allow.as_str(), "allow");
        assert_eq!(PermissionType::Deny.as_str(), "deny");
        assert_eq!(PermissionType::from_str("allow"), Some(PermissionType::Allow));
        assert_eq!(PermissionType::from_str("deny"), Some(PermissionType::Deny));
        assert_eq!(PermissionType::from_str("invalid"), None);
    }
}
