//! Manage configured library groups, member assignments, and collection-type constraints.
use super::*;

impl VirtualLibraryService {
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
