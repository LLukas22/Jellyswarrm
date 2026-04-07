use std::{collections::HashMap, sync::{atomic::{AtomicBool, Ordering}, Arc}, time::Duration};

use anyhow::{anyhow, Context, Result};
use jellyfin_api::{client::JellyfinClient, models::MediaFolder, ClientInfo};
use moka::future::Cache;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Row, SqlitePool};
use tokio::{sync::{RwLock, Semaphore}, task::JoinSet};
use tracing::{error, warn};

use crate::{
    config::AppConfig,
    encryption::{decrypt_password, HashedPassword},
    media_storage_service::MediaStorageService,
    models::{enums::{BaseItemKind, CollectionType}, generate_token, ItemsResponseWithCount, MediaItem, UserData},
    server_storage::{Server, ServerStorageService},
    unified_library_service::{DedupPolicy, UnifiedLibrary, UnifiedLibraryMember},
    user_authorization_service::UserAuthorizationService,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, FromRow)]
pub struct SyncedItem {
    pub id: i64,
    pub virtual_id: String,
    pub server_id: i64,
    pub server_url: String,
    pub visibility_scope: String,
    pub source_user_id: Option<String>,
    pub original_id: String,
    pub original_parent_id: Option<String>,
    pub root_library_id: String,
    pub root_library_name: Option<String>,
    pub item_type: String,
    pub collection_type: Option<String>,
    pub name: Option<String>,
    pub sort_name: Option<String>,
    pub original_title: Option<String>,
    pub overview: Option<String>,
    pub production_year: Option<i32>,
    pub community_rating: Option<f32>,
    pub run_time_ticks: Option<i64>,
    pub premiere_date: Option<String>,
    pub index_number: Option<i32>,
    pub parent_index_number: Option<i32>,
    pub is_folder: bool,
    pub child_count: Option<i32>,
    pub series_id: Option<String>,
    pub series_name: Option<String>,
    pub season_id: Option<String>,
    pub season_name: Option<String>,
    pub parent_id: Option<String>,
    pub parent_name: Option<String>,
    pub provider_ids_json: Option<String>,
    pub genres_json: Option<String>,
    pub tags_json: Option<String>,
    pub studios_json: Option<String>,
    pub people_json: Option<String>,
    pub image_tags_json: Option<String>,
    pub backdrop_image_tags_json: Option<String>,
    pub last_synced_at: chrono::DateTime<chrono::Utc>,
    pub sync_generation: i64,
    pub needs_detail_fetch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct SyncedUserData {
    pub id: i64,
    pub synced_item_id: i64,
    pub user_id: String,
    pub playback_position_ticks: i64,
    pub play_count: i32,
    pub is_favorite: bool,
    pub played: bool,
    pub played_percentage: Option<f64>,
    pub last_played_date: Option<String>,
    pub last_synced_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct SyncState {
    pub id: i64,
    pub server_id: i64,
    pub last_full_sync_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_incremental_sync_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_sync_generation: i64,
    pub sync_status: String,
    pub last_error: Option<String>,
    pub items_synced: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncOverview {
    pub servers: Vec<SyncState>,
    pub is_syncing: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncRunSummary {
    pub attempted_servers: usize,
    pub synced_servers: usize,
    pub synced_items: usize,
}

enum SyncContext {
    Admin {
        server: Server,
        client: Arc<JellyfinClient>,
        original_user_id: String,
    },
    User {
        server: Server,
        client: Arc<JellyfinClient>,
        original_user_id: String,
        local_user_id: String,
    },
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UnifiedBrowseQuery {
    pub parent_id: Option<String>,
    pub start_index: Option<i32>,
    pub limit: Option<i32>,
    pub search_term: Option<String>,
    pub include_item_types: Option<String>,
    pub sort_by: Option<String>,
    pub sort_order: Option<String>,
    pub recursive: Option<bool>,
    pub user_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LibrarySyncService {
    pool: SqlitePool,
    server_storage: Arc<ServerStorageService>,
    user_authorization: Arc<UserAuthorizationService>,
    media_storage: Arc<MediaStorageService>,
    config: Arc<RwLock<AppConfig>>,
    reqwest_client: reqwest::Client,
    client_info: ClientInfo,
    item_cache: Cache<String, SyncedItem>,
    search_cache: Cache<String, Vec<String>>,
    sync_semaphore: Arc<Semaphore>,
    is_syncing: Arc<AtomicBool>,
}

impl LibrarySyncService {
    pub fn new(
        pool: SqlitePool,
        server_storage: Arc<ServerStorageService>,
        user_authorization: Arc<UserAuthorizationService>,
        media_storage: Arc<MediaStorageService>,
        config: Arc<RwLock<AppConfig>>,
        reqwest_client: reqwest::Client,
        client_info: ClientInfo,
    ) -> Self {
        Self {
            pool,
            server_storage,
            user_authorization,
            media_storage,
            config,
            reqwest_client,
            client_info,
            item_cache: Cache::builder().time_to_live(Duration::from_secs(60 * 10)).max_capacity(50_000).build(),
            search_cache: Cache::builder().time_to_live(Duration::from_secs(60)).max_capacity(500).build(),
            sync_semaphore: Arc::new(Semaphore::new(4)),
            is_syncing: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn start_sync_loop(&self, interval_secs: u64) {
        let service = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(err) = service.sync_all_servers().await {
                    error!("Library sync failed: {err:?}");
                }
                tokio::time::sleep(Duration::from_secs(interval_secs)).await;
            }
        });
    }

    pub async fn sync_all_servers(&self) -> Result<SyncRunSummary> {
        if self.is_syncing.swap(true, Ordering::SeqCst) {
            return Ok(SyncRunSummary {
                attempted_servers: 0,
                synced_servers: 0,
                synced_items: 0,
            });
        }

        let result = async {
            let servers = self.server_storage.list_servers().await?;
            let attempted_servers = servers.len();
            let mut join_set = JoinSet::new();

            for server in servers {
                let permit = self.sync_semaphore.clone().acquire_owned().await?;
                let service = self.clone();
                join_set.spawn(async move {
                    let _permit = permit;
                    service.sync_server(&server).await.map(|count| (server.id, count))
                });
            }

            let mut synced_servers = 0;
            let mut synced_items = 0usize;
            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(Ok((_server_id, count))) => {
                        synced_servers += 1;
                        synced_items += count;
                    }
                    Ok(Err(err)) => warn!("Server sync failed: {err:?}"),
                    Err(err) => warn!("Sync task join failed: {err}"),
                }
            }

            self.rebuild_dedup_groups().await?;

            Ok(SyncRunSummary {
                attempted_servers,
                synced_servers,
                synced_items,
            })
        }
        .await;

        self.is_syncing.store(false, Ordering::SeqCst);
        result
    }

    pub async fn sync_server(&self, server: &Server) -> Result<usize> {
        let contexts = self.sync_contexts_for_server(server).await?;
        if contexts.is_empty() {
            self.set_sync_status(server.id, "idle", Some("no admin credentials or active user sessions".to_string()), 0, 0).await?;
            return Ok(0);
        }

        let generation = self.next_generation(server.id).await?;
        self.set_sync_status(server.id, "running", None, 0, generation).await?;

        let mut total_synced = 0usize;
        for context in contexts {
            let libraries = self.fetch_media_folders_for_context(&context).await?;
            for library in &libraries {
                total_synced += self.sync_library(&context, library, generation).await?;
            }
            let scope = self.sync_context_visibility_scope(&context);
            sqlx::query(
                "DELETE FROM synced_items WHERE server_id = ? AND visibility_scope = ? AND sync_generation < ?"
            )
            .bind(server.id)
            .bind(scope)
            .bind(generation)
            .execute(&self.pool)
            .await?;
        }

        self.set_sync_status(server.id, "idle", None, total_synced as i64, generation).await?;
        Ok(total_synced)
    }

    async fn sync_library(
        &self,
        context: &SyncContext,
        library: &MediaFolder,
        generation: i64,
    ) -> Result<usize> {
        let server = self.sync_context_server(context);
        let client = self.sync_context_client(context);
        let auth_user_id = self.sync_context_original_user_id(context);
        let mut start_index = 0;
        let page_size = 200;
        let mut synced = 0usize;

        loop {
            let page = self
                .fetch_items_page(server, client, auth_user_id, Some(&library.id), start_index, page_size)
                .await
                .with_context(|| format!("failed to fetch items for library {}", library.name))?;

            if page.items.is_empty() {
                break;
            }

            for item in page.items {
                self.upsert_synced_item(context, library, item, generation).await?;
                synced += 1;
            }

            start_index += page_size;
            if start_index >= page.total_record_count {
                break;
            }
        }

        Ok(synced)
    }

    pub async fn fetch_server_media_folders(&self, server: &Server) -> Result<Vec<MediaFolder>> {
        let contexts = self.sync_contexts_for_server(server).await?;
        if let Some(context) = contexts.first() {
            self.fetch_media_folders_for_context(context).await
        } else {
            Ok(Vec::new())
        }
    }

    async fn fetch_media_folders_for_context(&self, context: &SyncContext) -> Result<Vec<MediaFolder>> {
        let client = self.sync_context_client(context);
        let user_id = self.sync_context_original_user_id(context);
        Ok(client.get_media_folders(Some(user_id)).await?)
    }

    async fn admin_client_for_server(&self, server: &Server) -> Result<(JellyfinClient, String)> {
        let admin = self
            .server_storage
            .get_server_admin(server.id)
            .await?
            .ok_or_else(|| anyhow!("server '{}' has no admin credentials", server.name))?;

        let admin_password = {
            let cfg = self.config.read().await;
            let hashed: HashedPassword = cfg.password.clone().into();
            decrypt_password(&admin.password, &hashed)
                .with_context(|| format!("failed to decrypt admin password for {}", server.name))?
        };

        let client = JellyfinClient::new(server.url.as_str(), self.client_info.clone())?;
        let user = client
            .authenticate_by_name(&admin.username, admin_password.as_str())
            .await
            .with_context(|| format!("failed to authenticate admin on {}", server.name))?;
        Ok((client, user.id))
    }

    async fn sync_contexts_for_server(&self, server: &Server) -> Result<Vec<SyncContext>> {
        if let Ok((client, original_user_id)) = self.admin_client_for_server(server).await {
            return Ok(vec![SyncContext::Admin {
                server: server.clone(),
                client: Arc::new(client),
                original_user_id,
            }]);
        }

        let users = self.user_authorization.list_users().await?;
        let mut contexts = Vec::new();
        for user in users {
            let sessions = self.user_authorization.get_user_sessions(&user.id, None).await?;
            if let Some((session, _)) = sessions.into_iter().find(|(_, session_server)| session_server.id == server.id) {
                let client = JellyfinClient::new(server.url.as_str(), self.client_info.clone())?;
                client.with_token(session.jellyfin_token.clone()).await;
                contexts.push(SyncContext::User {
                    server: server.clone(),
                    client: Arc::new(client),
                    original_user_id: session.original_user_id,
                    local_user_id: user.id,
                });
            }
        }

        Ok(contexts)
    }

    fn sync_context_server<'a>(&self, context: &'a SyncContext) -> &'a Server {
        match context {
            SyncContext::Admin { server, .. } | SyncContext::User { server, .. } => server,
        }
    }

    fn sync_context_client<'a>(&self, context: &'a SyncContext) -> &'a JellyfinClient {
        match context {
            SyncContext::Admin { client, .. } | SyncContext::User { client, .. } => client.as_ref(),
        }
    }

    fn sync_context_original_user_id<'a>(&self, context: &'a SyncContext) -> &'a str {
        match context {
            SyncContext::Admin { original_user_id, .. }
            | SyncContext::User { original_user_id, .. } => original_user_id,
        }
    }

    fn sync_context_visibility_scope(&self, context: &SyncContext) -> String {
        match context {
            SyncContext::Admin { .. } => "global".to_string(),
            SyncContext::User { local_user_id, .. } => format!("user:{local_user_id}"),
        }
    }

    fn sync_context_source_user_id(&self, context: &SyncContext) -> Option<String> {
        match context {
            SyncContext::Admin { .. } => None,
            SyncContext::User { local_user_id, .. } => Some(local_user_id.clone()),
        }
    }

    async fn fetch_items_page(
        &self,
        server: &Server,
        client: &JellyfinClient,
        user_id: &str,
        parent_id: Option<&str>,
        start_index: i32,
        limit: i32,
    ) -> Result<ItemsResponseWithCount> {
        let url_path = format!("Users/{user_id}/Items");
        let token = client.get_token().await.ok_or_else(|| anyhow!("missing admin token"))?;
        let url = server.url.join(&url_path)?;

        let mut query: Vec<(String, String)> = vec![
            ("Recursive".into(), "true".into()),
            ("Fields".into(), "ProviderIds,Overview,OriginalTitle,SortName,Path,People,Studios,Genres,Tags,ParentId,ChildCount,DateCreated,MediaSources,MediaStreams".into()),
            ("Limit".into(), limit.to_string()),
            ("StartIndex".into(), start_index.to_string()),
        ];
        if let Some(parent_id) = parent_id {
            query.push(("ParentId".into(), parent_id.to_string()));
        }

        let auth_header = format!(
            "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\", Token=\"{}\"",
            self.client_info.client,
            self.client_info.device,
            self.client_info.device_id,
            self.client_info.version,
            token,
        );

        let response = self
            .reqwest_client
            .get(url)
            .header(AUTHORIZATION, auth_header)
            .header(CONTENT_TYPE, "application/json")
            .query(&query)
            .send()
            .await?
            .error_for_status()?;

        Ok(response.json::<ItemsResponseWithCount>().await?)
    }

    async fn upsert_synced_item(
        &self,
        context: &SyncContext,
        library: &MediaFolder,
        item: MediaItem,
        generation: i64,
    ) -> Result<()> {
        let server = self.sync_context_server(context);
        let visibility_scope = self.sync_context_visibility_scope(context);
        let source_user_id = self.sync_context_source_user_id(context);
        let mapping = self
            .media_storage
            .get_or_create_media_mapping(&item.id, server.url.as_str())
            .await?;
        let original_parent_id = item.parent_id.clone();
        let parent_virtual_id = match item.parent_id.as_deref() {
            Some(parent_id) => Some(
                self.media_storage
                    .get_or_create_media_mapping(parent_id, server.url.as_str())
                    .await?
                    .virtual_media_id,
            ),
            None => None,
        };
        let series_virtual_id = match item.series_id.as_deref() {
            Some(series_id) => Some(
                self.media_storage
                    .get_or_create_media_mapping(series_id, server.url.as_str())
                    .await?
                    .virtual_media_id,
            ),
            None => None,
        };
        let season_virtual_id = match item.season_id.as_deref() {
            Some(season_id) => Some(
                self.media_storage
                    .get_or_create_media_mapping(season_id, server.url.as_str())
                    .await?
                    .virtual_media_id,
            ),
            None => None,
        };
        let overview = item
            .extra
            .get("Overview")
            .or_else(|| item.extra.get("overview"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let production_year = item
            .extra
            .get("ProductionYear")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32);
        let community_rating = item
            .extra
            .get("CommunityRating")
            .and_then(|v| v.as_f64())
            .map(|v| v as f32);
        let run_time_ticks = item
            .extra
            .get("RunTimeTicks")
            .and_then(|v| v.as_i64())
            .or(item.media_sources.as_ref().and_then(max_bitrate));
        let premiere_date = item
            .extra
            .get("PremiereDate")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let index_number = item
            .extra
            .get("IndexNumber")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32);
        let parent_index_number = item
            .extra
            .get("ParentIndexNumber")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32);
        let season_name = item
            .extra
            .get("SeasonName")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let parent_name = item
            .extra
            .get("ParentName")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or(Some(library.name.clone()));
        let provider_ids_json = json_string(item.provider_ids.as_ref());
        let genres_json = json_string(item.extra.get("Genres"));
        let tags_json = json_string(item.tags.as_ref());
        let studios_json = json_string(item.extra.get("Studios"));
        let people_json = json_string(item.extra.get("People"));
        let image_tags_json = json_string(item.image_tags.as_ref());
        let backdrop_image_tags_json = json_string(item.backdrop_image_tags.as_ref());
        let collection_type = item
            .collection_type
            .clone()
            .map(collection_type_to_string)
            .or_else(|| library.collection_type.clone());
        let now = chrono::Utc::now();

        sqlx::query(
            r#"
            INSERT INTO synced_items (
                virtual_id, server_id, server_url, visibility_scope, source_user_id, original_id, original_parent_id, root_library_id, root_library_name, item_type, collection_type,
                name, sort_name, original_title, overview, production_year, community_rating, run_time_ticks,
                premiere_date, index_number, parent_index_number, is_folder, child_count,
                series_id, series_name, season_id, season_name, parent_id, parent_name,
                provider_ids_json, genres_json, tags_json, studios_json, people_json, image_tags_json,
                backdrop_image_tags_json, last_synced_at, sync_generation, needs_detail_fetch
            ) VALUES (
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
            )
            ON CONFLICT(server_url, original_id, visibility_scope) DO UPDATE SET
                virtual_id = excluded.virtual_id,
                server_id = excluded.server_id,
                source_user_id = excluded.source_user_id,
                original_parent_id = excluded.original_parent_id,
                root_library_id = excluded.root_library_id,
                root_library_name = excluded.root_library_name,
                item_type = excluded.item_type,
                collection_type = excluded.collection_type,
                name = excluded.name,
                sort_name = excluded.sort_name,
                original_title = excluded.original_title,
                overview = excluded.overview,
                production_year = excluded.production_year,
                community_rating = excluded.community_rating,
                run_time_ticks = excluded.run_time_ticks,
                premiere_date = excluded.premiere_date,
                index_number = excluded.index_number,
                parent_index_number = excluded.parent_index_number,
                is_folder = excluded.is_folder,
                child_count = excluded.child_count,
                series_id = excluded.series_id,
                series_name = excluded.series_name,
                season_id = excluded.season_id,
                season_name = excluded.season_name,
                parent_id = excluded.parent_id,
                parent_name = excluded.parent_name,
                provider_ids_json = excluded.provider_ids_json,
                genres_json = excluded.genres_json,
                tags_json = excluded.tags_json,
                studios_json = excluded.studios_json,
                people_json = excluded.people_json,
                image_tags_json = excluded.image_tags_json,
                backdrop_image_tags_json = excluded.backdrop_image_tags_json,
                last_synced_at = excluded.last_synced_at,
                sync_generation = excluded.sync_generation,
                needs_detail_fetch = excluded.needs_detail_fetch
            "#,
        )
        .bind(&mapping.virtual_media_id)
        .bind(server.id)
        .bind(server.url.as_str())
        .bind(visibility_scope)
        .bind(source_user_id)
        .bind(&mapping.original_media_id)
        .bind(original_parent_id)
        .bind(library.id.clone())
        .bind(Some(library.name.clone()))
        .bind(base_item_kind_to_string(&item.item_type))
        .bind(collection_type)
        .bind(item.name)
        .bind(item.sort_name)
        .bind(item.original_title)
        .bind(overview)
        .bind(production_year)
        .bind(community_rating)
        .bind(run_time_ticks)
        .bind(premiere_date)
        .bind(index_number)
        .bind(parent_index_number)
        .bind(item.is_folder.unwrap_or(false))
        .bind(item.child_count)
        .bind(series_virtual_id)
        .bind(item.series_name)
        .bind(season_virtual_id)
        .bind(season_name)
        .bind(parent_virtual_id)
        .bind(parent_name)
        .bind(provider_ids_json)
        .bind(genres_json)
        .bind(tags_json)
        .bind(studios_json)
        .bind(people_json)
        .bind(image_tags_json)
        .bind(backdrop_image_tags_json)
        .bind(now)
        .bind(generation)
        .bind(false)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn browse_unified_library(
        &self,
        library: &UnifiedLibrary,
        query: &UnifiedBrowseQuery,
    ) -> Result<crate::models::ItemsResponseWithCount> {
        let mut rows = normalize_visible_rows(self.fetch_library_rows(library, query).await?);
        if library.dedup_policy != DedupPolicy::ShowAll {
            rows = self.apply_dedup_policy(rows, library.dedup_policy).await?;
        }

        let total = rows.len() as i32;
        let start_index = query.start_index.unwrap_or(0).max(0) as usize;
        let limit = query.limit.unwrap_or(100).max(0) as usize;
        let rows = rows.into_iter().skip(start_index).take(limit.max(100)).collect::<Vec<_>>();

        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let user_data = if let Some(user_id) = &query.user_id {
                self.get_user_data(row.id, user_id).await?
            } else {
                None
            };
            items.push(self.row_to_media_item(row, user_data));
        }

        Ok(crate::models::ItemsResponseWithCount {
            items,
            total_record_count: total,
            start_index: query.start_index.unwrap_or(0),
        })
    }

    pub async fn unified_views(&self, user_id: Option<&str>) -> Result<Vec<MediaItem>> {
        let libraries = sqlx::query_as::<_, UnifiedLibraryRow>(
            r#"
            SELECT id, virtual_library_id, name, collection_type, sort_order, dedup_policy, created_at, updated_at
            FROM unified_libraries
            ORDER BY sort_order ASC, name ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut visible_libraries = Vec::new();
        for library in libraries {
            if self.library_has_visible_items(library.id, user_id).await? {
                visible_libraries.push(library);
            }
        }

        Ok(visible_libraries
            .into_iter()
            .map(|library| MediaItem {
                name: Some(library.name),
                server_id: None,
                id: library.virtual_library_id,
                item_id: None,
                series_id: None,
                series_name: None,
                season_id: None,
                etag: None,
                date_created: None,
                can_delete: Some(false),
                can_download: Some(false),
                sort_name: None,
                external_urls: None,
                path: None,
                enable_media_source_display: Some(false),
                channel_id: None,
                provider_ids: None,
                is_folder: Some(true),
                parent_id: None,
                parent_logo_item_id: None,
                parent_backdrop_item_id: None,
                parent_backdrop_image_tags: None,
                parent_logo_image_tag: None,
                parent_thumb_item_id: None,
                parent_thumb_image_tag: None,
                item_type: BaseItemKind::UserView,
                collection_type: Some(string_to_collection_type(&library.collection_type)),
                user_data: None,
                child_count: None,
                display_preferences_id: None,
                tags: Some(vec!["UnifiedLibrary".to_string()]),
                series_primary_image_tag: None,
                image_tags: None,
                backdrop_image_tags: None,
                image_blur_hashes: None,
                original_title: None,
                media_sources: None,
                media_streams: None,
                chapters: None,
                trickplay: None,
                extra: HashMap::from([
                    ("IsUnifiedLibrary".to_string(), serde_json::Value::Bool(true)),
                ]),
            })
            .collect())
    }

    pub async fn unified_library_by_virtual_id(
        &self,
        virtual_library_id: &str,
    ) -> Result<Option<UnifiedLibrary>> {
        let row = sqlx::query_as::<_, UnifiedLibraryRow>(
            r#"
            SELECT id, virtual_library_id, name, collection_type, sort_order, dedup_policy, created_at, updated_at
            FROM unified_libraries
            WHERE virtual_library_id = ?
            "#,
        )
        .bind(virtual_library_id)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(row) => Ok(Some(self.inflate_library(row).await?)),
            None => Ok(None),
        }
    }

    pub async fn search_items(
        &self,
        search_term: &str,
        user_id: Option<&str>,
        limit: i32,
    ) -> Result<crate::models::ItemsResponseWithCount> {
        let visibility_scope = user_visibility_scope(user_id);
        let cache_key = format!("search:{search_term}:{limit}:{:?}", visibility_scope);
        let virtual_ids = if let Some(cached) = self.search_cache.get(&cache_key).await {
            cached
        } else {
            let rows = match sqlx::query(
                r#"
                SELECT DISTINCT si.virtual_id
                FROM synced_items_fts fts
                JOIN synced_items si ON si.id = fts.rowid
                WHERE synced_items_fts MATCH ?
                  AND (si.visibility_scope = 'global' OR si.visibility_scope = ?)
                LIMIT ?
                "#,
            )
            .bind(search_term)
            .bind(visibility_scope.clone().unwrap_or_else(|| "global".to_string()))
            .bind(limit)
            .fetch_all(&self.pool)
            .await {
                Ok(rows) => rows,
                Err(_) => sqlx::query(
                    r#"
                    SELECT DISTINCT virtual_id
                    FROM synced_items
                    WHERE name LIKE ? OR original_title LIKE ? OR overview LIKE ?
                      AND (visibility_scope = 'global' OR visibility_scope = ?)
                    LIMIT ?
                    "#,
                )
                .bind(format!("%{search_term}%"))
                .bind(format!("%{search_term}%"))
                .bind(format!("%{search_term}%"))
                .bind(visibility_scope.clone().unwrap_or_else(|| "global".to_string()))
                .bind(limit)
                .fetch_all(&self.pool)
                .await?,
            };
            let ids = rows.into_iter().map(|row| row.get::<String, _>("virtual_id")).collect::<Vec<_>>();
            self.search_cache.insert(cache_key, ids.clone()).await;
            ids
        };

        let mut items = Vec::new();
        for virtual_id in virtual_ids {
            if let Some(row) = self.get_visible_synced_item_by_virtual_id(&virtual_id, user_id).await? {
                let user_data = if let Some(user_id) = user_id {
                    self.get_user_data(row.id, user_id).await?
                } else {
                    None
                };
                items.push(self.row_to_media_item(row, user_data));
            }
        }

        Ok(crate::models::ItemsResponseWithCount {
            total_record_count: items.len() as i32,
            items,
            start_index: 0,
        })
    }

    pub async fn rebuild_dedup_groups(&self) -> Result<()> {
        let rows = sqlx::query_as::<_, SyncedItem>(
            r#"
            SELECT id, virtual_id, server_id, server_url, visibility_scope, source_user_id, original_id, original_parent_id, root_library_id, root_library_name, item_type,
                   collection_type, name, sort_name, original_title, overview, production_year,
                   community_rating, run_time_ticks, premiere_date, index_number, parent_index_number,
                   CAST(is_folder AS INTEGER) as is_folder, child_count, series_id, series_name,
                   season_id, season_name, parent_id, parent_name, provider_ids_json, genres_json,
                   tags_json, studios_json, people_json, image_tags_json, backdrop_image_tags_json,
                   last_synced_at, sync_generation, CAST(needs_detail_fetch AS INTEGER) as needs_detail_fetch
            FROM synced_items
            WHERE provider_ids_json IS NOT NULL AND provider_ids_json != ''
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut groups: HashMap<String, Vec<SyncedItem>> = HashMap::new();
        for row in rows {
            if let Some(key) = canonical_provider_key(&row.provider_ids_json) {
                groups.entry(key).or_default().push(row);
            }
        }

        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM dedup_group_members").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM dedup_groups").execute(&mut *tx).await?;

        for (key, mut items) in groups {
            if items.len() < 2 {
                continue;
            }
            items.sort_by_key(|item| std::cmp::Reverse(self.quality_score(item)));
            let preferred = items.first().map(|item| item.id);
            let dedup_group_id = sqlx::query(
                "INSERT INTO dedup_groups (canonical_provider_key, preferred_item_id) VALUES (?, ?)"
            )
            .bind(&key)
            .bind(preferred)
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();

            for item in items {
                sqlx::query(
                    "INSERT INTO dedup_group_members (dedup_group_id, synced_item_id, quality_score) VALUES (?, ?, ?)"
                )
                .bind(dedup_group_id)
                .bind(item.id)
                .bind(self.quality_score(&item))
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }

    pub async fn sync_user_data(&self, user_id: &str) -> Result<usize> {
        let sessions = self
            .user_authorization
            .get_user_sessions(user_id, None)
            .await?;

        let _ = self.sync_user_accessible_servers(user_id).await;

        let mut upserts = 0usize;
        for (session, server) in sessions {
            let items = self.search_server_user_data(&server, &session.original_user_id, &session.jellyfin_token).await?;
            for item in items {
                if let Some(user_data) = item.user_data {
                    let mapping = self.media_storage.get_or_create_media_mapping(&item.id, server.url.as_str()).await?;
                    if let Some(row) = self.get_visible_synced_item_by_virtual_id(&mapping.virtual_media_id, Some(user_id)).await? {
                        self.upsert_user_data(row.id, user_id, &user_data).await?;
                        upserts += 1;
                    }
                }
            }
        }
        Ok(upserts)
    }

    async fn search_server_user_data(&self, server: &Server, original_user_id: &str, token: &str) -> Result<Vec<MediaItem>> {
        let url = server.url.join(&format!("Users/{original_user_id}/Items"))?;
        let auth_header = format!(
            "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\", Token=\"{}\"",
            self.client_info.client,
            self.client_info.device,
            self.client_info.device_id,
            self.client_info.version,
            token,
        );
        let response = self.reqwest_client.get(url)
            .header(AUTHORIZATION, auth_header)
            .query(&[("Recursive", "true"), ("Fields", "UserData")])
            .send()
            .await?
            .error_for_status()?;
        let payload = response.json::<ItemsResponseWithCount>().await?;
        Ok(payload.items)
    }

    async fn upsert_user_data(&self, synced_item_id: i64, user_id: &str, user_data: &UserData) -> Result<()> {
        let now = chrono::Utc::now();
        sqlx::query(
            r#"
            INSERT INTO synced_user_data (
                synced_item_id, user_id, playback_position_ticks, play_count, is_favorite,
                played, played_percentage, last_played_date, last_synced_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(synced_item_id, user_id) DO UPDATE SET
                playback_position_ticks = excluded.playback_position_ticks,
                play_count = excluded.play_count,
                is_favorite = excluded.is_favorite,
                played = excluded.played,
                played_percentage = excluded.played_percentage,
                last_played_date = excluded.last_played_date,
                last_synced_at = excluded.last_synced_at
            "#,
        )
        .bind(synced_item_id)
        .bind(user_id)
        .bind(user_data.playback_position_ticks)
        .bind(user_data.play_count)
        .bind(user_data.is_favorite)
        .bind(user_data.played)
        .bind(user_data.played_percentage)
        .bind(&user_data.last_played_date)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn sync_user_accessible_servers(&self, user_id: &str) -> Result<usize> {
        let sessions = self.user_authorization.get_user_sessions(user_id, None).await?;
        let mut server_ids = std::collections::BTreeSet::new();
        let mut synced_items = 0usize;
        for (_, server) in sessions {
            if server_ids.insert(server.id) {
                synced_items += self.sync_server(&server).await?;
            }
        }
        Ok(synced_items)
    }

    async fn library_has_visible_items(&self, library_id: i64, user_id: Option<&str>) -> Result<bool> {
        let count = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(1)
            FROM synced_items si
            JOIN unified_library_members ulm
              ON ulm.server_id = si.server_id AND ulm.original_library_id = si.root_library_id
            WHERE ulm.unified_library_id = ?
              AND ulm.enabled = 1
              AND (si.visibility_scope = 'global' OR si.visibility_scope = ?)
            "#,
        )
        .bind(library_id)
        .bind(user_visibility_scope(user_id).unwrap_or_else(|| "global".to_string()))
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    pub async fn sync_overview(&self) -> Result<SyncOverview> {
        let servers = sqlx::query_as::<_, SyncState>(
            r#"
            SELECT id, server_id, last_full_sync_at, last_incremental_sync_at, last_sync_generation,
                   sync_status, last_error, items_synced
            FROM sync_state
            ORDER BY server_id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(SyncOverview {
            servers,
            is_syncing: self.is_syncing.load(Ordering::SeqCst),
        })
    }

    pub async fn get_synced_item_by_virtual_id(&self, virtual_id: &str) -> Result<Option<SyncedItem>> {
        self.get_visible_synced_item_by_virtual_id(virtual_id, None).await
    }

    pub async fn get_visible_synced_item_by_virtual_id(&self, virtual_id: &str, user_id: Option<&str>) -> Result<Option<SyncedItem>> {
        if let Some(item) = self.item_cache.get(virtual_id).await {
            if is_item_visible_to_user(&item, user_id) {
                return Ok(Some(item));
            }
        }

        let row = sqlx::query_as::<_, SyncedItem>(
            r#"
            SELECT id, virtual_id, server_id, server_url, visibility_scope, source_user_id, original_id, original_parent_id, root_library_id, root_library_name, item_type,
                   collection_type, name, sort_name, original_title, overview, production_year,
                   community_rating, run_time_ticks, premiere_date, index_number, parent_index_number,
                   CAST(is_folder AS INTEGER) as is_folder, child_count, series_id, series_name,
                   season_id, season_name, parent_id, parent_name, provider_ids_json, genres_json,
                   tags_json, studios_json, people_json, image_tags_json, backdrop_image_tags_json,
                   last_synced_at, sync_generation, CAST(needs_detail_fetch AS INTEGER) as needs_detail_fetch
            FROM synced_items
            WHERE virtual_id = ?
              AND (visibility_scope = 'global' OR visibility_scope = ?)
            ORDER BY CASE WHEN visibility_scope = 'global' THEN 0 ELSE 1 END
            "#,
        )
        .bind(virtual_id)
        .bind(user_visibility_scope(user_id).unwrap_or_else(|| "global".to_string()))
        .fetch_optional(&self.pool)
        .await?;

        if let Some(item) = row {
            self.item_cache.insert(virtual_id.to_string(), item.clone()).await;
            Ok(Some(item))
        } else {
            Ok(None)
        }
    }

    async fn get_user_data(&self, synced_item_id: i64, user_id: &str) -> Result<Option<SyncedUserData>> {
        sqlx::query_as::<_, SyncedUserData>(
            r#"
            SELECT id, synced_item_id, user_id, playback_position_ticks, play_count,
                   CAST(is_favorite AS INTEGER) as is_favorite, CAST(played AS INTEGER) as played,
                   played_percentage, last_played_date, last_synced_at
            FROM synced_user_data
            WHERE synced_item_id = ? AND user_id = ?
            "#,
        )
        .bind(synced_item_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Into::into)
    }

    async fn fetch_library_rows(&self, library: &UnifiedLibrary, query: &UnifiedBrowseQuery) -> Result<Vec<SyncedItem>> {
        let mut sql = String::from(
            r#"
            SELECT si.id, si.virtual_id, si.server_id, si.server_url, si.visibility_scope, si.source_user_id, si.original_id, si.original_parent_id, si.root_library_id, si.root_library_name, si.item_type,
                   si.collection_type, si.name, si.sort_name, si.original_title, si.overview, si.production_year,
                   si.community_rating, si.run_time_ticks, si.premiere_date, si.index_number, si.parent_index_number,
                   CAST(si.is_folder AS INTEGER) as is_folder, si.child_count, si.series_id, si.series_name,
                   si.season_id, si.season_name, si.parent_id, si.parent_name, si.provider_ids_json, si.genres_json,
                   si.tags_json, si.studios_json, si.people_json, si.image_tags_json, si.backdrop_image_tags_json,
                   si.last_synced_at, si.sync_generation, CAST(si.needs_detail_fetch AS INTEGER) as needs_detail_fetch
            FROM synced_items si
            JOIN unified_library_members ulm
              ON ulm.server_id = si.server_id AND ulm.original_library_id = si.root_library_id
            WHERE ulm.unified_library_id = ? AND ulm.enabled = 1
              AND (si.visibility_scope = 'global' OR si.visibility_scope = ?)
            "#,
        );

        if query.recursive == Some(false) {
            sql.push_str(" AND (si.parent_id = ? OR si.original_parent_id IN (SELECT original_library_id FROM unified_library_members WHERE unified_library_id = ?))");
        }
        if query.include_item_types.as_deref().is_some_and(|s| !s.is_empty()) {
            sql.push_str(" AND LOWER(si.item_type) IN (");
            let names = query.include_item_types.as_ref().unwrap().split(',').map(|s| s.trim().to_lowercase()).collect::<Vec<_>>();
            sql.push_str(&vec!["?"; names.len()].join(","));
            sql.push(')');
        }
        if query.search_term.as_deref().is_some_and(|s| !s.is_empty()) {
            sql.push_str(" AND (si.name LIKE ? OR si.original_title LIKE ? OR si.overview LIKE ?)");
        }
        sql.push_str(" ORDER BY ");
        sql.push_str(sort_column(query.sort_by.as_deref()));
        sql.push(' ');
        sql.push_str(if query.sort_order.as_deref() == Some("Descending") { "DESC" } else { "ASC" });

        let mut q = sqlx::query_as::<_, SyncedItem>(&sql)
            .bind(library.id)
            .bind(user_visibility_scope(query.user_id.as_deref()).unwrap_or_else(|| "global".to_string()));
        if query.recursive == Some(false) {
            q = q.bind(query.parent_id.clone()).bind(library.id);
        }
        if let Some(include_item_types) = &query.include_item_types {
            for name in include_item_types.split(',').map(|s| s.trim().to_lowercase()) {
                q = q.bind(name);
            }
        }
        if let Some(search_term) = &query.search_term {
            let like = format!("%{search_term}%");
            q = q.bind(like.clone()).bind(like.clone()).bind(like);
        }
        Ok(q.fetch_all(&self.pool).await?)
    }

    async fn apply_dedup_policy(&self, rows: Vec<SyncedItem>, policy: DedupPolicy) -> Result<Vec<SyncedItem>> {
        if rows.is_empty() {
            return Ok(rows);
        }
        let mut best_by_group: HashMap<i64, SyncedItem> = HashMap::new();
        let mut ungrouped = Vec::new();

        for row in rows {
            let group = sqlx::query(
                r#"
                SELECT dgm.dedup_group_id, dg.preferred_item_id
                FROM dedup_group_members dgm
                JOIN dedup_groups dg ON dg.id = dgm.dedup_group_id
                WHERE dgm.synced_item_id = ?
                "#,
            )
            .bind(row.id)
            .fetch_optional(&self.pool)
            .await?;

            if let Some(group) = group {
                let dedup_group_id: i64 = group.get("dedup_group_id");
                let preferred_item_id: Option<i64> = group.get("preferred_item_id");
                match policy {
                    DedupPolicy::PreferServerPriority => {
                        best_by_group.entry(dedup_group_id).or_insert(row);
                    }
                    DedupPolicy::PreferHighestQuality => {
                        if preferred_item_id == Some(row.id) {
                            best_by_group.insert(dedup_group_id, row);
                        }
                    }
                    DedupPolicy::ShowAll => ungrouped.push(row),
                }
            } else {
                ungrouped.push(row);
            }
        }

        ungrouped.extend(best_by_group.into_values());
        Ok(ungrouped)
    }

    fn row_to_media_item(&self, row: SyncedItem, user_data: Option<SyncedUserData>) -> MediaItem {
        MediaItem {
            name: row.name,
            server_id: None,
            id: row.virtual_id,
            item_id: None,
            series_id: row.series_id,
            series_name: row.series_name,
            season_id: row.season_id,
            etag: None,
            date_created: None,
            can_delete: Some(false),
            can_download: Some(true),
            sort_name: row.sort_name,
            external_urls: None,
            path: None,
            enable_media_source_display: Some(true),
            channel_id: None,
            provider_ids: row.provider_ids_json.and_then(|s| serde_json::from_str(&s).ok()),
            is_folder: Some(row.is_folder),
            parent_id: row.parent_id,
            parent_logo_item_id: None,
            parent_backdrop_item_id: None,
            parent_backdrop_image_tags: row.backdrop_image_tags_json.and_then(|s| serde_json::from_str(&s).ok()),
            parent_logo_image_tag: None,
            parent_thumb_item_id: None,
            parent_thumb_image_tag: None,
            item_type: string_to_base_item_kind(&row.item_type),
            collection_type: row.collection_type.as_deref().map(string_to_collection_type),
            user_data: user_data.map(|data| UserData {
                playback_position_ticks: data.playback_position_ticks,
                play_count: data.play_count,
                is_favorite: data.is_favorite,
                played: data.played,
                key: generate_token(),
                item_id: row.original_id.clone(),
                played_percentage: data.played_percentage,
                last_played_date: data.last_played_date,
                unplayed_item_count: None,
            }),
            child_count: row.child_count,
            display_preferences_id: None,
            tags: row.tags_json.and_then(|s| serde_json::from_str(&s).ok()),
            series_primary_image_tag: None,
            image_tags: row.image_tags_json.and_then(|s| serde_json::from_str(&s).ok()),
            backdrop_image_tags: None,
            image_blur_hashes: None,
            original_title: row.original_title,
            media_sources: None,
            media_streams: None,
            chapters: None,
            trickplay: None,
            extra: HashMap::new(),
        }
    }

    fn quality_score(&self, item: &SyncedItem) -> i32 {
        let mut score = 0;
        if let Some(run_time_ticks) = item.run_time_ticks {
            score += (run_time_ticks / 1_000_000_000) as i32;
        }
        if item.overview.is_some() {
            score += 100;
        }
        score
    }

    async fn next_generation(&self, server_id: i64) -> Result<i64> {
        let generation = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(last_sync_generation), 0) + 1 FROM sync_state WHERE server_id = ?"
        )
        .bind(server_id)
        .fetch_one(&self.pool)
        .await
        .unwrap_or(1);
        Ok(generation)
    }

    async fn set_sync_status(
        &self,
        server_id: i64,
        status: &str,
        last_error: Option<String>,
        items_synced: i64,
        generation: i64,
    ) -> Result<()> {
        let now = chrono::Utc::now();
        sqlx::query(
            r#"
            INSERT INTO sync_state (server_id, last_full_sync_at, last_incremental_sync_at, last_sync_generation, sync_status, last_error, items_synced)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(server_id) DO UPDATE SET
                last_full_sync_at = excluded.last_full_sync_at,
                last_incremental_sync_at = excluded.last_incremental_sync_at,
                last_sync_generation = excluded.last_sync_generation,
                sync_status = excluded.sync_status,
                last_error = excluded.last_error,
                items_synced = excluded.items_synced
            "#,
        )
        .bind(server_id)
        .bind(now)
        .bind(now)
        .bind(generation)
        .bind(status)
        .bind(last_error)
        .bind(items_synced)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn inflate_library(&self, row: UnifiedLibraryRow) -> Result<UnifiedLibrary> {
        let members = sqlx::query_as::<_, UnifiedLibraryMember>(
            r#"
            SELECT id, unified_library_id, server_id, original_library_id, original_library_name,
                   CAST(enabled AS INTEGER) as enabled, created_at
            FROM unified_library_members
            WHERE unified_library_id = ?
            ORDER BY original_library_name ASC
            "#,
        )
        .bind(row.id)
        .fetch_all(&self.pool)
        .await?;
        Ok(UnifiedLibrary {
            id: row.id,
            virtual_library_id: row.virtual_library_id,
            name: row.name,
            collection_type: row.collection_type,
            sort_order: row.sort_order,
            dedup_policy: row.dedup_policy.parse().unwrap_or_default(),
            created_at: row.created_at,
            updated_at: row.updated_at,
            members,
        })
    }
}

#[derive(Debug, Clone, FromRow)]
struct UnifiedLibraryRow {
    id: i64,
    virtual_library_id: String,
    name: String,
    collection_type: String,
    sort_order: i32,
    dedup_policy: String,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

fn json_string<T: Serialize>(value: Option<T>) -> Option<String> {
    value.and_then(|value| serde_json::to_string(&value).ok())
}

fn base_item_kind_to_string(kind: &BaseItemKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "Unknown".to_string())
}

fn string_to_base_item_kind(kind: &str) -> BaseItemKind {
    serde_json::from_value(serde_json::Value::String(kind.to_string()))
        .unwrap_or(BaseItemKind::Unknown(kind.to_string()))
}

fn collection_type_to_string(kind: CollectionType) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_string())
}

fn string_to_collection_type(kind: &str) -> CollectionType {
    serde_json::from_value(serde_json::Value::String(kind.to_string())).unwrap_or_default()
}

fn canonical_provider_key(provider_ids_json: &Option<String>) -> Option<String> {
    let json = provider_ids_json.as_ref()?;
    let map = serde_json::from_str::<serde_json::Value>(json).ok()?;
    let object = map.as_object()?;
    for key in ["Tmdb", "Imdb", "Tvdb", "MusicBrainzAlbum", "MusicBrainzArtist"] {
        if let Some(value) = object.get(key).and_then(|v| v.as_str()) {
            return Some(format!("{}:{}", key.to_lowercase(), value));
        }
    }
    None
}

fn sort_column(sort_by: Option<&str>) -> &'static str {
    match sort_by.unwrap_or("SortName") {
        "DateCreated" => "si.last_synced_at",
        "ProductionYear" => "si.production_year",
        "CommunityRating" => "si.community_rating",
        "PremiereDate" => "si.premiere_date",
        "Name" => "COALESCE(si.name, si.sort_name)",
        _ => "COALESCE(si.sort_name, si.name)",
    }
}

