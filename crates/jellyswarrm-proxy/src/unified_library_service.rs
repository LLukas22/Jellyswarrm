use jellyfin_api::models::MediaFolder;
use sqlx::{FromRow, Row, SqlitePool};
use tracing::info;

use crate::{models::generate_token, server_storage::ServerStorageService};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DedupPolicy {
    #[default]
    ShowAll,
    PreferHighestQuality,
    PreferServerPriority,
}

impl DedupPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            DedupPolicy::ShowAll => "show_all",
            DedupPolicy::PreferHighestQuality => "prefer_highest_quality",
            DedupPolicy::PreferServerPriority => "prefer_server_priority",
        }
    }
}

impl std::str::FromStr for DedupPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "show_all" => Ok(Self::ShowAll),
            "prefer_highest_quality" => Ok(Self::PreferHighestQuality),
            "prefer_server_priority" => Ok(Self::PreferServerPriority),
            _ => Err(format!("invalid dedup policy: {s}")),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UnifiedLibrary {
    pub id: i64,
    pub virtual_library_id: String,
    pub name: String,
    pub collection_type: String,
    pub sort_order: i32,
    pub dedup_policy: DedupPolicy,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub members: Vec<UnifiedLibraryMember>,
}

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct UnifiedLibraryMember {
    pub id: i64,
    pub unified_library_id: i64,
    pub server_id: i64,
    pub original_library_id: String,
    pub original_library_name: String,
    pub enabled: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AvailableLibrary {
    pub server_id: i64,
    pub server_name: String,
    pub server_url: String,
    pub libraries: Vec<MediaFolder>,
}

#[derive(Debug, Clone)]
pub struct UnifiedLibraryService {
    pool: SqlitePool,
}

impl UnifiedLibraryService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn create(
        &self,
        name: &str,
        collection_type: &str,
        dedup_policy: DedupPolicy,
    ) -> Result<UnifiedLibrary, sqlx::Error> {
        let now = chrono::Utc::now();
        let virtual_library_id = generate_token();

        let id = sqlx::query(
            r#"
            INSERT INTO unified_libraries (virtual_library_id, name, collection_type, sort_order, dedup_policy, created_at, updated_at)
            VALUES (?, ?, ?, COALESCE((SELECT MAX(sort_order) + 1 FROM unified_libraries), 0), ?, ?, ?)
            "#,
        )
        .bind(&virtual_library_id)
        .bind(name)
        .bind(collection_type)
        .bind(dedup_policy.as_str())
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?
        .last_insert_rowid();

        info!("Created unified library '{}'", name);

        self.get(id).await?.ok_or(sqlx::Error::RowNotFound)
    }

    pub async fn update(
        &self,
        id: i64,
        name: &str,
        collection_type: &str,
        dedup_policy: DedupPolicy,
        sort_order: i32,
    ) -> Result<bool, sqlx::Error> {
        let now = chrono::Utc::now();
        let result = sqlx::query(
            r#"
            UPDATE unified_libraries
            SET name = ?, collection_type = ?, dedup_policy = ?, sort_order = ?, updated_at = ?
            WHERE id = ?
            "#,
        )
        .bind(name)
        .bind(collection_type)
        .bind(dedup_policy.as_str())
        .bind(sort_order)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn delete(&self, id: i64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM unified_libraries WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn get(&self, id: i64) -> Result<Option<UnifiedLibrary>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT id, virtual_library_id, name, collection_type, sort_order, dedup_policy, created_at, updated_at
            FROM unified_libraries
            WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(row) => Ok(Some(self.row_to_library(row).await?)),
            None => Ok(None),
        }
    }

    pub async fn get_by_virtual_id(
        &self,
        virtual_library_id: &str,
    ) -> Result<Option<UnifiedLibrary>, sqlx::Error> {
        let row = sqlx::query(
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
            Some(row) => Ok(Some(self.row_to_library(row).await?)),
            None => Ok(None),
        }
    }

    pub async fn list_all(&self) -> Result<Vec<UnifiedLibrary>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT id, virtual_library_id, name, collection_type, sort_order, dedup_policy, created_at, updated_at
            FROM unified_libraries
            ORDER BY sort_order ASC, name ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut libraries = Vec::with_capacity(rows.len());
        for row in rows {
            libraries.push(self.row_to_library(row).await?);
        }
        Ok(libraries)
    }

    pub async fn add_member(
        &self,
        unified_library_id: i64,
        server_id: i64,
        original_library_id: &str,
        original_library_name: &str,
    ) -> Result<UnifiedLibraryMember, sqlx::Error> {
        let now = chrono::Utc::now();
        let id = sqlx::query(
            r#"
            INSERT INTO unified_library_members (unified_library_id, server_id, original_library_id, original_library_name, enabled, created_at)
            VALUES (?, ?, ?, ?, 1, ?)
            ON CONFLICT(unified_library_id, server_id, original_library_id) DO UPDATE SET
                original_library_name = excluded.original_library_name,
                enabled = 1
            "#,
        )
        .bind(unified_library_id)
        .bind(server_id)
        .bind(original_library_id)
        .bind(original_library_name)
        .bind(now)
        .execute(&self.pool)
        .await?
        .last_insert_rowid();

        if id > 0 {
            return self.get_member(id).await?.ok_or(sqlx::Error::RowNotFound);
        }

        let row = sqlx::query_as::<_, UnifiedLibraryMember>(
            r#"
            SELECT id, unified_library_id, server_id, original_library_id, original_library_name,
                   CAST(enabled AS INTEGER) as enabled, created_at
            FROM unified_library_members
            WHERE unified_library_id = ? AND server_id = ? AND original_library_id = ?
            "#,
        )
        .bind(unified_library_id)
        .bind(server_id)
        .bind(original_library_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(row)
    }

    pub async fn remove_member(&self, member_id: i64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM unified_library_members WHERE id = ?")
            .bind(member_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn list_members(
        &self,
        unified_library_id: i64,
    ) -> Result<Vec<UnifiedLibraryMember>, sqlx::Error> {
        sqlx::query_as::<_, UnifiedLibraryMember>(
            r#"
            SELECT id, unified_library_id, server_id, original_library_id, original_library_name,
                   CAST(enabled AS INTEGER) as enabled, created_at
            FROM unified_library_members
            WHERE unified_library_id = ?
            ORDER BY original_library_name ASC
            "#,
        )
        .bind(unified_library_id)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn reorder(&self, ids: &[i64]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let now = chrono::Utc::now();
        for (sort_order, id) in ids.iter().enumerate() {
            sqlx::query(
                "UPDATE unified_libraries SET sort_order = ?, updated_at = ? WHERE id = ?",
            )
            .bind(sort_order as i32)
            .bind(now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_available_libraries(
        &self,
        server_storage: &ServerStorageService,
        library_sync: &crate::library_sync_service::LibrarySyncService,
    ) -> Result<Vec<AvailableLibrary>, anyhow::Error> {
        let servers = server_storage.list_servers().await?;
        let mut available = Vec::new();
        for server in servers {
            let libraries = library_sync.fetch_server_media_folders(&server).await?;
            available.push(AvailableLibrary {
                server_id: server.id,
                server_name: server.name,
                server_url: server.url.to_string(),
                libraries,
            });
        }
        Ok(available)
    }

    async fn get_member(&self, id: i64) -> Result<Option<UnifiedLibraryMember>, sqlx::Error> {
        sqlx::query_as::<_, UnifiedLibraryMember>(
            r#"
            SELECT id, unified_library_id, server_id, original_library_id, original_library_name,
                   CAST(enabled AS INTEGER) as enabled, created_at
            FROM unified_library_members
            WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    async fn row_to_library(&self, row: sqlx::sqlite::SqliteRow) -> Result<UnifiedLibrary, sqlx::Error> {
        let id: i64 = row.get("id");
        let members = self.list_members(id).await?;
        Ok(UnifiedLibrary {
            id,
            virtual_library_id: row.get("virtual_library_id"),
            name: row.get("name"),
            collection_type: row.get("collection_type"),
            sort_order: row.get("sort_order"),
            dedup_policy: row
                .get::<String, _>("dedup_policy")
                .parse()
                .unwrap_or_default(),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            members,
        })
    }
}
