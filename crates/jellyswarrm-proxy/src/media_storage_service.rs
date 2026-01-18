use std::time::Duration;

use sqlx::{FromRow, Row, SqlitePool};
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

use crate::models::generate_token;
use crate::server_storage::Server;
use moka::future::Cache;

/// Maximum number of retries for database operations
const MAX_RETRIES: u32 = 3;
/// Base delay between retries (will be multiplied by attempt number)
const RETRY_BASE_DELAY_MS: u64 = 50;

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
        server_url: &str,
    ) -> Result<MediaMapping, sqlx::Error> {
        let key = format!("{}|{}", original_media_id, server_url);
        if let Some(cached) = self.original_mapping_cache.get(&key).await {
            trace!("Cache hit for media mapping: {}", key);
            return Ok(cached);
        }
        let mapping = self
            ._get_or_create_media_mapping(original_media_id, server_url)
            .await?;
        self.original_mapping_cache
            .insert(key, mapping.clone())
            .await;
        Ok(mapping)
    }

    /// Pre-warm the cache by batch fetching existing mappings for a list of original IDs
    /// This reduces database round-trips when processing many media items
    pub async fn prewarm_cache_for_ids(
        &self,
        original_media_ids: &[String],
        server_url: &str,
    ) -> Result<(), sqlx::Error> {
        if original_media_ids.is_empty() {
            return Ok(());
        }

        // Normalize IDs
        let normalized_ids: Vec<String> = original_media_ids
            .iter()
            .map(|id| Self::normalize_uuid(id))
            .collect();

        // Check which IDs are not already cached
        let mut uncached_ids = Vec::new();
        for id in &normalized_ids {
            let key = format!("{}|{}", id, server_url);
            if self.original_mapping_cache.get(&key).await.is_none() {
                uncached_ids.push(id.clone());
            }
        }

        if uncached_ids.is_empty() {
            debug!("All {} IDs already cached", normalized_ids.len());
            return Ok(());
        }

        debug!(
            "Pre-warming cache: {} IDs uncached out of {}",
            uncached_ids.len(),
            normalized_ids.len()
        );

        // Batch fetch existing mappings (up to 500 at a time to avoid query limits)
        for chunk in uncached_ids.chunks(500) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let query = format!(
                r#"
                SELECT id, virtual_media_id, original_media_id, server_url, created_at
                FROM media_mappings
                WHERE original_media_id IN ({}) AND server_url = ?
                "#,
                placeholders
            );

            let mut query_builder = sqlx::query_as::<_, MediaMapping>(&query);
            for id in chunk {
                query_builder = query_builder.bind(id);
            }
            query_builder = query_builder.bind(server_url);

            let mappings = query_builder.fetch_all(&self.pool).await?;

            // Insert fetched mappings into cache
            for mapping in mappings {
                let key = format!("{}|{}", mapping.original_media_id, server_url);
                self.original_mapping_cache
                    .insert(key, mapping)
                    .await;
            }
        }

        Ok(())
    }

    async fn _get_or_create_media_mapping(
        &self,
        original_media_id: &str,
        server_url: &str,
    ) -> Result<MediaMapping, sqlx::Error> {
        let original_media_id = Self::normalize_uuid(original_media_id);
        let virtual_media_id = generate_token();
        let now = chrono::Utc::now();

        // Use a single efficient upsert query with retry logic for lock contention
        for attempt in 0..MAX_RETRIES {
            // Try INSERT first, then SELECT if conflict (more efficient than SELECT-then-INSERT)
            let result = sqlx::query_as::<_, MediaMapping>(
                r#"
                INSERT INTO media_mappings (virtual_media_id, original_media_id, server_url, created_at)
                VALUES (?, ?, ?, ?)
                ON CONFLICT(original_media_id, server_url) DO UPDATE SET id = id
                RETURNING id, virtual_media_id, original_media_id, server_url, created_at
                "#,
            )
            .bind(&virtual_media_id)
            .bind(&original_media_id)
            .bind(server_url)
            .bind(now)
            .fetch_one(&self.pool)
            .await;

            match result {
                Ok(mapping) => {
                    if mapping.virtual_media_id == virtual_media_id {
                        debug!(
                            "Created new media mapping: {} -> {} ({})",
                            &original_media_id, mapping.virtual_media_id, server_url
                        );
                    }
                    return Ok(mapping);
                }
                Err(ref e) if e.to_string().contains("database is locked") => {
                    // Database lock contention - retry with exponential backoff
                    if attempt < MAX_RETRIES - 1 {
                        let delay = RETRY_BASE_DELAY_MS * (attempt as u64 + 1);
                        warn!(
                            "Database locked, retrying media mapping in {}ms (attempt {}/{})",
                            delay, attempt + 1, MAX_RETRIES
                        );
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        continue;
                    }
                    // On final failure, return a generic error
                    error!("Database lock contention persisted after {} retries", MAX_RETRIES);
                }
                Err(e) => return Err(e),
            }
        }

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
        .bind(&virtual_media_id)
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
        self.original_mapping_cache.invalidate_all();
        self.mapping_with_server_cache.invalidate_all();
        Ok(deleted_count)
    }
}

#[cfg(test)]
mod tests {
    use crate::config::MIGRATOR;

    use super::*;

    #[tokio::test]
    async fn test_media_storage_service() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = MediaStorageService::new(pool.clone());

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
        MIGRATOR.run(&pool).await.unwrap();
        let service = MediaStorageService::new(pool.clone());

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
        MIGRATOR.run(&pool).await.unwrap();
        let service = MediaStorageService::new(pool.clone());

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
