use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Row, SqlitePool};
use tracing::info;
use uuid::Uuid;

/// Deduplication strategy for merged libraries
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeduplicationStrategy {
    /// Match by provider IDs (TMDB, IMDB, TVDB)
    #[default]
    ProviderIds,
    /// Match by title and year
    NameYear,
    /// No deduplication - show all copies
    None,
}

impl DeduplicationStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeduplicationStrategy::ProviderIds => "provider_ids",
            DeduplicationStrategy::NameYear => "name_year",
            DeduplicationStrategy::None => "none",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "provider_ids" => DeduplicationStrategy::ProviderIds,
            "name_year" => DeduplicationStrategy::NameYear,
            "none" => DeduplicationStrategy::None,
            _ => DeduplicationStrategy::ProviderIds,
        }
    }
}

/// Collection type for merged libraries
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CollectionType {
    Movies,
    TvShows,
    Music,
    Books,
    Mixed,
}

impl CollectionType {
    pub fn as_str(&self) -> &'static str {
        match self {
            CollectionType::Movies => "movies",
            CollectionType::TvShows => "tvshows",
            CollectionType::Music => "music",
            CollectionType::Books => "books",
            CollectionType::Mixed => "mixed",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "movies" => CollectionType::Movies,
            "tvshows" => CollectionType::TvShows,
            "music" => CollectionType::Music,
            "books" => CollectionType::Books,
            "mixed" => CollectionType::Mixed,
            _ => CollectionType::Mixed,
        }
    }

    /// Convert to Jellyfin collection type string
    pub fn to_jellyfin_type(&self) -> &'static str {
        match self {
            CollectionType::Movies => "movies",
            CollectionType::TvShows => "tvshows",
            CollectionType::Music => "music",
            CollectionType::Books => "books",
            CollectionType::Mixed => "mixed",
        }
    }
}

