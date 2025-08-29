use sqlx::{FromRow, Row, SqlitePool};
use tracing::{debug, info};
use uuid::Uuid;

use crate::models::generate_token;
use crate::server_storage::Server;

#[derive(Debug, Clone, FromRow)]
pub struct MediaMapping {
    pub id: i64,
    pub virtual_media_id: String,
    pub original_media_id: String,
    pub server_url: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct MediaStorageService {
    pool: SqlitePool,
}

impl MediaStorageService {
    pub async fn new(pool: SqlitePool) -> Result<Self, sqlx::Error> {
        // Create media_mappings table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS media_mappings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                virtual_media_id TEXT NOT NULL UNIQUE,
                original_media_id TEXT NOT NULL,
                server_url TEXT NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(original_media_id, server_url)
            )
            "#,
        )
        .execute(&pool)
        .await?;

        // Create indexes for better performance
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_media_mappings_virtual_id 
            ON media_mappings(virtual_media_id)
            "#,
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_media_mappings_original_server 
            ON media_mappings(original_media_id, server_url)
            "#,
        )
        .execute(&pool)
        .await?;

        info!("Media storage service database initialized");
        Ok(Self { pool })
    }

    /// Create or get a media mapping
    pub async fn get_or_create_media_mapping(
        &self,
        original_media_id: &str,
        server_url: &str,
    ) -> Result<MediaMapping, sqlx::Error> {
        let original_media_id = Self::normalize_uuid(original_media_id);

        // Try to find existing mapping
        if let Some(mapping) = self
            .get_media_mapping_by_original(&original_media_id, server_url)
            .await?
        {
            return Ok(mapping);
        }

        // Create new mapping
        let virtual_media_id = generate_token();
        let now = chrono::Utc::now();

        let inserted = sqlx::query_as::<_, MediaMapping>(
            r#"
            INSERT INTO media_mappings (virtual_media_id, original_media_id, server_url, created_at)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(original_media_id, server_url) DO NOTHING
            RETURNING id, virtual_media_id, original_media_id, server_url, created_at
            "#,
        )
        .bind(&virtual_media_id)
        .bind(&original_media_id)
        .bind(server_url)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = inserted {
            debug!(
                "Created new media mapping: {} -> {} ({})",
                &original_media_id, row.virtual_media_id, server_url
            );
            return Ok(row);
        }

        // Conflict path: fetch existing row. Happens if another process created it concurrently
        if let Some(existing) = self
            .get_media_mapping_by_original(&original_media_id, server_url)
            .await?
        {
            return Ok(existing);
        }

        // If we reach here, something went very wrong
        Err(sqlx::Error::RowNotFound)
    }

    pub fn normalize_uuid(s: &str) -> String {
        match Uuid::parse_str(s) {
            Ok(uuid) => uuid.simple().to_string(),
            Err(_) => s.to_string(),
        }
    }

    /// Get media mapping by virtual media ID
    pub async fn get_media_mapping_by_virtual(
        &self,
        virtual_media_id: &str,
    ) -> Result<Option<MediaMapping>, sqlx::Error> {
        let virtual_media_id = Self::normalize_uuid(virtual_media_id);

        let mapping = sqlx::query_as::<_, MediaMapping>(
            r#"
            SELECT id, virtual_media_id, original_media_id, server_url, created_at
            FROM media_mappings 
            WHERE virtual_media_id = ?
            "#,
        )
        .bind(virtual_media_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(mapping)
    }

    /// Get media mapping by original media ID and server
    pub async fn get_media_mapping_by_original(
        &self,
        original_media_id: &str,
        server_url: &str,
    ) -> Result<Option<MediaMapping>, sqlx::Error> {
        let original_media_id = Self::normalize_uuid(original_media_id);

        let mapping = sqlx::query_as::<_, MediaMapping>(
            r#"
            SELECT id, virtual_media_id, original_media_id, server_url, created_at
            FROM media_mappings 
            WHERE original_media_id = ? AND server_url = ?
            "#,
        )
        .bind(original_media_id)
        .bind(server_url)
        .fetch_optional(&self.pool)
        .await?;

        Ok(mapping)
    }

    /// Get media mapping with server information by virtual media ID
    pub async fn get_media_mapping_with_server(
        &self,
        virtual_media_id: &str,
    ) -> Result<Option<(MediaMapping, Server)>, sqlx::Error> {
        let virtual_media_id = Self::normalize_uuid(virtual_media_id);

        let row = sqlx::query(
            r#"
            SELECT 
                m.id as media_id,
                m.virtual_media_id,
                m.original_media_id,
                m.server_url as media_server_url,
                m.created_at as media_created_at,
                
                s.id as server_id,
                s.name as server_name,
                s.url as server_url_full,
                s.priority,
                s.created_at as server_created_at,
                s.updated_at as server_updated_at
            FROM media_mappings m
            JOIN servers s ON RTRIM(m.server_url, '/') = RTRIM(s.url, '/')
            WHERE m.virtual_media_id = ?
            "#,
        )
        .bind(virtual_media_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = row {
            let mapping = MediaMapping {
                id: row.get("media_id"),
                virtual_media_id: row.get("virtual_media_id"),
                original_media_id: row.get("original_media_id"),
                server_url: row.get("media_server_url"),
                created_at: row.get("media_created_at"),
            };

            let server = Server {
                id: row.get("server_id"),
                name: row.get("server_name"),
                url: url::Url::parse(row.get::<String, _>("server_url_full").as_str()).unwrap(),
                priority: row.get("priority"),
                created_at: row.get("server_created_at"),
                updated_at: row.get("server_updated_at"),
            };

            Ok(Some((mapping, server)))
        } else {
            Ok(None)
        }
    }

    /// Delete a media mapping
    pub async fn delete_media_mapping(&self, virtual_media_id: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"
            DELETE FROM media_mappings WHERE virtual_media_id = ?
            "#,
        )
        .bind(virtual_media_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() > 0 {
            info!("Deleted media mapping: {}", virtual_media_id);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Delete all media mappings for a specific server
    pub async fn delete_media_mappings_by_server(
        &self,
        server_url: &str,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            DELETE FROM media_mappings WHERE server_url = ?
            "#,
        )
        .bind(server_url)
        .execute(&self.pool)
        .await?;

        let deleted_count = result.rows_affected();
        if deleted_count > 0 {
            info!(
                "Deleted {} media mappings for server: {}",
                deleted_count, server_url
            );
        }
        Ok(deleted_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_media_storage_service() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let service = MediaStorageService::new(pool.clone()).await.unwrap();

        // Create media mapping
        let mapping = service
            .get_or_create_media_mapping("original-movie-123", "http://localhost:8096")
            .await
            .unwrap();

        assert_eq!(mapping.original_media_id, "original-movie-123");
        assert_eq!(mapping.server_url, "http://localhost:8096");

        // Get mapping by virtual ID
        let retrieved_mapping = service
            .get_media_mapping_by_virtual(&mapping.virtual_media_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(retrieved_mapping.virtual_media_id, mapping.virtual_media_id);
        assert_eq!(retrieved_mapping.original_media_id, "original-movie-123");
    }

    #[tokio::test]
    async fn test_get_media_mapping_with_server() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let service = MediaStorageService::new(pool.clone()).await.unwrap();

        // Create the servers table (normally done by ServerStorageService)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create a server in the servers table
        sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("Test Server")
        .bind("http://localhost:8096")
        .bind(100)
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .execute(&pool)
        .await
        .unwrap();

        // Create media mapping
        let mapping = service
            .get_or_create_media_mapping("original-movie-123", "http://localhost:8096")
            .await
            .unwrap();

        // Get mapping with server info
        let (retrieved_mapping, server) = service
            .get_media_mapping_with_server(&mapping.virtual_media_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(retrieved_mapping.virtual_media_id, mapping.virtual_media_id);
        assert_eq!(retrieved_mapping.original_media_id, "original-movie-123");
        assert_eq!(server.name, "Test Server");
        assert_eq!(
            server.url.as_str().trim_end_matches('/'),
            "http://localhost:8096"
        );
    }

    #[tokio::test]
    async fn test_delete_operations() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let service = MediaStorageService::new(pool.clone()).await.unwrap();

        // Create media mapping
        let mapping = service
            .get_or_create_media_mapping("movie-123", "http://localhost:8096")
            .await
            .unwrap();

        // Verify mapping exists
        assert!(service
            .get_media_mapping_by_virtual(&mapping.virtual_media_id)
            .await
            .unwrap()
            .is_some());

        // Delete mapping
        let deleted = service
            .delete_media_mapping(&mapping.virtual_media_id)
            .await
            .unwrap();

        assert!(deleted);

        // Verify mapping is gone
        assert!(service
            .get_media_mapping_by_virtual(&mapping.virtual_media_id)
            .await
            .unwrap()
            .is_none());
    }
}
