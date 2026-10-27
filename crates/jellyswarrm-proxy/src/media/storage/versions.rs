//! Resolve version groups, parent groups, and observed media-source routes.
use super::*;

impl MediaStorageService {
    /// Resolve episode continuity from persisted catalog identity, not the
    /// short-lived display-metadata cache. Ambiguous or foreign-viewer parent
    /// identities cannot establish a preference.
    pub async fn get_episode_series_id(
        &self,
        virtual_media_id: &str,
        viewer: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT alias.provider_id, scope.scope_key
            FROM movie_version_aliases alias
            JOIN movie_version_members member ON member.scope_id = alias.scope_id
                AND member.media_mapping_id = alias.media_mapping_id
            JOIN media_mappings mapping ON mapping.id = member.media_mapping_id
            JOIN movie_catalog_scopes scope ON scope.id = member.scope_id
            LEFT JOIN movie_version_group_ids ids ON ids.group_id = member.group_id
            WHERE alias.provider = 'series-group:episode'
                AND (mapping.virtual_media_id = ? OR ids.virtual_media_id = ?)
            "#,
        )
        .bind(Self::normalize_uuid(virtual_media_id))
        .bind(Self::normalize_uuid(virtual_media_id))
        .fetch_all(&self.pool)
        .await?;
        let mut series = BTreeSet::new();
        for row in rows {
            if !MediaCatalogScope::belongs_to(&row.try_get::<String, _>("scope_key")?, viewer) {
                continue;
            }
            let alias = MediaAlias::parse_storage(
                "series-group:episode",
                &row.try_get::<String, _>("provider_id")?,
            );
            if let Some(parent) = alias.as_ref().and_then(MediaAlias::episode_series_id) {
                series.insert(Self::normalize_uuid(parent));
            }
        }
        Ok((series.len() == 1).then(|| series.into_iter().next().unwrap()))
    }

    pub async fn get_media_version_group(
        &self,
        virtual_media_id: &str,
    ) -> Result<Option<MediaVersionGroup>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT
                group_row.id,
                group_id.virtual_media_id,
                group_row.scope_id,
                group_row.published,
                group_row.ambiguous
            FROM movie_version_group_ids group_id
            JOIN movie_version_groups group_row ON group_row.id = group_id.group_id
            WHERE group_id.virtual_media_id = ?
            "#,
        )
        .bind(Self::normalize_uuid(virtual_media_id))
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref()
            .map(Self::media_version_group_from_row)
            .transpose()
    }

    /// Detail-only reverse lookup. Never infer a group from memberships that
    /// have not been exposed as source routes. Multiple viewer scopes must agree
    /// on the owner and exact active membership; use only the oldest group.
    pub async fn get_media_version_detail_route(
        &self,
        virtual_media_id: &str,
        viewer: &str,
    ) -> Result<Option<(MediaVersionGroup, MediaMapping)>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT group_row.id, group_id.virtual_media_id,
                group_row.scope_id, group_row.published, group_row.ambiguous,
                scope.scope_key, route.member_mapping_id
            FROM movie_version_sources route
            JOIN movie_version_groups group_row ON group_row.id = route.group_id
                AND group_row.scope_id = route.scope_id
            JOIN movie_version_group_ids group_id ON group_id.group_id = group_row.id
                AND group_id.canonical = 1
            JOIN movie_catalog_scopes scope ON scope.id = route.scope_id
            JOIN media_mappings source ON source.id = route.source_mapping_id
            JOIN media_mappings member ON member.id = route.member_mapping_id
            WHERE (source.virtual_media_id = ? OR member.virtual_media_id = ?)
                AND group_row.published = 1 AND group_row.ambiguous = 0
                AND source.server_id = member.server_id
            ORDER BY group_row.id
            "#,
        )
        .bind(Self::normalize_uuid(virtual_media_id))
        .bind(Self::normalize_uuid(virtual_media_id))
        .fetch_all(&self.pool)
        .await?;
        let mut result: Option<(MediaVersionGroup, MediaMapping)> = None;
        let mut active_member_ids = HashSet::new();
        for row in rows {
            let key: String = row.try_get("scope_key")?;
            if !MediaCatalogScope::belongs_to(&key, viewer) {
                continue;
            }
            let group = Self::media_version_group_from_row(&row)?;
            let member_id: i64 = row.try_get("member_mapping_id")?;
            let members = self.get_media_version_members(group.id).await?;
            let Some(member) = members.iter().find(|member| member.mapping.id == member_id) else {
                continue;
            };
            let member_ids = members.iter().map(|member| member.mapping.id).collect();
            if let Some((_, owner)) = &result {
                if owner.id != member_id || active_member_ids != member_ids {
                    return Ok(None);
                }
            } else {
                active_member_ids = member_ids;
                result = Some((group, member.mapping.clone()));
            }
        }
        Ok(result)
    }

    /// Navigation may use a published superset of filtered observations.
    /// Incomparable groups still cannot establish a unique parent identity.
    pub async fn get_media_parent_group_id(
        &self,
        virtual_media_id: &str,
        viewer: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        self.get_media_parent_group_id_for_scope(virtual_media_id, viewer, None)
            .await
    }

    /// Prefer the catalog being reconciled. Filtered search/latest scopes may
    /// observe fewer copies; they must not veto that catalog's parent identity.
    pub async fn get_media_parent_group_id_for_scope(
        &self,
        virtual_media_id: &str,
        viewer: &str,
        preferred_scope: Option<&str>,
    ) -> Result<Option<String>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT g.id, ids.virtual_media_id, scope.scope_key
            FROM movie_version_members member
            JOIN media_mappings mapping ON mapping.id = member.media_mapping_id
            JOIN movie_version_groups g ON g.id = member.group_id
            JOIN movie_version_group_ids ids ON ids.group_id = g.id AND ids.canonical = 1
            JOIN movie_catalog_scopes scope ON scope.id = g.scope_id
            WHERE mapping.virtual_media_id = ? AND g.published = 1 AND g.ambiguous = 0
            ORDER BY g.id
            "#,
        )
        .bind(Self::normalize_uuid(virtual_media_id))
        .fetch_all(&self.pool)
        .await?;
        let mut candidates = Vec::<(String, HashSet<i64>)>::new();
        if let Some(scope) =
            preferred_scope.filter(|scope| MediaCatalogScope::belongs_to(scope, viewer))
        {
            for row in &rows {
                if row.try_get::<String, _>("scope_key")? == scope {
                    let members = self.get_media_version_members(row.try_get("id")?).await?;
                    if members.iter().any(|member| {
                        member.mapping.virtual_media_id == Self::normalize_uuid(virtual_media_id)
                    }) {
                        return Ok(Some(row.try_get("virtual_media_id")?));
                    }
                }
            }
        }
        for row in rows {
            let key: String = row.try_get("scope_key")?;
            if !MediaCatalogScope::belongs_to(&key, viewer) {
                continue;
            }
            let members = self.get_media_version_members(row.try_get("id")?).await?;
            if !members.iter().any(|member| {
                member.mapping.virtual_media_id == Self::normalize_uuid(virtual_media_id)
            }) {
                continue;
            }
            let ids = members.iter().map(|member| member.mapping.id).collect();
            candidates.push((row.try_get("virtual_media_id")?, ids));
        }
        // A search containing A+B is compatible with a browse containing A+B+C.
        // A+B and A+C alone do not agree. Inspect every candidate before deciding,
        // as their common superset may have been published after both subsets.
        Ok(candidates
            .iter()
            .find(|(_, members)| {
                candidates
                    .iter()
                    .all(|(_, observed)| observed.is_subset(members))
            })
            .map(|(id, _)| id.clone()))
    }

    pub async fn get_media_version_members(
        &self,
        group_id: i64,
    ) -> Result<Vec<MediaVersionMember>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT
                m.id AS media_id,
                m.virtual_media_id,
                m.original_media_id,
                m.server_id AS media_server_id,
                m.server_url AS media_server_url,
                m.created_at AS media_created_at,
                s.id AS server_id,
                s.name AS server_name,
                s.url AS server_url_full,
                s.priority,
                s.media_streaming_mode,
                s.created_at AS server_created_at,
                s.updated_at AS server_updated_at
            FROM movie_version_members vm
            JOIN media_mappings m ON m.id = vm.media_mapping_id
            JOIN servers s ON s.id = m.server_id
            JOIN movie_version_groups version_group ON version_group.id = vm.group_id
            WHERE vm.group_id = ? AND version_group.ambiguous = 0
              AND EXISTS (
                  SELECT 1 FROM movie_catalog_sightings sighting
                  WHERE sighting.scope_id = vm.scope_id
                    AND sighting.media_mapping_id = vm.media_mapping_id
              )
              AND EXISTS (
                  SELECT 1 FROM movie_version_aliases alias
                  WHERE alias.scope_id = vm.scope_id
                    AND alias.media_mapping_id = vm.media_mapping_id
              )
            "#,
        )
        .bind(group_id)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let mapping = MediaMapping {
                    id: row.try_get("media_id")?,
                    virtual_media_id: row.try_get("virtual_media_id")?,
                    original_media_id: row.try_get("original_media_id")?,
                    server_id: ServerId::new(row.try_get("media_server_id")?),
                    server_url: row.try_get("media_server_url")?,
                    created_at: row.try_get("media_created_at")?,
                };
                Ok(MediaVersionMember {
                    mapping,
                    server: Server::from_session_join_row(&row)?,
                })
            })
            .collect()
    }

    pub async fn get_media_version_members_by_virtual_id(
        &self,
        group_virtual_id: &str,
    ) -> Result<Vec<MediaVersionMember>, sqlx::Error> {
        let Some(group) = self.get_media_version_group(group_virtual_id).await? else {
            return Ok(Vec::new());
        };
        self.get_media_version_members(group.id).await
    }

    /// Replaces source routing observations only for members included in this
    /// detail response, leaving inaccessible or temporarily offline members
    /// untouched.
    pub async fn replace_media_version_sources(
        &self,
        group_id: i64,
        generation: ReconciliationGeneration,
        refreshed_member_mapping_ids: &[i64],
        observations: &[MediaVersionSourceObservation],
    ) -> Result<bool, sqlx::Error> {
        self.store_media_version_sources(
            group_id,
            generation.0,
            refreshed_member_mapping_ids,
            observations,
            true,
        )
        .await
    }

    /// PlaybackInfo can be filtered by source, live stream, or client capabilities.
    /// Record its positive observations without removing other advertised versions.
    pub async fn observe_media_version_sources(
        &self,
        group_id: i64,
        generation: ReconciliationGeneration,
        member_mapping_ids: &[i64],
        observations: &[MediaVersionSourceObservation],
    ) -> Result<bool, sqlx::Error> {
        self.store_media_version_sources(
            group_id,
            generation.0,
            member_mapping_ids,
            observations,
            false,
        )
        .await
    }

    pub(super) async fn store_media_version_sources(
        &self,
        group_id: i64,
        generation: i64,
        refreshed_member_mapping_ids: &[i64],
        observations: &[MediaVersionSourceObservation],
        replace: bool,
    ) -> Result<bool, sqlx::Error> {
        let _reconciliation_guard = self.media_version_reconciliation.lock().await;
        let mut transaction = self.pool.begin().await?;
        let refreshed_members = refreshed_member_mapping_ids
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        if observations
            .iter()
            .any(|observation| !refreshed_members.contains(&observation.member_mapping_id))
        {
            transaction.rollback().await?;
            return Ok(false);
        }
        let mut scope_id = None;
        for member_id in refreshed_member_mapping_ids {
            let current_member: Option<(i64, i64)> = sqlx::query_as(
                r#"
                SELECT member.sources_generation, member.scope_id
                FROM movie_version_members member
                JOIN movie_version_groups version_group ON version_group.id = member.group_id
                WHERE member.group_id = ? AND member.media_mapping_id = ?
                  AND version_group.ambiguous = 0
                  AND EXISTS (
                      SELECT 1 FROM movie_catalog_sightings sighting
                      WHERE sighting.scope_id = member.scope_id
                        AND sighting.media_mapping_id = member.media_mapping_id
                  )
                "#,
            )
            .bind(group_id)
            .bind(member_id)
            .fetch_optional(&mut *transaction)
            .await?;
            let Some((current_generation, member_scope_id)) = current_member else {
                transaction.rollback().await?;
                return Ok(false);
            };
            if current_generation >= generation
                || scope_id.is_some_and(|scope_id| scope_id != member_scope_id)
            {
                transaction.rollback().await?;
                return Ok(false);
            }
            scope_id = Some(member_scope_id);
            if replace {
                sqlx::query(
                    "DELETE FROM movie_version_sources WHERE scope_id = ? AND group_id = ? AND member_mapping_id = ?",
                )
                .bind(member_scope_id)
                .bind(group_id)
                .bind(member_id)
                .execute(&mut *transaction)
                .await?;
            }
            sqlx::query(
                "UPDATE movie_version_members SET sources_generation = ? WHERE group_id = ? AND media_mapping_id = ?",
            )
            .bind(generation)
            .bind(group_id)
            .bind(member_id)
            .execute(&mut *transaction)
            .await?;
        }

        let Some(scope_id) = scope_id else {
            transaction.commit().await?;
            return Ok(observations.is_empty());
        };
        for observation in observations {
            Self::upsert_movie_version_source_route(
                &mut transaction,
                scope_id,
                group_id,
                observation.member_mapping_id,
                &observation.source_virtual_id,
            )
            .await?;
        }

        transaction.commit().await?;
        Ok(true)
    }

    pub(super) async fn upsert_movie_version_source_route(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        scope_id: i64,
        group_id: i64,
        member_mapping_id: i64,
        source_virtual_id: &str,
    ) -> Result<(), sqlx::Error> {
        let source_mapping_id: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM media_mappings WHERE virtual_media_id = ?")
                .bind(Self::normalize_uuid(source_virtual_id))
                .fetch_optional(&mut **transaction)
                .await?;
        let Some((source_mapping_id,)) = source_mapping_id else {
            return Ok(());
        };

        sqlx::query(
            r#"
            INSERT INTO movie_version_sources (
                scope_id, group_id, member_mapping_id, source_mapping_id, observed_at
            )
            SELECT ?, ?, ?, ?, ?
            WHERE EXISTS (
                SELECT 1 FROM movie_version_members member
                JOIN media_mappings member_mapping ON member_mapping.id = member.media_mapping_id
                JOIN media_mappings source_mapping ON source_mapping.id = ?
                WHERE member.scope_id = ? AND member.group_id = ? AND member.media_mapping_id = ?
                  AND member_mapping.server_id = source_mapping.server_id
                  AND EXISTS (
                      SELECT 1 FROM movie_catalog_sightings sighting
                      WHERE sighting.scope_id = member.scope_id
                        AND sighting.media_mapping_id = member.media_mapping_id
                  )
            )
            ON CONFLICT (scope_id, group_id, source_mapping_id) DO UPDATE SET
                member_mapping_id = excluded.member_mapping_id,
                observed_at = excluded.observed_at
            "#,
        )
        .bind(scope_id)
        .bind(group_id)
        .bind(member_mapping_id)
        .bind(source_mapping_id)
        .bind(chrono::Utc::now())
        .bind(source_mapping_id)
        .bind(scope_id)
        .bind(group_id)
        .bind(member_mapping_id)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    pub async fn get_media_version_source_route(
        &self,
        group_id: i64,
        source_virtual_id: &str,
    ) -> Result<Option<MediaVersionSourceRoute>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT
                source.id AS source_id,
                source.virtual_media_id AS source_virtual_media_id,
                source.original_media_id AS source_original_media_id,
                source.server_id AS source_server_id,
                source.server_url AS source_server_url,
                source.created_at AS source_created_at,
                member.id AS member_id,
                member.virtual_media_id AS member_virtual_media_id,
                member.original_media_id AS member_original_media_id,
                member.server_id AS member_server_id,
                member.server_url AS member_server_url,
                member.created_at AS member_created_at
            FROM movie_version_sources source_route
            JOIN movie_version_members version_member ON
                version_member.scope_id = source_route.scope_id
                AND version_member.group_id = source_route.group_id
                AND version_member.media_mapping_id = source_route.member_mapping_id
            JOIN movie_version_groups version_group ON
                version_group.id = version_member.group_id
                AND version_group.scope_id = version_member.scope_id
            JOIN media_mappings source ON source.id = source_route.source_mapping_id
            JOIN media_mappings member ON member.id = source_route.member_mapping_id
            WHERE source_route.group_id = ? AND source.virtual_media_id = ?
              AND version_group.ambiguous = 0
              AND EXISTS (
                  SELECT 1 FROM movie_catalog_sightings sighting
                  WHERE sighting.scope_id = version_member.scope_id
                    AND sighting.media_mapping_id = version_member.media_mapping_id
              )
            "#,
        )
        .bind(group_id)
        .bind(Self::normalize_uuid(source_virtual_id))
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|row| MediaVersionSourceRoute {
            source_mapping: MediaMapping {
                id: row.get("source_id"),
                virtual_media_id: row.get("source_virtual_media_id"),
                original_media_id: row.get("source_original_media_id"),
                server_id: ServerId::new(row.get("source_server_id")),
                server_url: row.get("source_server_url"),
                created_at: row.get("source_created_at"),
            },
            member_mapping: MediaMapping {
                id: row.get("member_id"),
                virtual_media_id: row.get("member_virtual_media_id"),
                original_media_id: row.get("member_original_media_id"),
                server_id: ServerId::new(row.get("member_server_id")),
                server_url: row.get("member_server_url"),
                created_at: row.get("member_created_at"),
            },
        }))
    }

    pub(super) fn media_version_group_from_row(
        row: &SqliteRow,
    ) -> Result<MediaVersionGroup, sqlx::Error> {
        Ok(MediaVersionGroup {
            id: row.try_get("id")?,
            virtual_media_id: row.try_get("virtual_media_id")?,
            scope_id: row.try_get("scope_id")?,
            published: row.try_get("published")?,
            ambiguous: row.try_get::<i64, _>("ambiguous")? != 0,
        })
    }
}
