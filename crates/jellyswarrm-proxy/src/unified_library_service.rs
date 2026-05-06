use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use jellyfin_api::{
    ClientInfo, JellyfinClient,
    models::{BaseItem, IncludeBaseItemFields, IncludeItemTypes},
};
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use tokio::task::JoinSet;
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::media_storage_service::MediaStorageService;
use crate::models::enums::CollectionType;
use crate::server_storage::ServerStorageService;
use crate::user_authorization_service::UserAuthorizationService;

// ─── Public domain types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupMode {
    Auto,
    Manual,
}

impl std::fmt::Display for GroupMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupMode::Auto => write!(f, "auto"),
            GroupMode::Manual => write!(f, "manual"),
        }
    }
}

impl TryFrom<String> for GroupMode {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        match s.as_str() {
            "auto" => Ok(GroupMode::Auto),
            "manual" => Ok(GroupMode::Manual),
            other => Err(anyhow::anyhow!("invalid group mode: {}", other)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedLibraryGroup {
    pub id: i64,
    pub name: String,
    pub library_type: CollectionType,
    pub virtual_id: String,
    pub mode: GroupMode,
    pub global_tag_filter: Option<Vec<String>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedLibrarySource {
    pub id: i64,
    pub group_id: i64,
    pub server_id: i64,
    pub jellyfin_library_id: String,
    pub jellyfin_library_name: String,
    pub tag_filter: Option<Vec<String>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedLibrary {
    pub id: i64,
    pub server_id: i64,
    pub jellyfin_library_id: String,
    pub jellyfin_library_name: String,
    pub collection_type: String,
    pub cached_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedLibraryAggregation {
    pub items: Vec<BaseItem>,
    pub total_count: i32,
    pub offline_servers: Vec<String>,
    pub unmatched_count: i32,
}

// ─── Internal types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DeduplicationKey {
    Tmdb(String),
    Imdb(String),
}

#[derive(FromRow)]
struct GroupRow {
    id: i64,
    name: String,
    library_type: String,
    virtual_id: String,
    mode: String,
    global_tag_filter: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl TryFrom<GroupRow> for UnifiedLibraryGroup {
    type Error = anyhow::Error;
    fn try_from(row: GroupRow) -> Result<Self, Self::Error> {
        let library_type: CollectionType =
            serde_json::from_value(serde_json::Value::String(row.library_type))
                .context("invalid library_type in DB")?;
        let mode = GroupMode::try_from(row.mode)?;
        let global_tag_filter = row
            .global_tag_filter
            .map(|s| serde_json::from_str::<Vec<String>>(&s))
            .transpose()
            .context("invalid global_tag_filter JSON")?;
        Ok(Self {
            id: row.id,
            name: row.name,
            library_type,
            virtual_id: row.virtual_id,
            mode,
            global_tag_filter,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(FromRow)]
struct SourceRow {
    id: i64,
    group_id: i64,
    server_id: i64,
    jellyfin_library_id: String,
    jellyfin_library_name: String,
    tag_filter: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl TryFrom<SourceRow> for UnifiedLibrarySource {
    type Error = anyhow::Error;
    fn try_from(row: SourceRow) -> Result<Self, Self::Error> {
        let tag_filter = row
            .tag_filter
            .map(|s| serde_json::from_str::<Vec<String>>(&s))
            .transpose()
            .context("invalid tag_filter JSON")?;
        Ok(Self {
            id: row.id,
            group_id: row.group_id,
            server_id: row.server_id,
            jellyfin_library_id: row.jellyfin_library_id,
            jellyfin_library_name: row.jellyfin_library_name,
            tag_filter,
            created_at: row.created_at,
        })
    }
}

#[derive(FromRow)]
struct CachedLibraryRow {
    id: i64,
    server_id: i64,
    jellyfin_library_id: String,
    jellyfin_library_name: String,
    collection_type: String,
    cached_at: chrono::DateTime<chrono::Utc>,
}

impl From<CachedLibraryRow> for CachedLibrary {
    fn from(row: CachedLibraryRow) -> Self {
        Self {
            id: row.id,
            server_id: row.server_id,
            jellyfin_library_id: row.jellyfin_library_id,
            jellyfin_library_name: row.jellyfin_library_name,
            collection_type: row.collection_type,
            cached_at: row.cached_at,
        }
    }
}

// ─── Service ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct UnifiedLibraryService {
    pool: SqlitePool,
    server_storage: Arc<ServerStorageService>,
    user_authorization: Arc<UserAuthorizationService>,
    media_storage: Arc<MediaStorageService>,
    http_client: reqwest::Client,
    aggregation_cache: Cache<String, (Vec<BaseItem>, Vec<String>)>,
}

impl UnifiedLibraryService {
    pub fn new(
        pool: SqlitePool,
        server_storage: Arc<ServerStorageService>,
        user_authorization: Arc<UserAuthorizationService>,
        media_storage: Arc<MediaStorageService>,
        http_client: reqwest::Client,
    ) -> Self {
        Self {
            pool,
            server_storage,
            user_authorization,
            media_storage,
            http_client,
            aggregation_cache: Cache::builder()
                .time_to_live(Duration::from_secs(60 * 30))
                .max_capacity(1_000)
                .build(),
        }
    }

    // ─── GroupStore ───────────────────────────────────────────────────────────

    pub async fn create_group(
        &self,
        name: &str,
        library_type: CollectionType,
        mode: GroupMode,
    ) -> Result<UnifiedLibraryGroup, anyhow::Error> {
        let virtual_id = Uuid::new_v4().to_string();
        let library_type_str = collection_type_str(&library_type);
        let mode_str = mode.to_string();

        let row: GroupRow = sqlx::query_as(
            r#"
            INSERT INTO unified_library_groups (name, library_type, virtual_id, mode)
            VALUES (?, ?, ?, ?)
            RETURNING id, name, library_type, virtual_id, mode, global_tag_filter, created_at, updated_at
            "#,
        )
        .bind(name)
        .bind(&library_type_str)
        .bind(&virtual_id)
        .bind(&mode_str)
        .fetch_one(&self.pool)
        .await
        .context("create_group")?;

        UnifiedLibraryGroup::try_from(row)
    }

    pub async fn set_group_mode(
        &self,
        group_id: i64,
        mode: GroupMode,
    ) -> Result<bool, anyhow::Error> {
        let result = sqlx::query(
            "UPDATE unified_library_groups SET mode = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(mode.to_string())
        .bind(group_id)
        .execute(&self.pool)
        .await
        .context("set_group_mode")?;

        if result.rows_affected() > 0 {
            self.aggregation_cache.invalidate_all();
        }
        Ok(result.rows_affected() > 0)
    }

    pub async fn set_global_tag_filter(
        &self,
        group_id: i64,
        tags: Option<Vec<String>>,
    ) -> Result<bool, anyhow::Error> {
        let tags_json = tags
            .as_deref()
            .map(serde_json::to_string)
            .transpose()
            .context("serialize global_tag_filter")?;

        let result = sqlx::query(
            "UPDATE unified_library_groups SET global_tag_filter = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(tags_json)
        .bind(group_id)
        .execute(&self.pool)
        .await
        .context("set_global_tag_filter")?;

        if result.rows_affected() > 0 {
            self.aggregation_cache.invalidate_all();
        }
        Ok(result.rows_affected() > 0)
    }

    pub async fn list_groups(&self) -> Result<Vec<UnifiedLibraryGroup>, anyhow::Error> {
        let rows: Vec<GroupRow> = sqlx::query_as(
            "SELECT id, name, library_type, virtual_id, mode, global_tag_filter, created_at, updated_at \
             FROM unified_library_groups ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await
        .context("list_groups")?;

        rows.into_iter().map(UnifiedLibraryGroup::try_from).collect()
    }

    pub async fn get_group_by_id(
        &self,
        id: i64,
    ) -> Result<Option<UnifiedLibraryGroup>, anyhow::Error> {
        let row: Option<GroupRow> = sqlx::query_as(
            "SELECT id, name, library_type, virtual_id, mode, global_tag_filter, created_at, updated_at \
             FROM unified_library_groups WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .context("get_group_by_id")?;

        row.map(UnifiedLibraryGroup::try_from).transpose()
    }

    pub async fn get_group_by_virtual_id(
        &self,
        virtual_id: &str,
    ) -> Result<Option<UnifiedLibraryGroup>, anyhow::Error> {
        let row: Option<GroupRow> = sqlx::query_as(
            "SELECT id, name, library_type, virtual_id, mode, global_tag_filter, created_at, updated_at \
             FROM unified_library_groups WHERE virtual_id = ?",
        )
        .bind(virtual_id)
        .fetch_optional(&self.pool)
        .await
        .context("get_group_by_virtual_id")?;

        row.map(UnifiedLibraryGroup::try_from).transpose()
    }

    pub async fn delete_group(&self, id: i64) -> Result<bool, anyhow::Error> {
        let result = sqlx::query("DELETE FROM unified_library_groups WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .context("delete_group")?;

        if result.rows_affected() > 0 {
            self.aggregation_cache.invalidate_all();
        }

        Ok(result.rows_affected() > 0)
    }

    // ─── SourceStore ──────────────────────────────────────────────────────────

    pub async fn add_source(
        &self,
        group_id: i64,
        server_id: i64,
        jellyfin_library_id: &str,
        jellyfin_library_name: &str,
        tag_filter: Option<Vec<String>>,
    ) -> Result<UnifiedLibrarySource, anyhow::Error> {
        let tag_filter_json = tag_filter
            .as_deref()
            .map(serde_json::to_string)
            .transpose()
            .context("serialize tag_filter")?;

        let row: SourceRow = sqlx::query_as(
            r#"
            INSERT INTO unified_library_sources
                (group_id, server_id, jellyfin_library_id, jellyfin_library_name, tag_filter)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(group_id, server_id, jellyfin_library_id)
            DO UPDATE SET jellyfin_library_name = excluded.jellyfin_library_name,
                          tag_filter             = excluded.tag_filter
            RETURNING id, group_id, server_id, jellyfin_library_id, jellyfin_library_name, tag_filter, created_at
            "#,
        )
        .bind(group_id)
        .bind(server_id)
        .bind(jellyfin_library_id)
        .bind(jellyfin_library_name)
        .bind(tag_filter_json)
        .fetch_one(&self.pool)
        .await
        .context("add_source")?;

        self.aggregation_cache.invalidate_all();
        UnifiedLibrarySource::try_from(row)
    }

    pub async fn remove_source(&self, source_id: i64) -> Result<bool, anyhow::Error> {
        let result = sqlx::query("DELETE FROM unified_library_sources WHERE id = ?")
            .bind(source_id)
            .execute(&self.pool)
            .await
            .context("remove_source")?;

        if result.rows_affected() > 0 {
            self.aggregation_cache.invalidate_all();
        }
        Ok(result.rows_affected() > 0)
    }

    pub async fn list_sources_for_group(
        &self,
        group_id: i64,
    ) -> Result<Vec<UnifiedLibrarySource>, anyhow::Error> {
        let rows: Vec<SourceRow> = sqlx::query_as(
            "SELECT id, group_id, server_id, jellyfin_library_id, jellyfin_library_name, tag_filter, created_at \
             FROM unified_library_sources WHERE group_id = ? ORDER BY server_id, jellyfin_library_name",
        )
        .bind(group_id)
        .fetch_all(&self.pool)
        .await
        .context("list_sources_for_group")?;

        rows.into_iter()
            .map(UnifiedLibrarySource::try_from)
            .collect()
    }

    // ─── LibraryCache ─────────────────────────────────────────────────────────

    pub async fn refresh_library_cache(
        &self,
        server: &crate::server_storage::Server,
        session: &crate::user_authorization_service::AuthorizationSession,
    ) -> Result<usize, anyhow::Error> {
        let client = JellyfinClient::new_with_client(
            server.url.as_str(),
            ClientInfo::default(),
            self.http_client.clone(),
        )
        .context("build JellyfinClient for cache refresh")?;
        client.with_token(session.jellyfin_token.clone()).await;

        let folders = client
            .get_media_folders(Some(&session.original_user_id))
            .await
            .context("get_media_folders")?;

        sqlx::query("DELETE FROM server_library_cache WHERE server_id = ?")
            .bind(server.id)
            .execute(&self.pool)
            .await
            .context("clear library cache")?;

        let count = folders.len();
        for folder in &folders {
            let ct = folder.collection_type.as_deref().unwrap_or("");
            sqlx::query(
                r#"
                INSERT INTO server_library_cache
                    (server_id, jellyfin_library_id, jellyfin_library_name, collection_type, cached_at)
                VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP)
                ON CONFLICT(server_id, jellyfin_library_id)
                DO UPDATE SET jellyfin_library_name = excluded.jellyfin_library_name,
                              collection_type        = excluded.collection_type,
                              cached_at              = CURRENT_TIMESTAMP
                "#,
            )
            .bind(server.id)
            .bind(&folder.id)
            .bind(&folder.name)
            .bind(ct)
            .execute(&self.pool)
            .await
            .context("upsert library cache entry")?;
        }

        Ok(count)
    }

    pub async fn refresh_library_cache_with_token(
        &self,
        server: &crate::server_storage::Server,
        token: &str,
    ) -> Result<usize, anyhow::Error> {
        let client = JellyfinClient::new_with_client(
            server.url.as_str(),
            ClientInfo::default(),
            self.http_client.clone(),
        )
        .context("build JellyfinClient for cache refresh")?;
        client.with_token(token.to_string()).await;

        let folders = client
            .get_media_folders(None)
            .await
            .context("get_media_folders")?;

        sqlx::query("DELETE FROM server_library_cache WHERE server_id = ?")
            .bind(server.id)
            .execute(&self.pool)
            .await
            .context("clear library cache")?;

        let count = folders.len();
        for folder in &folders {
            let ct = folder.collection_type.as_deref().unwrap_or("");
            sqlx::query(
                r#"
                INSERT INTO server_library_cache
                    (server_id, jellyfin_library_id, jellyfin_library_name, collection_type, cached_at)
                VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP)
                ON CONFLICT(server_id, jellyfin_library_id)
                DO UPDATE SET jellyfin_library_name = excluded.jellyfin_library_name,
                              collection_type        = excluded.collection_type,
                              cached_at              = CURRENT_TIMESTAMP
                "#,
            )
            .bind(server.id)
            .bind(&folder.id)
            .bind(&folder.name)
            .bind(ct)
            .execute(&self.pool)
            .await
            .context("upsert library cache entry")?;
        }

        Ok(count)
    }

    pub async fn get_cached_libraries(
        &self,
        server_id: i64,
        collection_type: Option<&CollectionType>,
    ) -> Result<Vec<CachedLibrary>, anyhow::Error> {
        let rows: Vec<CachedLibraryRow> = if let Some(ct) = collection_type {
            let ct_str = collection_type_str(ct);
            sqlx::query_as(
                "SELECT id, server_id, jellyfin_library_id, jellyfin_library_name, collection_type, cached_at \
                 FROM server_library_cache WHERE server_id = ? AND collection_type = ? ORDER BY jellyfin_library_name",
            )
            .bind(server_id)
            .bind(ct_str)
            .fetch_all(&self.pool)
            .await
            .context("get_cached_libraries (filtered)")?
        } else {
            sqlx::query_as(
                "SELECT id, server_id, jellyfin_library_id, jellyfin_library_name, collection_type, cached_at \
                 FROM server_library_cache WHERE server_id = ? ORDER BY jellyfin_library_name",
            )
            .bind(server_id)
            .fetch_all(&self.pool)
            .await
            .context("get_cached_libraries (all)")?
        };

        Ok(rows.into_iter().map(CachedLibrary::from).collect())
    }

    // ─── ViewsSupport ─────────────────────────────────────────────────────────

    pub async fn get_virtual_library_stubs(
        &self,
    ) -> Result<Vec<BaseItem>, anyhow::Error> {
        let groups = self.list_groups().await?;
        let stubs = groups
            .into_iter()
            .map(|g| {
                let mut extra = HashMap::new();
                extra.insert(
                    "CollectionType".to_string(),
                    serde_json::Value::String(collection_type_str(&g.library_type)),
                );
                extra.insert("IsFolder".to_string(), serde_json::Value::Bool(true));
                BaseItem {
                    name: g.name,
                    id: g.virtual_id,
                    type_: "CollectionFolder".to_string(),
                    image_tags: None,
                    production_year: None,
                    run_time_ticks: None,
                    community_rating: None,
                    extra,
                }
            })
            .collect();
        Ok(stubs)
    }

    pub async fn get_covered_collection_types(
        &self,
    ) -> Result<Vec<CollectionType>, anyhow::Error> {
        let groups = self.list_groups().await?;
        let mut seen = std::collections::HashSet::new();
        let unique: Vec<CollectionType> = groups
            .into_iter()
            .filter_map(|g| {
                let s = collection_type_str(&g.library_type);
                if seen.insert(s) { Some(g.library_type) } else { None }
            })
            .collect();
        Ok(unique)
    }

    // ─── Aggregation ──────────────────────────────────────────────────────────

    pub async fn get_aggregated_items(
        &self,
        group: &UnifiedLibraryGroup,
        user_id: &str,
        start_index: usize,
        limit: Option<usize>,
    ) -> Result<UnifiedLibraryAggregation, anyhow::Error> {
        let cache_key = format!("{}|{}", group.id, user_id);

        if let Some((cached_items, offline_servers)) =
            self.aggregation_cache.get(&cache_key).await
        {
            let total_count = cached_items.len() as i32;
            let page = match limit {
                Some(n) => apply_pagination(&cached_items, start_index, n),
                None => cached_items[start_index.min(cached_items.len())..].to_vec(),
            };
            return Ok(UnifiedLibraryAggregation {
                items: page,
                total_count,
                offline_servers,
                unmatched_count: 0,
            });
        }

        let sessions = self
            .user_authorization
            .get_user_sessions(user_id, None)
            .await
            .context("get_user_sessions")?;

        if sessions.is_empty() {
            return Ok(UnifiedLibraryAggregation {
                items: Vec::new(),
                total_count: 0,
                offline_servers: Vec::new(),
                unmatched_count: 0,
            });
        }

        let fields = vec![
            IncludeBaseItemFields::ProviderIds,
            IncludeBaseItemFields::MediaSources,
            IncludeBaseItemFields::SortName,
            IncludeBaseItemFields::PrimaryImageAspectRatio,
            IncludeBaseItemFields::Tags,
        ];

        let mut all_candidates: Vec<(i32, i64, BaseItem)> = Vec::new();
        let mut offline_servers: Vec<String> = Vec::new();

        match group.mode {
            GroupMode::Auto => {
                let mut join_set: JoinSet<Result<(Vec<BaseItem>, i32, i64), String>> =
                    JoinSet::new();

                for (session, server) in sessions {
                    let library_type = group.library_type.clone();
                    let http_client = self.http_client.clone();
                    let fields_clone = fields.clone();
                    let media_storage = self.media_storage.clone();

                    join_set.spawn(async move {
                        fetch_items_from_server(
                            &server,
                            &session,
                            &library_type,
                            None,
                            &http_client,
                            fields_clone,
                            &media_storage,
                        )
                        .await
                    });
                }

                let global_filter = group.global_tag_filter.as_deref();

                while let Some(result) = join_set.join_next().await {
                    match result {
                        Ok(Ok((items, priority, server_id))) => {
                            let filtered = apply_tag_filter(items, global_filter);
                            debug!(
                                "Auto: server_id={} priority={} after_filter={}",
                                server_id,
                                priority,
                                filtered.len()
                            );
                            for item in filtered {
                                all_candidates.push((priority, server_id, item));
                            }
                        }
                        Ok(Err(server_name)) => {
                            warn!("Server {} is offline or returned error", server_name);
                            offline_servers.push(server_name);
                        }
                        Err(e) => {
                            error!("JoinSet task panicked: {}", e);
                        }
                    }
                }
            }

            GroupMode::Manual => {
                let sources = self
                    .list_sources_for_group(group.id)
                    .await
                    .context("list_sources_for_group")?;

                let session_map: HashMap<
                    i64,
                    (
                        crate::user_authorization_service::AuthorizationSession,
                        crate::server_storage::Server,
                    ),
                > = sessions
                    .into_iter()
                    .map(|(s, srv)| (srv.id, (s, srv)))
                    .collect();

                for source in sources {
                    let Some((session, server)) = session_map.get(&source.server_id) else {
                        debug!(
                            "Manual: no session for server_id={}, skipping source {}",
                            source.server_id, source.jellyfin_library_id
                        );
                        continue;
                    };

                    let result = fetch_items_from_server(
                        server,
                        session,
                        &group.library_type,
                        Some(&source.jellyfin_library_id),
                        &self.http_client,
                        fields.clone(),
                        &self.media_storage,
                    )
                    .await;

                    match result {
                        Ok((items, priority, server_id)) => {
                            let filtered =
                                apply_tag_filter(items, source.tag_filter.as_deref());
                            debug!(
                                "Manual: source={} server_id={} after_filter={}",
                                source.jellyfin_library_id,
                                server_id,
                                filtered.len()
                            );
                            for item in filtered {
                                all_candidates.push((priority, server_id, item));
                            }
                        }
                        Err(server_name) => {
                            warn!("Server {} is offline or returned error", server_name);
                            offline_servers.push(server_name);
                        }
                    }
                }
            }
        }

        debug!("Total candidates before dedup: {}", all_candidates.len());
        let (mut merged_items, unmatched_count) = dedup_and_merge(all_candidates);
        debug!(
            "After dedup: {} items ({} unmatched/no-provider-id)",
            merged_items.len(),
            unmatched_count
        );
        sort_items(&mut merged_items);

        self.aggregation_cache
            .insert(cache_key, (merged_items.clone(), offline_servers.clone()))
            .await;

        let total_count = merged_items.len() as i32;
        let page = match limit {
            Some(n) => apply_pagination(&merged_items, start_index, n),
            None => merged_items[start_index.min(merged_items.len())..].to_vec(),
        };

        Ok(UnifiedLibraryAggregation {
            items: page,
            total_count,
            offline_servers,
            unmatched_count,
        })
    }
}

// ─── Module-private: server fetcher ──────────────────────────────────────────

async fn fetch_items_from_server(
    server: &crate::server_storage::Server,
    session: &crate::user_authorization_service::AuthorizationSession,
    library_type: &CollectionType,
    parent_id: Option<&str>,
    http_client: &reqwest::Client,
    fields: Vec<IncludeBaseItemFields>,
    media_storage: &MediaStorageService,
) -> Result<(Vec<BaseItem>, i32, i64), String> {
    let server_name = server.name.clone();
    let server_url = server.url.as_str().to_string();

    let client = JellyfinClient::new_with_client(
        &server_url,
        ClientInfo::default(),
        http_client.clone(),
    )
    .map_err(|_| server_name.clone())?;

    client.with_token(session.jellyfin_token.clone()).await;

    let item_types = collection_type_to_item_types(library_type);
    if item_types.is_none() {
        return Ok((Vec::new(), server.priority, server.id));
    }

    const PAGE_SIZE: i32 = 1_000;
    let mut all_raw_items: Vec<BaseItem> = Vec::new();
    let mut page_start = 0i32;
    loop {
        let resp = client
            .get_items(
                &session.original_user_id,
                parent_id,
                true,
                item_types.clone(),
                Some(PAGE_SIZE),
                Some(page_start),
                None,
                None,
                Some(fields.clone()),
            )
            .await
            .map_err(|_| server_name.clone())?;

        let fetched = resp.items.len() as i32;
        debug!(
            "[{}] page start={} fetched={} total={}",
            server_name, page_start, fetched, resp.total_record_count
        );
        all_raw_items.extend(resp.items);
        page_start += fetched;

        if fetched == 0 || page_start >= resp.total_record_count {
            break;
        }
    }
    debug!("[{}] total raw items fetched: {}", server_name, all_raw_items.len());

    let mut virtualized = Vec::with_capacity(all_raw_items.len());
    for mut item in all_raw_items {
        item.id = media_storage
            .get_or_create_media_mapping(&item.id, &server_url)
            .await
            .map(|m| m.virtual_media_id)
            .map_err(|_| server_name.clone())?;

        if let Some(tags) = item.image_tags.take() {
            let mut virtual_tags = std::collections::HashMap::new();
            for (tag_type, tag_id) in tags {
                let vtag = media_storage
                    .get_or_create_media_mapping(&tag_id, &server_url)
                    .await
                    .map(|m| m.virtual_media_id)
                    .map_err(|_| server_name.clone())?;
                virtual_tags.insert(tag_type, vtag);
            }
            item.image_tags = Some(virtual_tags);
        }

        if let Some(sources) = item.extra.get_mut("MediaSources") {
            if let Some(arr) = sources.as_array_mut() {
                for source in arr.iter_mut() {
                    if let Some(id) = source.get("Id").and_then(|v| v.as_str()).map(String::from) {
                        let vid = media_storage
                            .get_or_create_media_mapping(&id, &server_url)
                            .await
                            .map(|m| m.virtual_media_id)
                            .map_err(|_| server_name.clone())?;
                        source["Id"] = serde_json::Value::String(vid);
                    }
                }
            }
        }

        virtualized.push(item);
    }

    Ok((virtualized, server.priority, server.id))
}

// ─── Module-private: pure business logic ─────────────────────────────────────

fn apply_tag_filter(items: Vec<BaseItem>, tags: Option<&[String]>) -> Vec<BaseItem> {
    let Some(tags) = tags else { return items; };
    if tags.is_empty() {
        return items;
    }
    let lower_tags: Vec<String> = tags.iter().map(|t| t.to_lowercase()).collect();
    items
        .into_iter()
        .filter(|item| {
            let item_tags: Vec<String> = item
                .extra
                .get("Tags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                        .collect()
                })
                .unwrap_or_default();
            lower_tags.iter().any(|t| item_tags.contains(t))
        })
        .collect()
}

fn collection_type_to_item_types(ct: &CollectionType) -> Option<Vec<IncludeItemTypes>> {
    match ct {
        CollectionType::Movies => Some(vec![IncludeItemTypes::Movie]),
        CollectionType::TvShows => Some(vec![IncludeItemTypes::Series]),
        CollectionType::Music => Some(vec![IncludeItemTypes::MusicAlbum]),
        CollectionType::MusicVideos => Some(vec![IncludeItemTypes::MusicVideo]),
        CollectionType::Books => Some(vec![IncludeItemTypes::Book]),
        CollectionType::Photos => Some(vec![IncludeItemTypes::Photo]),
        CollectionType::BoxSets => Some(vec![IncludeItemTypes::BoxSet]),
        CollectionType::Trailers => Some(vec![IncludeItemTypes::Trailer]),
        CollectionType::Playlists => Some(vec![IncludeItemTypes::Playlist]),
        _ => None,
    }
}

fn collection_type_str(ct: &CollectionType) -> String {
    serde_json::to_value(ct)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "unknown".to_string())
}

fn extract_dedup_key(provider_ids: &Option<serde_json::Value>) -> Option<DeduplicationKey> {
    let obj = provider_ids.as_ref()?.as_object()?;
    if let Some(id) = obj.get("Tmdb").and_then(|v| v.as_str()) {
        return Some(DeduplicationKey::Tmdb(id.to_string()));
    }
    if let Some(id) = obj.get("Imdb").and_then(|v| v.as_str()) {
        return Some(DeduplicationKey::Imdb(id.to_string()));
    }
    None
}

fn dedup_and_merge(all_candidates: Vec<(i32, i64, BaseItem)>) -> (Vec<BaseItem>, i32) {
    let mut matched: HashMap<DeduplicationKey, Vec<(i32, i64, BaseItem)>> = HashMap::new();
    let mut unmatched: Vec<BaseItem> = Vec::new();

    for (priority, server_id, item) in all_candidates {
        let provider_ids = item.extra.get("ProviderIds").cloned();
        match extract_dedup_key(&provider_ids) {
            Some(key) => matched.entry(key).or_default().push((priority, server_id, item)),
            None => unmatched.push(item),
        }
    }

    let unmatched_count = unmatched.len() as i32;
    let mut result = unmatched;

    for (_, mut candidates) in matched {
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        result.push(merge_items(candidates));
    }

    (result, unmatched_count)
}

fn merge_items(mut candidates: Vec<(i32, i64, BaseItem)>) -> BaseItem {
    if candidates.len() == 1 {
        return candidates.remove(0).2;
    }

    let (_, _, mut base) = candidates.remove(0);

    let mut seen: std::collections::HashSet<(i64, i64)> = std::collections::HashSet::new();
    let mut merged: Vec<serde_json::Value> = Vec::new();

    let base_sources = base
        .extra
        .get("MediaSources")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    collect_unique_sources(&base_sources, &mut seen, &mut merged);

    for (_, _, item) in &candidates {
        let src = item
            .extra
            .get("MediaSources")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        collect_unique_sources(&src, &mut seen, &mut merged);
    }

    if !merged.is_empty() {
        base.extra
            .insert("MediaSources".to_string(), serde_json::Value::Array(merged));
    }

    base
}

fn collect_unique_sources(
    sources: &serde_json::Value,
    seen: &mut std::collections::HashSet<(i64, i64)>,
    out: &mut Vec<serde_json::Value>,
) {
    if let Some(arr) = sources.as_array() {
        for source in arr {
            let w = source["Width"].as_i64().unwrap_or(0);
            let h = source["Height"].as_i64().unwrap_or(0);
            if seen.insert((w, h)) {
                out.push(source.clone());
            }
        }
    }
}

fn sort_items(items: &mut Vec<BaseItem>) {
    items.sort_by(|a, b| {
        let a_sort = a
            .extra
            .get("SortName")
            .and_then(|v| v.as_str())
            .unwrap_or(&a.name);
        let b_sort = b
            .extra
            .get("SortName")
            .and_then(|v| v.as_str())
            .unwrap_or(&b.name);
        a_sort.to_lowercase().cmp(&b_sort.to_lowercase())
    });
}

fn apply_pagination(items: &[BaseItem], start_index: usize, limit: usize) -> Vec<BaseItem> {
    if limit == 0 {
        return Vec::new();
    }
    let start = start_index.min(items.len());
    let end = (start + limit).min(items.len());
    items[start..end].to_vec()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    use crate::config::MIGRATOR;

    // ── Helpers ───────────────────────────────────────────────────────────────

    async fn make_service() -> UnifiedLibraryService {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();

        let server_storage = Arc::new(crate::server_storage::ServerStorageService::new(pool.clone()));
        let user_auth = Arc::new(
            crate::user_authorization_service::UserAuthorizationService::new(pool.clone()),
        );
        let media_storage = Arc::new(crate::media_storage_service::MediaStorageService::new(pool.clone()));
        let http_client = reqwest::Client::new();

        UnifiedLibraryService::new(pool, server_storage, user_auth, media_storage, http_client)
    }

    fn make_item(id: &str, name: &str, extra: HashMap<String, serde_json::Value>) -> BaseItem {
        BaseItem {
            id: id.to_string(),
            name: name.to_string(),
            type_: "Movie".to_string(),
            image_tags: None,
            production_year: None,
            run_time_ticks: None,
            community_rating: None,
            extra,
        }
    }

    fn make_item_with_tags(id: &str, tags: &[&str]) -> BaseItem {
        let mut extra = HashMap::new();
        extra.insert(
            "Tags".to_string(),
            serde_json::json!(tags),
        );
        make_item(id, id, extra)
    }

    fn tmdb_provider(id: &str) -> serde_json::Value {
        serde_json::json!({"Tmdb": id})
    }

    fn imdb_provider(id: &str) -> serde_json::Value {
        serde_json::json!({"Imdb": id})
    }

    fn make_source(w: i64, h: i64) -> serde_json::Value {
        serde_json::json!({"Width": w, "Height": h})
    }

    // ── GroupStore CRUD ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_create_and_list_groups() {
        let svc = make_service().await;

        let group = svc
            .create_group("Unified Movies", CollectionType::Movies, GroupMode::Auto)
            .await
            .unwrap();

        assert_eq!(group.name, "Unified Movies");
        assert!(matches!(group.library_type, CollectionType::Movies));
        assert!(matches!(group.mode, GroupMode::Auto));
        assert!(!group.virtual_id.is_empty());
        assert!(group.global_tag_filter.is_none());

        let groups = svc.list_groups().await.unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "Unified Movies");
    }

    #[tokio::test]
    async fn test_create_manual_group() {
        let svc = make_service().await;

        let group = svc
            .create_group("Adult Movies", CollectionType::Movies, GroupMode::Manual)
            .await
            .unwrap();

        assert!(matches!(group.mode, GroupMode::Manual));
    }

    #[tokio::test]
    async fn test_set_group_mode() {
        let svc = make_service().await;
        let group = svc
            .create_group("Test", CollectionType::Movies, GroupMode::Auto)
            .await
            .unwrap();

        let ok = svc.set_group_mode(group.id, GroupMode::Manual).await.unwrap();
        assert!(ok);

        let updated = svc.get_group_by_id(group.id).await.unwrap().unwrap();
        assert!(matches!(updated.mode, GroupMode::Manual));

        let not_found = svc.set_group_mode(9999, GroupMode::Auto).await.unwrap();
        assert!(!not_found);
    }

    #[tokio::test]
    async fn test_set_global_tag_filter() {
        let svc = make_service().await;
        let group = svc
            .create_group("Test", CollectionType::Movies, GroupMode::Auto)
            .await
            .unwrap();

        let ok = svc
            .set_global_tag_filter(group.id, Some(vec!["adult".to_string(), "kids".to_string()]))
            .await
            .unwrap();
        assert!(ok);

        let updated = svc.get_group_by_id(group.id).await.unwrap().unwrap();
        let filter = updated.global_tag_filter.unwrap();
        assert_eq!(filter.len(), 2);
        assert!(filter.contains(&"adult".to_string()));

        let ok2 = svc.set_global_tag_filter(group.id, None).await.unwrap();
        assert!(ok2);
        let cleared = svc.get_group_by_id(group.id).await.unwrap().unwrap();
        assert!(cleared.global_tag_filter.is_none());
    }

    #[tokio::test]
    async fn test_get_group_by_id() {
        let svc = make_service().await;
        let created = svc
            .create_group("TV Shows", CollectionType::TvShows, GroupMode::Auto)
            .await
            .unwrap();

        let found = svc.get_group_by_id(created.id).await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "TV Shows");

        let missing = svc.get_group_by_id(9999).await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_get_group_by_virtual_id() {
        let svc = make_service().await;
        let created = svc
            .create_group("Music Lib", CollectionType::Music, GroupMode::Auto)
            .await
            .unwrap();

        let found = svc
            .get_group_by_virtual_id(&created.virtual_id)
            .await
            .unwrap();
        assert!(found.is_some());

        let missing = svc
            .get_group_by_virtual_id("00000000-0000-0000-0000-000000000000")
            .await
            .unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_delete_group() {
        let svc = make_service().await;
        let group = svc
            .create_group("To Delete", CollectionType::Movies, GroupMode::Auto)
            .await
            .unwrap();

        let deleted = svc.delete_group(group.id).await.unwrap();
        assert!(deleted);

        let not_found = svc.get_group_by_id(group.id).await.unwrap();
        assert!(not_found.is_none());

        let double_delete = svc.delete_group(group.id).await.unwrap();
        assert!(!double_delete);
    }

    #[tokio::test]
    async fn test_unique_name_constraint() {
        let svc = make_service().await;
        svc.create_group("Dupe", CollectionType::Movies, GroupMode::Auto).await.unwrap();
        let result = svc.create_group("Dupe", CollectionType::TvShows, GroupMode::Auto).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_virtual_library_stubs() {
        let svc = make_service().await;
        svc.create_group("My Movies", CollectionType::Movies, GroupMode::Auto).await.unwrap();
        svc.create_group("My TV", CollectionType::TvShows, GroupMode::Auto).await.unwrap();

        let stubs = svc.get_virtual_library_stubs().await.unwrap();
        assert_eq!(stubs.len(), 2);

        assert_eq!(stubs[0].name, "My Movies");
        assert_eq!(stubs[0].type_, "CollectionFolder");
        assert!(!stubs[0].id.is_empty());
        assert_eq!(stubs[1].name, "My TV");
    }

    #[tokio::test]
    async fn test_get_covered_collection_types() {
        let svc = make_service().await;
        let types = svc.get_covered_collection_types().await.unwrap();
        assert!(types.is_empty());

        svc.create_group("Movies A", CollectionType::Movies, GroupMode::Auto).await.unwrap();
        svc.create_group("Movies B", CollectionType::Movies, GroupMode::Manual).await.unwrap();
        svc.create_group("TV", CollectionType::TvShows, GroupMode::Auto).await.unwrap();

        let types = svc.get_covered_collection_types().await.unwrap();
        assert_eq!(types.len(), 2);
        assert!(types.contains(&CollectionType::Movies));
        assert!(types.contains(&CollectionType::TvShows));
    }

    // ── apply_tag_filter unit tests ───────────────────────────────────────────

    #[test]
    fn test_tag_filter_none_passthrough() {
        let items = vec![make_item_with_tags("1", &["adult"]), make_item("2", "b", HashMap::new())];
        let result = apply_tag_filter(items.clone(), None);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_tag_filter_empty_slice_passthrough() {
        let items = vec![make_item_with_tags("1", &["adult"])];
        let result = apply_tag_filter(items, Some(&[]));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_tag_filter_or_match() {
        let items = vec![
            make_item_with_tags("1", &["adult"]),
            make_item_with_tags("2", &["kids"]),
            make_item("3", "no-tags", HashMap::new()),
        ];
        let filter = vec!["adult".to_string()];
        let result = apply_tag_filter(items, Some(&filter));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "1");
    }

    #[test]
    fn test_tag_filter_case_insensitive() {
        let items = vec![make_item_with_tags("1", &["Adult"])];
        let filter = vec!["adult".to_string()];
        let result = apply_tag_filter(items, Some(&filter));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_tag_filter_no_match_excluded() {
        let items = vec![make_item_with_tags("1", &["kids"])];
        let filter = vec!["adult".to_string()];
        let result = apply_tag_filter(items, Some(&filter));
        assert!(result.is_empty());
    }

    #[test]
    fn test_tag_filter_no_tags_field_excluded() {
        let items = vec![make_item("1", "no-tags", HashMap::new())];
        let filter = vec!["adult".to_string()];
        let result = apply_tag_filter(items, Some(&filter));
        assert!(result.is_empty());
    }

    // ── Pure function unit tests (Session 1, unchanged) ───────────────────────

    #[test]
    fn test_extract_dedup_key_tmdb() {
        let ids = Some(tmdb_provider("123"));
        assert_eq!(
            extract_dedup_key(&ids),
            Some(DeduplicationKey::Tmdb("123".to_string()))
        );
    }

    #[test]
    fn test_extract_dedup_key_imdb_fallback() {
        let ids = Some(imdb_provider("tt456"));
        assert_eq!(
            extract_dedup_key(&ids),
            Some(DeduplicationKey::Imdb("tt456".to_string()))
        );
    }

    #[test]
    fn test_extract_dedup_key_tmdb_preferred_over_imdb() {
        let ids = Some(serde_json::json!({"Tmdb": "1", "Imdb": "tt2"}));
        assert_eq!(
            extract_dedup_key(&ids),
            Some(DeduplicationKey::Tmdb("1".to_string()))
        );
    }

    #[test]
    fn test_extract_dedup_key_no_ids() {
        assert_eq!(extract_dedup_key(&None), None);
        assert_eq!(extract_dedup_key(&Some(serde_json::json!({}))), None);
    }

    #[test]
    fn test_sort_items_ascending() {
        let mut items = vec![
            make_item("1", "Zephyr", {
                let mut m = HashMap::new();
                m.insert("SortName".to_string(), serde_json::json!("zephyr"));
                m
            }),
            make_item("2", "Alpha", {
                let mut m = HashMap::new();
                m.insert("SortName".to_string(), serde_json::json!("alpha"));
                m
            }),
            make_item("3", "Mango", {
                let mut m = HashMap::new();
                m.insert("SortName".to_string(), serde_json::json!("mango"));
                m
            }),
        ];
        sort_items(&mut items);
        assert_eq!(items[0].name, "Alpha");
        assert_eq!(items[1].name, "Mango");
        assert_eq!(items[2].name, "Zephyr");
    }

    #[test]
    fn test_apply_pagination_basic() {
        let items: Vec<BaseItem> = (0..10)
            .map(|i| make_item(&i.to_string(), &i.to_string(), HashMap::new()))
            .collect();
        let page = apply_pagination(&items, 3, 4);
        assert_eq!(page.len(), 4);
        assert_eq!(page[0].id, "3");
        assert_eq!(page[3].id, "6");
    }

    #[test]
    fn test_apply_pagination_past_end() {
        let items: Vec<BaseItem> = (0..5)
            .map(|i| make_item(&i.to_string(), &i.to_string(), HashMap::new()))
            .collect();
        let page = apply_pagination(&items, 3, 10);
        assert_eq!(page.len(), 2);
    }

    #[test]
    fn test_apply_pagination_start_beyond_len() {
        let items: Vec<BaseItem> = (0..3)
            .map(|i| make_item(&i.to_string(), &i.to_string(), HashMap::new()))
            .collect();
        let page = apply_pagination(&items, 10, 5);
        assert_eq!(page.len(), 0);
    }

    #[test]
    fn test_apply_pagination_zero_limit() {
        let items: Vec<BaseItem> = (0..5)
            .map(|i| make_item(&i.to_string(), &i.to_string(), HashMap::new()))
            .collect();
        let page = apply_pagination(&items, 0, 0);
        assert_eq!(page.len(), 0);
    }

    #[test]
    fn test_merge_items_single() {
        let item = make_item("1", "Movie", HashMap::new());
        let result = merge_items(vec![(100, 1, item.clone())]);
        assert_eq!(result.id, item.id);
    }

    #[test]
    fn test_merge_items_same_resolution_uses_highest_priority() {
        let source = serde_json::Value::Array(vec![make_source(1920, 1080)]);
        let high = make_item("1", "High Priority", {
            let mut m = HashMap::new();
            m.insert("MediaSources".to_string(), source.clone());
            m
        });
        let low = make_item("2", "Low Priority", {
            let mut m = HashMap::new();
            m.insert("MediaSources".to_string(), source.clone());
            m
        });
        let result = merge_items(vec![(100, 1, high.clone()), (50, 2, low)]);
        assert_eq!(result.id, "1");
        let sources = result.extra["MediaSources"].as_array().unwrap();
        assert_eq!(sources.len(), 1);
    }

    #[test]
    fn test_merge_items_different_resolutions_merges_sources() {
        let src_1080p = serde_json::Value::Array(vec![make_source(1920, 1080)]);
        let src_4k = serde_json::Value::Array(vec![make_source(3840, 2160)]);
        let item_a = make_item("a", "Server A", {
            let mut m = HashMap::new();
            m.insert("MediaSources".to_string(), src_1080p);
            m
        });
        let item_b = make_item("b", "Server B", {
            let mut m = HashMap::new();
            m.insert("MediaSources".to_string(), src_4k);
            m
        });
        let result = merge_items(vec![(100, 1, item_a), (50, 2, item_b)]);
        let sources = result.extra["MediaSources"].as_array().unwrap();
        assert_eq!(sources.len(), 2);
    }

    // ── Proptest suites ───────────────────────────────────────────────────────

    fn arb_tmdb_id() -> impl Strategy<Value = String> {
        "[0-9]{1,7}".prop_map(String::from)
    }

    fn arb_sort_name() -> impl Strategy<Value = String> {
        "[a-z]{1,20}".prop_map(String::from)
    }

    fn arb_tag() -> impl Strategy<Value = String> {
        "[a-z]{1,10}".prop_map(String::from)
    }

    fn arb_base_item_with_tmdb() -> impl Strategy<Value = BaseItem> {
        (
            "[a-z0-9]{8}".prop_map(String::from),
            "[A-Za-z ]{1,30}".prop_map(String::from),
            arb_tmdb_id(),
            arb_sort_name(),
        )
            .prop_map(|(id, name, tmdb_id, sort_name)| {
                let mut extra = HashMap::new();
                extra.insert("ProviderIds".to_string(), serde_json::json!({"Tmdb": tmdb_id}));
                extra.insert("SortName".to_string(), serde_json::Value::String(sort_name));
                BaseItem {
                    id,
                    name,
                    type_: "Movie".to_string(),
                    image_tags: None,
                    production_year: None,
                    run_time_ticks: None,
                    community_rating: None,
                    extra,
                }
            })
    }

    fn arb_base_item_no_id() -> impl Strategy<Value = BaseItem> {
        (
            "[a-z0-9]{8}".prop_map(String::from),
            "[A-Za-z ]{1,30}".prop_map(String::from),
            arb_sort_name(),
        )
            .prop_map(|(id, name, sort_name)| {
                let mut extra = HashMap::new();
                extra.insert("SortName".to_string(), serde_json::Value::String(sort_name));
                BaseItem {
                    id,
                    name,
                    type_: "Movie".to_string(),
                    image_tags: None,
                    production_year: None,
                    run_time_ticks: None,
                    community_rating: None,
                    extra,
                }
            })
    }

    fn arb_base_item_with_tags(
        tags: Vec<String>,
    ) -> impl Strategy<Value = BaseItem> {
        "[a-z0-9]{8}".prop_map(String::from).prop_map(move |id| {
            let mut extra = HashMap::new();
            extra.insert("Tags".to_string(), serde_json::json!(tags));
            make_item(&id, &id, extra)
        })
    }

    proptest! {
        // ── Session 1 invariants (unchanged) ─────────────────────────────────

        #[test]
        fn prop_dedup_key_deterministic(item in arb_base_item_with_tmdb()) {
            let ids = item.extra.get("ProviderIds").cloned();
            let k1 = extract_dedup_key(&ids);
            let k2 = extract_dedup_key(&ids);
            prop_assert_eq!(k1, k2);
        }

        #[test]
        fn prop_no_id_yields_none(item in arb_base_item_no_id()) {
            let ids = item.extra.get("ProviderIds").cloned();
            prop_assert_eq!(extract_dedup_key(&ids), None);
        }

        #[test]
        fn prop_sort_items_non_decreasing(
            names in proptest::collection::vec(arb_sort_name(), 0..20)
        ) {
            let mut items: Vec<BaseItem> = names
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    let mut extra = HashMap::new();
                    extra.insert("SortName".to_string(), serde_json::Value::String(n.clone()));
                    make_item(&i.to_string(), n, extra)
                })
                .collect();
            sort_items(&mut items);
            for window in items.windows(2) {
                let a = window[0].extra["SortName"].as_str().unwrap_or("").to_lowercase();
                let b = window[1].extra["SortName"].as_str().unwrap_or("").to_lowercase();
                prop_assert!(a <= b);
            }
        }

        #[test]
        fn prop_sort_items_preserves_count(
            names in proptest::collection::vec(arb_sort_name(), 0..20)
        ) {
            let original_len = names.len();
            let mut items: Vec<BaseItem> = names
                .into_iter()
                .enumerate()
                .map(|(i, n)| make_item(&i.to_string(), &n, HashMap::new()))
                .collect();
            sort_items(&mut items);
            prop_assert_eq!(items.len(), original_len);
        }

        #[test]
        fn prop_pagination_length_bounded(
            count in 0usize..50,
            start in 0usize..60,
            limit in 0usize..30,
        ) {
            let items: Vec<BaseItem> = (0..count)
                .map(|i| make_item(&i.to_string(), &i.to_string(), HashMap::new()))
                .collect();
            let page = apply_pagination(&items, start, limit);
            prop_assert!(page.len() <= limit);
        }

        #[test]
        fn prop_pagination_start_correct(
            ids in proptest::collection::vec("[0-9]{1,4}".prop_map(String::from), 1..30),
            limit in 1usize..10,
        ) {
            let items: Vec<BaseItem> = ids
                .iter()
                .map(|id| make_item(id, id, HashMap::new()))
                .collect();
            let start = ids.len() / 2;
            let page = apply_pagination(&items, start, limit);
            if !page.is_empty() {
                prop_assert_eq!(&page[0].id, &ids[start]);
            }
        }

        #[test]
        fn prop_merge_single_item_unchanged(item in arb_base_item_with_tmdb()) {
            let id = item.id.clone();
            let result = merge_items(vec![(100, 1, item)]);
            prop_assert_eq!(result.id, id);
        }

        #[test]
        fn prop_dedup_idempotency(items in proptest::collection::vec(arb_base_item_with_tmdb(), 1..10)) {
            let candidates: Vec<(i32, i64, BaseItem)> = items
                .into_iter()
                .enumerate()
                .map(|(i, item)| (100 - i as i32, i as i64, item))
                .collect();
            let (first_pass, _) = dedup_and_merge(candidates.clone());
            let second_candidates: Vec<(i32, i64, BaseItem)> = first_pass
                .clone()
                .into_iter()
                .enumerate()
                .map(|(i, item)| (100 - i as i32, i as i64, item))
                .collect();
            let (second_pass, _) = dedup_and_merge(second_candidates);
            prop_assert_eq!(first_pass.len(), second_pass.len());
        }

        #[test]
        fn prop_merge_priority_invariant(tmdb_id in arb_tmdb_id()) {
            let high_priority = {
                let mut extra = HashMap::new();
                extra.insert("ProviderIds".to_string(), serde_json::json!({"Tmdb": &tmdb_id}));
                make_item("high", "High", extra)
            };
            let low_priority = {
                let mut extra = HashMap::new();
                extra.insert("ProviderIds".to_string(), serde_json::json!({"Tmdb": &tmdb_id}));
                make_item("low", "Low", extra)
            };
            let result = merge_items(vec![(200, 1, high_priority), (100, 2, low_priority)]);
            prop_assert_eq!(result.id, "high");
        }

        // ── apply_tag_filter PBT invariants (Session 2) ───────────────────────

        #[test]
        fn prop_tag_filter_empty_passthrough(
            tags in proptest::collection::vec(arb_tag(), 0..10)
        ) {
            let items: Vec<BaseItem> = tags
                .iter()
                .enumerate()
                .map(|(i, t)| make_item_with_tags(&i.to_string(), &[t.as_str()]))
                .collect();
            let original_len = items.len();
            let result = apply_tag_filter(items, None);
            prop_assert_eq!(result.len(), original_len);
        }

        #[test]
        fn prop_tag_filter_count_invariant(
            item_tags in proptest::collection::vec(arb_tag(), 0..10),
            filter_tags in proptest::collection::vec(arb_tag(), 1..5),
        ) {
            let items: Vec<BaseItem> = item_tags
                .iter()
                .enumerate()
                .map(|(i, t)| make_item_with_tags(&i.to_string(), &[t.as_str()]))
                .collect();
            let original_len = items.len();
            let result = apply_tag_filter(items, Some(&filter_tags));
            prop_assert!(result.len() <= original_len);
        }

        #[test]
        fn prop_tag_filter_or_invariant(
            filter_tags in proptest::collection::vec(arb_tag(), 1..4),
            matching_tag in arb_tag(),
        ) {
            let filter_with_match: Vec<String> = {
                let mut v = filter_tags.clone();
                v.push(matching_tag.clone());
                v
            };
            let items = vec![make_item_with_tags("1", &[matching_tag.as_str()])];
            let result = apply_tag_filter(items, Some(&filter_with_match));
            prop_assert_eq!(result.len(), 1);
            let item_tags: Vec<String> = result[0]
                .extra
                .get("Tags")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_lowercase())).collect())
                .unwrap_or_default();
            let lower_filter: Vec<String> = filter_with_match.iter().map(|t| t.to_lowercase()).collect();
            prop_assert!(lower_filter.iter().any(|t| item_tags.contains(t)));
        }

        #[test]
        fn prop_tag_filter_idempotent(
            filter_tags in proptest::collection::vec(arb_tag(), 1..4),
            item_tags in proptest::collection::vec(arb_tag(), 0..6),
        ) {
            let items: Vec<BaseItem> = item_tags
                .iter()
                .enumerate()
                .map(|(i, t)| make_item_with_tags(&i.to_string(), &[t.as_str()]))
                .collect();
            let first = apply_tag_filter(items, Some(&filter_tags));
            let first_len = first.len();
            let second = apply_tag_filter(first, Some(&filter_tags));
            prop_assert_eq!(second.len(), first_len);
        }

        #[test]
        fn prop_tag_filter_case_insensitive(tag in arb_tag()) {
            let upper_tag = tag.to_uppercase();
            let item = make_item_with_tags("1", &[upper_tag.as_str()]);
            let filter = vec![tag.to_lowercase()];
            let result = apply_tag_filter(vec![item], Some(&filter));
            prop_assert_eq!(result.len(), 1);
        }
    }
}
