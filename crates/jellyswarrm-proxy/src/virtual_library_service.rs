use std::collections::HashMap;

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

#[derive(Debug, Clone)]
pub struct VirtualLibraryMember {
    pub server_url: String,
    pub virtual_library_id: String,
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

    pub async fn resolve(&self, virtual_id: &str) -> Result<VirtualLibraryResolution, sqlx::Error> {
        if let Some((group, members)) = self.resolve_group(virtual_id).await? {
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

        if let Some((library, members)) = self.resolve_automatic_library(virtual_id).await? {
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
    ) -> Result<Option<VirtualLibraryRoutingTarget>, sqlx::Error> {
        if let Some(group) = self.get_group(virtual_id).await? {
            let group_virtual_id = normalize_library_id(&group.virtual_id);
            let member: Option<(i64, String)> = sqlx::query_as(
                "SELECT m.server_id, m.original_library_id \
                 FROM library_group_members m \
                 JOIN library_groups g ON g.virtual_id = m.group_virtual_id \
                 JOIN servers s ON s.id = m.server_id \
                 WHERE m.group_virtual_id = ? \
                 ORDER BY CASE WHEN g.duplicate_policy = 'PreferServer' \
                                         AND g.preferred_server_id = m.server_id THEN 0 ELSE 1 END, \
                          s.priority DESC, s.name, m.server_id, m.original_library_id \
                 LIMIT 1",
            )
            .bind(group_virtual_id)
            .fetch_optional(&self.pool)
            .await?;

            let Some((server_id, original_library_id)) = member else {
                return Ok(None);
            };

            let Some(server) = self
                .server_storage
                .get_server_by_id(ServerId::new(server_id))
                .await?
            else {
                return Ok(None);
            };
            let mapping = self
                .media_storage
                .get_or_create_media_mapping(&original_library_id, &server)
                .await?;
            return Ok(Some(VirtualLibraryRoutingTarget { mapping, server }));
        }

        for member_virtual_id in self
            .get_ordered_automatic_member_virtual_ids(virtual_id)
            .await?
        {
            if let Some((mapping, server)) = self
                .media_storage
                .get_media_mapping_with_server(&member_virtual_id)
                .await?
            {
                return Ok(Some(VirtualLibraryRoutingTarget { mapping, server }));
            }
        }

        Ok(None)
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

    async fn get_automatic_library_by_collection_type(
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
        merged_virtual_id: &str,
        members: &[(String, String)],
    ) -> Result<(), sqlx::Error> {
        let merged_virtual_id = normalize_library_id(merged_virtual_id);
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM merged_library_members WHERE merged_virtual_id = ?")
            .bind(&merged_virtual_id)
            .execute(&mut *tx)
            .await?;
        for (server_url, virtual_library_id) in members {
            sqlx::query(
                "INSERT INTO merged_library_members \
                 (merged_virtual_id, server_url, virtual_library_id) VALUES (?, ?, ?)",
            )
            .bind(&merged_virtual_id)
            .bind(server_url)
            .bind(virtual_library_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        debug!(
            "Upserted {} members for merged library {}",
            members.len(),
            merged_virtual_id
        );
        Ok(())
    }

    async fn get_automatic_library_members(
        &self,
        merged_virtual_id: &str,
    ) -> Result<Vec<VirtualLibraryMember>, sqlx::Error> {
        let merged_virtual_id = normalize_library_id(merged_virtual_id);
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT server_url, virtual_library_id \
             FROM merged_library_members WHERE merged_virtual_id = ?",
        )
        .bind(&merged_virtual_id)
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

    async fn get_ordered_automatic_member_virtual_ids(
        &self,
        merged_virtual_id: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        let merged_virtual_id = normalize_library_id(merged_virtual_id);
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT m.virtual_library_id \
             FROM merged_library_members m \
             LEFT JOIN media_mappings mm ON mm.virtual_media_id = m.virtual_library_id \
             LEFT JOIN servers s ON s.id = mm.server_id \
             WHERE m.merged_virtual_id = ? \
             ORDER BY s.priority DESC, m.server_url, m.virtual_library_id",
        )
        .bind(&merged_virtual_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn resolve_automatic_library(
        &self,
        virtual_id: &str,
    ) -> Result<Option<(AutomaticVirtualLibrary, Vec<VirtualLibraryMember>)>, sqlx::Error> {
        let Some(library) = self.get_automatic_library(virtual_id).await? else {
            return Ok(None);
        };
        let members = self
            .get_automatic_library_members(&library.virtual_id)
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

        let resolution = service.resolve(&group.virtual_id).await.unwrap();

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
        let library = service
            .get_or_create_automatic_library("movies:anime", "Anime")
            .await
            .unwrap();
        service
            .upsert_automatic_library_members(
                &library.virtual_id,
                &[(server.url.to_string(), mapping.virtual_media_id.clone())],
            )
            .await
            .unwrap();

        let target = service
            .routing_target(&library.virtual_id)
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
        let service = test_service(&pool);
        let automatic = service
            .get_or_create_automatic_library("movies:anime", "Anime")
            .await
            .unwrap();
        service
            .upsert_automatic_library_members(
                &automatic.virtual_id,
                &[("http://a:8096".to_string(), "member-id".to_string())],
            )
            .await
            .unwrap();
        service.create_group("Manual group").await.unwrap();
        assert_eq!(
            service.presentation_mode(false).await.unwrap(),
            VirtualLibraryMode::Manual
        );

        let resolution = service.resolve(&automatic.virtual_id).await.unwrap();

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
        service
            .add_member(&group.virtual_id, server.id, &original_library_id, "Anime")
            .await
            .unwrap();

        let target = service
            .routing_target(&group.virtual_id)
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

        let target = service
            .routing_target(&group.virtual_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(target.server.id, ServerId::new(1));
    }
}
