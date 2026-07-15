use std::{collections::HashMap, sync::Arc, time::Duration};

use moka::future::Cache;
use sqlx::SqlitePool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    duplicate_policy::DuplicatePolicy,
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

#[derive(Debug, Clone)]
pub struct AutomaticVirtualLibrary {
    pub virtual_id: String,
    pub collection_type: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualLibraryMember {
    pub server_url: String,
    pub virtual_library_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AutomaticMembershipCacheKey {
    virtual_id: String,
    access_scope_key: String,
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

    fn key(&self) -> String {
        format!("{}:{}", self.user_id, server_set_key(&self.server_ids))
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
    pub duplicate_policy: DuplicatePolicy,
    pub preferred_server_id: Option<ServerId>,
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
}

#[derive(Debug, Clone)]
pub enum VirtualLibrary {
    Automatic(AutomaticVirtualLibrary),
    Manual(LibraryGroup),
}

#[derive(Debug, Clone)]
pub struct ResolvedVirtualLibrary {
    pub library: VirtualLibrary,
    pub members: Vec<VirtualLibraryMember>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualLibraryMode {
    Automatic,
    Manual,
    Disabled,
}

#[derive(Debug, Clone)]
pub enum VirtualLibraryResolution {
    Unknown,
    Empty(VirtualLibrary),
    Resolved(ResolvedVirtualLibrary),
}

#[derive(Debug, Clone)]
pub struct VirtualLibraryRoutingTarget {
    pub mapping: MediaMapping,
    pub server: Server,
}

#[derive(Debug, Clone)]
pub struct VirtualLibraryService {
    pool: SqlitePool,
    server_storage: ServerStorageService,
    media_storage: MediaStorageService,
    automatic_membership_cache: Cache<AutomaticMembershipCacheKey, Vec<VirtualLibraryMember>>,
    automatic_membership_locks:
        Arc<tokio::sync::Mutex<HashMap<AutomaticMembershipCacheKey, Arc<tokio::sync::Mutex<()>>>>>,
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
            automatic_membership_cache: Cache::builder()
                .time_to_live(Duration::from_secs(60 * 30))
                .max_capacity(10_000)
                .build(),
            automatic_membership_locks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    async fn automatic_membership_lock(
        &self,
        cache_key: &AutomaticMembershipCacheKey,
    ) -> Arc<tokio::sync::Mutex<()>> {
        self.automatic_membership_locks
            .lock()
            .await
            .entry(cache_key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    pub async fn presentation_mode(
        &self,
        automatic_merging_enabled: bool,
    ) -> Result<VirtualLibraryMode, sqlx::Error> {
        if automatic_merging_enabled {
            return Ok(VirtualLibraryMode::Automatic);
        }

        if self.has_groups().await? {
            Ok(VirtualLibraryMode::Manual)
        } else {
            Ok(VirtualLibraryMode::Disabled)
        }
    }

    pub async fn resolve(
        &self,
        virtual_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<VirtualLibraryResolution, sqlx::Error> {
        if let Some((group, members)) = self.resolve_group(virtual_id, access_scope).await? {
            let library = VirtualLibrary::Manual(group);
            return if members.is_empty() {
                Ok(VirtualLibraryResolution::Empty(library))
            } else {
                Ok(VirtualLibraryResolution::Resolved(ResolvedVirtualLibrary {
                    library,
                    members,
                }))
            };
        }

        if let Some((library, members)) = self
            .resolve_automatic_library(virtual_id, access_scope)
            .await?
        {
            let library = VirtualLibrary::Automatic(library);
            return if members.is_empty() {
                Ok(VirtualLibraryResolution::Empty(library))
            } else {
                Ok(VirtualLibraryResolution::Resolved(ResolvedVirtualLibrary {
                    library,
                    members,
                }))
            };
        }

        Ok(VirtualLibraryResolution::Unknown)
    }

    pub async fn routing_target(
        &self,
        virtual_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) -> Result<Option<VirtualLibraryRoutingTarget>, sqlx::Error> {
        if let Some(group) = self.get_group(virtual_id).await? {
            let group_virtual_id = normalize_library_id(&group.virtual_id);
            let members: Vec<(i64, String)> = sqlx::query_as(
                "SELECT m.server_id, m.original_library_id \
                 FROM library_group_members m \
                 JOIN library_groups g ON g.virtual_id = m.group_virtual_id \
                 JOIN servers s ON s.id = m.server_id \
                 WHERE m.group_virtual_id = ? \
                 ORDER BY CASE WHEN g.duplicate_policy = 'PreferServer' \
                                         AND g.preferred_server_id = m.server_id THEN 0 ELSE 1 END, \
                           s.priority DESC, s.name, m.server_id, m.original_library_id",
            )
            .bind(group_virtual_id)
            .fetch_all(&self.pool)
            .await?;

            for (server_id, original_library_id) in members {
                let server_id = ServerId::new(server_id);
                if !server_is_allowed(server_id, access_scope, required_server_id) {
                    continue;
                }
                let Some(server) = self.server_storage.get_server_by_id(server_id).await? else {
                    continue;
                };
                let mapping = self
                    .media_storage
                    .get_or_create_media_mapping(&original_library_id, &server)
                    .await?;
                return Ok(Some(VirtualLibraryRoutingTarget { mapping, server }));
            }

            return Ok(None);
        }

        let mut targets = Vec::new();
        for member in self
            .get_automatic_library_members(virtual_id, access_scope)
            .await?
        {
            if let Some((mapping, server)) = self
                .media_storage
                .get_media_mapping_with_server(&member.virtual_library_id)
                .await?
            {
                if !server_is_allowed(server.id, access_scope, required_server_id) {
                    continue;
                }
                targets.push(VirtualLibraryRoutingTarget { mapping, server });
            }
        }

        targets.sort_by(|left, right| {
            right
                .server
                .priority
                .cmp(&left.server.priority)
                .then_with(|| left.server.name.cmp(&right.server.name))
                .then_with(|| left.server.id.as_i64().cmp(&right.server.id.as_i64()))
        });
        Ok(targets.into_iter().next())
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

    pub async fn has_automatic_library_snapshot(
        &self,
        automatic_virtual_id: &str,
        access_scope: &VirtualLibraryAccessScope,
    ) -> Result<bool, sqlx::Error> {
        Ok(self
            .get_scoped_automatic_library_members(
                &normalize_library_id(automatic_virtual_id),
                &access_scope.key(),
            )
            .await?
            .is_some())
    }

    pub async fn clear_automatic_library_snapshot(
        &self,
        collection_type: &str,
        access_scope: &VirtualLibraryAccessScope,
    ) -> Result<(), sqlx::Error> {
        let Some(library) = self
            .get_automatic_library_by_collection_type(collection_type)
            .await?
        else {
            return Ok(());
        };
        let cache_key = AutomaticMembershipCacheKey {
            virtual_id: library.virtual_id.clone(),
            access_scope_key: access_scope.key(),
        };
        let write_lock = self.automatic_membership_lock(&cache_key).await;
        let _guard = write_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO automatic_library_snapshots \
             (automatic_virtual_id, access_scope_key, updated_at) \
             VALUES (?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(automatic_virtual_id, access_scope_key) DO UPDATE SET \
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&library.virtual_id)
        .bind(&cache_key.access_scope_key)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM automatic_library_members \
             WHERE automatic_virtual_id = ? AND access_scope_key = ?",
        )
        .bind(&library.virtual_id)
        .bind(&cache_key.access_scope_key)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.automatic_membership_cache
            .insert(cache_key, Vec::new())
            .await;
        Ok(())
    }

    pub async fn reconcile_automatic_library_snapshots(
        &self,
        access_scope: &VirtualLibraryAccessScope,
        active_collection_types: &[String],
    ) -> Result<(), sqlx::Error> {
        let collection_types: Vec<(String,)> = sqlx::query_as(
            "SELECT l.collection_type \
             FROM automatic_library_snapshots s \
             JOIN merged_libraries l ON l.virtual_id = s.automatic_virtual_id \
             WHERE s.access_scope_key = ? \
               AND EXISTS ( \
                   SELECT 1 FROM automatic_library_members m \
                   WHERE m.automatic_virtual_id = s.automatic_virtual_id \
                     AND m.access_scope_key = s.access_scope_key \
               )",
        )
        .bind(access_scope.key())
        .fetch_all(&self.pool)
        .await?;

        for (collection_type,) in collection_types {
            if !active_collection_types.contains(&collection_type) {
                self.clear_automatic_library_snapshot(&collection_type, access_scope)
                    .await?;
            }
        }
        Ok(())
    }

    async fn get_automatic_library(
        &self,
        virtual_id: &str,
    ) -> Result<Option<AutomaticVirtualLibrary>, sqlx::Error> {
        let virtual_id = normalize_library_id(virtual_id);
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT virtual_id, collection_type, name \
             FROM merged_libraries WHERE virtual_id = ?",
        )
        .bind(&virtual_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(virtual_id, collection_type, name)| AutomaticVirtualLibrary {
                virtual_id,
                collection_type,
                name,
            },
        ))
    }

    pub async fn get_automatic_library_by_collection_type(
        &self,
        collection_type: &str,
    ) -> Result<Option<AutomaticVirtualLibrary>, sqlx::Error> {
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT virtual_id, collection_type, name \
             FROM merged_libraries WHERE collection_type = ?",
        )
        .bind(collection_type)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(virtual_id, collection_type, name)| AutomaticVirtualLibrary {
                virtual_id,
                collection_type,
                name,
            },
        ))
    }

    pub async fn upsert_automatic_library_members(
        &self,
        automatic_virtual_id: &str,
        access_scope: &VirtualLibraryAccessScope,
        members: &[(ServerId, String, String)],
    ) -> Result<(), sqlx::Error> {
        let automatic_virtual_id = normalize_library_id(automatic_virtual_id);
        let access_scope_key = access_scope.key();
        let cache_key = AutomaticMembershipCacheKey {
            virtual_id: automatic_virtual_id.clone(),
            access_scope_key: access_scope_key.clone(),
        };
        let write_lock = self.automatic_membership_lock(&cache_key).await;
        let _guard = write_lock.lock().await;
        let mut scoped_members = members.to_vec();
        scoped_members.sort_by(|left, right| {
            left.0
                .as_i64()
                .cmp(&right.0.as_i64())
                .then_with(|| left.2.cmp(&right.2))
        });
        scoped_members.dedup_by_key(|member| member.0);
        let mut canonical_members = scoped_members
            .iter()
            .map(
                |(_server_id, server_url, virtual_library_id)| VirtualLibraryMember {
                    server_url: server_url.clone(),
                    virtual_library_id: virtual_library_id.clone(),
                },
            )
            .collect::<Vec<_>>();
        canonical_members.sort_by(|left, right| {
            left.server_url
                .cmp(&right.server_url)
                .then_with(|| left.virtual_library_id.cmp(&right.virtual_library_id))
        });

        self.automatic_membership_cache.invalidate(&cache_key).await;
        if self
            .get_scoped_automatic_library_members_unlocked(&automatic_virtual_id, &access_scope_key)
            .await?
            .is_some_and(|persisted| persisted == canonical_members)
        {
            return Ok(());
        }

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO automatic_library_snapshots \
             (automatic_virtual_id, access_scope_key, updated_at) \
             VALUES (?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(automatic_virtual_id, access_scope_key) DO UPDATE SET \
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&automatic_virtual_id)
        .bind(&access_scope_key)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM automatic_library_members \
             WHERE automatic_virtual_id = ? AND access_scope_key = ?",
        )
        .bind(&automatic_virtual_id)
        .bind(&access_scope_key)
        .execute(&mut *tx)
        .await?;
        for (server_id, _server_url, virtual_library_id) in &scoped_members {
            sqlx::query(
                "INSERT INTO automatic_library_members \
                 (automatic_virtual_id, access_scope_key, server_id, virtual_library_id) \
                 VALUES (?, ?, ?, ?)",
            )
            .bind(&automatic_virtual_id)
            .bind(&access_scope_key)
            .bind(server_id.as_i64())
            .bind(virtual_library_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.automatic_membership_cache
            .insert(cache_key, canonical_members)
            .await;
        debug!(
            "Upserted {} members for automatic library {} and access scope {}",
            scoped_members.len(),
            automatic_virtual_id,
            access_scope_key
        );
        Ok(())
    }

    async fn get_scoped_automatic_library_members(
        &self,
        automatic_virtual_id: &str,
        access_scope_key: &str,
    ) -> Result<Option<Vec<VirtualLibraryMember>>, sqlx::Error> {
        let cache_key = AutomaticMembershipCacheKey {
            virtual_id: automatic_virtual_id.to_string(),
            access_scope_key: access_scope_key.to_string(),
        };
        let write_lock = self.automatic_membership_lock(&cache_key).await;
        let _guard = write_lock.lock().await;
        self.get_scoped_automatic_library_members_unlocked(automatic_virtual_id, access_scope_key)
            .await
    }

    async fn get_scoped_automatic_library_members_unlocked(
        &self,
        automatic_virtual_id: &str,
        access_scope_key: &str,
    ) -> Result<Option<Vec<VirtualLibraryMember>>, sqlx::Error> {
        let cache_key = AutomaticMembershipCacheKey {
            virtual_id: automatic_virtual_id.to_string(),
            access_scope_key: access_scope_key.to_string(),
        };
        if let Some(members) = self.automatic_membership_cache.get(&cache_key).await {
            return Ok(Some(members));
        }

        let snapshot_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS( \
                 SELECT 1 FROM automatic_library_snapshots \
                 WHERE automatic_virtual_id = ? AND access_scope_key = ? \
             )",
        )
        .bind(automatic_virtual_id)
        .bind(access_scope_key)
        .fetch_one(&self.pool)
        .await?;
        if !snapshot_exists {
            return Ok(None);
        }

        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT s.url, m.virtual_library_id \
             FROM automatic_library_members m \
             JOIN servers s ON s.id = m.server_id \
              WHERE m.automatic_virtual_id = ? AND m.access_scope_key = ? \
             ORDER BY s.url, m.virtual_library_id",
        )
        .bind(automatic_virtual_id)
        .bind(access_scope_key)
        .fetch_all(&self.pool)
        .await?;
        let members = rows
            .into_iter()
            .map(|(server_url, virtual_library_id)| VirtualLibraryMember {
                server_url,
                virtual_library_id,
            })
            .collect::<Vec<_>>();
        self.automatic_membership_cache
            .insert(cache_key, members.clone())
            .await;
        Ok(Some(members))
    }

    async fn get_automatic_library_members(
        &self,
        automatic_virtual_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Vec<VirtualLibraryMember>, sqlx::Error> {
        let automatic_virtual_id = normalize_library_id(automatic_virtual_id);
        if let Some(access_scope) = access_scope {
            return Ok(self
                .get_scoped_automatic_library_members(&automatic_virtual_id, &access_scope.key())
                .await?
                .unwrap_or_default());
        }

        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT server_url, virtual_library_id \
             FROM merged_library_members WHERE merged_virtual_id = ?",
        )
        .bind(&automatic_virtual_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(server_url, virtual_library_id)| VirtualLibraryMember {
                server_url,
                virtual_library_id,
            })
            .collect())
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

    pub async fn list_groups(&self) -> Result<Vec<LibraryGroup>, sqlx::Error> {
        let rows: Vec<(String, String, i32, String, Option<i64>)> = sqlx::query_as(
            "SELECT virtual_id, name, sort_order, duplicate_policy, preferred_server_id \
             FROM library_groups ORDER BY sort_order, name",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(virtual_id, name, sort_order, duplicate_policy, preferred_server_id)| {
                    LibraryGroup {
                        virtual_id,
                        name,
                        sort_order,
                        duplicate_policy: duplicate_policy
                            .parse()
                            .unwrap_or(DuplicatePolicy::ServerPriority),
                        preferred_server_id: preferred_server_id.map(ServerId::new),
                    }
                },
            )
            .collect())
    }

