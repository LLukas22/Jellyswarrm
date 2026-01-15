//! Admin user storage service for multi-admin support

use chrono::{DateTime, Utc};
use sqlx::{FromRow, SqlitePool};
use tracing::{error, info};

use crate::encryption::{hash_password, HashedPassword};

/// Represents an admin user in the system
#[derive(Debug, Clone, FromRow)]
pub struct AdminUser {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub is_super_admin: bool,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Service for managing admin users
#[derive(Debug, Clone)]
pub struct AdminStorageService {
    pool: SqlitePool,
}

impl AdminStorageService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Initialize the first super admin from config if no admins exist
    pub async fn init_from_config(
        &self,
        config_username: &str,
        config_password: &HashedPassword,
    ) -> Result<(), sqlx::Error> {
        // Check if any admin users exist
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM admin_users")
            .fetch_one(&self.pool)
            .await?;

        if count.0 == 0 {
            // Create the first super admin from config
            info!(
                "No admin users found, creating initial super admin: {}",
                config_username
            );
            self.create_admin(config_username, config_password, true)
                .await?;
        }

        Ok(())
    }

    /// Create a new admin user
    pub async fn create_admin(
        &self,
        username: &str,
        password_hash: &HashedPassword,
        is_super_admin: bool,
    ) -> Result<i64, sqlx::Error> {
        let now = Utc::now();

        let result = sqlx::query(
            r#"
            INSERT INTO admin_users (username, password_hash, is_super_admin, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(username)
        .bind(password_hash.as_str())
        .bind(is_super_admin)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let admin_id = result.last_insert_rowid();
        info!(
            "Created admin user: {} (ID: {}, super_admin: {})",
            username, admin_id, is_super_admin
        );
        Ok(admin_id)
    }

    /// Get admin user by username
    pub async fn get_admin_by_username(
        &self,
        username: &str,
    ) -> Result<Option<AdminUser>, sqlx::Error> {
        let admin = sqlx::query_as::<_, AdminUser>(
            r#"
            SELECT id, username, password_hash, is_super_admin, last_login_at, created_at, updated_at
            FROM admin_users
            WHERE username = ?
            "#,
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;

        Ok(admin)
    }

    /// Get admin user by ID
    pub async fn get_admin_by_id(&self, id: i64) -> Result<Option<AdminUser>, sqlx::Error> {
        let admin = sqlx::query_as::<_, AdminUser>(
            r#"
            SELECT id, username, password_hash, is_super_admin, last_login_at, created_at, updated_at
            FROM admin_users
            WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(admin)
    }

    /// List all admin users
    pub async fn list_admins(&self) -> Result<Vec<AdminUser>, sqlx::Error> {
        let admins = sqlx::query_as::<_, AdminUser>(
            r#"
            SELECT id, username, password_hash, is_super_admin, last_login_at, created_at, updated_at
            FROM admin_users
            ORDER BY is_super_admin DESC, username ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(admins)
    }

    /// Authenticate an admin user by username and password
    pub async fn authenticate(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<AdminUser>, sqlx::Error> {
        let password_hash = hash_password(password);

        let admin = sqlx::query_as::<_, AdminUser>(
            r#"
            SELECT id, username, password_hash, is_super_admin, last_login_at, created_at, updated_at
            FROM admin_users
            WHERE username = ? AND password_hash = ?
            "#,
        )
        .bind(username)
        .bind(&password_hash)
        .fetch_optional(&self.pool)
        .await?;

        if admin.is_some() {
            // Update last login time
            self.update_last_login(username).await?;
        }

        Ok(admin)
    }

    /// Update admin's last login timestamp
    pub async fn update_last_login(&self, username: &str) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            r#"
            UPDATE admin_users
            SET last_login_at = ?, updated_at = ?
            WHERE username = ?
            "#,
        )
        .bind(now)
        .bind(now)
        .bind(username)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Update admin password
    pub async fn update_password(
        &self,
        admin_id: i64,
        new_password_hash: &HashedPassword,
    ) -> Result<bool, sqlx::Error> {
        let now = Utc::now();

        let result = sqlx::query(
            r#"
            UPDATE admin_users
            SET password_hash = ?, updated_at = ?
            WHERE id = ?
            "#,
        )
        .bind(new_password_hash.as_str())
        .bind(now)
        .bind(admin_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Delete an admin user (only super admins can do this, and cannot delete themselves or the last super admin)
    pub async fn delete_admin(&self, admin_id: i64) -> Result<bool, sqlx::Error> {
        // Check if this is the last super admin
        let super_admin_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM admin_users WHERE is_super_admin = TRUE")
                .fetch_one(&self.pool)
                .await?;

        let admin = self.get_admin_by_id(admin_id).await?;
        if let Some(admin) = admin {
            if admin.is_super_admin && super_admin_count.0 <= 1 {
                error!("Cannot delete the last super admin");
                return Ok(false);
            }
        }

        let result = sqlx::query("DELETE FROM admin_users WHERE id = ?")
            .bind(admin_id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() > 0 {
            info!("Deleted admin user ID: {}", admin_id);
        }

        Ok(result.rows_affected() > 0)
    }

    /// Count admin users
    pub async fn count_admins(&self) -> Result<i64, sqlx::Error> {
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM admin_users")
            .fetch_one(&self.pool)
            .await?;
        Ok(count.0)
    }

    /// Promote an admin to super admin
    pub async fn promote_to_super_admin(&self, admin_id: i64) -> Result<bool, sqlx::Error> {
        let now = Utc::now();

        let result = sqlx::query(
            r#"
            UPDATE admin_users
            SET is_super_admin = TRUE, updated_at = ?
            WHERE id = ?
            "#,
        )
        .bind(now)
        .bind(admin_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Demote a super admin (only if there are other super admins)
    pub async fn demote_from_super_admin(&self, admin_id: i64) -> Result<bool, sqlx::Error> {
        // Check if there are other super admins
        let super_admin_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM admin_users WHERE is_super_admin = TRUE")
                .fetch_one(&self.pool)
                .await?;

        if super_admin_count.0 <= 1 {
            error!("Cannot demote the last super admin");
            return Ok(false);
        }

        let now = Utc::now();

        let result = sqlx::query(
            r#"
            UPDATE admin_users
            SET is_super_admin = FALSE, updated_at = ?
            WHERE id = ?
            "#,
        )
        .bind(now)
        .bind(admin_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MIGRATOR;

    #[tokio::test]
    async fn test_admin_crud() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();

        let service = AdminStorageService::new(pool);

        // Create admin
        let password_hash = HashedPassword::from_password("testpass");
        let admin_id = service
            .create_admin("testadmin", &password_hash, true)
            .await
            .unwrap();

        assert!(admin_id > 0);

        // Get admin by username
        let admin = service
            .get_admin_by_username("testadmin")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(admin.username, "testadmin");
        assert!(admin.is_super_admin);

        // Authenticate
        let auth_admin = service
            .authenticate("testadmin", "testpass")
            .await
            .unwrap();
        assert!(auth_admin.is_some());

        // Wrong password
        let auth_fail = service
            .authenticate("testadmin", "wrongpass")
            .await
            .unwrap();
        assert!(auth_fail.is_none());

        // List admins
        let admins = service.list_admins().await.unwrap();
        assert_eq!(admins.len(), 1);

        // Count admins
        let count = service.count_admins().await.unwrap();
        assert_eq!(count, 1);
    }
}
