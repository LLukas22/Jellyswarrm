use std::collections::HashMap;

use sqlx::SqlitePool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    duplicate_policy::{DuplicatePolicy, DuplicatePolicyConfig},
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
    Configured(LibraryGroup),
}

impl VirtualLibrary {
    pub fn name(&self) -> &str {
        match self {
            Self::Automatic(library) => &library.name,
            Self::Configured(group) => &group.name,
        }
    }

    pub fn duplicate_config(&self) -> DuplicatePolicyConfig {
        match self {
            Self::Automatic(_) => DuplicatePolicyConfig {
                policy: DuplicatePolicy::ShowAll,
                preferred_server_id: None,
            },
            Self::Configured(group) => DuplicatePolicyConfig {
                policy: group.duplicate_policy,
                preferred_server_id: group.preferred_server_id,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedVirtualLibrary {
    pub library: VirtualLibrary,
    pub members: Vec<VirtualLibraryMember>,
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
            return Ok(resolution(VirtualLibrary::Configured(group), members));
        }

        if let Some((library, members)) = self
            .resolve_automatic_library(virtual_id, access_scope)
            .await?
        {
            return Ok(resolution(VirtualLibrary::Automatic(library), members));
        }

        Ok(VirtualLibraryResolution::Unknown)
    }

    pub async fn routing_target(
        &self,
        virtual_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) -> Result<Option<VirtualLibraryRoutingTarget>, sqlx::Error> {
        let VirtualLibraryResolution::Resolved(resolved) =
            self.resolve(virtual_id, access_scope).await?
        else {
            return Ok(None);
        };
        let preferred_server = match resolved.library {
            VirtualLibrary::Configured(group)
                if group.duplicate_policy == DuplicatePolicy::PreferServer =>
            {
                group.preferred_server_id
            }
            _ => None,
        };

        Ok(resolved
            .members
            .into_iter()
            .filter(|member| server_is_allowed(member.server.id, access_scope, required_server_id))
            .max_by(|left, right| {
                let left_preferred = Some(left.server.id) == preferred_server;
                let right_preferred = Some(right.server.id) == preferred_server;
                left_preferred
                    .cmp(&right_preferred)
                    .then_with(|| left.server.priority.cmp(&right.server.priority))
                    .then_with(|| right.server.name.cmp(&left.server.name))
                    .then_with(|| right.server.id.as_i64().cmp(&left.server.id.as_i64()))
            })
            .map(|member| VirtualLibraryRoutingTarget {
                mapping: member.mapping,
                server: member.server,
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

    pub async fn has_automatic_library_snapshot(
        &self,
        automatic_virtual_id: &str,
        access_scope: &VirtualLibraryAccessScope,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM automatic_library_snapshots \
             WHERE automatic_virtual_id = ? AND access_scope_key = ?)",
        )
        .bind(normalize_library_id(automatic_virtual_id))
        .bind(access_scope.key())
        .fetch_one(&self.pool)
        .await
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
        self.replace_automatic_snapshot(&library.virtual_id, access_scope, &[])
            .await
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

    pub async fn upsert_automatic_library_members(
        &self,
        automatic_virtual_id: &str,
        access_scope: &VirtualLibraryAccessScope,
        members: &[(ServerId, String)],
    ) -> Result<(), sqlx::Error> {
        self.replace_automatic_snapshot(automatic_virtual_id, access_scope, members)
            .await
    }

    async fn replace_automatic_snapshot(
        &self,
        automatic_virtual_id: &str,
        access_scope: &VirtualLibraryAccessScope,
        members: &[(ServerId, String)],
    ) -> Result<(), sqlx::Error> {
        let automatic_virtual_id = normalize_library_id(automatic_virtual_id);
        let access_scope_key = access_scope.key();
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
        for (server_id, virtual_library_id) in members {
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
        debug!(
            "Upserted {} members for automatic library {} and access scope {}",
            members.len(),
            automatic_virtual_id,
            access_scope_key
        );
        Ok(())
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
              AND m.access_scope_key = snapshot.access_scope_key \
             WHERE snapshot.automatic_virtual_id = ? AND snapshot.access_scope_key = ? \
             ORDER BY m.server_id, m.virtual_library_id",
        )
        .bind(normalize_library_id(automatic_virtual_id))
        .bind(access_scope.key())
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

fn resolution(
    library: VirtualLibrary,
    members: Vec<VirtualLibraryMember>,
) -> VirtualLibraryResolution {
    if members.is_empty() {
        VirtualLibraryResolution::Empty(library)
    } else {
        VirtualLibraryResolution::Resolved(ResolvedVirtualLibrary { library, members })
    }
}

fn server_set_key(server_ids: &[ServerId]) -> String {
    server_ids
        .iter()
        .map(|server_id| server_id.as_i64())
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
        ) -> VirtualLibraryRoutingTarget {
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
            self.service
                .upsert_automatic_library_members(&library.virtual_id, scope, members)
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

    #[test]
    fn automatic_library_presentation_shows_all_duplicates() {
        let library = VirtualLibrary::Automatic(AutomaticVirtualLibrary {
            virtual_id: "id".to_string(),
            collection_type: "movies".to_string(),
            name: "Movies".to_string(),
        });

        assert_eq!(library.duplicate_config().policy, DuplicatePolicy::ShowAll);
    }

    fn scope(user_id: &str, server_ids: &[i64]) -> VirtualLibraryAccessScope {
        VirtualLibraryAccessScope::new(user_id, server_ids.iter().copied().map(ServerId::new))
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
            VirtualLibraryResolution::Empty(VirtualLibrary::Configured(_))
        ));
    }

    #[tokio::test]
    async fn create_group_defaults_to_server_priority() {
        let fixture = Fixture::new(&[]).await;
        let group = fixture.service.create_group("Anime").await.unwrap();

        assert_eq!(group.duplicate_policy, DuplicatePolicy::ServerPriority);
    }

    #[tokio::test]
    async fn add_member_replaces_existing_source_assignment() {
        let fixture = Fixture::new(&[(1, 100)]).await;
        let service = &fixture.service;
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
            .add_member(&group.virtual_id, ServerId::new(1), "library", "Anime")
            .await
            .unwrap();

        let result = service
            .add_member("missing-group", ServerId::new(1), "library", "Anime")
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

        assert!(matches!(
            resolution,
            VirtualLibraryResolution::Resolved(ResolvedVirtualLibrary {
                library: VirtualLibrary::Automatic(_),
                ..
            })
        ));
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
    async fn routing_target_prefers_configured_server() {
        let fixture = Fixture::new(&[(1, 100), (2, 200)]).await;
        let service = &fixture.service;
        let group = fixture
            .configured_group(&[(1, "library-a"), (2, "library-b")])
            .await;
        service
            .update_group_policy(
                &group.virtual_id,
                DuplicatePolicy::PreferServer,
                Some(ServerId::new(1)),
            )
            .await
            .unwrap();
        let scope = scope("user", &[1, 2]);

        let target = fixture.route(&group.virtual_id, &scope, None).await;

        assert_eq!(target.server.id, ServerId::new(1));
    }

    #[tokio::test]
    async fn automatic_memberships_are_isolated_by_server_set() {
        let fixture = Fixture::new(&[(1, 100), (2, 200), (3, 300)]).await;
        let library = fixture.automatic_library().await;
        let first_scope = scope("user", &[1, 2]);
        let second_scope = scope("user", &[2, 3]);
        let member_a = fixture.member(1, "member-a").await;
        let member_b = fixture.member(2, "member-b").await;
        let member_c = fixture.member(3, "member-c").await;
        fixture
            .snapshot(
                &library,
                &first_scope,
                &[member_a.clone(), member_b.clone()],
            )
            .await;
        fixture
            .snapshot(
                &library,
                &second_scope,
                &[member_b.clone(), member_c.clone()],
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
        assert_resolved_members(second, &[&member_b.1, &member_c.1]);
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
        service
            .upsert_automatic_library_members(&library.virtual_id, &scope, &members)
            .await
            .unwrap();

        let resolution = service
            .resolve(&library.virtual_id, Some(&scope))
            .await
            .unwrap();

        assert_resolved_members(resolution, &[&members[0].1, &members[1].1]);
    }

    #[tokio::test]
    async fn cleared_snapshot_does_not_fall_back_to_legacy_members() {
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

        service
            .clear_automatic_library_snapshot("movies:anime", &scope)
            .await
            .unwrap();
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

        service
            .reconcile_automatic_library_snapshots(&scope, &[])
            .await
            .unwrap();
        let resolution = service
            .resolve(&library.virtual_id, Some(&scope))
            .await
            .unwrap();

        assert_empty_automatic(resolution);
    }

    #[tokio::test]
    async fn routing_target_skips_unauthorized_preferred_server() {
        let fixture = Fixture::new(&[(1, 100), (2, 200)]).await;
        let service = &fixture.service;
        let group = fixture
            .configured_group(&[(1, "library-a"), (2, "library-b")])
            .await;
        service
            .update_group_policy(
                &group.virtual_id,
                DuplicatePolicy::PreferServer,
                Some(ServerId::new(1)),
            )
            .await
            .unwrap();
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
        assert!(matches!(
            resolution,
            VirtualLibraryResolution::Empty(VirtualLibrary::Automatic(_))
        ));
    }
}
