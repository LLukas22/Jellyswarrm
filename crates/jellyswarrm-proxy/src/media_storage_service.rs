use std::time::Duration;

use sqlx::{sqlite::SqliteRow, Row, SqlitePool};
use tracing::{debug, error, info, trace};
use uuid::Uuid;

#[cfg(test)]
use crate::config::MediaStreamingMode;
use crate::models::generate_token;
use crate::server_id::ServerId;
use crate::server_storage::Server;
#[cfg(test)]
use crate::server_url::ServerUrl;
use moka::future::Cache;

#[derive(Debug, Clone)]
pub struct MediaMapping {
    pub id: i64,
    pub virtual_media_id: String,
    pub original_media_id: String,
    pub server_id: ServerId,
    pub server_url: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl<'r> sqlx::FromRow<'r, SqliteRow> for MediaMapping {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            virtual_media_id: row.try_get("virtual_media_id")?,
            original_media_id: row.try_get("original_media_id")?,
            server_id: ServerId::new(row.try_get("server_id")?),
            server_url: row.try_get("server_url")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct MediaStorageService {
    pool: SqlitePool,
    original_mapping_cache: Cache<String, MediaMapping>,
    mapping_with_server_cache: Cache<String, (MediaMapping, Server)>,
}

impl MediaStorageService {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            original_mapping_cache: Cache::builder()
                .time_to_live(Duration::from_secs(60 * 30))
                .max_capacity(100_000)
                .build(),
            mapping_with_server_cache: Cache::builder()
                .time_to_live(Duration::from_secs(60 * 30))
                .max_capacity(10_000)
                .build(),
        }
    }

    /// Create or get a media mapping
    pub async fn get_or_create_media_mapping(
        &self,
        original_media_id: &str,
        server: &Server,
    ) -> Result<MediaMapping, sqlx::Error> {
        let original_media_id = Self::normalize_uuid(original_media_id);
        let server_id = server.id;
        let key = format!("{}|{}", original_media_id, server_id);
        if let Some(cached) = self.original_mapping_cache.get(&key).await {
            trace!("Cache hit for media mapping: {}", key);
            return Ok(cached);
        }
        let mapping = self
            ._get_or_create_media_mapping(&original_media_id, server)
            .await?;
        self.original_mapping_cache
            .insert(key, mapping.clone())
            .await;
        Ok(mapping)
    }

