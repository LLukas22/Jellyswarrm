use std::{
    collections::{HashMap, HashSet},
    io,
    sync::Arc,
    time::Duration,
};

use sqlx::{sqlite::SqliteRow, Row, SqlitePool};
use tracing::{debug, error, info, trace};
use uuid::Uuid;

#[cfg(test)]
use crate::config::MediaStreamingMode;
use crate::server_id::ServerId;
use crate::server_storage::Server;
#[cfg(test)]
use crate::server_url::ServerUrl;
use crate::{
    duplicate_handling::{MovieIdentity, MovieObservation, MovieProvider, StableMovieGroup},
    models::generate_token,
};
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

#[derive(Debug, Clone)]
pub struct MovieVersionGroup {
    pub id: i64,
    pub virtual_media_id: String,
    pub identity: MovieIdentity,
    ambiguous: bool,
}

#[derive(Debug, Clone)]
pub struct MovieVersionMember {
    pub mapping: MediaMapping,
    pub server: Server,
}

#[derive(Debug, Clone)]
pub struct MovieVersionSourceObservation {
    pub member_mapping_id: i64,
    pub source_virtual_id: String,
}

#[derive(Debug, Clone)]
pub struct MovieVersionSourceRoute {
    pub source_mapping: MediaMapping,
    pub member_mapping: MediaMapping,
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
    movie_version_reconciliation: Arc<tokio::sync::Mutex<()>>,
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
            movie_version_reconciliation: Arc::new(tokio::sync::Mutex::new(())),
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

    /// Reconciles identities for every observed movie and returns the stable
    /// aggregate ID assigned to each identity. Observing a movie without a
    /// reliable provider identity clears any stale group membership.
    pub async fn observe_movie_versions(
        &self,
        observations: &[MovieObservation],
    ) -> Result<HashMap<MovieIdentity, StableMovieGroup>, sqlx::Error> {
        let _reconciliation_guard = self.movie_version_reconciliation.lock().await;
        let mut transaction = self.pool.begin().await?;
        let mut observed_identities = HashSet::new();

        for observation in observations {
            let mapping: Option<(i64, i64)> = sqlx::query_as(
                "SELECT id, server_id FROM media_mappings WHERE virtual_media_id = ?",
            )
            .bind(Self::normalize_uuid(&observation.virtual_media_id))
            .fetch_optional(&mut *transaction)
            .await?;
            let Some((mapping_id, server_id)) = mapping else {
                continue;
            };

            let Some(identity) = observation.identity.as_ref() else {
                sqlx::query("DELETE FROM movie_version_sources WHERE member_mapping_id = ?")
                    .bind(mapping_id)
                    .execute(&mut *transaction)
                    .await?;
                sqlx::query("DELETE FROM movie_version_members WHERE media_mapping_id = ?")
                    .bind(mapping_id)
                    .execute(&mut *transaction)
                    .await?;
                continue;
            };
            let group = Self::get_or_create_movie_version_group(&mut transaction, identity).await?;
            if observation.ambiguous {
                Self::mark_movie_version_group_ambiguous(&mut transaction, group.id).await?;
                observed_identities.remove(identity);
                continue;
            }
            if group.ambiguous {
                observed_identities.remove(identity);
                continue;
            }
            let conflicting_member: Option<(i64,)> = sqlx::query_as(
                r#"
                SELECT media_mapping_id
                FROM movie_version_members
                WHERE group_id = ? AND server_id = ? AND media_mapping_id != ?
                "#,
            )
            .bind(group.id)
            .bind(server_id)
            .bind(mapping_id)
            .fetch_optional(&mut *transaction)
            .await?;
            if conflicting_member.is_some() {
                Self::mark_movie_version_group_ambiguous(&mut transaction, group.id).await?;
                observed_identities.remove(identity);
                continue;
            }
            let previous_group_id: Option<(i64,)> = sqlx::query_as(
                "SELECT group_id FROM movie_version_members WHERE media_mapping_id = ?",
            )
            .bind(mapping_id)
            .fetch_optional(&mut *transaction)
            .await?;
            if previous_group_id.is_some_and(|(group_id,)| group_id != group.id) {
                sqlx::query("DELETE FROM movie_version_sources WHERE member_mapping_id = ?")
                    .bind(mapping_id)
                    .execute(&mut *transaction)
                    .await?;
            }
            sqlx::query(
                r#"
                INSERT INTO movie_version_members (
                    group_id, media_mapping_id, server_id, observed_at
                )
                VALUES (?, ?, ?, ?)
                ON CONFLICT (media_mapping_id) DO UPDATE SET
                    group_id = excluded.group_id,
                    server_id = excluded.server_id,
                    observed_at = excluded.observed_at
                "#,
            )
            .bind(group.id)
            .bind(mapping_id)
            .bind(server_id)
            .bind(chrono::Utc::now())
            .execute(&mut *transaction)
            .await?;
            for source_virtual_id in &observation.source_virtual_ids {
                Self::upsert_movie_version_source_route(
                    &mut transaction,
                    group.id,
                    mapping_id,
                    source_virtual_id,
                )
                .await?;
            }
            observed_identities.insert(identity.clone());
        }

        sqlx::query(
            r#"
            DELETE FROM movie_version_groups
            WHERE ambiguous = 0 AND NOT EXISTS (
                SELECT 1
                FROM movie_version_members
                WHERE movie_version_members.group_id = movie_version_groups.id
            )
            "#,
        )
        .execute(&mut *transaction)
        .await?;

        let mut stable_groups = HashMap::new();
        for identity in observed_identities {
            let row: Option<(String, i64)> = sqlx::query_as(
                r#"
                SELECT g.virtual_media_id, COUNT(m.media_mapping_id)
                FROM movie_version_groups g
                JOIN movie_version_members m ON m.group_id = g.id
                WHERE g.provider = ? AND g.provider_id = ? AND g.ambiguous = 0
                GROUP BY g.id
                "#,
            )
            .bind(identity.provider.as_str())
            .bind(&identity.provider_id)
            .fetch_optional(&mut *transaction)
            .await?;
            if let Some(row) = row {
                stable_groups.insert(
                    identity,
                    StableMovieGroup {
                        virtual_media_id: row.0,
                        member_count: row.1.max(0) as usize,
                    },
                );
            }
        }

        transaction.commit().await?;
        Ok(stable_groups)
    }