/// A merged library definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedLibrary {
    pub id: i64,
    pub virtual_id: String,
    pub name: String,
    pub collection_type: CollectionType,
    pub dedup_strategy: DeduplicationStrategy,
    pub created_by: Option<String>,
    pub is_global: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A source library for a merged library
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct MergedLibrarySource {
    pub id: i64,
    pub merged_library_id: i64,
    pub server_id: i64,
    pub library_id: String,
    pub library_name: Option<String>,
    pub priority: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Request to create a merged library (for JSON API)
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateMergedLibraryRequest {
    pub name: String,
    pub collection_type: String,
    pub dedup_strategy: Option<String>,
    pub is_global: Option<bool>,
}

/// Request to add a source to a merged library (for JSON API)
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddSourceRequest {
    pub server_id: i64,
    pub library_id: String,
    pub library_name: Option<String>,
    pub priority: Option<i32>,
}

/// Storage service for merged libraries
#[derive(Debug, Clone)]
pub struct MergedLibraryStorageService {
    pool: SqlitePool,
}

impl MergedLibraryStorageService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Create a new merged library
    pub async fn create_merged_library(
        &self,
        name: &str,
        collection_type: &str,
        dedup_strategy: Option<&str>,
        created_by: Option<&str>,
        is_global: bool,
    ) -> Result<MergedLibrary, sqlx::Error> {
        let virtual_id = format!("merged-{}", Uuid::new_v4());
        let now = chrono::Utc::now();
        let strategy = dedup_strategy.unwrap_or("provider_ids");

        let result = sqlx::query(
            r#"
            INSERT INTO merged_libraries (virtual_id, name, collection_type, dedup_strategy, created_by, is_global, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&virtual_id)
        .bind(name)
        .bind(collection_type)
        .bind(strategy)
        .bind(created_by)
        .bind(is_global)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        info!("Created merged library: {} ({})", name, virtual_id);

        Ok(MergedLibrary {
            id,
            virtual_id,
            name: name.to_string(),
            collection_type: CollectionType::from_str(collection_type),
            dedup_strategy: DeduplicationStrategy::from_str(strategy),
            created_by: created_by.map(|s| s.to_string()),
            is_global,
            created_at: now,
            updated_at: now,
        })
    }

    /// Get a merged library by ID
    pub async fn get_merged_library(&self, id: i64) -> Result<Option<MergedLibrary>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT id, virtual_id, name, collection_type, dedup_strategy, created_by, is_global, created_at, updated_at
            FROM merged_libraries
            WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| self.row_to_merged_library(r)))
    }

    /// Get a merged library by virtual ID
    pub async fn get_merged_library_by_virtual_id(
        &self,
        virtual_id: &str,
    ) -> Result<Option<MergedLibrary>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT id, virtual_id, name, collection_type, dedup_strategy, created_by, is_global, created_at, updated_at
            FROM merged_libraries
            WHERE virtual_id = ?
            "#,
        )
        .bind(virtual_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| self.row_to_merged_library(r)))
    }

    /// List all merged libraries
    pub async fn list_merged_libraries(&self) -> Result<Vec<MergedLibrary>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT id, virtual_id, name, collection_type, dedup_strategy, created_by, is_global, created_at, updated_at
            FROM merged_libraries
            ORDER BY name ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|r| self.row_to_merged_library(r)).collect())
    }

    /// List merged libraries visible to a user (global + user's own)
    pub async fn list_merged_libraries_for_user(
        &self,
        user_id: Option<&str>,
    ) -> Result<Vec<MergedLibrary>, sqlx::Error> {
        let rows = if let Some(uid) = user_id {
            sqlx::query(
                r#"
                SELECT id, virtual_id, name, collection_type, dedup_strategy, created_by, is_global, created_at, updated_at
                FROM merged_libraries
                WHERE is_global = TRUE OR created_by = ?
                ORDER BY name ASC
                "#,
            )
            .bind(uid)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                r#"
                SELECT id, virtual_id, name, collection_type, dedup_strategy, created_by, is_global, created_at, updated_at
                FROM merged_libraries
                WHERE is_global = TRUE
                ORDER BY name ASC
                "#,
            )
            .fetch_all(&self.pool)
            .await?
        };

        Ok(rows.into_iter().map(|r| self.row_to_merged_library(r)).collect())
    }

    /// Update a merged library
    pub async fn update_merged_library(
        &self,
        id: i64,
        name: Option<&str>,
        dedup_strategy: Option<&str>,
        is_global: Option<bool>,
    ) -> Result<bool, sqlx::Error> {
        let now = chrono::Utc::now();

        // Build dynamic update query
        let mut updates = vec!["updated_at = ?"];
        let mut has_updates = false;

        if name.is_some() {
            updates.push("name = ?");
            has_updates = true;
        }
        if dedup_strategy.is_some() {
            updates.push("dedup_strategy = ?");
            has_updates = true;
        }
        if is_global.is_some() {
            updates.push("is_global = ?");
            has_updates = true;
        }

        if !has_updates {
            return Ok(false);
        }

        let query = format!(
            "UPDATE merged_libraries SET {} WHERE id = ?",
            updates.join(", ")
        );

        let mut q = sqlx::query(&query).bind(now);

        if let Some(n) = name {
            q = q.bind(n);
        }
        if let Some(s) = dedup_strategy {
            q = q.bind(s);
        }
        if let Some(g) = is_global {
            q = q.bind(g);
        }

        let result = q.bind(id).execute(&self.pool).await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete a merged library
    pub async fn delete_merged_library(&self, id: i64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM merged_libraries WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() > 0 {
            info!("Deleted merged library ID: {}", id);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Add a source library to a merged library
    pub async fn add_source(
        &self,
        merged_library_id: i64,
        server_id: i64,
        library_id: &str,
        library_name: Option<&str>,
        priority: i32,
    ) -> Result<MergedLibrarySource, sqlx::Error> {
        let now = chrono::Utc::now();

        let result = sqlx::query(
            r#"
            INSERT INTO merged_library_sources (merged_library_id, server_id, library_id, library_name, priority, created_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(merged_library_id)
        .bind(server_id)
        .bind(library_id)
        .bind(library_name)
        .bind(priority)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        info!(
            "Added source {} from server {} to merged library {}",
            library_id, server_id, merged_library_id
        );

        Ok(MergedLibrarySource {
            id,
            merged_library_id,
            server_id,
            library_id: library_id.to_string(),
            library_name: library_name.map(|s| s.to_string()),
            priority,
            created_at: now,
        })
    }

    /// Get sources for a merged library
    pub async fn get_sources(
        &self,
        merged_library_id: i64,
    ) -> Result<Vec<MergedLibrarySource>, sqlx::Error> {
        let rows = sqlx::query_as::<_, MergedLibrarySource>(
            r#"
            SELECT id, merged_library_id, server_id, library_id, library_name, priority, created_at
            FROM merged_library_sources
            WHERE merged_library_id = ?
            ORDER BY priority DESC
            "#,
        )
        .bind(merged_library_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Remove a source from a merged library
    pub async fn remove_source(&self, source_id: i64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM merged_library_sources WHERE id = ?")
            .bind(source_id)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Remove a source by library ID
    pub async fn remove_source_by_library(
        &self,
        merged_library_id: i64,
        server_id: i64,
        library_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"
            DELETE FROM merged_library_sources
            WHERE merged_library_id = ? AND server_id = ? AND library_id = ?
            "#,
        )
        .bind(merged_library_id)
        .bind(server_id)
        .bind(library_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Check if a virtual ID is a merged library
    pub async fn is_merged_library(&self, virtual_id: &str) -> Result<bool, sqlx::Error> {
        // Quick check based on prefix
        if !virtual_id.starts_with("merged-") {
            return Ok(false);
        }

        let row = sqlx::query("SELECT 1 FROM merged_libraries WHERE virtual_id = ?")
            .bind(virtual_id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.is_some())
    }

    /// Internal method to convert database row to MergedLibrary struct
    fn row_to_merged_library(&self, row: sqlx::sqlite::SqliteRow) -> MergedLibrary {
        MergedLibrary {
            id: row.get("id"),
            virtual_id: row.get("virtual_id"),
            name: row.get("name"),
            collection_type: CollectionType::from_str(row.get("collection_type")),
            dedup_strategy: DeduplicationStrategy::from_str(row.get("dedup_strategy")),
            created_by: row.get("created_by"),
            is_global: row.get("is_global"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::MIGRATOR;
    use super::*;

    #[tokio::test]
    async fn test_merged_library_crud() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();

        let service = MergedLibraryStorageService::new(pool);

        // Create a merged library
        let lib = service
            .create_merged_library("All Movies", "movies", Some("provider_ids"), Some("admin"), true)
            .await
            .unwrap();

        assert_eq!(lib.name, "All Movies");
        assert!(lib.virtual_id.starts_with("merged-"));
        assert_eq!(lib.collection_type, CollectionType::Movies);
        assert_eq!(lib.dedup_strategy, DeduplicationStrategy::ProviderIds);
        assert!(lib.is_global);

        // Get by ID
        let fetched = service.get_merged_library(lib.id).await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().name, "All Movies");

        // Get by virtual ID
        let fetched = service
            .get_merged_library_by_virtual_id(&lib.virtual_id)
            .await
            .unwrap();
        assert!(fetched.is_some());

        // List all
        let all = service.list_merged_libraries().await.unwrap();
        assert_eq!(all.len(), 1);

        // Delete
        let deleted = service.delete_merged_library(lib.id).await.unwrap();
        assert!(deleted);

        let all = service.list_merged_libraries().await.unwrap();
        assert_eq!(all.len(), 0);
    }

    #[tokio::test]
    async fn test_is_merged_library() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();

        let service = MergedLibraryStorageService::new(pool);

        // Non-merged IDs should return false
        assert!(!service.is_merged_library("regular-id").await.unwrap());
        assert!(!service.is_merged_library("abc123").await.unwrap());

        // Create a merged library
        let lib = service
            .create_merged_library("Test", "movies", None, None, true)
            .await
            .unwrap();

        // Its virtual ID should return true
        assert!(service.is_merged_library(&lib.virtual_id).await.unwrap());

        // Random merged- prefix should return false (not in DB)
        assert!(!service.is_merged_library("merged-nonexistent").await.unwrap());
    }
}
