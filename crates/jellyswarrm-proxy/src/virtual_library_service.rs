use std::{cmp::Ordering, collections::HashMap, sync::Arc};

use sqlx::{Sqlite, SqlitePool, Transaction};
use tokio::sync::Mutex;
use tracing::debug;
use uuid::Uuid;

use crate::{
    media_storage_service::{MediaMapping, MediaStorageService},
    server_id::ServerId,
    server_storage::{Server, ServerStorageService},
};

pub fn normalize_library_id(id: &str) -> String {
    match Uuid::parse_str(id) {
        Ok(uuid) => uuid.simple().to_string(),
        Err(_) => id.to_string(),
    }
}

pub(crate) fn compare_virtual_library_routes(
    left_server: &Server,
    left_original_id: &str,
    right_server: &Server,
    right_original_id: &str,
) -> Ordering {
    left_server
        .priority
        .cmp(&right_server.priority)
        .then_with(|| right_server.name.cmp(&left_server.name))
        .then_with(|| right_server.id.as_i64().cmp(&left_server.id.as_i64()))
        .then_with(|| left_original_id.cmp(right_original_id))
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AutomaticVirtualLibrary {
    pub virtual_id: String,
    pub collection_type: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct VirtualLibraryMember {
    pub mapping: MediaMapping,
    pub server: Server,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualLibraryAccessScope {
    user_id: String,
    server_ids: Vec<ServerId>,
}

impl VirtualLibraryAccessScope {
    pub fn new(user_id: impl Into<String>, server_ids: impl IntoIterator<Item = ServerId>) -> Self {
        let mut server_ids = server_ids.into_iter().collect::<Vec<_>>();
        server_ids.sort_by_key(|server_id| server_id.as_i64());
        server_ids.dedup();
        Self {
            user_id: user_id.into(),
            server_ids,
        }
    }

    pub(crate) fn user_id(&self) -> &str {
        &self.user_id
    }

    pub(crate) fn allows(&self, server_id: ServerId) -> bool {
        self.server_ids.contains(&server_id)
    }
}

#[derive(Debug, Clone)]
pub struct LibraryGroup {
    pub virtual_id: String,
    pub name: String,
    pub sort_order: i32,
    pub collection_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LibraryGroupMemberRecord {
    pub group_virtual_id: String,
    pub server_id: ServerId,
    pub original_library_id: String,
    pub library_name: String,
}

#[derive(Debug, Clone)]
pub struct LibraryAssignment {
    pub group_virtual_id: String,
    pub group_name: String,
    pub group_sort_order: i32,
    pub collection_type: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AssignLibraryError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("virtual library not found")]
    GroupNotFound,
    #[error("library collection type is required")]
    MissingCollectionType,
    #[error("virtual library accepts {group_type}, not {library_type}")]
    CollectionTypeMismatch {
        group_type: String,
        library_type: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredLibrary {
    pub server_id: ServerId,
    pub original_library_id: String,
    pub name: String,
    pub collection_type: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedVirtualLibrary {
    pub name: String,
    pub members: Vec<VirtualLibraryMember>,
    pub catalog_scope_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryGrouping {
    Automatic,
    Configured,
    None,
}

#[derive(Debug, Clone)]
pub enum VirtualLibraryResolution {
    Unknown,
    Empty { name: String },
    Resolved(ResolvedVirtualLibrary),
}

#[derive(Debug, Default)]
struct AutomaticRefreshGeneration {
    latest_started: u64,
    latest_committed: u64,
}

#[derive(Debug, Clone)]
pub struct VirtualLibraryService {
    pool: SqlitePool,
    server_storage: ServerStorageService,
    media_storage: MediaStorageService,
    automatic_refresh_generations: Arc<Mutex<HashMap<String, AutomaticRefreshGeneration>>>,
}

impl VirtualLibraryService {
    pub fn new(
        pool: SqlitePool,
        server_storage: ServerStorageService,
        media_storage: MediaStorageService,
    ) -> Self {
        Self {
            pool,
            server_storage,
            media_storage,
            automatic_refresh_generations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn library_grouping(
        &self,
        automatic_merging_enabled: bool,
    ) -> Result<LibraryGrouping, sqlx::Error> {
        if automatic_merging_enabled {
            return Ok(LibraryGrouping::Automatic);
        }

        if self.has_groups().await? {
            Ok(LibraryGrouping::Configured)
        } else {
            Ok(LibraryGrouping::None)
        }
    }

    pub async fn resolve(
        &self,
        virtual_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<VirtualLibraryResolution, sqlx::Error> {
        if let Some((group, members)) = self.resolve_group(virtual_id, access_scope).await? {
            let viewer = access_scope
                .map(VirtualLibraryAccessScope::user_id)
                .unwrap_or("anonymous");
            return Ok(resolution(
                group.name,
                members,
                format!(
                    "configured:{}:{}",
                    normalize_library_id(&group.virtual_id),
                    viewer
                ),
            ));
        }

        if let Some((library, members)) = self
            .resolve_automatic_library(virtual_id, access_scope)
            .await?
        {
            let Some(access_scope) = access_scope else {
                return Ok(VirtualLibraryResolution::Empty { name: library.name });
            };
            return Ok(resolution(
                library.name,
                members,
                format!(
                    "automatic:{}:{}",
                    normalize_library_id(&library.virtual_id),
                    access_scope.user_id()
                ),
            ));
        }

        Ok(VirtualLibraryResolution::Unknown)
    }

    pub async fn routing_target(
        &self,
        virtual_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) -> Result<Option<VirtualLibraryMember>, sqlx::Error> {
        let VirtualLibraryResolution::Resolved(resolved) =
            self.resolve(virtual_id, access_scope).await?
        else {
            return Ok(None);
        };
        Ok(resolved
            .members
            .into_iter()
            .filter(|member| server_is_allowed(member.server.id, access_scope, required_server_id))
            .max_by(|left, right| {
                compare_virtual_library_routes(
                    &left.server,
                    &left.mapping.original_media_id,
                    &right.server,
                    &right.mapping.original_media_id,
                )
            }))
    }

    pub async fn get_or_create_automatic_library(
        &self,
        collection_type: &str,
        name: &str,
    ) -> Result<AutomaticVirtualLibrary, sqlx::Error> {
        let virtual_id = Uuid::new_v4().simple().to_string();
        sqlx::query(
            "INSERT INTO merged_libraries (virtual_id, collection_type, name) \
             VALUES (?, ?, ?) \
             ON CONFLICT(collection_type) DO UPDATE SET name = excluded.name",
        )
        .bind(&virtual_id)
        .bind(collection_type)
        .bind(name)
        .execute(&self.pool)
        .await?;

        // Re-fetch preserves the existing virtual ID when the merge key already exists.
        self.get_automatic_library_by_collection_type(collection_type)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }

    async fn get_automatic_library(
        &self,
        virtual_id: &str,
    ) -> Result<Option<AutomaticVirtualLibrary>, sqlx::Error> {
        let virtual_id = normalize_library_id(virtual_id);
        sqlx::query_as(
            "SELECT virtual_id, collection_type, name \
             FROM merged_libraries WHERE virtual_id = ?",
        )
        .bind(&virtual_id)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn get_automatic_library_by_collection_type(
        &self,
        collection_type: &str,
    ) -> Result<Option<AutomaticVirtualLibrary>, sqlx::Error> {
        sqlx::query_as(
            "SELECT virtual_id, collection_type, name \
             FROM merged_libraries WHERE collection_type = ?",
        )
        .bind(collection_type)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn begin_automatic_library_refresh(
        &self,
        access_scope: &VirtualLibraryAccessScope,
    ) -> u64 {
        let mut generations = self.automatic_refresh_generations.lock().await;
        let state = generations
            .entry(access_scope.user_id().to_string())
            .or_default();
        state.latest_started = state.latest_started.saturating_add(1);
        state.latest_started
    }

    pub async fn reconcile_automatic_library_members(
        &self,
        access_scope: &VirtualLibraryAccessScope,
        refresh_generation: u64,
        refreshed_server_ids: &[ServerId],
        discovered_members: &[(String, ServerId, String)],
    ) -> Result<bool, sqlx::Error> {
        let user_id = access_scope.user_id().to_string();
        let mut generation_guard = self.automatic_refresh_generations.lock().await;
        let generation_state = generation_guard.entry(user_id.clone()).or_default();
        if refresh_generation < generation_state.latest_committed {
            debug!(
                "Skipped superseded automatic library refresh {} for access scope {}",
                refresh_generation, user_id
            );
            return Ok(false);
        }
        let mut tx = self.pool.begin().await?;

        for (automatic_virtual_id, _, _) in discovered_members {
            sqlx::query(
                "INSERT INTO automatic_library_snapshots \
                 (automatic_virtual_id, user_id, updated_at) \
                 VALUES (?, ?, CURRENT_TIMESTAMP) \
                 ON CONFLICT(automatic_virtual_id, user_id) DO UPDATE SET \
                     updated_at = CURRENT_TIMESTAMP",
            )
            .bind(normalize_library_id(automatic_virtual_id))
            .bind(&user_id)
            .execute(&mut *tx)
            .await?;
        }

        for server_id in refreshed_server_ids {
            sqlx::query(
                "DELETE FROM automatic_library_members \
                 WHERE user_id = ? AND server_id = ?",
            )
            .bind(&user_id)
            .bind(server_id.as_i64())
            .execute(&mut *tx)
            .await?;
        }

        for (automatic_virtual_id, server_id, virtual_library_id) in discovered_members {
            sqlx::query(
                "INSERT INTO automatic_library_members \
                 (automatic_virtual_id, user_id, server_id, virtual_library_id) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(normalize_library_id(automatic_virtual_id))
            .bind(&user_id)
            .bind(server_id.as_i64())
            .bind(virtual_library_id)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        generation_state.latest_committed = refresh_generation;
        drop(generation_guard);
        debug!(
            "Reconciled automatic library members from {} refreshed servers for access scope {}",
            refreshed_server_ids.len(),
            user_id
        );
        Ok(true)
    }

    async fn get_automatic_library_members(
        &self,
        automatic_virtual_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Vec<VirtualLibraryMember>, sqlx::Error> {
        let Some(access_scope) = access_scope else {
            return Ok(Vec::new());
        };
        let rows: Vec<(Option<String>,)> = sqlx::query_as(
            "SELECT m.virtual_library_id \
             FROM automatic_library_snapshots snapshot \
             LEFT JOIN automatic_library_members m \
               ON m.automatic_virtual_id = snapshot.automatic_virtual_id \
               AND m.user_id = snapshot.user_id \
              WHERE snapshot.automatic_virtual_id = ? AND snapshot.user_id = ? \
               AND (m.virtual_library_id IS NULL OR NOT EXISTS ( \
                   SELECT 1 FROM media_mappings mapping \
                   JOIN library_group_members configured \
                     ON configured.server_id = m.server_id \
                    AND configured.original_library_id = mapping.original_media_id \
                   WHERE mapping.virtual_media_id = m.virtual_library_id \
               )) \
             ORDER BY m.server_id, m.virtual_library_id",
        )
        .bind(normalize_library_id(automatic_virtual_id))
        .bind(access_scope.user_id())
        .fetch_all(&self.pool)
        .await?;

        let mut members = Vec::new();
        for (virtual_library_id,) in rows {
            let Some(virtual_library_id) = virtual_library_id else {
                continue;
            };
            let Some((mapping, server)) = self
                .media_storage
                .get_media_mapping_with_server(&virtual_library_id)
                .await?
            else {
                continue;
            };
            if access_scope.allows(server.id) {
                members.push(VirtualLibraryMember { mapping, server });
            }
        }
        Ok(members)
    }

    async fn resolve_automatic_library(
        &self,
        virtual_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Option<(AutomaticVirtualLibrary, Vec<VirtualLibraryMember>)>, sqlx::Error> {
        let Some(library) = self.get_automatic_library(virtual_id).await? else {
            return Ok(None);
        };
        let members = self
            .get_automatic_library_members(&library.virtual_id, access_scope)
            .await?;
        Ok(Some((library, members)))
    }

    async fn has_groups(&self) -> Result<bool, sqlx::Error> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM library_groups")
            .fetch_one(&self.pool)
            .await?;
        Ok(count > 0)
    }

    pub async fn track_discovered_libraries(
        &self,
        libraries: &[DiscoveredLibrary],
    ) -> Result<(), sqlx::Error> {
        if libraries.is_empty() {
            return Ok(());
        }

        let mut tx = self.pool.begin().await?;
        upsert_discovered_libraries(&mut tx, libraries).await?;
        tx.commit().await
    }

    pub async fn replace_discovered_libraries(
        &self,
        server_id: ServerId,
        libraries: &[DiscoveredLibrary],
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM discovered_libraries WHERE server_id = ?")
            .bind(server_id.as_i64())
            .execute(&mut *tx)
            .await?;
        upsert_discovered_libraries(
            &mut tx,
            &libraries
                .iter()
                .filter(|library| library.server_id == server_id)
                .cloned()
                .collect::<Vec<_>>(),
        )
        .await?;
        tx.commit().await
    }

    pub async fn list_discovered_libraries(&self) -> Result<Vec<DiscoveredLibrary>, sqlx::Error> {
        let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
            "SELECT server_id, original_library_id, name, collection_type \
             FROM discovered_libraries ORDER BY server_id, name, original_library_id",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(server_id, original_library_id, name, collection_type)| DiscoveredLibrary {
                    server_id: ServerId::new(server_id),
                    original_library_id,
                    name,
                    collection_type,
                },
            )
            .collect())
    }

    pub async fn get_discovered_library(
        &self,
        server_id: ServerId,
        original_library_id: &str,
    ) -> Result<Option<DiscoveredLibrary>, sqlx::Error> {
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT original_library_id, name, collection_type \
             FROM discovered_libraries \
             WHERE server_id = ? AND original_library_id = ?",
        )
        .bind(server_id.as_i64())
        .bind(normalize_library_id(original_library_id))
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(original_library_id, name, collection_type)| DiscoveredLibrary {
                server_id,
                original_library_id,
                name,
                collection_type,
            },
        ))
    }

    pub async fn list_groups(&self) -> Result<Vec<LibraryGroup>, sqlx::Error> {
        let rows: Vec<(String, String, i32, Option<String>)> = sqlx::query_as(
            "SELECT virtual_id, name, sort_order, collection_type \
             FROM library_groups ORDER BY sort_order, name",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(virtual_id, name, sort_order, collection_type)| LibraryGroup {
                    virtual_id,
                    name,
                    sort_order,
                    collection_type,
                },
            )
            .collect())
    }

    pub async fn create_group(&self, name: &str) -> Result<LibraryGroup, sqlx::Error> {
        let virtual_id = Uuid::new_v4().simple().to_string();
        sqlx::query(
            "INSERT INTO library_groups (virtual_id, name, sort_order) \
             SELECT ?, ?, COALESCE(MAX(sort_order), -1) + 1 FROM library_groups",
        )
        .bind(&virtual_id)
        .bind(name.trim())
        .execute(&self.pool)
        .await?;

        self.get_group(&virtual_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }

    pub async fn rename_group(&self, virtual_id: &str, name: &str) -> Result<bool, sqlx::Error> {
        let virtual_id = normalize_library_id(virtual_id);
        let result = sqlx::query("UPDATE library_groups SET name = ? WHERE virtual_id = ?")
            .bind(name.trim())
            .bind(virtual_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn delete_group(&self, virtual_id: &str) -> Result<bool, sqlx::Error> {
        let virtual_id = normalize_library_id(virtual_id);
        let result = sqlx::query("DELETE FROM library_groups WHERE virtual_id = ?")
            .bind(virtual_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn add_member(
        &self,
        group_virtual_id: &str,
        server_id: ServerId,
        original_library_id: &str,
        library_name: &str,
        collection_type: &str,
    ) -> Result<(), AssignLibraryError> {
        let group_virtual_id = normalize_library_id(group_virtual_id);
        let original_library_id = normalize_library_id(original_library_id);
        let collection_type = collection_type.trim().to_ascii_lowercase();
        if collection_type.is_empty() {
            return Err(AssignLibraryError::MissingCollectionType);
        }
        let mut tx = self.pool.begin().await?;

        let previous_group: Option<(String,)> = sqlx::query_as(
            "SELECT group_virtual_id FROM library_group_members \
             WHERE server_id = ? AND original_library_id = ?",
        )
        .bind(server_id.as_i64())
        .bind(&original_library_id)
        .fetch_optional(&mut *tx)
        .await?;

        let type_update = sqlx::query(
            "UPDATE library_groups SET collection_type = COALESCE(collection_type, ?) \
             WHERE virtual_id = ? \
               AND (collection_type IS NULL OR LOWER(collection_type) = ?)",
        )
        .bind(&collection_type)
        .bind(&group_virtual_id)
        .bind(&collection_type)
        .execute(&mut *tx)
        .await?;

        if type_update.rows_affected() == 0 {
            let existing: Option<(Option<String>,)> =
                sqlx::query_as("SELECT collection_type FROM library_groups WHERE virtual_id = ?")
                    .bind(&group_virtual_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            return match existing {
                Some((Some(group_type),)) => Err(AssignLibraryError::CollectionTypeMismatch {
                    group_type,
                    library_type: collection_type,
                }),
                Some((None,)) => Err(AssignLibraryError::Database(sqlx::Error::RowNotFound)),
                None => Err(AssignLibraryError::GroupNotFound),
            };
        }

        sqlx::query(
            "INSERT INTO library_group_members \
              (group_virtual_id, server_id, original_library_id, library_name) \
              VALUES (?, ?, ?, ?) \
              ON CONFLICT(server_id, original_library_id) DO UPDATE SET \
                  group_virtual_id = excluded.group_virtual_id, \
                  library_name = excluded.library_name",
        )
        .bind(&group_virtual_id)
        .bind(server_id.as_i64())
        .bind(&original_library_id)
        .bind(library_name.trim())
        .execute(&mut *tx)
        .await?;

        if let Some((previous_group,)) = previous_group {
            if previous_group != group_virtual_id {
                reset_empty_group_type(&mut tx, &previous_group).await?;
            }
        }

        tx.commit().await?;

        debug!(
            "Assigned library {} on server {} to group {}",
            original_library_id, server_id, group_virtual_id
        );
        Ok(())
    }

    pub async fn remove_member(
        &self,
        group_virtual_id: &str,
        server_id: ServerId,
        original_library_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let group_virtual_id = normalize_library_id(group_virtual_id);
        let original_library_id = normalize_library_id(original_library_id);
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "DELETE FROM library_group_members \
             WHERE group_virtual_id = ? AND server_id = ? AND original_library_id = ?",
        )
        .bind(&group_virtual_id)
        .bind(server_id.as_i64())
        .bind(&original_library_id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() > 0 {
            reset_empty_group_type(&mut tx, &group_virtual_id).await?;
        }
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn list_members(
        &self,
        group_virtual_id: &str,
    ) -> Result<Vec<LibraryGroupMemberRecord>, sqlx::Error> {
        let group_virtual_id = normalize_library_id(group_virtual_id);
        let rows: Vec<(String, i64, String, String)> = sqlx::query_as(
            "SELECT group_virtual_id, server_id, original_library_id, library_name \
             FROM library_group_members WHERE group_virtual_id = ? \
             ORDER BY library_name, server_id, original_library_id",
        )
        .bind(group_virtual_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(group_virtual_id, server_id, original_library_id, library_name)| {
                    LibraryGroupMemberRecord {
                        group_virtual_id,
                        server_id: ServerId::new(server_id),
                        original_library_id,
                        library_name,
                    }
                },
            )
            .collect())
    }

    pub async fn get_assignments(
        &self,
    ) -> Result<HashMap<(ServerId, String), LibraryAssignment>, sqlx::Error> {
        let rows: Vec<(i64, String, String, String, i32, Option<String>)> = sqlx::query_as(
            "SELECT m.server_id, m.original_library_id, g.virtual_id, g.name, g.sort_order, \
                    g.collection_type \
             FROM library_group_members m \
             JOIN library_groups g ON g.virtual_id = m.group_virtual_id",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(
                    server_id,
                    original_library_id,
                    group_virtual_id,
                    group_name,
                    group_sort_order,
                    collection_type,
                )| {
                    (
                        (ServerId::new(server_id), original_library_id),
                        LibraryAssignment {
                            group_virtual_id,
                            group_name,
                            group_sort_order,
                            collection_type,
                        },
                    )
                },
            )
            .collect())
    }

    async fn resolve_group(
        &self,
        virtual_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Option<(LibraryGroup, Vec<VirtualLibraryMember>)>, sqlx::Error> {
        let Some(group) = self.get_group(virtual_id).await? else {
            return Ok(None);
        };

        let records = self.list_members(&group.virtual_id).await?;
        if records.is_empty() {
            return Ok(Some((group, Vec::new())));
        }

        let mut members = Vec::new();
        for record in records {
            if !server_is_allowed(record.server_id, access_scope, None) {
                continue;
            }
            let Some(server) = self
                .server_storage
                .get_server_by_id(record.server_id)
                .await?
            else {
                continue;
            };

            let mapping = self
                .media_storage
                .get_or_create_media_mapping(&record.original_library_id, &server)
                .await?;

            members.push(VirtualLibraryMember { mapping, server });
        }

        Ok(Some((group, members)))
    }

    pub async fn get_group(&self, virtual_id: &str) -> Result<Option<LibraryGroup>, sqlx::Error> {
        let virtual_id = normalize_library_id(virtual_id);
        let row: Option<(String, String, i32, Option<String>)> = sqlx::query_as(
            "SELECT virtual_id, name, sort_order, collection_type \
             FROM library_groups WHERE virtual_id = ?",
        )
        .bind(&virtual_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(virtual_id, name, sort_order, collection_type)| LibraryGroup {
                virtual_id,
                name,
                sort_order,
                collection_type,
            },
        ))
    }
}

async fn reset_empty_group_type(
    tx: &mut Transaction<'_, Sqlite>,
    group_virtual_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE library_groups SET collection_type = NULL \
         WHERE virtual_id = ? \
           AND NOT EXISTS ( \
               SELECT 1 FROM library_group_members WHERE group_virtual_id = ? \
           )",
    )
    .bind(group_virtual_id)
    .bind(group_virtual_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn upsert_discovered_libraries(
    tx: &mut Transaction<'_, Sqlite>,
    libraries: &[DiscoveredLibrary],
) -> Result<(), sqlx::Error> {
    for library in libraries {
        sqlx::query(
            "INSERT INTO discovered_libraries \
             (server_id, original_library_id, name, collection_type, last_seen_at) \
             VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(server_id, original_library_id) DO UPDATE SET \
                 name = excluded.name, \
                 collection_type = excluded.collection_type, \
                 last_seen_at = CURRENT_TIMESTAMP",
        )
        .bind(library.server_id.as_i64())
        .bind(normalize_library_id(&library.original_library_id))
        .bind(&library.name)
        .bind(&library.collection_type)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

fn resolution(
    name: String,
    members: Vec<VirtualLibraryMember>,
    catalog_scope_key: String,
) -> VirtualLibraryResolution {
    if members.is_empty() {
        VirtualLibraryResolution::Empty { name }
    } else {
        VirtualLibraryResolution::Resolved(ResolvedVirtualLibrary {
            name,
            members,
            catalog_scope_key,
        })
    }
}

fn server_is_allowed(
    server_id: ServerId,
    access_scope: Option<&VirtualLibraryAccessScope>,
    required_server_id: Option<ServerId>,
) -> bool {
    required_server_id.is_none_or(|required| required == server_id)
        && access_scope.is_none_or(|scope| scope.allows(server_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::MIGRATOR, server_storage::ServerStorageService};

    struct Fixture {
        pool: SqlitePool,
        service: VirtualLibraryService,
    }

    impl Fixture {
        async fn new(servers: &[(i64, i32)]) -> Self {
            let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
            MIGRATOR.run(&pool).await.unwrap();
            for &(id, priority) in servers {
                sqlx::query(
                    "INSERT INTO servers \
                     (id, name, url, priority, media_streaming_mode, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, 'Redirect', datetime('now'), datetime('now'))",
                )
                .bind(id)
                .bind(format!("Server {id}"))
                .bind(format!("http://server-{id}:8096"))
                .bind(priority)
                .execute(&pool)
                .await
                .unwrap();
            }
            let service = VirtualLibraryService::new(
                pool.clone(),
                ServerStorageService::new(pool.clone()),
                MediaStorageService::new(pool.clone()),
            );
            Self { pool, service }
        }

        async fn member(&self, server_id: i64, original_id: &str) -> (ServerId, String) {
            let server_id = ServerId::new(server_id);
            let server = self
                .service
                .server_storage
                .get_server_by_id(server_id)
                .await
                .unwrap()
                .unwrap();
            let mapping = self
                .service
                .media_storage
                .get_or_create_media_mapping(original_id, &server)
                .await
                .unwrap();
            (server_id, mapping.virtual_media_id)
        }

        async fn automatic_library(&self) -> AutomaticVirtualLibrary {
            self.service
                .get_or_create_automatic_library("movies:anime", "Anime")
                .await
                .unwrap()
        }

        async fn configured_group(&self, members: &[(i64, &str)]) -> LibraryGroup {
            let group = self.service.create_group("Anime").await.unwrap();
            for &(server_id, original_id) in members {
                self.service
                    .add_member(
                        &group.virtual_id,
                        ServerId::new(server_id),
                        original_id,
                        "Anime",
                        "movies",
                    )
                    .await
                    .unwrap();
            }
            group
        }

        async fn route(
            &self,
            virtual_id: &str,
            scope: &VirtualLibraryAccessScope,
            required_server: Option<ServerId>,
        ) -> VirtualLibraryMember {
            self.service
                .routing_target(virtual_id, Some(scope), required_server)
                .await
                .unwrap()
                .unwrap()
        }

        async fn snapshot(
            &self,
            library: &AutomaticVirtualLibrary,
            scope: &VirtualLibraryAccessScope,
            members: &[(ServerId, String)],
        ) {
            let generation = self.service.begin_automatic_library_refresh(scope).await;
            let mut server_ids = members
                .iter()
                .map(|(server_id, _)| *server_id)
                .collect::<Vec<_>>();
            server_ids.sort_by_key(|server_id| server_id.as_i64());
            server_ids.dedup();
            let discovered = members
                .iter()
                .map(|(server_id, member_id)| {
                    (library.virtual_id.clone(), *server_id, member_id.clone())
                })
                .collect::<Vec<_>>();
            self.service
                .reconcile_automatic_library_members(scope, generation, &server_ids, &discovered)
                .await
                .unwrap();
        }

        async fn assigned_group_id(&self, server_id: i64, original_id: &str) -> String {
            self.service
                .get_assignments()
                .await
                .unwrap()
                .get(&(ServerId::new(server_id), original_id.to_string()))
                .unwrap()
                .group_virtual_id
                .clone()
        }

        fn reloaded_service(&self) -> VirtualLibraryService {
            VirtualLibraryService::new(
                self.pool.clone(),
                ServerStorageService::new(self.pool.clone()),
                MediaStorageService::new(self.pool.clone()),
            )
        }
    }

    fn scope(user_id: &str, server_ids: &[i64]) -> VirtualLibraryAccessScope {
        VirtualLibraryAccessScope::new(user_id, server_ids.iter().copied().map(ServerId::new))
    }

    fn discovered_library(server_id: i64, id: &str, name: &str) -> DiscoveredLibrary {
        DiscoveredLibrary {
            server_id: ServerId::new(server_id),
            original_library_id: id.to_string(),
            name: name.to_string(),
            collection_type: "movies".to_string(),
        }
    }

    #[tokio::test]
    async fn tracked_library_metadata_is_updated() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        fixture
            .service
            .track_discovered_libraries(&[discovered_library(1, "library", "Old name")])
            .await
            .unwrap();

        fixture
            .service
            .track_discovered_libraries(&[discovered_library(1, "library", "Movies")])
            .await
            .unwrap();

        assert_eq!(
            fixture.service.list_discovered_libraries().await.unwrap(),
            vec![discovered_library(1, "library", "Movies")]
        );
    }

    #[tokio::test]
    async fn authoritative_discovery_replaces_only_its_server() {
        let fixture = Fixture::new(&[(1, 100), (2, 200)]).await;
        fixture
            .service
            .track_discovered_libraries(&[
                discovered_library(1, "stale", "Stale"),
                discovered_library(2, "other", "Other"),
            ])
            .await
            .unwrap();

        fixture
            .service
            .replace_discovered_libraries(
                ServerId::new(1),
                &[discovered_library(1, "current", "Current")],
            )
            .await
            .unwrap();

        assert_eq!(
            fixture.service.list_discovered_libraries().await.unwrap(),
            vec![
                discovered_library(1, "current", "Current"),
                discovered_library(2, "other", "Other"),
            ]
        );
    }

    #[tokio::test]
    async fn create_group_and_assign_member() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let service = &fixture.service;
        let group = service.create_group("Anime").await.unwrap();
        service
            .add_member(
                &group.virtual_id,
                ServerId::new(1),
                "abc-def-1234-5678-901234567890",
                "Anime",
                "movies",
            )
            .await
            .unwrap();

        assert_eq!(
            service
                .get_assignments()
                .await
                .unwrap()
                .values()
                .next()
                .map(|assignment| assignment.group_name.as_str()),
            Some("Anime")
        );
    }

    #[tokio::test]
    async fn first_member_sets_virtual_library_collection_type() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let group = fixture.service.create_group("Movies").await.unwrap();

        fixture
            .service
            .add_member(
                &group.virtual_id,
                ServerId::new(1),
                "library",
                "Movies",
                "movies",
            )
            .await
            .unwrap();

        assert_eq!(
            fixture
                .service
                .get_group(&group.virtual_id)
                .await
                .unwrap()
                .and_then(|group| group.collection_type),
            Some("movies".to_string())
        );
    }

    #[tokio::test]
    async fn incompatible_member_is_rejected_without_moving_existing_assignment() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let movies = fixture.service.create_group("Movies").await.unwrap();
        let shows = fixture.service.create_group("Shows").await.unwrap();
        fixture
            .service
            .add_member(
                &movies.virtual_id,
                ServerId::new(1),
                "movie-library",
                "Movies",
                "movies",
            )
            .await
            .unwrap();
        fixture
            .service
            .add_member(
                &shows.virtual_id,
                ServerId::new(1),
                "show-library",
                "Shows",
                "tvshows",
            )
            .await
            .unwrap();

        let error = fixture
            .service
            .add_member(
                &shows.virtual_id,
                ServerId::new(1),
                "movie-library",
                "Movies",
                "movies",
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AssignLibraryError::CollectionTypeMismatch { .. }
        ));
        assert_eq!(
            fixture.assigned_group_id(1, "movie-library").await,
            movies.virtual_id
        );
    }

    #[tokio::test]
    async fn moving_last_member_clears_previous_group_collection_type() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let first = fixture.service.create_group("First").await.unwrap();
        let second = fixture.service.create_group("Second").await.unwrap();
        fixture
            .service
            .add_member(
                &first.virtual_id,
                ServerId::new(1),
                "library",
                "Movies",
                "movies",
            )
            .await
            .unwrap();

        fixture
            .service
            .add_member(
                &second.virtual_id,
                ServerId::new(1),
                "library",
                "Movies",
                "movies",
            )
            .await
            .unwrap();

        assert_eq!(
            fixture
                .service
                .get_group(&first.virtual_id)
                .await
                .unwrap()
                .and_then(|group| group.collection_type),
            None
        );
    }

    #[tokio::test]
    async fn removing_last_member_clears_group_collection_type() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let group = fixture.service.create_group("Movies").await.unwrap();
        fixture
            .service
            .add_member(
                &group.virtual_id,
                ServerId::new(1),
                "library",
                "Movies",
                "movies",
            )
            .await
            .unwrap();

        fixture
            .service
            .remove_member(&group.virtual_id, ServerId::new(1), "library")
            .await
            .unwrap();

        assert_eq!(
            fixture
                .service
                .get_group(&group.virtual_id)
                .await
                .unwrap()
                .and_then(|group| group.collection_type),
            None
        );
    }

    #[tokio::test]
    async fn library_grouping_is_none_without_groups() {
        let fixture = Fixture::new(&[]).await;
        let grouping = fixture.service.library_grouping(false).await.unwrap();

        assert_eq!(grouping, LibraryGrouping::None);
    }

    #[tokio::test]
    async fn library_grouping_is_configured_when_groups_exist() {
        let fixture = Fixture::new(&[]).await;
        let service = &fixture.service;
        service.create_group("Anime").await.unwrap();

        let grouping = service.library_grouping(false).await.unwrap();

        assert_eq!(grouping, LibraryGrouping::Configured);
    }

    #[tokio::test]
    async fn library_grouping_is_automatic_when_enabled() {
        let fixture = Fixture::new(&[]).await;
        let service = &fixture.service;
        service.create_group("Anime").await.unwrap();

        let grouping = service.library_grouping(true).await.unwrap();

        assert_eq!(grouping, LibraryGrouping::Automatic);
    }

    #[tokio::test]
    async fn resolve_returns_empty_for_known_empty_group() {
        let fixture = Fixture::new(&[]).await;
        let service = &fixture.service;
        let group = service.create_group("Anime").await.unwrap();

        let resolution = service.resolve(&group.virtual_id, None).await.unwrap();

        assert!(matches!(
            resolution,
            VirtualLibraryResolution::Empty { name } if name == "Anime"
        ));
    }

    #[tokio::test]
    async fn add_member_replaces_existing_source_assignment() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let service = &fixture.service;
        let first = service.create_group("Anime").await.unwrap();
        let second = service.create_group("Movies").await.unwrap();
        service
            .add_member(
                &first.virtual_id,
                ServerId::new(1),
                "library",
                "Anime",
                "movies",
            )
            .await
            .unwrap();

        service
            .add_member(
                &second.virtual_id,
                ServerId::new(1),
                "library",
                "Movies",
                "movies",
            )
            .await
            .unwrap();

        assert_eq!(
            fixture.assigned_group_id(1, "library").await,
            second.virtual_id
        );
    }

    #[tokio::test]
    async fn failed_reassignment_preserves_existing_assignment() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let service = &fixture.service;
        let group = service.create_group("Anime").await.unwrap();
        service
            .add_member(
                &group.virtual_id,
                ServerId::new(1),
                "library",
                "Anime",
                "movies",
            )
            .await
            .unwrap();

        let result = service
            .add_member(
                "missing-group",
                ServerId::new(1),
                "library",
                "Anime",
                "movies",
            )
            .await;

        assert!(result.is_err());
        assert_eq!(
            fixture.assigned_group_id(1, "library").await,
            group.virtual_id
        );
    }

    #[tokio::test]
    async fn automatic_virtual_library_routes_to_member() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let member = fixture.member(1, "automatic-library").await;
        let scope = scope("user", &[1]);
        let library = fixture.automatic_library().await;
        fixture
            .snapshot(&library, &scope, std::slice::from_ref(&member))
            .await;

        let target = fixture.route(&library.virtual_id, &scope, None).await;

        assert_eq!(target.mapping.virtual_media_id, member.1);
    }

    #[tokio::test]
    async fn automatic_virtual_library_keeps_id_and_refreshes_name() {
        let fixture = Fixture::new(&[]).await;
        let service = &fixture.service;
        let original = service
            .get_or_create_automatic_library("movies:anime", "Anime")
            .await
            .unwrap();

        let updated = service
            .get_or_create_automatic_library("movies:anime", "Animation")
            .await
            .unwrap();

        assert_eq!(
            (updated.virtual_id, updated.collection_type, updated.name),
            (
                original.virtual_id,
                "movies:anime".to_string(),
                "Animation".to_string()
            )
        );
    }

    #[tokio::test]
    async fn automatic_library_resolves_when_configured_groups_exist() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let service = &fixture.service;
        let automatic = fixture.automatic_library().await;
        let scope = scope("user", &[1]);
        let member = fixture.member(1, "member-id").await;
        fixture.snapshot(&automatic, &scope, &[member]).await;
        service.create_group("Configured group").await.unwrap();

        let resolution = service
            .resolve(&automatic.virtual_id, Some(&scope))
            .await
            .unwrap();

        let VirtualLibraryResolution::Resolved(resolved) = resolution else {
            panic!("expected resolved automatic library");
        };
        assert_eq!(resolved.name, automatic.name);
    }

    #[tokio::test]
    async fn configured_virtual_library_routes_to_member() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let original_library_id = "library-a";
        let group = fixture.configured_group(&[(1, original_library_id)]).await;
        let scope = scope("user", &[1]);

        let target = fixture.route(&group.virtual_id, &scope, None).await;

        assert_eq!(target.mapping.original_media_id, original_library_id);
    }

    #[tokio::test]
    async fn configured_routing_target_uses_server_priority() {
        let fixture = Fixture::new(&[(1, 100), (2, 200)]).await;
        let group = fixture
            .configured_group(&[(1, "library-a"), (2, "library-b")])
            .await;
        let scope = scope("user", &[1, 2]);

        let target = fixture.route(&group.virtual_id, &scope, None).await;

        assert_eq!(target.server.id, ServerId::new(2));
    }

    #[tokio::test]
    async fn automatic_memberships_follow_user_across_available_server_sets() {
        let fixture = Fixture::new(&[(1, 100), (2, 200), (3, 300)]).await;
        let library = fixture.automatic_library().await;
        let first_scope = scope("user", &[1, 2]);
        let second_scope = scope("user", &[2, 3]);
        let member_a = fixture.member(1, "member-a").await;
        let member_b = fixture.member(2, "member-b").await;
        fixture
            .snapshot(
                &library,
                &first_scope,
                &[member_a.clone(), member_b.clone()],
            )
            .await;

        let reloaded_service = fixture.reloaded_service();
        let first = reloaded_service
            .resolve(&library.virtual_id, Some(&first_scope))
            .await
            .unwrap();
        let second = reloaded_service
            .resolve(&library.virtual_id, Some(&second_scope))
            .await
            .unwrap();

        assert_resolved_members(first, &[&member_a.1, &member_b.1]);
        assert_resolved_members(second, &[&member_b.1]);
    }

    #[tokio::test]
    async fn automatic_memberships_exclude_manually_assigned_libraries() {
        let fixture = Fixture::new(&[(1, 100), (2, 200)]).await;
        let library = fixture.automatic_library().await;
        let scope = scope("user", &[1, 2]);
        let assigned_member = fixture.member(1, "assigned-library").await;
        let automatic_member = fixture.member(2, "automatic-library").await;
        fixture
            .snapshot(
                &library,
                &scope,
                &[assigned_member, automatic_member.clone()],
            )
            .await;
        fixture.configured_group(&[(1, "assigned-library")]).await;

        let resolution = fixture
            .service
            .resolve(&library.virtual_id, Some(&scope))
            .await
            .unwrap();

        assert_resolved_members(resolution, &[&automatic_member.1]);
    }

    #[tokio::test]
    async fn automatic_membership_returns_when_manual_assignment_is_removed() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let library = fixture.automatic_library().await;
        let scope = scope("user", &[1]);
        let member = fixture.member(1, "library").await;
        fixture
            .snapshot(&library, &scope, std::slice::from_ref(&member))
            .await;
        let group = fixture.configured_group(&[(1, "library")]).await;

        fixture
            .service
            .remove_member(&group.virtual_id, ServerId::new(1), "library")
            .await
            .unwrap();
        let resolution = fixture
            .service
            .resolve(&library.virtual_id, Some(&scope))
            .await
            .unwrap();

        assert_resolved_members(resolution, &[&member.1]);
    }

    #[tokio::test]
    async fn automatic_memberships_are_isolated_by_user_for_same_server_set() {
        let fixture = Fixture::new(&[(1, 100), (2, 200)]).await;
        let service = &fixture.service;
        let library = fixture.automatic_library().await;
        let first_scope = scope("user-a", &[1, 2]);
        let second_scope = scope("user-b", &[1, 2]);
        let member_a = fixture.member(1, "member-a").await;
        let member_b = fixture.member(2, "member-b").await;
        fixture
            .snapshot(
                &library,
                &first_scope,
                &[member_a.clone(), member_b.clone()],
            )
            .await;
        fixture
            .snapshot(&library, &second_scope, std::slice::from_ref(&member_b))
            .await;

        let first = service
            .resolve(&library.virtual_id, Some(&first_scope))
            .await
            .unwrap();
        let second = service
            .resolve(&library.virtual_id, Some(&second_scope))
            .await
            .unwrap();

        assert_resolved_members(first, &[&member_a.1, &member_b.1]);
        assert_resolved_members(second, &[&member_b.1]);
    }

    #[tokio::test]
    async fn automatic_routing_uses_required_server_with_full_access_scope() {
        let fixture = Fixture::new(&[(1, 100), (2, 200)]).await;
        let member_a = fixture.member(1, "library-a").await;
        let member_b = fixture.member(2, "library-b").await;
        let library = fixture.automatic_library().await;
        let scope = scope("user", &[1, 2]);
        fixture
            .snapshot(&library, &scope, &[member_a.clone(), member_b])
            .await;

        let target = fixture
            .route(&library.virtual_id, &scope, Some(member_a.0))
            .await;

        assert_eq!(target.server.id, member_a.0);
    }

    #[tokio::test]
    async fn automatic_snapshot_keeps_multiple_members_from_same_server() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let service = &fixture.service;
        let library = fixture.automatic_library().await;
        let scope = scope("user", &[1]);
        let members = [
            fixture.member(1, "member-a").await,
            fixture.member(1, "member-b").await,
        ];
        fixture.snapshot(&library, &scope, &members).await;

        let resolution = service
            .resolve(&library.virtual_id, Some(&scope))
            .await
            .unwrap();

        assert_resolved_members(resolution, &[&members[0].1, &members[1].1]);
    }

    #[tokio::test]
    async fn automatic_library_does_not_fall_back_to_legacy_members() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let service = &fixture.service;
        let library = fixture.automatic_library().await;
        sqlx::query(
            "INSERT INTO merged_library_members \
             (merged_virtual_id, server_url, virtual_library_id) VALUES (?, ?, ?)",
        )
        .bind(&library.virtual_id)
        .bind("http://server-1:8096")
        .bind("legacy-member")
        .execute(&fixture.pool)
        .await
        .unwrap();
        let scope = scope("user", &[1]);

        let resolution = service
            .resolve(&library.virtual_id, Some(&scope))
            .await
            .unwrap();

        assert_empty_automatic(resolution);
    }

    #[tokio::test]
    async fn reconciliation_clears_library_missing_from_successful_refresh() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let service = &fixture.service;
        let library = fixture.automatic_library().await;
        let scope = scope("user", &[1]);
        let member = fixture.member(1, "member-a").await;
        fixture.snapshot(&library, &scope, &[member]).await;

        let generation = service.begin_automatic_library_refresh(&scope).await;
        service
            .reconcile_automatic_library_members(&scope, generation, &[ServerId::new(1)], &[])
            .await
            .unwrap();
        let resolution = service
            .resolve(&library.virtual_id, Some(&scope))
            .await
            .unwrap();

        assert_empty_automatic(resolution);
    }

    #[tokio::test]
    async fn partial_reconciliation_replaces_only_refreshed_server_members() {
        let fixture = Fixture::new(&[(1, 100), (2, 200)]).await;
        let service = &fixture.service;
        let library = fixture.automatic_library().await;
        let scope = scope("user", &[1, 2]);
        let stale_member = fixture.member(1, "stale-library").await;
        let unavailable_member = fixture.member(2, "unavailable-library").await;
        fixture
            .snapshot(
                &library,
                &scope,
                &[stale_member, unavailable_member.clone()],
            )
            .await;
        let replacement = fixture.member(1, "replacement-library").await;
        let refresh_generation = service.begin_automatic_library_refresh(&scope).await;

        service
            .reconcile_automatic_library_members(
                &scope,
                refresh_generation,
                &[ServerId::new(1)],
                &[(
                    library.virtual_id.clone(),
                    replacement.0,
                    replacement.1.clone(),
                )],
            )
            .await
            .unwrap();
        let resolution = service
            .resolve(&library.virtual_id, Some(&scope))
            .await
            .unwrap();

        assert_resolved_members(resolution, &[&replacement.1, &unavailable_member.1]);
    }

    #[tokio::test]
    async fn partial_reconciliation_removes_missing_refreshed_server_members() {
        let fixture = Fixture::new(&[(1, 100), (2, 200)]).await;
        let service = &fixture.service;
        let library = fixture.automatic_library().await;
        let scope = scope("user", &[1, 2]);
        let removed_member = fixture.member(1, "removed-library").await;
        let unavailable_member = fixture.member(2, "unavailable-library").await;
        fixture
            .snapshot(
                &library,
                &scope,
                &[removed_member, unavailable_member.clone()],
            )
            .await;
        let refresh_generation = service.begin_automatic_library_refresh(&scope).await;

        service
            .reconcile_automatic_library_members(
                &scope,
                refresh_generation,
                &[ServerId::new(1)],
                &[],
            )
            .await
            .unwrap();
        let resolution = service
            .resolve(&library.virtual_id, Some(&scope))
            .await
            .unwrap();

        assert_resolved_members(resolution, &[&unavailable_member.1]);
    }

    #[tokio::test]
    async fn superseded_refresh_cannot_overwrite_newer_members() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let service = &fixture.service;
        let library = fixture.automatic_library().await;
        let scope = scope("user", &[1]);
        let stale_member = fixture.member(1, "stale-library").await;
        let current_member = fixture.member(1, "current-library").await;
        let stale_generation = service.begin_automatic_library_refresh(&scope).await;
        let current_generation = service.begin_automatic_library_refresh(&scope).await;

        let current_applied = service
            .reconcile_automatic_library_members(
                &scope,
                current_generation,
                &[ServerId::new(1)],
                &[(
                    library.virtual_id.clone(),
                    current_member.0,
                    current_member.1.clone(),
                )],
            )
            .await
            .unwrap();
        let stale_applied = service
            .reconcile_automatic_library_members(
                &scope,
                stale_generation,
                &[ServerId::new(1)],
                &[(library.virtual_id.clone(), stale_member.0, stale_member.1)],
            )
            .await
            .unwrap();
        let resolution = service
            .resolve(&library.virtual_id, Some(&scope))
            .await
            .unwrap();

        assert!(current_applied);
        assert!(!stale_applied);
        assert_resolved_members(resolution, &[&current_member.1]);
    }

    #[tokio::test]
    async fn failed_newer_refresh_does_not_discard_older_success() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let service = &fixture.service;
        let library = fixture.automatic_library().await;
        let scope = scope("user", &[1]);
        let member = fixture.member(1, "discovered-library").await;
        let successful_generation = service.begin_automatic_library_refresh(&scope).await;
        let _failed_generation = service.begin_automatic_library_refresh(&scope).await;

        let applied = service
            .reconcile_automatic_library_members(
                &scope,
                successful_generation,
                &[ServerId::new(1)],
                &[(library.virtual_id.clone(), member.0, member.1.clone())],
            )
            .await
            .unwrap();
        let resolution = service
            .resolve(&library.virtual_id, Some(&scope))
            .await
            .unwrap();

        assert!(applied);
        assert_resolved_members(resolution, &[&member.1]);
    }

    #[tokio::test]
    async fn routing_target_only_uses_authorized_servers() {
        let fixture = Fixture::new(&[(1, 100), (2, 200)]).await;
        let group = fixture
            .configured_group(&[(1, "library-a"), (2, "library-b")])
            .await;
        let scope = scope("user", &[2]);

        let target = fixture.route(&group.virtual_id, &scope, None).await;

        assert_eq!(target.server.id, ServerId::new(2));
    }

    fn assert_resolved_members(resolution: VirtualLibraryResolution, expected: &[&str]) {
        let VirtualLibraryResolution::Resolved(resolved) = resolution else {
            panic!("expected resolved virtual library");
        };
        let mut actual = resolved
            .members
            .into_iter()
            .map(|member| member.mapping.virtual_media_id)
            .collect::<Vec<_>>();
        let mut expected = expected.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        actual.sort_unstable();
        expected.sort_unstable();
        assert_eq!(actual, expected);
    }

    fn assert_empty_automatic(resolution: VirtualLibraryResolution) {
        assert!(matches!(resolution, VirtualLibraryResolution::Empty { .. }));
    }
}
