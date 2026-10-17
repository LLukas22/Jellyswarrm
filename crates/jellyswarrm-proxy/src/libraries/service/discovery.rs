//! Discover backend libraries and reconcile generation-fenced automatic and viewer inventories.
use super::*;

impl VirtualLibraryService {
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

    pub(super) async fn get_automatic_library(
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

    pub(super) async fn get_automatic_library_members(
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
                    JOIN library_groups configured_group ON configured_group.virtual_id = configured.group_virtual_id \
                    JOIN merged_libraries automatic ON automatic.virtual_id = m.automatic_virtual_id \
                      AND (LOWER(automatic.collection_type) = LOWER(configured_group.collection_type) \
                           OR LOWER(automatic.collection_type) LIKE LOWER(configured_group.collection_type) || ':%') \
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

    /// Use the shared persistent clock so in-flight inventories remain ordered
    /// across service clones and independently for each viewer/backend pair.
    pub async fn begin_viewer_inventory(
        &self,
    ) -> Result<crate::media::storage::ReconciliationGeneration, sqlx::Error> {
        self.media_storage.begin_media_reconciliation().await
    }

    pub async fn replace_viewer_libraries(
        &self,
        user_id: &str,
        server_id: ServerId,
        generation: crate::media::storage::ReconciliationGeneration,
        libraries: &[DiscoveredLibrary],
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "INSERT INTO library_viewer_inventories (user_id, server_id, generation) VALUES (?, ?, ?) \
             ON CONFLICT(user_id, server_id) DO UPDATE SET generation = excluded.generation \
             WHERE excluded.generation > library_viewer_inventories.generation",
        )
        .bind(user_id).bind(server_id.as_i64()).bind(generation.as_i64())
        .execute(&mut *tx).await?;
        if updated.rows_affected() == 0 {
            return Ok(());
        }
        sqlx::query("DELETE FROM library_viewer_members WHERE user_id = ? AND server_id = ?")
            .bind(user_id)
            .bind(server_id.as_i64())
            .execute(&mut *tx)
            .await?;
        for library in libraries
            .iter()
            .filter(|library| library.server_id == server_id)
        {
            sqlx::query("INSERT INTO library_viewer_members (user_id, server_id, original_library_id, collection_type) VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING")
                .bind(user_id).bind(server_id.as_i64())
                .bind(normalize_library_id(&library.original_library_id))
                .bind(library.collection_type.trim().to_ascii_lowercase())
                .execute(&mut *tx).await?;
        }
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
}