fn max_bitrate(media_sources: &Vec<crate::models::MediaSource>) -> Option<i64> {
    media_sources.iter().filter_map(|source| source.bitrate).max()
}

fn user_visibility_scope(user_id: Option<&str>) -> Option<String> {
    user_id.map(|user_id| format!("user:{user_id}"))
}

fn is_item_visible_to_user(item: &SyncedItem, user_id: Option<&str>) -> bool {
    item.visibility_scope == "global"
        || user_visibility_scope(user_id)
            .map(|scope| item.visibility_scope == scope)
            .unwrap_or(false)
}

fn normalize_visible_rows(rows: Vec<SyncedItem>) -> Vec<SyncedItem> {
    let mut by_virtual = std::collections::BTreeMap::<String, SyncedItem>::new();
    for row in rows {
        match by_virtual.get(&row.virtual_id) {
            Some(existing) if existing.visibility_scope == "global" => {}
            _ => {
                by_virtual.insert(row.virtual_id.clone(), row);
            }
        }
    }
    by_virtual.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{AppConfig, CLIENT_INFO, MIGRATOR},
        media_storage_service::MediaStorageService,
        server_storage::ServerStorageService,
        unified_library_service::UnifiedLibraryService,
        user_authorization_service::UserAuthorizationService,
    };
    use sqlx::SqlitePool;

    async fn setup_service() -> (SqlitePool, LibrarySyncService, i64) {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();

        let server_storage = Arc::new(ServerStorageService::new(pool.clone()));
        let user_authorization = Arc::new(UserAuthorizationService::new(pool.clone()));
        let media_storage = Arc::new(MediaStorageService::new(pool.clone()));

        let server_id = server_storage
            .add_server(
                "server-a",
                "http://localhost:8096",
                100,
                crate::config::MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();

        let service = LibrarySyncService::new(
            pool.clone(),
            server_storage,
            user_authorization,
            media_storage,
            Arc::new(RwLock::new(AppConfig::default())),
            reqwest::Client::new(),
            CLIENT_INFO.clone(),
        );

        (pool, service, server_id)
    }

    async fn insert_unified_library(
        pool: &SqlitePool,
        virtual_library_id: &str,
        name: &str,
        collection_type: &str,
    ) -> i64 {
        let now = chrono::Utc::now();
        sqlx::query(
            r#"
            INSERT INTO unified_libraries (virtual_library_id, name, collection_type, sort_order, dedup_policy, created_at, updated_at)
            VALUES (?, ?, ?, 0, 'show_all', ?, ?)
            "#,
        )
        .bind(virtual_library_id)
        .bind(name)
        .bind(collection_type)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
    }

    async fn insert_unified_member(pool: &SqlitePool, unified_library_id: i64, server_id: i64, root_library_id: &str) {
        sqlx::query(
            r#"
            INSERT INTO unified_library_members (unified_library_id, server_id, original_library_id, original_library_name, enabled, created_at)
            VALUES (?, ?, ?, 'Movies', 1, ?)
            "#,
        )
        .bind(unified_library_id)
        .bind(server_id)
        .bind(root_library_id)
        .bind(chrono::Utc::now())
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_synced_item(
        pool: &SqlitePool,
        server_id: i64,
        virtual_id: &str,
        visibility_scope: &str,
        source_user_id: Option<&str>,
        original_id: &str,
        root_library_id: &str,
        name: &str,
    ) {
        sqlx::query(
            r#"
            INSERT INTO synced_items (
                virtual_id, server_id, server_url, visibility_scope, source_user_id,
                original_id, root_library_id, root_library_name, item_type, collection_type,
                name, sort_name, is_folder, sync_generation, needs_detail_fetch
            ) VALUES (?, ?, 'http://localhost:8096/', ?, ?, ?, ?, 'Movies', 'Movie', 'movies', ?, ?, 0, 1, 0)
            "#,
        )
        .bind(virtual_id)
        .bind(server_id)
        .bind(visibility_scope)
        .bind(source_user_id)
        .bind(original_id)
        .bind(root_library_id)
        .bind(name)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn search_items_respects_user_scope() {
        let (pool, service, server_id) = setup_service().await;

        insert_synced_item(&pool, server_id, "global-item", "global", None, "orig-global", "lib-a", "Shared Movie").await;
        insert_synced_item(&pool, server_id, "user1-item", "user:user-1", Some("user-1"), "orig-u1", "lib-a", "Private User One").await;
        insert_synced_item(&pool, server_id, "user2-item", "user:user-2", Some("user-2"), "orig-u2", "lib-a", "Private User Two").await;

        let anonymous = service.search_items("Private", None, 20).await.unwrap();
        assert!(anonymous.items.is_empty());

        let user1 = service.search_items("Private", Some("user-1"), 20).await.unwrap();
        assert_eq!(user1.items.len(), 1);
        assert_eq!(user1.items[0].name.as_deref(), Some("Private User One"));

        let user2 = service.search_items("Private", Some("user-2"), 20).await.unwrap();
        assert_eq!(user2.items.len(), 1);
        assert_eq!(user2.items[0].name.as_deref(), Some("Private User Two"));
    }

    #[tokio::test]
    async fn unified_browse_prefers_global_and_filters_private_rows() {
        let (pool, service, server_id) = setup_service().await;

        let unified_id = insert_unified_library(&pool, "unified-lib", "Movies", "movies").await;
        insert_unified_member(&pool, unified_id, server_id, "lib-a").await;

        insert_synced_item(&pool, server_id, "shared-virtual", "global", None, "orig-global", "lib-a", "Global Preferred").await;
        insert_synced_item(&pool, server_id, "shared-virtual", "user:user-1", Some("user-1"), "orig-u1-dup", "lib-a", "Private Duplicate").await;
        insert_synced_item(&pool, server_id, "only-user1", "user:user-1", Some("user-1"), "orig-u1-only", "lib-a", "User One Only").await;

        let library = UnifiedLibraryService::new(pool.clone())
            .get(unified_id)
            .await
            .unwrap()
            .unwrap();

        let user1 = service
            .browse_unified_library(
                &library,
                &UnifiedBrowseQuery {
                    user_id: Some("user-1".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(user1.items.len(), 2);
        assert!(user1.items.iter().any(|item| item.name.as_deref() == Some("Global Preferred")));
        assert!(user1.items.iter().any(|item| item.name.as_deref() == Some("User One Only")));
        assert!(!user1.items.iter().any(|item| item.name.as_deref() == Some("Private Duplicate")));

        let user2 = service
            .browse_unified_library(
                &library,
                &UnifiedBrowseQuery {
                    user_id: Some("user-2".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(user2.items.len(), 1);
        assert_eq!(user2.items[0].name.as_deref(), Some("Global Preferred"));
    }

    #[tokio::test]
    async fn unified_views_only_show_libraries_visible_to_user() {
        let (pool, service, server_id) = setup_service().await;

        let global_lib = insert_unified_library(&pool, "global-lib", "Global Movies", "movies").await;
        let private_lib = insert_unified_library(&pool, "private-lib", "Private Movies", "movies").await;
        insert_unified_member(&pool, global_lib, server_id, "lib-global").await;
        insert_unified_member(&pool, private_lib, server_id, "lib-private").await;

        insert_synced_item(&pool, server_id, "global-movie", "global", None, "orig-global", "lib-global", "Visible To Everyone").await;
        insert_synced_item(&pool, server_id, "private-movie", "user:user-1", Some("user-1"), "orig-private", "lib-private", "Visible To User One").await;

        let anonymous = service.unified_views(None).await.unwrap();
        assert_eq!(anonymous.len(), 1);
        assert_eq!(anonymous[0].name.as_deref(), Some("Global Movies"));

        let user1 = service.unified_views(Some("user-1")).await.unwrap();
        assert_eq!(user1.len(), 2);
        assert!(user1.iter().any(|item| item.name.as_deref() == Some("Global Movies")));
        assert!(user1.iter().any(|item| item.name.as_deref() == Some("Private Movies")));

        let user2 = service.unified_views(Some("user-2")).await.unwrap();
        assert_eq!(user2.len(), 1);
        assert_eq!(user2[0].name.as_deref(), Some("Global Movies"));
    }
}
