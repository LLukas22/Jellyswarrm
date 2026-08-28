use std::{
    collections::{BTreeSet, HashMap, HashSet},
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
    duplicate_handling::{MovieAlias, MovieObservation, MovieProvider, StableMovieGroup},
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
    pub scope_id: i64,
    pub published: bool,
    pub ambiguous: bool,
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

#[derive(Debug, Clone)]
pub struct MovieCatalogSnapshot {
    pub source_key: String,
    pub server_id: ServerId,
    pub complete: bool,
    pub observations: Vec<MovieObservation>,
}

#[derive(Debug)]
struct ActiveMovieMember {
    mapping_id: i64,
    virtual_media_id: String,
    server_id: ServerId,
    group_id: Option<i64>,
    aliases: BTreeSet<MovieAlias>,
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

    pub async fn begin_movie_reconciliation(&self) -> Result<i64, sqlx::Error> {
        let (generation,): (i64,) = sqlx::query_as(
            "UPDATE movie_version_clock SET generation = generation + 1 WHERE singleton = 1 RETURNING generation",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(generation)
    }

    pub async fn reconcile_movie_catalog(
        &self,
        scope_key: &str,
        generation: i64,
        snapshots: &[MovieCatalogSnapshot],
        prune_missing_sources: bool,
    ) -> Result<HashMap<String, StableMovieGroup>, sqlx::Error> {
        let _guard = self.movie_version_reconciliation.lock().await;
        let mut transaction = self.pool.begin().await?;
        let scope_id = Self::movie_scope_id(&mut transaction, scope_key).await?;
        let (committed_generation,): (i64,) =
            sqlx::query_as("SELECT committed_generation FROM movie_catalog_scopes WHERE id = ?")
                .bind(scope_id)
                .fetch_one(&mut *transaction)
                .await?;
        if generation <= committed_generation {
            let stable_groups = Self::rebuild_movie_groups(&mut transaction, scope_id).await?;
            transaction.commit().await?;
            return Ok(stable_groups);
        }

        let mut resolved_snapshots = Vec::with_capacity(snapshots.len());
        let mut aliases_by_mapping = HashMap::<i64, BTreeSet<MovieAlias>>::new();
        for snapshot in snapshots {
            let source_id = Self::movie_source_id(
                &mut transaction,
                scope_id,
                &snapshot.source_key,
                snapshot.server_id,
            )
            .await?;
            let mut observed_mapping_ids = HashSet::new();
            for observation in &snapshot.observations {
                let mapping: Option<(i64,)> = sqlx::query_as(
                    "SELECT id FROM media_mappings WHERE virtual_media_id = ? AND server_id = ?",
                )
                .bind(Self::normalize_uuid(&observation.virtual_media_id))
                .bind(snapshot.server_id.as_i64())
                .fetch_optional(&mut *transaction)
                .await?;
                let Some((mapping_id,)) = mapping else {
                    continue;
                };
                observed_mapping_ids.insert(mapping_id);
                aliases_by_mapping
                    .entry(mapping_id)
                    .or_default()
                    .extend(observation.aliases.iter().cloned());
            }
            resolved_snapshots.push((source_id, snapshot, observed_mapping_ids));
        }

        for (mapping_id, aliases) in aliases_by_mapping {
            sqlx::query(
                r#"
                INSERT INTO movie_version_members (
                    scope_id, media_mapping_id, aliases_generation, observed_at
                ) VALUES (?, ?, ?, ?)
                ON CONFLICT (scope_id, media_mapping_id) DO UPDATE SET
                    aliases_generation = excluded.aliases_generation,
                    observed_at = excluded.observed_at
                "#,
            )
            .bind(scope_id)
            .bind(mapping_id)
            .bind(generation)
            .bind(chrono::Utc::now())
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "DELETE FROM movie_version_aliases WHERE scope_id = ? AND media_mapping_id = ?",
            )
            .bind(scope_id)
            .bind(mapping_id)
            .execute(&mut *transaction)
            .await?;
            for alias in aliases {
                sqlx::query(
                    "INSERT INTO movie_version_aliases (scope_id, media_mapping_id, provider, provider_id) VALUES (?, ?, ?, ?)",
                )
                .bind(scope_id)
                .bind(mapping_id)
                .bind(alias.provider.as_str())
                .bind(alias.provider_id)
                .execute(&mut *transaction)
                .await?;
            }
        }

        for (source_id, snapshot, observed_mapping_ids) in resolved_snapshots {
            for mapping_id in &observed_mapping_ids {
                sqlx::query(
                    r#"
                    INSERT INTO movie_catalog_sightings (
                        source_id, scope_id, media_mapping_id, generation
                    ) VALUES (?, ?, ?, ?)
                    ON CONFLICT (source_id, media_mapping_id) DO UPDATE SET
                        generation = excluded.generation
                    "#,
                )
                .bind(source_id)
                .bind(scope_id)
                .bind(*mapping_id)
                .bind(generation)
                .execute(&mut *transaction)
                .await?;
            }

            if snapshot.complete {
                let existing = sqlx::query(
                    "SELECT media_mapping_id FROM movie_catalog_sightings WHERE source_id = ?",
                )
                .bind(source_id)
                .fetch_all(&mut *transaction)
                .await?;
                for row in existing {
                    let mapping_id: i64 = row.get("media_mapping_id");
                    if !observed_mapping_ids.contains(&mapping_id) {
                        sqlx::query(
                            "DELETE FROM movie_catalog_sightings WHERE source_id = ? AND media_mapping_id = ?",
                        )
                        .bind(source_id)
                        .bind(mapping_id)
                        .execute(&mut *transaction)
                        .await?;
                    }
                }
            }
            sqlx::query("UPDATE movie_catalog_sources SET committed_generation = ? WHERE id = ?")
                .bind(generation)
                .bind(source_id)
                .execute(&mut *transaction)
                .await?;
        }

        sqlx::query("UPDATE movie_catalog_scopes SET committed_generation = ? WHERE id = ?")
            .bind(generation)
            .bind(scope_id)
            .execute(&mut *transaction)
            .await?;

        let scope_sources_generation: (i64,) =
            sqlx::query_as("SELECT sources_generation FROM movie_catalog_scopes WHERE id = ?")
                .bind(scope_id)
                .fetch_one(&mut *transaction)
                .await?;
        if prune_missing_sources && generation > scope_sources_generation.0 {
            let current_source_keys = snapshots
                .iter()
                .map(|snapshot| snapshot.source_key.as_str())
                .collect::<HashSet<_>>();
            let source_rows =
                sqlx::query("SELECT id, source_key FROM movie_catalog_sources WHERE scope_id = ?")
                    .bind(scope_id)
                    .fetch_all(&mut *transaction)
                    .await?;
            for row in source_rows {
                let source_key: String = row.get("source_key");
                if !current_source_keys.contains(source_key.as_str()) {
                    let source_id: i64 = row.get("id");
                    sqlx::query("DELETE FROM movie_catalog_sources WHERE id = ?")
                        .bind(source_id)
                        .execute(&mut *transaction)
                        .await?;
                }
            }
            sqlx::query("UPDATE movie_catalog_scopes SET sources_generation = ? WHERE id = ?")
                .bind(generation)
                .bind(scope_id)
                .execute(&mut *transaction)
                .await?;
        }

        let stable_groups = Self::rebuild_movie_groups(&mut transaction, scope_id).await?;
        transaction.commit().await?;
        Ok(stable_groups)
    }

    async fn movie_scope_id(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        scope_key: &str,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query(
            "INSERT INTO movie_catalog_scopes (scope_key) VALUES (?) ON CONFLICT DO NOTHING",
        )
        .bind(scope_key)
        .execute(&mut **transaction)
        .await?;
        let (id,): (i64,) =
            sqlx::query_as("SELECT id FROM movie_catalog_scopes WHERE scope_key = ?")
                .bind(scope_key)
                .fetch_one(&mut **transaction)
                .await?;
        Ok(id)
    }

    async fn movie_source_id(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        scope_id: i64,
        source_key: &str,
        server_id: ServerId,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query(
            "INSERT INTO movie_catalog_sources (scope_id, source_key, server_id) VALUES (?, ?, ?) ON CONFLICT (scope_id, source_key) DO UPDATE SET server_id = excluded.server_id",
        )
        .bind(scope_id)
        .bind(source_key)
        .bind(server_id.as_i64())
        .execute(&mut **transaction)
        .await?;
        let (id,): (i64,) = sqlx::query_as(
            "SELECT id FROM movie_catalog_sources WHERE scope_id = ? AND source_key = ?",
        )
        .bind(scope_id)
        .bind(source_key)
        .fetch_one(&mut **transaction)
        .await?;
        Ok(id)
    }

    async fn rebuild_movie_groups(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        scope_id: i64,
    ) -> Result<HashMap<String, StableMovieGroup>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT
                mapping.id,
                mapping.virtual_media_id,
                mapping.server_id,
                member.group_id
            FROM movie_version_members member
            JOIN media_mappings mapping ON mapping.id = member.media_mapping_id
            WHERE member.scope_id = ?
              AND EXISTS (
                  SELECT 1 FROM movie_catalog_sightings sighting
                  WHERE sighting.scope_id = member.scope_id
                    AND sighting.media_mapping_id = member.media_mapping_id
              )
              AND EXISTS (
                  SELECT 1 FROM movie_version_aliases alias
                  WHERE alias.scope_id = member.scope_id
                    AND alias.media_mapping_id = member.media_mapping_id
              )
            "#,
        )
        .bind(scope_id)
        .fetch_all(&mut **transaction)
        .await?;
        let mut members = rows
            .into_iter()
            .map(|row| ActiveMovieMember {
                mapping_id: row.get("id"),
                virtual_media_id: row.get("virtual_media_id"),
                server_id: ServerId::new(row.get("server_id")),
                group_id: row.get("group_id"),
                aliases: BTreeSet::new(),
            })
            .collect::<Vec<_>>();
        let member_positions = members
            .iter()
            .enumerate()
            .map(|(index, member)| (member.mapping_id, index))
            .collect::<HashMap<_, _>>();
        let alias_rows = sqlx::query(
            r#"
            SELECT media_mapping_id, provider, provider_id
            FROM movie_version_aliases
            WHERE scope_id = ?
            "#,
        )
        .bind(scope_id)
        .fetch_all(&mut **transaction)
        .await?;
        for row in alias_rows {
            let mapping_id: i64 = row.get("media_mapping_id");
            let Some(&position) = member_positions.get(&mapping_id) else {
                continue;
            };
            let provider: String = row.get("provider");
            let Some(provider) = MovieProvider::parse(&provider) else {
                continue;
            };
            members[position].aliases.insert(MovieAlias {
                provider,
                provider_id: row.get("provider_id"),
            });
        }

        sqlx::query(
            r#"
            UPDATE movie_version_members
            SET group_id = NULL
            WHERE scope_id = ?
              AND EXISTS (
                  SELECT 1 FROM movie_catalog_sightings sighting
                  WHERE sighting.scope_id = movie_version_members.scope_id
                    AND sighting.media_mapping_id = movie_version_members.media_mapping_id
              )
              AND NOT EXISTS (
                  SELECT 1 FROM movie_version_aliases alias
                  WHERE alias.scope_id = movie_version_members.scope_id
                    AND alias.media_mapping_id = movie_version_members.media_mapping_id
              )
            "#,
        )
        .bind(scope_id)
        .execute(&mut **transaction)
        .await?;

        let mut parents = (0..members.len()).collect::<Vec<_>>();
        let mut alias_owners = HashMap::new();
        for (index, member) in members.iter().enumerate() {
            for alias in &member.aliases {
                if let Some(owner) = alias_owners.insert(alias.clone(), index) {
                    union_indexes(&mut parents, owner, index);
                }
            }
        }
        let mut components = HashMap::<usize, Vec<usize>>::new();
        for index in 0..members.len() {
            let root = find_index(&mut parents, index);
            components.entry(root).or_default().push(index);
        }
        let mut components = components.into_values().collect::<Vec<_>>();
        components.sort_by_key(|component| {
            component
                .iter()
                .map(|index| members[*index].mapping_id)
                .min()
                .unwrap_or_default()
        });

        let group_alias_rows = sqlx::query(
            "SELECT group_id, provider, provider_id FROM movie_version_group_aliases WHERE scope_id = ?",
        )
        .bind(scope_id)
        .fetch_all(&mut **transaction)
        .await?;
        let mut historical_groups = HashMap::new();
        for row in group_alias_rows {
            let provider: String = row.get("provider");
            let Some(provider) = MovieProvider::parse(&provider) else {
                continue;
            };
            historical_groups.insert(
                MovieAlias {
                    provider,
                    provider_id: row.get("provider_id"),
                },
                row.get::<i64, _>("group_id"),
            );
        }
        let component_groups = components
            .iter()
            .map(|component| {
                let mut group_ids = component
                    .iter()
                    .filter_map(|index| members[*index].group_id)
                    .collect::<HashSet<_>>();
                for alias in component
                    .iter()
                    .flat_map(|index| members[*index].aliases.iter())
                {
                    if let Some(group_id) = historical_groups.get(alias) {
                        group_ids.insert(*group_id);
                    }
                }
                let mut group_ids = group_ids.into_iter().collect::<Vec<_>>();
                group_ids.sort_unstable();
                group_ids
            })
            .collect::<Vec<_>>();

        let mut group_components = HashMap::<i64, Vec<usize>>::new();
        for (component_index, group_ids) in component_groups.iter().enumerate() {
            for group_id in group_ids {
                group_components
                    .entry(*group_id)
                    .or_default()
                    .push(component_index);
            }
        }
        let split_groups = group_components
            .iter()
            .filter_map(|(group_id, component_indexes)| {
                (component_indexes.len() > 1).then_some(*group_id)
            })
            .collect::<HashSet<_>>();
        for group_id in &split_groups {
            sqlx::query(
                "UPDATE movie_version_groups SET ambiguous = 1 WHERE id = ? AND scope_id = ?",
            )
            .bind(group_id)
            .bind(scope_id)
            .execute(&mut **transaction)
            .await?;
        }

        let mut assignments = HashMap::new();
        for (component, existing_groups) in components.into_iter().zip(component_groups) {
            let group_id = if let Some(group_id) = existing_groups
                .iter()
                .copied()
                .find(|group_id| !split_groups.contains(group_id))
            {
                group_id
            } else {
                Self::create_movie_group(transaction, scope_id).await?
            };

            for losing_group_id in existing_groups.iter().copied() {
                if losing_group_id == group_id || split_groups.contains(&losing_group_id) {
                    continue;
                }
                sqlx::query("UPDATE movie_version_group_ids SET canonical = 0 WHERE group_id = ?")
                    .bind(losing_group_id)
                    .execute(&mut **transaction)
                    .await?;
                sqlx::query("UPDATE movie_version_group_ids SET group_id = ? WHERE group_id = ?")
                    .bind(group_id)
                    .bind(losing_group_id)
                    .execute(&mut **transaction)
                    .await?;
            }

            for index in &component {
                if members[*index].group_id != Some(group_id) {
                    sqlx::query(
                        "DELETE FROM movie_version_sources WHERE scope_id = ? AND member_mapping_id = ?",
                    )
                    .bind(scope_id)
                    .bind(members[*index].mapping_id)
                    .execute(&mut **transaction)
                    .await?;
                }
                sqlx::query(
                    "UPDATE movie_version_members SET group_id = ? WHERE scope_id = ? AND media_mapping_id = ?",
                )
                .bind(group_id)
                .bind(scope_id)
                .bind(members[*index].mapping_id)
                .execute(&mut **transaction)
                .await?;
            }
            for losing_group_id in existing_groups.iter().copied() {
                if losing_group_id == group_id || split_groups.contains(&losing_group_id) {
                    continue;
                }
                sqlx::query(
                    "DELETE FROM movie_version_sources WHERE scope_id = ? AND group_id = ?",
                )
                .bind(scope_id)
                .bind(losing_group_id)
                .execute(&mut **transaction)
                .await?;
                sqlx::query(
                    "UPDATE movie_version_members SET group_id = ? WHERE scope_id = ? AND group_id = ?",
                )
                .bind(group_id)
                .bind(scope_id)
                .bind(losing_group_id)
                .execute(&mut **transaction)
                .await?;
                sqlx::query("DELETE FROM movie_version_groups WHERE id = ? AND scope_id = ?")
                    .bind(losing_group_id)
                    .bind(scope_id)
                    .execute(&mut **transaction)
                    .await?;
            }

            for old_group_id in existing_groups
                .iter()
                .copied()
                .chain(std::iter::once(group_id))
            {
                sqlx::query(
                    "DELETE FROM movie_version_group_aliases WHERE scope_id = ? AND group_id = ?",
                )
                .bind(scope_id)
                .bind(old_group_id)
                .execute(&mut **transaction)
                .await?;
            }
            let component_aliases = component
                .iter()
                .flat_map(|index| members[*index].aliases.iter())
                .collect::<BTreeSet<_>>();
            for alias in component_aliases {
                sqlx::query(
                    r#"
                    INSERT INTO movie_version_group_aliases (
                        scope_id, group_id, provider, provider_id
                    ) VALUES (?, ?, ?, ?)
                    ON CONFLICT (scope_id, provider, provider_id) DO UPDATE SET
                        group_id = excluded.group_id
                    "#,
                )
                .bind(scope_id)
                .bind(group_id)
                .bind(alias.provider.as_str())
                .bind(&alias.provider_id)
                .execute(&mut **transaction)
                .await?;
            }

            let server_count = component
                .iter()
                .map(|index| members[*index].server_id)
                .collect::<HashSet<_>>()
                .len();
            let ambiguous = server_count != component.len();
            let publish = !ambiguous && server_count > 1;
            sqlx::query(
                "UPDATE movie_version_groups SET ambiguous = ?, published = CASE WHEN published = 1 OR ? THEN 1 ELSE 0 END WHERE id = ?",
            )
            .bind(ambiguous)
            .bind(publish)
            .bind(group_id)
            .execute(&mut **transaction)
            .await?;
            let (virtual_media_id, published): (String, bool) = sqlx::query_as(
                r#"
                SELECT id.virtual_media_id, group_row.published
                FROM movie_version_group_ids id
                JOIN movie_version_groups group_row ON group_row.id = id.group_id
                WHERE id.group_id = ? AND id.canonical = 1
                "#,
            )
            .bind(group_id)
            .fetch_one(&mut **transaction)
            .await?;
            let stable_group = StableMovieGroup {
                virtual_media_id,
                active_member_count: component.len(),
                ambiguous,
                published,
            };
            for index in component {
                assignments.insert(
                    members[index].virtual_media_id.clone(),
                    stable_group.clone(),
                );
            }
        }
        Ok(assignments)
    }

    async fn create_movie_group(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        scope_id: i64,
    ) -> Result<i64, sqlx::Error> {
        let result =
            sqlx::query("INSERT INTO movie_version_groups (scope_id, created_at) VALUES (?, ?)")
                .bind(scope_id)
                .bind(chrono::Utc::now())
                .execute(&mut **transaction)
                .await?;
        let group_id = result.last_insert_rowid();
        sqlx::query(
            "INSERT INTO movie_version_group_ids (virtual_media_id, group_id, canonical, created_at) VALUES (?, ?, 1, ?)",
        )
        .bind(generate_token())
        .bind(group_id)
        .bind(chrono::Utc::now())
        .execute(&mut **transaction)
        .await?;
        Ok(group_id)
    }

    pub async fn get_movie_version_group(
        &self,
        virtual_media_id: &str,
    ) -> Result<Option<MovieVersionGroup>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT
                group_row.id,
                group_id.virtual_media_id,
                group_row.scope_id,
                group_row.published,
                group_row.ambiguous
            FROM movie_version_group_ids group_id
            JOIN movie_version_groups group_row ON group_row.id = group_id.group_id
            WHERE group_id.virtual_media_id = ?
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
            JOIN movie_version_groups version_group ON version_group.id = vm.group_id
            WHERE vm.group_id = ? AND version_group.ambiguous = 0
              AND EXISTS (
                  SELECT 1 FROM movie_catalog_sightings sighting
                  WHERE sighting.scope_id = vm.scope_id
                    AND sighting.media_mapping_id = vm.media_mapping_id
              )
              AND EXISTS (
                  SELECT 1 FROM movie_version_aliases alias
                  WHERE alias.scope_id = vm.scope_id
                    AND alias.media_mapping_id = vm.media_mapping_id
              )
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
        generation: i64,
        refreshed_member_mapping_ids: &[i64],
        observations: &[MovieVersionSourceObservation],
    ) -> Result<bool, sqlx::Error> {
        let _reconciliation_guard = self.movie_version_reconciliation.lock().await;
        let mut transaction = self.pool.begin().await?;
        let refreshed_members = refreshed_member_mapping_ids
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        if observations
            .iter()
            .any(|observation| !refreshed_members.contains(&observation.member_mapping_id))
        {
            transaction.rollback().await?;
            return Ok(false);
        }
        let mut scope_id = None;
        for member_id in refreshed_member_mapping_ids {
            let current_member: Option<(i64, i64)> = sqlx::query_as(
                r#"
                SELECT member.sources_generation, member.scope_id
                FROM movie_version_members member
                JOIN movie_version_groups version_group ON version_group.id = member.group_id
                WHERE member.group_id = ? AND member.media_mapping_id = ?
                  AND version_group.ambiguous = 0
                  AND EXISTS (
                      SELECT 1 FROM movie_catalog_sightings sighting
                      WHERE sighting.scope_id = member.scope_id
                        AND sighting.media_mapping_id = member.media_mapping_id
                  )
                "#,
            )
            .bind(group_id)
            .bind(member_id)
            .fetch_optional(&mut *transaction)
            .await?;
            let Some((current_generation, member_scope_id)) = current_member else {
                transaction.rollback().await?;
                return Ok(false);
            };
            if current_generation >= generation
                || scope_id.is_some_and(|scope_id| scope_id != member_scope_id)
            {
                transaction.rollback().await?;
                return Ok(false);
            }
            scope_id = Some(member_scope_id);
            sqlx::query(
                "DELETE FROM movie_version_sources WHERE scope_id = ? AND group_id = ? AND member_mapping_id = ?",
            )
            .bind(member_scope_id)
            .bind(group_id)
            .bind(member_id)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "UPDATE movie_version_members SET sources_generation = ? WHERE group_id = ? AND media_mapping_id = ?",
            )
            .bind(generation)
            .bind(group_id)
            .bind(member_id)
            .execute(&mut *transaction)
            .await?;
        }

        let Some(scope_id) = scope_id else {
            transaction.commit().await?;
            return Ok(observations.is_empty());
        };
        for observation in observations {
            Self::upsert_movie_version_source_route(
                &mut transaction,
                scope_id,
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
        scope_id: i64,
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
                scope_id, group_id, member_mapping_id, source_mapping_id, observed_at
            )
            SELECT ?, ?, ?, ?, ?
            WHERE EXISTS (
                SELECT 1 FROM movie_version_members member
                JOIN media_mappings member_mapping ON member_mapping.id = member.media_mapping_id
                JOIN media_mappings source_mapping ON source_mapping.id = ?
                WHERE member.scope_id = ? AND member.group_id = ? AND member.media_mapping_id = ?
                  AND member_mapping.server_id = source_mapping.server_id
                  AND EXISTS (
                      SELECT 1 FROM movie_catalog_sightings sighting
                      WHERE sighting.scope_id = member.scope_id
                        AND sighting.media_mapping_id = member.media_mapping_id
                  )
            )
            ON CONFLICT (scope_id, group_id, source_mapping_id) DO UPDATE SET
                member_mapping_id = excluded.member_mapping_id,
                observed_at = excluded.observed_at
            "#,
        )
        .bind(scope_id)
        .bind(group_id)
        .bind(member_mapping_id)
        .bind(source_mapping_id)
        .bind(chrono::Utc::now())
        .bind(source_mapping_id)
        .bind(scope_id)
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
            JOIN movie_version_members version_member ON
                version_member.scope_id = source_route.scope_id
                AND version_member.group_id = source_route.group_id
                AND version_member.media_mapping_id = source_route.member_mapping_id
            JOIN movie_version_groups version_group ON
                version_group.id = version_member.group_id
                AND version_group.scope_id = version_member.scope_id
            JOIN media_mappings source ON source.id = source_route.source_mapping_id
            JOIN media_mappings member ON member.id = source_route.member_mapping_id
            WHERE source_route.group_id = ? AND source.virtual_media_id = ?
              AND version_group.ambiguous = 0
              AND EXISTS (
                  SELECT 1 FROM movie_catalog_sightings sighting
                  WHERE sighting.scope_id = version_member.scope_id
                    AND sighting.media_mapping_id = version_member.media_mapping_id
              )
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
        Ok(MovieVersionGroup {
            id: row.try_get("id")?,
            virtual_media_id: row.try_get("virtual_media_id")?,
            scope_id: row.try_get("scope_id")?,
            published: row.try_get("published")?,
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

fn find_index(parents: &mut [usize], index: usize) -> usize {
    if parents[index] != index {
        parents[index] = find_index(parents, parents[index]);
    }
    parents[index]
}

fn union_indexes(parents: &mut [usize], left: usize, right: usize) {
    let left = find_index(parents, left);
    let right = find_index(parents, right);
    if left != right {
        let (root, child) = if left < right {
            (left, right)
        } else {
            (right, left)
        };
        parents[child] = root;
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    use crate::config::MIGRATOR;

    use super::*;

    async fn foreign_key_pool() -> SqlitePool {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap()
    }

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
        let pool = foreign_key_pool().await;
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

        let alias = MovieAlias {
            provider: MovieProvider::Tmdb,
            provider_id: "42".to_string(),
        };
        let primary_observation = MovieObservation {
            virtual_media_id: primary_mapping.virtual_media_id.clone(),
            aliases: BTreeSet::from([alias.clone()]),
        };
        let sibling_observation = MovieObservation {
            virtual_media_id: sibling_mapping.virtual_media_id.clone(),
            aliases: BTreeSet::from([alias.clone()]),
        };
        let generation = service.begin_movie_reconciliation().await.unwrap();
        let assignments = service
            .reconcile_movie_catalog(
                "configured:library:user",
                generation,
                &[
                    MovieCatalogSnapshot {
                        source_key: "primary:library".to_string(),
                        server_id: primary.id,
                        complete: true,
                        observations: vec![primary_observation.clone()],
                    },
                    MovieCatalogSnapshot {
                        source_key: "sibling:library".to_string(),
                        server_id: sibling.id,
                        complete: true,
                        observations: vec![sibling_observation.clone()],
                    },
                ],
                true,
            )
            .await
            .unwrap();
        let assignment = assignments
            .get(&primary_mapping.virtual_media_id)
            .unwrap()
            .clone();
        let aggregate_id = assignment.virtual_media_id;
        assert_eq!(assignment.active_member_count, 2);
        assert!(assignment.published);
        assert!(!assignment.ambiguous);

        let group = service
            .get_movie_version_group(&aggregate_id)
            .await
            .unwrap()
            .unwrap();
        assert!(group.published);
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
        let source_generation = service.begin_movie_reconciliation().await.unwrap();
        service
            .replace_movie_version_sources(
                group.id,
                source_generation,
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

        let same_server_copy = service
            .get_or_create_media_mapping("movie-copy", &sibling)
            .await
            .unwrap();
        let copy_observation = MovieObservation {
            virtual_media_id: same_server_copy.virtual_media_id.clone(),
            aliases: BTreeSet::from([alias.clone()]),
        };
        let ambiguous_generation = service.begin_movie_reconciliation().await.unwrap();
        let assignments = service
            .reconcile_movie_catalog(
                "configured:library:user",
                ambiguous_generation,
                &[MovieCatalogSnapshot {
                    source_key: "sibling:library".to_string(),
                    server_id: sibling.id,
                    complete: false,
                    observations: vec![sibling_observation.clone(), copy_observation],
                }],
                false,
            )
            .await
            .unwrap();
        assert!(assignments[&sibling_mapping.virtual_media_id].ambiguous);

        let recovery_generation = service.begin_movie_reconciliation().await.unwrap();
        let recovered = service
            .reconcile_movie_catalog(
                "configured:library:user",
                recovery_generation,
                &[MovieCatalogSnapshot {
                    source_key: "sibling:library".to_string(),
                    server_id: sibling.id,
                    complete: true,
                    observations: vec![sibling_observation.clone()],
                }],
                false,
            )
            .await
            .unwrap();
        let recovered = &recovered[&sibling_mapping.virtual_media_id];
        assert!(!recovered.ambiguous);
        assert_eq!(recovered.virtual_media_id, aggregate_id);

        let stale_generation = service.begin_movie_reconciliation().await.unwrap();
        let fresh_generation = service.begin_movie_reconciliation().await.unwrap();
        service
            .reconcile_movie_catalog(
                "configured:library:user",
                fresh_generation,
                &[MovieCatalogSnapshot {
                    source_key: "sibling:library".to_string(),
                    server_id: sibling.id,
                    complete: true,
                    observations: Vec::new(),
                }],
                false,
            )
            .await
            .unwrap();
        let stale = service
            .reconcile_movie_catalog(
                "configured:library:user",
                stale_generation,
                &[MovieCatalogSnapshot {
                    source_key: "sibling:library".to_string(),
                    server_id: sibling.id,
                    complete: true,
                    observations: vec![sibling_observation],
                }],
                false,
            )
            .await
            .unwrap();
        assert!(!stale.contains_key(&sibling_mapping.virtual_media_id));

        let replacement_primary = service
            .get_or_create_media_mapping("replacement-primary", &primary)
            .await
            .unwrap();
        let replacement_sibling = service
            .get_or_create_media_mapping("replacement-sibling", &sibling)
            .await
            .unwrap();
        let replacement_generation = service.begin_movie_reconciliation().await.unwrap();
        let replacements = service
            .reconcile_movie_catalog(
                "configured:library:user",
                replacement_generation,
                &[
                    MovieCatalogSnapshot {
                        source_key: "primary:library".to_string(),
                        server_id: primary.id,
                        complete: true,
                        observations: vec![MovieObservation {
                            virtual_media_id: replacement_primary.virtual_media_id.clone(),
                            aliases: BTreeSet::from([alias.clone()]),
                        }],
                    },
                    MovieCatalogSnapshot {
                        source_key: "sibling:library".to_string(),
                        server_id: sibling.id,
                        complete: true,
                        observations: vec![MovieObservation {
                            virtual_media_id: replacement_sibling.virtual_media_id.clone(),
                            aliases: BTreeSet::from([alias]),
                        }],
                    },
                ],
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            replacements[&replacement_primary.virtual_media_id].virtual_media_id,
            aggregate_id
        );

        let foreign_key_violations = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(foreign_key_violations.is_empty());
    }

    #[tokio::test]
    async fn scoped_movie_migration_preserves_legacy_aggregate_routes() {
        let pool = foreign_key_pool().await;
        sqlx::raw_sql(
            r#"
            CREATE TABLE servers (id INTEGER PRIMARY KEY);
            CREATE TABLE media_mappings (
                id INTEGER PRIMARY KEY,
                virtual_media_id TEXT NOT NULL UNIQUE,
                original_media_id TEXT NOT NULL,
                server_id INTEGER NOT NULL,
                server_url TEXT NOT NULL,
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO servers (id) VALUES (1), (2);
            INSERT INTO media_mappings (
                id, virtual_media_id, original_media_id, server_id, server_url
            ) VALUES
                (1, 'member-a', 'movie-a', 1, 'http://a'),
                (2, 'member-b', 'movie-b', 2, 'http://b'),
                (3, 'source-b', 'source-b', 2, 'http://b');
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../migrations/20260827130000_movie_versions.up.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(
            r#"
            INSERT INTO movie_version_groups (
                id, virtual_media_id, provider, provider_id, ambiguous
            ) VALUES (7, 'aggregate-7', 'tmdb', '42', 0);
            INSERT INTO movie_version_members (
                group_id, media_mapping_id, server_id
            ) VALUES (7, 1, 1), (7, 2, 2);
            INSERT INTO movie_version_sources (
                group_id, member_mapping_id, source_mapping_id
            ) VALUES (7, 2, 3);
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::raw_sql(include_str!(
            "../migrations/20260827140000_scoped_movie_versions.up.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let migrated_group: (i64, String, bool) = sqlx::query_as(
            r#"
            SELECT version_group.id, group_id.virtual_media_id, version_group.published
            FROM movie_version_groups version_group
            JOIN movie_version_group_ids group_id ON group_id.group_id = version_group.id
            WHERE version_group.id = 7 AND group_id.canonical = 1
            "#,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(migrated_group, (7, "aggregate-7".to_string(), true));
        let route_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM movie_version_sources WHERE group_id = 7")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(route_count.0, 1);
        assert!(sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap()
            .is_empty());

        sqlx::raw_sql(include_str!(
            "../migrations/20260827140000_scoped_movie_versions.down.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let restored: (String, String, String) = sqlx::query_as(
            "SELECT virtual_media_id, provider, provider_id FROM movie_version_groups WHERE id = 7",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            restored,
            (
                "aggregate-7".to_string(),
                "tmdb".to_string(),
                "42".to_string()
            )
        );
        assert!(sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn rebuilding_one_scope_does_not_move_another_scopes_source_routes() {
        let pool = foreign_key_pool().await;
        MIGRATOR.run(&pool).await.unwrap();
        let service = MediaStorageService::new(pool.clone());
        let primary = create_test_server_with_url(&pool, "http://scope-a.example:8096").await;
        let sibling = create_test_server_with_url(&pool, "http://scope-b.example:8096").await;
        let primary_mapping = service
            .get_or_create_media_mapping("movie-primary", &primary)
            .await
            .unwrap();
        let sibling_mapping = service
            .get_or_create_media_mapping("movie-sibling", &sibling)
            .await
            .unwrap();
        let source_mapping = service
            .get_or_create_media_mapping("source-sibling", &sibling)
            .await
            .unwrap();
        let alias = MovieAlias {
            provider: MovieProvider::Tmdb,
            provider_id: "42".to_string(),
        };
        let snapshots = || {
            vec![
                MovieCatalogSnapshot {
                    source_key: "primary:library".to_string(),
                    server_id: primary.id,
                    complete: true,
                    observations: vec![MovieObservation {
                        virtual_media_id: primary_mapping.virtual_media_id.clone(),
                        aliases: BTreeSet::from([alias.clone()]),
                    }],
                },
                MovieCatalogSnapshot {
                    source_key: "sibling:library".to_string(),
                    server_id: sibling.id,
                    complete: true,
                    observations: vec![MovieObservation {
                        virtual_media_id: sibling_mapping.virtual_media_id.clone(),
                        aliases: BTreeSet::from([alias.clone()]),
                    }],
                },
            ]
        };
        let mut groups = Vec::new();
        for scope in ["configured:library:user-a", "configured:library:user-b"] {
            let generation = service.begin_movie_reconciliation().await.unwrap();
            let assignments = service
                .reconcile_movie_catalog(scope, generation, &snapshots(), true)
                .await
                .unwrap();
            let group = service
                .get_movie_version_group(
                    &assignments[&sibling_mapping.virtual_media_id].virtual_media_id,
                )
                .await
                .unwrap()
                .unwrap();
            let source_generation = service.begin_movie_reconciliation().await.unwrap();
            assert!(service
                .replace_movie_version_sources(
                    group.id,
                    source_generation,
                    &[sibling_mapping.id],
                    &[MovieVersionSourceObservation {
                        member_mapping_id: sibling_mapping.id,
                        source_virtual_id: source_mapping.virtual_media_id.clone(),
                    }],
                )
                .await
                .unwrap());
            groups.push(group);
        }

        let split_generation = service.begin_movie_reconciliation().await.unwrap();
        let mut split_snapshots = snapshots();
        split_snapshots[1].observations[0].aliases = BTreeSet::from([MovieAlias {
            provider: MovieProvider::Tmdb,
            provider_id: "99".to_string(),
        }]);
        service
            .reconcile_movie_catalog(
                "configured:library:user-a",
                split_generation,
                &split_snapshots,
                true,
            )
            .await
            .unwrap();

        assert!(service
            .get_movie_version_source_route(groups[1].id, &source_mapping.virtual_media_id)
            .await
            .unwrap()
            .is_some());
        assert!(sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap()
            .is_empty());
    }
}
