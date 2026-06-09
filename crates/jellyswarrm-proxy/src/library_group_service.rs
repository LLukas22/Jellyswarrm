use std::collections::HashMap;

use sqlx::SqlitePool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    merged_library_service::MergedLibraryMember,
    server_id::ServerId,
    AppState,
};

pub fn normalize_library_id(id: &str) -> String {
    match Uuid::parse_str(id) {
        Ok(uuid) => uuid.simple().to_string(),
        Err(_) => id.to_string(),
    }
}

#[derive(Debug, Clone)]
pub struct LibraryGroup {
    pub virtual_id: String,
    pub name: String,
    pub sort_order: i32,
    pub duplicate_policy: crate::duplicate_policy::DuplicatePolicy,
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
pub struct LibraryGroupService {
    pool: SqlitePool,
}

impl LibraryGroupService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn has_groups(&self) -> Result<bool, sqlx::Error> {
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
                            .unwrap_or(crate::duplicate_policy::DuplicatePolicy::ShowAll),
                        preferred_server_id: preferred_server_id.map(ServerId::new),
                    }
                },
            )
            .collect())
    }

    pub async fn create_group(&self, name: &str) -> Result<LibraryGroup, sqlx::Error> {
        let virtual_id = Uuid::new_v4().simple().to_string();
        let sort_order: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(sort_order), -1) + 1 FROM library_groups")
            .fetch_one(&self.pool)
            .await?;

        sqlx::query(
            "INSERT INTO library_groups (virtual_id, name, sort_order) VALUES (?, ?, ?)",
        )
        .bind(&virtual_id)
        .bind(name.trim())
        .bind(sort_order as i32)
        .execute(&self.pool)
        .await?;

        Ok(LibraryGroup {
            virtual_id,
            name: name.trim().to_string(),
            sort_order: sort_order as i32,
            duplicate_policy: crate::duplicate_policy::DuplicatePolicy::ShowAll,
            preferred_server_id: None,
        })
    }

    pub async fn update_group_policy(
        &self,
        virtual_id: &str,
        duplicate_policy: crate::duplicate_policy::DuplicatePolicy,
        preferred_server_id: Option<ServerId>,
    ) -> Result<bool, sqlx::Error> {
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
        let result = sqlx::query("UPDATE library_groups SET name = ? WHERE virtual_id = ?")
            .bind(name.trim())
            .bind(virtual_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn delete_group(&self, virtual_id: &str) -> Result<bool, sqlx::Error> {
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
        let original_library_id = normalize_library_id(original_library_id);

        sqlx::query(
            "DELETE FROM library_group_members WHERE server_id = ? AND original_library_id = ?",
        )
        .bind(server_id.as_i64())
        .bind(&original_library_id)
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "INSERT INTO library_group_members \
             (group_virtual_id, server_id, original_library_id, library_name) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(group_virtual_id)
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
        let rows: Vec<(String, i64, String, String)> = sqlx::query_as(
            "SELECT group_virtual_id, server_id, original_library_id, library_name \
             FROM library_group_members WHERE group_virtual_id = ? \
             ORDER BY library_name",
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
            .map(|(server_id, original_library_id, group_virtual_id, group_name)| {
                (
                    (ServerId::new(server_id), original_library_id),
                    LibraryAssignment {
                        group_virtual_id,
                        group_name,
                    },
                )
            })
            .collect())
    }

    pub async fn resolve(
        &self,
        state: &AppState,
        virtual_id: &str,
    ) -> Result<Option<(LibraryGroup, Vec<MergedLibraryMember>)>, sqlx::Error> {
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
            let server = match state
                .server_storage
                .get_server_by_id(record.server_id)
                .await
            {
                Ok(Some(server)) => server,
                Ok(None) => continue,
                Err(e) => return Err(e),
            };

            let mapping = state
                .media_storage
                .get_or_create_media_mapping(&record.original_library_id, &server)
                .await?;

            members.push(MergedLibraryMember {
                server_url: server.url.to_string(),
                virtual_library_id: mapping.virtual_media_id,
            });
        }

        Ok(Some((group, members)))
    }

    pub async fn get_group(&self, virtual_id: &str) -> Result<Option<LibraryGroup>, sqlx::Error> {
        let row: Option<(String, String, i32, String, Option<i64>)> = sqlx::query_as(
            "SELECT virtual_id, name, sort_order, duplicate_policy, preferred_server_id \
             FROM library_groups WHERE virtual_id = ?",
        )
        .bind(virtual_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(virtual_id, name, sort_order, duplicate_policy, preferred_server_id)| LibraryGroup {
                virtual_id,
                name,
                sort_order,
                duplicate_policy: duplicate_policy
                    .parse()
                    .unwrap_or(crate::duplicate_policy::DuplicatePolicy::ShowAll),
                preferred_server_id: preferred_server_id.map(ServerId::new),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MIGRATOR;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn create_group_and_assign_member() {
        let pool = test_pool().await;
        let service = LibraryGroupService::new(pool.clone());

        sqlx::query(
            "INSERT INTO servers (id, name, url, priority, media_streaming_mode, created_at, updated_at) \
             VALUES (1, 'Server A', 'http://a:8096', 100, 'Redirect', datetime('now'), datetime('now'))",
        )
        .execute(&pool)
        .await
        .unwrap();

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
            assignments
                .values()
                .next()
                .map(|a| a.group_name.as_str()),
            Some("Anime")
        );
    }
}