    async fn get_or_create_movie_version_group(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        identity: &MovieIdentity,
    ) -> Result<MovieVersionGroup, sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO movie_version_groups (
                virtual_media_id, provider, provider_id, created_at
            )
            VALUES (?, ?, ?, ?)
            ON CONFLICT (provider, provider_id) DO NOTHING
            "#,
        )
        .bind(generate_token())
        .bind(identity.provider.as_str())
        .bind(&identity.provider_id)
        .bind(chrono::Utc::now())
        .execute(&mut **transaction)
        .await?;

        let row = sqlx::query(
            r#"
            SELECT id, virtual_media_id, provider, provider_id, ambiguous
            FROM movie_version_groups
            WHERE provider = ? AND provider_id = ?
            "#,
        )
        .bind(identity.provider.as_str())
        .bind(&identity.provider_id)
        .fetch_one(&mut **transaction)
        .await?;
        Self::movie_version_group_from_row(&row)
    }

    async fn mark_movie_version_group_ambiguous(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        group_id: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE movie_version_groups SET ambiguous = 1 WHERE id = ?")
            .bind(group_id)
            .execute(&mut **transaction)
            .await?;
        sqlx::query("DELETE FROM movie_version_members WHERE group_id = ?")
            .bind(group_id)
            .execute(&mut **transaction)
            .await?;
        Ok(())
    }

    pub async fn get_movie_version_group(
        &self,
        virtual_media_id: &str,
    ) -> Result<Option<MovieVersionGroup>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT id, virtual_media_id, provider, provider_id, ambiguous
            FROM movie_version_groups
            WHERE virtual_media_id = ? AND ambiguous = 0
            "#,
        )
        .bind(Self::normalize_uuid(virtual_media_id))
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref()
            .map(Self::movie_version_group_from_row)
            .transpose()
    }

    pub async fn get_movie_version_members(
        &self,
        group_id: i64,
    ) -> Result<Vec<MovieVersionMember>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT
                m.id AS media_id,
                m.virtual_media_id,
                m.original_media_id,
                m.server_id AS media_server_id,
                m.server_url AS media_server_url,
                m.created_at AS media_created_at,
                s.id AS server_id,
                s.name AS server_name,
                s.url AS server_url_full,
                s.priority,
                s.media_streaming_mode,
                s.created_at AS server_created_at,
                s.updated_at AS server_updated_at
            FROM movie_version_members vm
            JOIN media_mappings m ON m.id = vm.media_mapping_id
            JOIN servers s ON s.id = m.server_id
            WHERE vm.group_id = ?
            "#,
        )
        .bind(group_id)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let mapping = MediaMapping {
                    id: row.try_get("media_id")?,
                    virtual_media_id: row.try_get("virtual_media_id")?,
                    original_media_id: row.try_get("original_media_id")?,
                    server_id: ServerId::new(row.try_get("media_server_id")?),
                    server_url: row.try_get("media_server_url")?,
                    created_at: row.try_get("media_created_at")?,
                };
                Ok(MovieVersionMember {
                    mapping,
                    server: Server::from_session_join_row(&row)?,
                })
            })
            .collect()
    }

    pub async fn get_movie_version_members_by_virtual_id(
        &self,
        group_virtual_id: &str,
    ) -> Result<Vec<MovieVersionMember>, sqlx::Error> {
        let Some(group) = self.get_movie_version_group(group_virtual_id).await? else {
            return Ok(Vec::new());
        };
        self.get_movie_version_members(group.id).await
    }

    /// Replaces source routing observations only for members included in this
    /// detail response, leaving inaccessible or temporarily offline members
    /// untouched.
    pub async fn replace_movie_version_sources(
        &self,
        group_id: i64,
        refreshed_member_mapping_ids: &[i64],
        observations: &[MovieVersionSourceObservation],
    ) -> Result<bool, sqlx::Error> {
        let _reconciliation_guard = self.movie_version_reconciliation.lock().await;
        let mut transaction = self.pool.begin().await?;
        for member_id in refreshed_member_mapping_ids {
            let is_current_member: (bool,) = sqlx::query_as(
                r#"
                SELECT EXISTS (
                    SELECT 1
                    FROM movie_version_members
                    WHERE group_id = ? AND media_mapping_id = ?
                )
                "#,
            )
            .bind(group_id)
            .bind(member_id)
            .fetch_one(&mut *transaction)
            .await?;
            if !is_current_member.0 {
                transaction.rollback().await?;
                return Ok(false);
            }
            sqlx::query(
                "DELETE FROM movie_version_sources WHERE group_id = ? AND member_mapping_id = ?",
            )
            .bind(group_id)
            .bind(member_id)
            .execute(&mut *transaction)
            .await?;
        }

        for observation in observations {
            Self::upsert_movie_version_source_route(
                &mut transaction,
                group_id,
                observation.member_mapping_id,
                &observation.source_virtual_id,
            )
            .await?;
        }

        transaction.commit().await?;
        Ok(true)
    }

    async fn upsert_movie_version_source_route(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        group_id: i64,
        member_mapping_id: i64,
        source_virtual_id: &str,
    ) -> Result<(), sqlx::Error> {
        let source_mapping_id: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM media_mappings WHERE virtual_media_id = ?")
                .bind(Self::normalize_uuid(source_virtual_id))
                .fetch_optional(&mut **transaction)
                .await?;
        let Some((source_mapping_id,)) = source_mapping_id else {
            return Ok(());
        };

        sqlx::query(
            r#"
            INSERT INTO movie_version_sources (
                group_id, member_mapping_id, source_mapping_id, observed_at
            )
            SELECT ?, ?, ?, ?
            WHERE EXISTS (
                SELECT 1
                FROM movie_version_members
                WHERE group_id = ? AND media_mapping_id = ?
            )
            ON CONFLICT (source_mapping_id) DO UPDATE SET
                group_id = excluded.group_id,
                member_mapping_id = excluded.member_mapping_id,
                observed_at = excluded.observed_at
            "#,
        )
        .bind(group_id)
        .bind(member_mapping_id)
        .bind(source_mapping_id)
        .bind(chrono::Utc::now())
        .bind(group_id)
        .bind(member_mapping_id)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    pub async fn get_movie_version_source_route(
        &self,
        group_id: i64,
        source_virtual_id: &str,
    ) -> Result<Option<MovieVersionSourceRoute>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT
                source.id AS source_id,
                source.virtual_media_id AS source_virtual_media_id,
                source.original_media_id AS source_original_media_id,
                source.server_id AS source_server_id,
                source.server_url AS source_server_url,
                source.created_at AS source_created_at,
                member.id AS member_id,
                member.virtual_media_id AS member_virtual_media_id,
                member.original_media_id AS member_original_media_id,
                member.server_id AS member_server_id,
                member.server_url AS member_server_url,
                member.created_at AS member_created_at
            FROM movie_version_sources source_route
            JOIN movie_version_members version_member
                ON version_member.group_id = source_route.group_id
                AND version_member.media_mapping_id = source_route.member_mapping_id
            JOIN media_mappings source ON source.id = source_route.source_mapping_id
            JOIN media_mappings member ON member.id = source_route.member_mapping_id
            WHERE source_route.group_id = ? AND source.virtual_media_id = ?
            "#,
        )
        .bind(group_id)
        .bind(Self::normalize_uuid(source_virtual_id))
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|row| MovieVersionSourceRoute {
            source_mapping: MediaMapping {
                id: row.get("source_id"),
                virtual_media_id: row.get("source_virtual_media_id"),
                original_media_id: row.get("source_original_media_id"),
                server_id: ServerId::new(row.get("source_server_id")),
                server_url: row.get("source_server_url"),
                created_at: row.get("source_created_at"),
            },
            member_mapping: MediaMapping {
                id: row.get("member_id"),
                virtual_media_id: row.get("member_virtual_media_id"),
                original_media_id: row.get("member_original_media_id"),
                server_id: ServerId::new(row.get("member_server_id")),
                server_url: row.get("member_server_url"),
                created_at: row.get("member_created_at"),
            },
        }))
    }

    fn movie_version_group_from_row(row: &SqliteRow) -> Result<MovieVersionGroup, sqlx::Error> {
        let provider_value: String = row.try_get("provider")?;
        let provider = MovieProvider::parse(&provider_value).ok_or_else(|| {
            sqlx::Error::Decode(Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown movie provider {provider_value}"),
            )))
        })?;
        Ok(MovieVersionGroup {
            id: row.try_get("id")?,
            virtual_media_id: row.try_get("virtual_media_id")?,
            identity: MovieIdentity {
                provider,
                provider_id: row.try_get("provider_id")?,
            },
            ambiguous: row.try_get::<i64, _>("ambiguous")? != 0,
        })
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
        create_test_server_with_url(pool, "http://localhost:8096").await
    }

    async fn create_test_server_with_url(pool: &SqlitePool, url: &str) -> Server {
        let now = chrono::Utc::now();
        let result = sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(url)
        .bind(url)
        .bind(100)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();

        Server {
            id: ServerId::new(result.last_insert_rowid()),
            name: "Test Server".to_string(),
            url: ServerUrl::parse(url).unwrap(),
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
        assert_eq!(server.name, "http://localhost:8096");
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

    #[tokio::test]
    async fn movie_groups_have_stable_ids_and_exact_source_routes() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = MediaStorageService::new(pool.clone());
        let primary = create_test_server_with_url(&pool, "http://primary.example:8096").await;
        let sibling = create_test_server_with_url(&pool, "http://sibling.example:8096").await;

        let primary_mapping = service
            .get_or_create_media_mapping("movie-1", &primary)
            .await
            .unwrap();
        let sibling_mapping = service
            .get_or_create_media_mapping("movie-1", &sibling)
            .await
            .unwrap();

        let identity = MovieIdentity {
            provider: MovieProvider::Tmdb,
            provider_id: "42".to_string(),
        };
        let mut observations = vec![
            MovieObservation {
                virtual_media_id: primary_mapping.virtual_media_id.clone(),
                identity: Some(identity.clone()),
                ambiguous: false,
                source_virtual_ids: Vec::new(),
            },
            MovieObservation {
                virtual_media_id: sibling_mapping.virtual_media_id.clone(),
                identity: Some(identity.clone()),
                ambiguous: false,
                source_virtual_ids: Vec::new(),
            },
        ];
        let assignments = service.observe_movie_versions(&observations).await.unwrap();
        let assignment = assignments.get(&identity).unwrap().clone();
        let aggregate_id = assignment.virtual_media_id;
        assert_eq!(assignment.member_count, 2);
        let repeated = service.observe_movie_versions(&observations).await.unwrap();
        assert_eq!(
            repeated
                .get(&identity)
                .map(|group| group.virtual_media_id.as_str()),
            Some(aggregate_id.as_str())
        );

        let group = service
            .get_movie_version_group(&aggregate_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(group.identity, identity);
        assert_eq!(
            service
                .get_movie_version_members(group.id)
                .await
                .unwrap()
                .len(),
            2
        );

        let source_mapping = service
            .get_or_create_media_mapping("source-1", &sibling)
            .await
            .unwrap();
        observations[1]
            .source_virtual_ids
            .push(source_mapping.virtual_media_id.clone());
        service.observe_movie_versions(&observations).await.unwrap();
        assert!(service
            .get_movie_version_source_route(group.id, &source_mapping.virtual_media_id)
            .await
            .unwrap()
            .is_some());
        service
            .replace_movie_version_sources(
                group.id,
                &[sibling_mapping.id],
                &[MovieVersionSourceObservation {
                    member_mapping_id: sibling_mapping.id,
                    source_virtual_id: source_mapping.virtual_media_id.clone(),
                }],
            )
            .await
            .unwrap();
        let route = service
            .get_movie_version_source_route(group.id, &source_mapping.virtual_media_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(route.source_mapping.id, source_mapping.id);
        assert_eq!(route.member_mapping.id, sibling_mapping.id);

        service.observe_movie_versions(&observations).await.unwrap();
        assert!(service
            .get_movie_version_source_route(group.id, &source_mapping.virtual_media_id)
            .await
            .unwrap()
            .is_some());

        service
            .observe_movie_versions(&[MovieObservation {
                virtual_media_id: primary_mapping.virtual_media_id,
                identity: None,
                ambiguous: false,
                source_virtual_ids: Vec::new(),
            }])
            .await
            .unwrap();
        assert_eq!(
            service
                .get_movie_version_members(group.id)
                .await
                .unwrap()
                .len(),
            1
        );

        let same_server_copy = service
            .get_or_create_media_mapping("movie-copy", &sibling)
            .await
            .unwrap();
        let assignments = service
            .observe_movie_versions(&[
                MovieObservation {
                    virtual_media_id: sibling_mapping.virtual_media_id,
                    identity: Some(identity.clone()),
                    ambiguous: false,
                    source_virtual_ids: Vec::new(),
                },
                MovieObservation {
                    virtual_media_id: same_server_copy.virtual_media_id,
                    identity: Some(identity.clone()),
                    ambiguous: false,
                    source_virtual_ids: Vec::new(),
                },
            ])
            .await
            .unwrap();
        assert!(!assignments.contains_key(&identity));
        assert!(service
            .get_movie_version_group(&aggregate_id)
            .await
            .unwrap()
            .is_none());
    }
}
