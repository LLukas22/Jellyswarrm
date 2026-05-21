use sqlx::SqlitePool;
use tracing::debug;
use uuid::Uuid;

// Jellyfin clients (especially Android TV) send UUIDs in hyphenated form; the proxy
// stores them without hyphens, so normalize before any DB comparison.
fn normalize_uuid(s: &str) -> String {
    match Uuid::parse_str(s) {
        Ok(uuid) => uuid.simple().to_string(),
        Err(_) => s.to_string(),
    }
}

#[derive(Debug, Clone)]
pub struct MergedLibrary {
    pub virtual_id: String,
    pub collection_type: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct MergedLibraryMember {
    pub server_url: String,
    pub virtual_library_id: String,
}

#[derive(Debug, Clone)]
pub struct MergedLibraryService {
    pool: SqlitePool,
}

impl MergedLibraryService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn get_or_create(
        &self,
        collection_type: &str,
        name: &str,
    ) -> Result<MergedLibrary, sqlx::Error> {
        if let Some(lib) = self.get_by_collection_type(collection_type).await? {
            return Ok(lib);
        }

        let virtual_id = Uuid::new_v4().simple().to_string();
        sqlx::query(
            "INSERT INTO merged_libraries (virtual_id, collection_type, name) \
             VALUES (?, ?, ?) \
             ON CONFLICT(collection_type) DO NOTHING",
        )
        .bind(&virtual_id)
        .bind(collection_type)
        .bind(name)
        .execute(&self.pool)
        .await?;

        // Re-fetch handles the DO NOTHING race.
        self.get_by_collection_type(collection_type)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }

    pub async fn get_by_virtual_id(
        &self,
        virtual_id: &str,
    ) -> Result<Option<MergedLibrary>, sqlx::Error> {
        let virtual_id = normalize_uuid(virtual_id);
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT virtual_id, collection_type, name \
             FROM merged_libraries WHERE virtual_id = ?",
        )
        .bind(&virtual_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(vid, ct, name)| MergedLibrary {
            virtual_id: vid,
            collection_type: ct,
            name,
        }))
    }

    async fn get_by_collection_type(
        &self,
        collection_type: &str,
    ) -> Result<Option<MergedLibrary>, sqlx::Error> {
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT virtual_id, collection_type, name \
             FROM merged_libraries WHERE collection_type = ?",
        )
        .bind(collection_type)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(vid, ct, name)| MergedLibrary {
            virtual_id: vid,
            collection_type: ct,
            name,
        }))
    }

    pub async fn upsert_members(
        &self,
        merged_virtual_id: &str,
        members: &[(String, String)],
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // Delete-then-insert so servers that no longer carry a library don't leave
        // stale rows that would route fan-out requests to the wrong place.
        sqlx::query("DELETE FROM merged_library_members WHERE merged_virtual_id = ?")
            .bind(merged_virtual_id)
            .execute(&mut *tx)
            .await?;
        for (server_url, virtual_library_id) in members {
            sqlx::query(
                "INSERT INTO merged_library_members \
                 (merged_virtual_id, server_url, virtual_library_id) VALUES (?, ?, ?)",
            )
            .bind(merged_virtual_id)
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

    pub async fn get_members(
        &self,
        merged_virtual_id: &str,
    ) -> Result<Vec<MergedLibraryMember>, sqlx::Error> {
        let merged_virtual_id = normalize_uuid(merged_virtual_id);
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT server_url, virtual_library_id \
             FROM merged_library_members WHERE merged_virtual_id = ?",
        )
        .bind(&merged_virtual_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(server_url, virtual_library_id)| MergedLibraryMember {
                server_url,
                virtual_library_id,
            })
            .collect())
    }

    pub async fn get_first_member_virtual_id(
        &self,
        merged_virtual_id: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        let merged_virtual_id = normalize_uuid(merged_virtual_id);
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT virtual_library_id FROM merged_library_members \
             WHERE merged_virtual_id = ? \
             ORDER BY server_url LIMIT 1",
        )
        .bind(&merged_virtual_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(id,)| id))
    }

    pub async fn resolve(
        &self,
        virtual_id: &str,
    ) -> Result<Option<(MergedLibrary, Vec<MergedLibraryMember>)>, sqlx::Error> {
        let Some(lib) = self.get_by_virtual_id(virtual_id).await? else {
            return Ok(None);
        };
        let members = self.get_members(&lib.virtual_id).await?;
        Ok(Some((lib, members)))
    }
}
