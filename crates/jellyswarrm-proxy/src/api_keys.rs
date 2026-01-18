//! API key management for programmatic access

use chrono::{DateTime, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, SqlitePool};
use tracing::info;

/// API key stored in database (with hashed key)
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: i64,
    pub name: String,
    pub key_hash: String,
    pub key_prefix: String, // First 8 chars for identification
    pub permissions: String, // JSON array of permissions
    pub created_by: String,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// API key with the actual key (only returned on creation)
#[derive(Debug, Clone, Serialize)]
pub struct ApiKeyWithSecret {
    pub id: i64,
    pub name: String,
    pub key: String, // The actual API key (only available once!)
    pub key_prefix: String,
    pub permissions: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Permissions that can be granted to an API key
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ApiKeyPermission {
    #[serde(rename = "read:users")]
    ReadUsers,
    #[serde(rename = "write:users")]
    WriteUsers,
    #[serde(rename = "read:servers")]
    ReadServers,
    #[serde(rename = "write:servers")]
    WriteServers,
    #[serde(rename = "read:stats")]
    ReadStats,
    #[serde(rename = "read:health")]
    ReadHealth,
    #[serde(rename = "read:audit")]
    ReadAudit,
    #[serde(rename = "admin:full")]
    AdminFull,
}

impl ApiKeyPermission {
    pub fn all() -> Vec<ApiKeyPermission> {
        vec![
            ApiKeyPermission::ReadUsers,
            ApiKeyPermission::WriteUsers,
            ApiKeyPermission::ReadServers,
            ApiKeyPermission::WriteServers,
            ApiKeyPermission::ReadStats,
            ApiKeyPermission::ReadHealth,
            ApiKeyPermission::ReadAudit,
            ApiKeyPermission::AdminFull,
        ]
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ApiKeyPermission::ReadUsers => "read:users",
            ApiKeyPermission::WriteUsers => "write:users",
            ApiKeyPermission::ReadServers => "read:servers",
            ApiKeyPermission::WriteServers => "write:servers",
            ApiKeyPermission::ReadStats => "read:stats",
            ApiKeyPermission::ReadHealth => "read:health",
            ApiKeyPermission::ReadAudit => "read:audit",
            ApiKeyPermission::AdminFull => "admin:full",
        }
    }
}

/// API key management service
#[derive(Clone)]
pub struct ApiKeyService {
    pool: SqlitePool,
}

impl ApiKeyService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Generate a secure random API key
    fn generate_key() -> String {
        let mut rng = rand::rng();
        let key_bytes: [u8; 32] = rng.random();
        format!("jsw_{}", hex::encode(key_bytes))
    }

    /// Hash an API key for storage
    fn hash_key(key: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Create a new API key
    pub async fn create_key(
        &self,
        name: &str,
        permissions: Vec<String>,
        created_by: &str,
        expires_in_days: Option<i64>,
    ) -> Result<ApiKeyWithSecret, sqlx::Error> {
        let key = Self::generate_key();
        let key_hash = Self::hash_key(&key);
        let key_prefix = key.chars().take(12).collect::<String>(); // "jsw_" + 8 chars
        let permissions_json = serde_json::to_string(&permissions).unwrap_or_else(|_| "[]".to_string());
        let now = Utc::now();
        let expires_at = expires_in_days.map(|days| now + chrono::Duration::days(days));

        let result = sqlx::query_as::<_, (i64,)>(
            r#"
            INSERT INTO api_keys (name, key_hash, key_prefix, permissions, created_by, expires_at, created_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            RETURNING id
            "#,
        )
        .bind(name)
        .bind(&key_hash)
        .bind(&key_prefix)
        .bind(&permissions_json)
        .bind(created_by)
        .bind(expires_at)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;

        info!("Created API key '{}' (prefix: {})", name, key_prefix);

        Ok(ApiKeyWithSecret {
            id: result.0,
            name: name.to_string(),
            key,
            key_prefix,
            permissions,
            expires_at,
            created_at: now,
        })
    }

    /// Validate an API key and return its details if valid
    pub async fn validate_key(&self, key: &str) -> Result<Option<ApiKey>, sqlx::Error> {
        let key_hash = Self::hash_key(key);

        let api_key = sqlx::query_as::<_, ApiKey>(
            r#"
            SELECT id, name, key_hash, key_prefix, permissions, created_by, last_used_at, expires_at, created_at
            FROM api_keys
            WHERE key_hash = ?
            "#,
        )
        .bind(&key_hash)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(ref key) = api_key {
            // Check if expired
            if let Some(expires_at) = key.expires_at {
                if expires_at < Utc::now() {
                    return Ok(None);
                }
            }

            // Update last used timestamp
            let _ = sqlx::query("UPDATE api_keys SET last_used_at = ? WHERE id = ?")
                .bind(Utc::now())
                .bind(key.id)
                .execute(&self.pool)
                .await;
        }

        Ok(api_key)
    }

    /// Check if an API key has a specific permission
    pub fn has_permission(api_key: &ApiKey, permission: &str) -> bool {
        let permissions: Vec<String> = serde_json::from_str(&api_key.permissions).unwrap_or_default();

        // admin:full grants all permissions
        if permissions.contains(&"admin:full".to_string()) {
            return true;
        }

        permissions.contains(&permission.to_string())
    }

    /// List all API keys (without the actual keys)
    pub async fn list_keys(&self) -> Result<Vec<ApiKey>, sqlx::Error> {
        let keys = sqlx::query_as::<_, ApiKey>(
            r#"
            SELECT id, name, key_hash, key_prefix, permissions, created_by, last_used_at, expires_at, created_at
            FROM api_keys
            ORDER BY created_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(keys)
    }

    /// Delete an API key
    pub async fn delete_key(&self, id: i64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM api_keys WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() > 0 {
            info!("Deleted API key with id: {}", id);
        }

        Ok(result.rows_affected() > 0)
    }

    /// Revoke all API keys for a user
    pub async fn revoke_keys_for_user(&self, username: &str) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM api_keys WHERE created_by = ?")
            .bind(username)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() > 0 {
            info!(
                "Revoked {} API keys for user: {}",
                result.rows_affected(),
                username
            );
        }

        Ok(result.rows_affected())
    }

    /// Cleanup expired API keys
    pub async fn cleanup_expired(&self) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM api_keys WHERE expires_at IS NOT NULL AND expires_at < ?")
            .bind(Utc::now())
            .execute(&self.pool)
            .await?;

        if result.rows_affected() > 0 {
            info!("Cleaned up {} expired API keys", result.rows_affected());
        }

        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_key_format() {
        let key = ApiKeyService::generate_key();
        assert!(key.starts_with("jsw_"));
        assert_eq!(key.len(), 68); // "jsw_" + 64 hex chars
    }

    #[test]
    fn test_hash_key_deterministic() {
        let key = "jsw_test123";
        let hash1 = ApiKeyService::hash_key(key);
        let hash2 = ApiKeyService::hash_key(key);
        assert_eq!(hash1, hash2);
    }
}