    pub async fn create_group(&self, name: &str) -> Result<LibraryGroup, sqlx::Error> {
        let virtual_id = Uuid::new_v4().simple().to_string();
        sqlx::query(
            "INSERT INTO library_groups (virtual_id, name, sort_order, duplicate_policy) \
             SELECT ?, ?, COALESCE(MAX(sort_order), -1) + 1, ? FROM library_groups",
        )
        .bind(&virtual_id)
        .bind(name.trim())
        .bind(DuplicatePolicy::ServerPriority.to_string())
        .execute(&self.pool)
        .await?;

        self.get_group(&virtual_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }

    pub async fn update_group_policy(
        &self,
        virtual_id: &str,
        duplicate_policy: DuplicatePolicy,
        preferred_server_id: Option<ServerId>,
    ) -> Result<bool, sqlx::Error> {
        let virtual_id = normalize_library_id(virtual_id);
        let result = sqlx::query(
            "UPDATE library_groups SET duplicate_policy = ?, preferred_server_id = ? WHERE virtual_id = ?",
        )
        .bind(duplicate_policy.to_string())
        .bind(preferred_server_id.map(|id| id.as_i64()))
        .bind(virtual_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
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
    ) -> Result<(), sqlx::Error> {
        let group_virtual_id = normalize_library_id(group_virtual_id);
        let original_library_id = normalize_library_id(original_library_id);

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
        .execute(&self.pool)
        .await?;

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
        let result = sqlx::query(
            "DELETE FROM library_group_members \
             WHERE group_virtual_id = ? AND server_id = ? AND original_library_id = ?",
        )
        .bind(group_virtual_id)
        .bind(server_id.as_i64())
        .bind(&original_library_id)
        .execute(&self.pool)
        .await?;
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
        let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
            "SELECT m.server_id, m.original_library_id, g.virtual_id, g.name \
             FROM library_group_members m \
             JOIN library_groups g ON g.virtual_id = m.group_virtual_id",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(server_id, original_library_id, group_virtual_id, group_name)| {
                    (
                        (ServerId::new(server_id), original_library_id),
                        LibraryAssignment {
                            group_virtual_id,
                            group_name,
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
        let virtual_id = normalize_library_id(virtual_id);
        let group = match self.get_group(&virtual_id).await? {
            Some(group) => group,
            None => return Ok(None),
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
            let server = match self.server_storage.get_server_by_id(record.server_id).await {
                Ok(Some(server)) => server,
                Ok(None) => continue,
                Err(e) => return Err(e),
            };

            let mapping = self
                .media_storage
                .get_or_create_media_mapping(&record.original_library_id, &server)
                .await?;

            members.push(VirtualLibraryMember {
                server_url: server.url.to_string(),
                virtual_library_id: mapping.virtual_media_id,
            });
        }

        Ok(Some((group, members)))
    }

    pub async fn get_group(&self, virtual_id: &str) -> Result<Option<LibraryGroup>, sqlx::Error> {
        let virtual_id = normalize_library_id(virtual_id);
        let row: Option<(String, String, i32, String, Option<i64>)> = sqlx::query_as(
            "SELECT virtual_id, name, sort_order, duplicate_policy, preferred_server_id \
             FROM library_groups WHERE virtual_id = ?",
        )
        .bind(&virtual_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(virtual_id, name, sort_order, duplicate_policy, preferred_server_id)| LibraryGroup {
                virtual_id,
                name,
                sort_order,
                duplicate_policy: duplicate_policy
                    .parse()
                    .unwrap_or(DuplicatePolicy::ServerPriority),
                preferred_server_id: preferred_server_id.map(ServerId::new),
            },
        ))
    }
}

fn server_set_key(server_ids: &[ServerId]) -> String {
    let mut ids = server_ids
        .iter()
        .map(|server_id| server_id.as_i64())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids.into_iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",")
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

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        pool
    }

    async fn insert_server(pool: &SqlitePool) {
        insert_server_with(pool, 1, "Server A", "http://a:8096", 100).await;
    }

    async fn insert_server_with(pool: &SqlitePool, id: i64, name: &str, url: &str, priority: i32) {
        sqlx::query(
            "INSERT INTO servers (id, name, url, priority, media_streaming_mode, created_at, updated_at) \
             VALUES (?, ?, ?, ?, 'Redirect', datetime('now'), datetime('now'))",
        )
        .bind(id)
        .bind(name)
        .bind(url)
        .bind(priority)
        .execute(pool)
        .await
        .unwrap();
    }

    fn test_service(pool: &SqlitePool) -> VirtualLibraryService {
        VirtualLibraryService::new(
            pool.clone(),
            ServerStorageService::new(pool.clone()),
            MediaStorageService::new(pool.clone()),
        )
    }

    fn access_scope(user_id: &str, server_ids: &[i64]) -> VirtualLibraryAccessScope {
        VirtualLibraryAccessScope::new(user_id, server_ids.iter().copied().map(ServerId::new))
    }

    #[tokio::test]
    async fn create_group_and_assign_member() {
        let pool = test_pool().await;
        let service = test_service(&pool);

        insert_server(&pool).await;

        let group = service.create_group("Anime").await.unwrap();
        service
            .add_member(
                &group.virtual_id,
                ServerId::new(1),
                "abc-def-1234-5678-901234567890",
                "Anime",
            )
            .await
            .unwrap();

        let assignments = service.get_assignments().await.unwrap();
        assert_eq!(assignments.len(), 1);
        assert_eq!(
            assignments.values().next().map(|a| a.group_name.as_str()),
            Some("Anime")
        );
    }

    #[tokio::test]
    async fn presentation_mode_is_disabled_without_groups() {
        let pool = test_pool().await;
        let service = test_service(&pool);

        let mode = service.presentation_mode(false).await.unwrap();

        assert_eq!(mode, VirtualLibraryMode::Disabled);
    }

    #[tokio::test]
    async fn presentation_mode_is_manual_when_groups_exist() {
        let pool = test_pool().await;
        let service = test_service(&pool);
        service.create_group("Anime").await.unwrap();

        let mode = service.presentation_mode(false).await.unwrap();

        assert_eq!(mode, VirtualLibraryMode::Manual);
    }

    #[tokio::test]
    async fn presentation_mode_is_automatic_even_when_groups_exist() {
        let pool = test_pool().await;
        let service = test_service(&pool);
        service.create_group("Anime").await.unwrap();

        let mode = service.presentation_mode(true).await.unwrap();

        assert_eq!(mode, VirtualLibraryMode::Automatic);
    }

    #[tokio::test]
    async fn resolve_distinguishes_empty_group_from_unknown_id() {
        let pool = test_pool().await;
        let service = test_service(&pool);
        let group = service.create_group("Anime").await.unwrap();

        let resolution = service.resolve(&group.virtual_id, None).await.unwrap();

        assert!(matches!(
            resolution,
            VirtualLibraryResolution::Empty(VirtualLibrary::Manual(_))
        ));
    }

    #[tokio::test]
    async fn create_group_defaults_to_server_priority() {
        let pool = test_pool().await;
        let service = test_service(&pool);

        let group = service.create_group("Anime").await.unwrap();

        assert_eq!(group.duplicate_policy, DuplicatePolicy::ServerPriority);
    }

    #[tokio::test]
    async fn add_member_reassigns_source_library_atomically() {
        let pool = test_pool().await;
        insert_server(&pool).await;
        let service = test_service(&pool);
        let first = service.create_group("Anime").await.unwrap();
        let second = service.create_group("Movies").await.unwrap();
        service
            .add_member(&first.virtual_id, ServerId::new(1), "library", "Anime")
            .await
            .unwrap();

        service
            .add_member(&second.virtual_id, ServerId::new(1), "library", "Movies")
            .await
            .unwrap();

        let assignments = service.get_assignments().await.unwrap();
        assert_eq!(
            assignments
                .get(&(ServerId::new(1), "library".to_string()))
                .map(|assignment| assignment.group_virtual_id.as_str()),
            Some(second.virtual_id.as_str())
        );
    }

    #[tokio::test]
    async fn failed_reassignment_preserves_existing_assignment() {
        let pool = test_pool().await;
        insert_server(&pool).await;
        let service = test_service(&pool);
        let group = service.create_group("Anime").await.unwrap();
        service
            .add_member(&group.virtual_id, ServerId::new(1), "library", "Anime")
            .await
            .unwrap();

        let result = service
            .add_member("missing-group", ServerId::new(1), "library", "Anime")
            .await;

        assert!(result.is_err());
        let assignments = service.get_assignments().await.unwrap();
        assert_eq!(
            assignments
                .get(&(ServerId::new(1), "library".to_string()))
                .map(|assignment| assignment.group_virtual_id.as_str()),
            Some(group.virtual_id.as_str())
        );
    }

    #[tokio::test]
    async fn automatic_virtual_library_resolves_first_member() {
        let pool = test_pool().await;
        insert_server(&pool).await;
        let service = test_service(&pool);
        let server = service
            .server_storage
            .get_server_by_id(ServerId::new(1))
            .await
            .unwrap()
            .unwrap();
        let mapping = service
            .media_storage
            .get_or_create_media_mapping("automatic-library", &server)
            .await
            .unwrap();
        let scope = access_scope("user", &[server.id.as_i64()]);
        let library = service
            .get_or_create_automatic_library("movies:anime", "Anime")
            .await
            .unwrap();
        service
            .upsert_automatic_library_members(
                &library.virtual_id,
                &scope,
                &[(
                    server.id,
                    server.url.to_string(),
                    mapping.virtual_media_id.clone(),
                )],
            )
            .await
            .unwrap();

        let target = service
            .routing_target(&library.virtual_id, Some(&scope), None)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(target.mapping.virtual_media_id, mapping.virtual_media_id);
    }

    #[tokio::test]
    async fn automatic_virtual_library_keeps_id_and_refreshes_name() {
        let pool = test_pool().await;
        let service = test_service(&pool);
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
    async fn persisted_automatic_library_resolves_in_manual_presentation_mode() {
        let pool = test_pool().await;
        insert_server(&pool).await;
        let service = test_service(&pool);
        let automatic = service
            .get_or_create_automatic_library("movies:anime", "Anime")
            .await
            .unwrap();
        let scope = access_scope("user", &[1]);
        service
            .upsert_automatic_library_members(
                &automatic.virtual_id,
                &scope,
                &[(
                    ServerId::new(1),
                    "http://a:8096".to_string(),
                    "member-id".to_string(),
                )],
            )
            .await
            .unwrap();
        service.create_group("Manual group").await.unwrap();
        assert_eq!(
            service.presentation_mode(false).await.unwrap(),
            VirtualLibraryMode::Manual
        );

        let resolution = service
            .resolve(&automatic.virtual_id, Some(&scope))
            .await
            .unwrap();

        assert!(matches!(
            resolution,
            VirtualLibraryResolution::Resolved(ResolvedVirtualLibrary {
                library: VirtualLibrary::Automatic(_),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn manual_virtual_library_resolves_first_member() {
        let pool = test_pool().await;
        insert_server(&pool).await;
        let service = test_service(&pool);
        let server = ServerStorageService::new(pool)
            .get_server_by_id(ServerId::new(1))
            .await
            .unwrap()
            .unwrap();
        let original_library_id = Uuid::new_v4().simple().to_string();
        let group = service.create_group("Anime").await.unwrap();
        let scope = access_scope("user", &[server.id.as_i64()]);
        service
            .add_member(&group.virtual_id, server.id, &original_library_id, "Anime")
            .await
            .unwrap();

        let target = service
            .routing_target(&group.virtual_id, Some(&scope), None)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(target.mapping.original_media_id, original_library_id);
    }

    #[tokio::test]
    async fn routing_target_prefers_configured_server() {
        let pool = test_pool().await;
        insert_server_with(&pool, 1, "Server A", "http://a:8096", 100).await;
        insert_server_with(&pool, 2, "Server B", "http://b:8096", 200).await;
        let service = test_service(&pool);
        let group = service.create_group("Anime").await.unwrap();
        service
            .add_member(&group.virtual_id, ServerId::new(1), "library-a", "Anime")
            .await
            .unwrap();
        service
            .add_member(&group.virtual_id, ServerId::new(2), "library-b", "Anime")
            .await
            .unwrap();
        service
            .update_group_policy(
                &group.virtual_id,
                DuplicatePolicy::PreferServer,
                Some(ServerId::new(1)),
            )
            .await
            .unwrap();
        let scope = access_scope("user", &[1, 2]);

        let target = service
            .routing_target(&group.virtual_id, Some(&scope), None)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(target.server.id, ServerId::new(1));
    }

    #[tokio::test]
    async fn automatic_memberships_are_isolated_by_server_set() {
        let pool = test_pool().await;
        insert_server_with(&pool, 1, "Server A", "http://a:8096", 100).await;
        insert_server_with(&pool, 2, "Server B", "http://b:8096", 200).await;
        insert_server_with(&pool, 3, "Server C", "http://c:8096", 300).await;
        let service = test_service(&pool);
        let library = service
            .get_or_create_automatic_library("movies:anime", "Anime")
            .await
            .unwrap();
        let first_scope = access_scope("user-a", &[1, 2]);
        let second_scope = access_scope("user-b", &[2, 3]);
        service
            .upsert_automatic_library_members(
                &library.virtual_id,
                &first_scope,
                &[
                    (
                        ServerId::new(1),
                        "http://a:8096".to_string(),
                        "member-a".to_string(),
                    ),
                    (
                        ServerId::new(2),
                        "http://b:8096".to_string(),
                        "member-b".to_string(),
                    ),
                ],
            )
            .await
            .unwrap();
        service
            .upsert_automatic_library_members(
                &library.virtual_id,
                &second_scope,
                &[
                    (
                        ServerId::new(2),
                        "http://b:8096".to_string(),
                        "member-b".to_string(),
                    ),
                    (
                        ServerId::new(3),
                        "http://c:8096".to_string(),
                        "member-c".to_string(),
                    ),
                ],
            )
            .await
            .unwrap();

        let reloaded_service = test_service(&pool);
        let first = reloaded_service
            .resolve(&library.virtual_id, Some(&first_scope))
            .await
            .unwrap();
        let second = reloaded_service
            .resolve(&library.virtual_id, Some(&second_scope))
            .await
            .unwrap();

        assert_eq!(
            resolved_member_ids(first),
            vec!["member-a".to_string(), "member-b".to_string()]
        );
        assert_eq!(
            resolved_member_ids(second),
            vec!["member-b".to_string(), "member-c".to_string()]
        );
    }

    #[tokio::test]
    async fn automatic_memberships_are_isolated_by_user_for_same_server_set() {
        let pool = test_pool().await;
        insert_server_with(&pool, 1, "Server A", "http://a:8096", 100).await;
        insert_server_with(&pool, 2, "Server B", "http://b:8096", 200).await;
        let service = test_service(&pool);
        let library = service
            .get_or_create_automatic_library("movies:anime", "Anime")
            .await
            .unwrap();
        let first_scope = access_scope("user-a", &[1, 2]);
        let second_scope = access_scope("user-b", &[1, 2]);
        service
            .upsert_automatic_library_members(
                &library.virtual_id,
                &first_scope,
                &[
                    (
                        ServerId::new(1),
                        "http://a:8096".to_string(),
                        "member-a".to_string(),
                    ),
                    (
                        ServerId::new(2),
                        "http://b:8096".to_string(),
                        "member-b".to_string(),
                    ),
                ],
            )
            .await
            .unwrap();
        service
            .upsert_automatic_library_members(
                &library.virtual_id,
                &second_scope,
                &[(
                    ServerId::new(2),
                    "http://b:8096".to_string(),
                    "member-b".to_string(),
                )],
            )
            .await
            .unwrap();

        let first = service
            .resolve(&library.virtual_id, Some(&first_scope))
            .await
            .unwrap();
        let second = service
            .resolve(&library.virtual_id, Some(&second_scope))
            .await
            .unwrap();

        assert_eq!(
            resolved_member_ids(first),
            vec!["member-a".to_string(), "member-b".to_string()]
        );
        assert_eq!(resolved_member_ids(second), vec!["member-b".to_string()]);
    }

    #[tokio::test]
    async fn automatic_routing_uses_required_server_with_full_access_scope() {
        let pool = test_pool().await;
        insert_server_with(&pool, 1, "Server A", "http://a:8096", 100).await;
        insert_server_with(&pool, 2, "Server B", "http://b:8096", 200).await;
        let service = test_service(&pool);
        let server_a = service
            .server_storage
            .get_server_by_id(ServerId::new(1))
            .await
            .unwrap()
            .unwrap();
        let server_b = service
            .server_storage
            .get_server_by_id(ServerId::new(2))
            .await
            .unwrap()
            .unwrap();
        let mapping_a = service
            .media_storage
            .get_or_create_media_mapping("library-a", &server_a)
            .await
            .unwrap();
        let mapping_b = service
            .media_storage
            .get_or_create_media_mapping("library-b", &server_b)
            .await
            .unwrap();
        let library = service
            .get_or_create_automatic_library("movies:anime", "Anime")
            .await
            .unwrap();
        let scope = access_scope("user", &[1, 2]);
        service
            .upsert_automatic_library_members(
                &library.virtual_id,
                &scope,
                &[
                    (
                        server_a.id,
                        server_a.url.to_string(),
                        mapping_a.virtual_media_id,
                    ),
                    (
                        server_b.id,
                        server_b.url.to_string(),
                        mapping_b.virtual_media_id,
                    ),
                ],
            )
            .await
            .unwrap();

        let target = service
            .routing_target(&library.virtual_id, Some(&scope), Some(server_a.id))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(target.server.id, server_a.id);
    }

    #[tokio::test]
    async fn unchanged_automatic_membership_uses_cache_without_database_write() {
        let pool = test_pool().await;
        insert_server(&pool).await;
        let service = test_service(&pool);
        let library = service
            .get_or_create_automatic_library("movies:anime", "Anime")
            .await
            .unwrap();
        let scope = access_scope("user", &[1]);
        let members = [(
            ServerId::new(1),
            "http://a:8096".to_string(),
            "member-a".to_string(),
        )];
        service
            .upsert_automatic_library_members(&library.virtual_id, &scope, &members)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER reject_automatic_membership_delete \
             BEFORE DELETE ON automatic_library_members \
             BEGIN SELECT RAISE(FAIL, 'unexpected write'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let result = service
            .upsert_automatic_library_members(&library.virtual_id, &scope, &members)
            .await;

        assert!(result.is_ok(), "unchanged cached snapshot was rewritten");
    }

    #[tokio::test]
    async fn cleared_snapshot_does_not_fall_back_to_legacy_members() {
        let pool = test_pool().await;
        insert_server(&pool).await;
        let service = test_service(&pool);
        let library = service
            .get_or_create_automatic_library("movies:anime", "Anime")
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO merged_library_members \
             (merged_virtual_id, server_url, virtual_library_id) VALUES (?, ?, ?)",
        )
        .bind(&library.virtual_id)
        .bind("http://a:8096")
        .bind("legacy-member")
        .execute(&pool)
        .await
        .unwrap();
        let scope = access_scope("user", &[1]);

        service
            .clear_automatic_library_snapshot("movies:anime", &scope)
            .await
            .unwrap();
        let resolution = service
            .resolve(&library.virtual_id, Some(&scope))
            .await
            .unwrap();

        assert!(matches!(
            resolution,
            VirtualLibraryResolution::Empty(VirtualLibrary::Automatic(_))
        ));
    }

    #[tokio::test]
    async fn reconciliation_clears_library_missing_from_successful_refresh() {
        let pool = test_pool().await;
        insert_server(&pool).await;
        let service = test_service(&pool);
        let library = service
            .get_or_create_automatic_library("movies:anime", "Anime")
            .await
            .unwrap();
        let scope = access_scope("user", &[1]);
        service
            .upsert_automatic_library_members(
                &library.virtual_id,
                &scope,
                &[(
                    ServerId::new(1),
                    "http://a:8096".to_string(),
                    "member-a".to_string(),
                )],
            )
            .await
            .unwrap();

        service
            .reconcile_automatic_library_snapshots(&scope, &[])
            .await
            .unwrap();
        let resolution = service
            .resolve(&library.virtual_id, Some(&scope))
            .await
            .unwrap();

        assert!(matches!(
            resolution,
            VirtualLibraryResolution::Empty(VirtualLibrary::Automatic(_))
        ));
    }

    #[tokio::test]
    async fn routing_target_skips_unauthorized_preferred_server() {
        let pool = test_pool().await;
        insert_server_with(&pool, 1, "Server A", "http://a:8096", 100).await;
        insert_server_with(&pool, 2, "Server B", "http://b:8096", 200).await;
        let service = test_service(&pool);
        let group = service.create_group("Anime").await.unwrap();
        service
            .add_member(&group.virtual_id, ServerId::new(1), "library-a", "Anime")
            .await
            .unwrap();
        service
            .add_member(&group.virtual_id, ServerId::new(2), "library-b", "Anime")
            .await
            .unwrap();
        service
            .update_group_policy(
                &group.virtual_id,
                DuplicatePolicy::PreferServer,
                Some(ServerId::new(1)),
            )
            .await
            .unwrap();
        let scope = access_scope("user", &[2]);

        let target = service
            .routing_target(&group.virtual_id, Some(&scope), None)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(target.server.id, ServerId::new(2));
    }

    fn resolved_member_ids(resolution: VirtualLibraryResolution) -> Vec<String> {
        let VirtualLibraryResolution::Resolved(resolved) = resolution else {
            panic!("expected resolved virtual library");
        };
        let mut ids = resolved
            .members
            .into_iter()
            .map(|member| member.virtual_library_id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }
}