    async fn _get_or_create_media_mapping(
        &self,
        original_media_id: &str,
        server: &Server,
    ) -> Result<MediaMapping, sqlx::Error> {
        let original_media_id = Self::normalize_uuid(original_media_id);

        // Try to find existing mapping
        if let Some(mapping) = self
            .get_media_mapping_by_original(&original_media_id, server.id)
            .await?
        {
            return Ok(mapping);
        }

        // Create new mapping
        let virtual_media_id = generate_token();
        let now = chrono::Utc::now();

        let inserted = sqlx::query_as::<_, MediaMapping>(
            r#"
            INSERT INTO media_mappings (virtual_media_id, original_media_id, server_id, server_url, created_at)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(original_media_id, server_id) DO NOTHING
            RETURNING id, virtual_media_id, original_media_id, server_id, server_url, created_at
            "#,
        )
        .bind(&virtual_media_id)
        .bind(&original_media_id)
        .bind(server.id.as_i64())
        .bind(server.url.as_str())
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = inserted {
            debug!(
                "Created new media mapping: {} -> {} ({})",
                &original_media_id,
                row.virtual_media_id,
                server.url.as_str()
            );
            return Ok(row);
        }

        // Conflict path: fetch existing row. Happens if another process created it concurrently
        if let Some(existing) = self
            .get_media_mapping_by_original(&original_media_id, server.id)
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
            SELECT id, virtual_media_id, original_media_id, server_id, server_url, created_at
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
        server_id: ServerId,
    ) -> Result<Option<MediaMapping>, sqlx::Error> {
        let original_media_id = Self::normalize_uuid(original_media_id);

        let mapping = sqlx::query_as::<_, MediaMapping>(
            r#"
            SELECT id, virtual_media_id, original_media_id, server_id, server_url, created_at
            FROM media_mappings 
            WHERE original_media_id = ? AND server_id = ?
            "#,
        )
        .bind(original_media_id)
        .bind(server_id.as_i64())
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

        if let Some(cached) = self.mapping_with_server_cache.get(&virtual_media_id).await {
            trace!(
                "Cache hit for media mapping with server: {}",
                virtual_media_id
            );
            return Ok(Some(cached));
        }

        let row = sqlx::query(
            r#"
            SELECT 
                m.id as media_id,
                m.virtual_media_id,
                m.original_media_id,
                m.server_id as media_server_id,
                m.server_url as media_server_url,
                m.created_at as media_created_at,
                
                s.id as server_id,
                s.name as server_name,
                s.url as server_url_full,
                s.priority,
                s.media_streaming_mode,
                s.created_at as server_created_at,
                s.updated_at as server_updated_at
            FROM media_mappings m
            JOIN servers s ON m.server_id = s.id
            WHERE m.virtual_media_id = ?
            "#,
        )
        .bind(&virtual_media_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = row {
            let mapping = MediaMapping {
                id: row.get("media_id"),
                virtual_media_id: row.get("virtual_media_id"),
                original_media_id: row.get("original_media_id"),
                server_id: ServerId::new(row.get("media_server_id")),
                server_url: row.get("media_server_url"),
                created_at: row.get("media_created_at"),
            };

            let server = Server::from_session_join_row(&row)?;

            self.mapping_with_server_cache
                .insert(virtual_media_id, (mapping.clone(), server.clone()))
                .await;
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
            {
                let id_to_invalidate = virtual_media_id.to_string();
                if let Err(e) =
                    self.original_mapping_cache
                        .invalidate_entries_if(move |_, value| {
                            value.virtual_media_id == id_to_invalidate
                        })
                {
                    error!("Failed to invalidate cache entry: {}", e);
                    self.original_mapping_cache.invalidate_all();
                }
            }
            // Also invalidate the mapping_with_server_cache
            self.mapping_with_server_cache
                .invalidate(virtual_media_id)
                .await;
            info!("Deleted media mapping: {}", virtual_media_id);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Delete all media mappings for a specific server
    pub async fn delete_media_mappings_by_server(
        &self,
        server: &Server,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            DELETE FROM media_mappings WHERE server_id = ?
            "#,
        )
        .bind(server.id.as_i64())
        .execute(&self.pool)
        .await?;

        let deleted_count = result.rows_affected();
        if deleted_count > 0 {
            info!(
                "Deleted {} media mappings for server: {}",
                deleted_count,
                server.url.as_str()
            );
        }
        self.original_mapping_cache.invalidate_all();
        self.mapping_with_server_cache.invalidate_all();
        Ok(deleted_count)
    }
}

#[cfg(test)]
mod tests {
    use crate::config::MIGRATOR;

    use super::*;

    async fn create_test_server(pool: &SqlitePool) -> Server {
        let now = chrono::Utc::now();
        let result = sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("Test Server")
        .bind("http://localhost:8096")
        .bind(100)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();

        Server {
            id: ServerId::new(result.last_insert_rowid()),
            name: "Test Server".to_string(),
            url: ServerUrl::parse("http://localhost:8096").unwrap(),
            priority: 100,
            media_streaming_mode: MediaStreamingMode::Redirect,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn test_media_storage_service() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = MediaStorageService::new(pool.clone());
        let server = create_test_server(&pool).await;

        // Create media mapping
        let mapping = service
            .get_or_create_media_mapping("original-movie-123", &server)
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
        MIGRATOR.run(&pool).await.unwrap();
        let service = MediaStorageService::new(pool.clone());
        let server = create_test_server(&pool).await;

        // Create media mapping
        let mapping = service
            .get_or_create_media_mapping("original-movie-123", &server)
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
        assert_eq!(server.url.as_str(), "http://localhost:8096");
    }

    #[tokio::test]
    async fn test_delete_operations() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = MediaStorageService::new(pool.clone());
        let server = create_test_server(&pool).await;

        // Create media mapping
        let mapping = service
            .get_or_create_media_mapping("movie-123", &server)
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
