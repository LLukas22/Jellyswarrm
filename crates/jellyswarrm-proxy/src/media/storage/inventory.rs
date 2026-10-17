//! Reconcile complete or partial catalog evidence and rebuild stable media groups atomically.
use super::*;

impl MediaStorageService {
    pub async fn begin_media_reconciliation(
        &self,
    ) -> Result<ReconciliationGeneration, sqlx::Error> {
        let (generation,): (i64,) = sqlx::query_as(
            "UPDATE movie_version_clock SET generation = generation + 1 WHERE singleton = 1 RETURNING generation",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(ReconciliationGeneration(generation))
    }

    #[cfg(test)]
    pub async fn reconcile_media_catalog(
        &self,
        scope_key: &str,
        generation: ReconciliationGeneration,
        snapshots: &[MediaCatalogSnapshot],
        prune_missing_sources: bool,
    ) -> Result<HashMap<String, StableMediaGroup>, sqlx::Error> {
        self.reconcile_media_catalog_with_coverage(
            scope_key,
            generation,
            snapshots,
            prune_missing_sources,
            &MediaKind::ALL.into_iter().collect(),
        )
        .await
    }

    /// A complete source scan replaces sightings only for the types covered by
    /// the query. Per-type watermarks also fence older full or filtered scans.
    #[cfg(test)]
    pub async fn reconcile_media_catalog_with_coverage(
        &self,
        scope_key: &str,
        generation: ReconciliationGeneration,
        snapshots: &[MediaCatalogSnapshot],
        prune_missing_sources: bool,
        complete_types: &BTreeSet<MediaKind>,
    ) -> Result<HashMap<String, StableMediaGroup>, sqlx::Error> {
        self.reconcile_inventory(
            scope_key,
            generation,
            snapshots,
            prune_missing_sources,
            complete_types,
        )
        .await
    }

    pub async fn apply_inventory_update(
        &self,
        scope_key: &str,
        generation: ReconciliationGeneration,
        update: InventoryUpdate,
    ) -> Result<HashMap<String, StableMediaGroup>, sqlx::Error> {
        match update {
            InventoryUpdate::Observations(mut snapshots) => {
                for snapshot in &mut snapshots {
                    snapshot.complete = false;
                }
                self.reconcile_inventory(scope_key, generation, &snapshots, false, &BTreeSet::new())
                    .await
            }
            InventoryUpdate::CompleteInventory(inventory) => {
                self.reconcile_inventory(
                    scope_key,
                    generation,
                    &inventory.snapshots,
                    inventory.prune_missing_sources,
                    &inventory.coverage,
                )
                .await
            }
        }
    }

    pub(super) async fn reconcile_inventory(
        &self,
        scope_key: &str,
        generation: ReconciliationGeneration,
        snapshots: &[MediaCatalogSnapshot],
        prune_missing_sources: bool,
        complete_types: &BTreeSet<MediaKind>,
    ) -> Result<HashMap<String, StableMediaGroup>, sqlx::Error> {
        let generation = generation.0;
        let _guard = self.media_version_reconciliation.lock().await;
        let mut transaction = self.pool.begin().await?;
        let scope_id = Self::media_scope_id(&mut transaction, scope_key).await?;
        let (scope_sources_generation,): (i64,) =
            sqlx::query_as("SELECT sources_generation FROM movie_catalog_scopes WHERE id = ?")
                .bind(scope_id)
                .fetch_one(&mut *transaction)
                .await?;

        let mut resolved_snapshots = Vec::with_capacity(snapshots.len());
        let mut aliases_by_mapping = HashMap::<i64, (MediaKind, BTreeSet<MediaAlias>)>::new();
        for snapshot in snapshots {
            let source_generation: Option<(i64,)> = sqlx::query_as(
                "SELECT committed_generation FROM movie_catalog_sources WHERE scope_id = ? AND source_key = ?",
            )
            .bind(scope_id)
            .bind(&snapshot.source_key)
            .fetch_optional(&mut *transaction)
            .await?;
            // Complete inventories fence off deleted sightings; the source-list
            // watermark also prevents older requests from recreating pruned sources.
            if source_generation.map_or(generation <= scope_sources_generation, |(committed,)| {
                generation <= committed
            }) {
                continue;
            }
            let source_id = Self::media_source_id(
                &mut transaction,
                scope_id,
                &snapshot.source_key,
                snapshot.server_id,
            )
            .await?;
            let type_generations = sqlx::query_as::<_, (String, i64)>(
                "SELECT media_kind, committed_generation FROM movie_catalog_type_generations WHERE source_id = ?",
            )
            .bind(source_id)
            .fetch_all(&mut *transaction)
            .await?
            .into_iter()
            .collect::<HashMap<_, _>>();
            let mut observed_mapping_ids = HashSet::new();
            for observation in &snapshot.observations {
                if type_generations
                    .get(observation.kind.as_str())
                    .is_some_and(|committed| generation <= *committed)
                {
                    continue;
                }
                let mapping: Option<(i64,)> = sqlx::query_as(
                    "SELECT id FROM media_mappings WHERE virtual_media_id = ? AND server_id = ?",
                )
                .bind(Self::normalize_uuid(&observation.virtual_media_id))
                .bind(snapshot.server_id.as_i64())
                .fetch_optional(&mut *transaction)
                .await?;
                let Some((mapping_id,)) = mapping else {
                    continue;
                };
                observed_mapping_ids.insert(mapping_id);
                aliases_by_mapping
                    .entry(mapping_id)
                    .or_insert_with(|| (observation.kind, BTreeSet::new()))
                    .1
                    .extend(observation.aliases.iter().cloned());
            }
            resolved_snapshots.push((source_id, snapshot, observed_mapping_ids, type_generations));
        }

        for (mapping_id, (kind, aliases)) in aliases_by_mapping {
            let updated = sqlx::query(
                r#"
                INSERT INTO movie_version_members (
                    scope_id, media_mapping_id, aliases_generation, observed_at, media_kind
                ) VALUES (?, ?, ?, ?, ?)
                ON CONFLICT (scope_id, media_mapping_id) DO UPDATE SET
                    aliases_generation = excluded.aliases_generation,
                    media_kind = excluded.media_kind,
                    observed_at = excluded.observed_at
                WHERE movie_version_members.aliases_generation < excluded.aliases_generation
                "#,
            )
            .bind(scope_id)
            .bind(mapping_id)
            .bind(generation)
            .bind(chrono::Utc::now())
            .bind(kind.as_str())
            .execute(&mut *transaction)
            .await?;
            if updated.rows_affected() == 0 {
                continue;
            }
            sqlx::query(
                "DELETE FROM movie_version_aliases WHERE scope_id = ? AND media_mapping_id = ?",
            )
            .bind(scope_id)
            .bind(mapping_id)
            .execute(&mut *transaction)
            .await?;
            for alias in aliases {
                sqlx::query(
                    "INSERT INTO movie_version_aliases (scope_id, media_mapping_id, provider, provider_id) VALUES (?, ?, ?, ?)",
                )
                .bind(scope_id)
                .bind(mapping_id)
                .bind(alias.storage_provider())
                .bind(alias.provider_id)
                .execute(&mut *transaction)
                .await?;
            }
        }

        for (source_id, snapshot, observed_mapping_ids, type_generations) in resolved_snapshots {
            for mapping_id in &observed_mapping_ids {
                sqlx::query(
                    r#"
                    INSERT INTO movie_catalog_sightings (
                        source_id, scope_id, media_mapping_id, generation
                    ) VALUES (?, ?, ?, ?)
                    ON CONFLICT (source_id, media_mapping_id) DO UPDATE SET
                        generation = MAX(movie_catalog_sightings.generation, excluded.generation)
                    "#,
                )
                .bind(source_id)
                .bind(scope_id)
                .bind(*mapping_id)
                .bind(generation)
                .execute(&mut *transaction)
                .await?;
            }

            if snapshot.complete {
                let existing = sqlx::query(
                    "SELECT sighting.media_mapping_id, member.media_kind FROM movie_catalog_sightings sighting JOIN movie_version_members member ON member.scope_id = sighting.scope_id AND member.media_mapping_id = sighting.media_mapping_id WHERE sighting.source_id = ?",
                )
                .bind(source_id)
                .fetch_all(&mut *transaction)
                .await?;
                for row in existing {
                    let mapping_id: i64 = row.get("media_mapping_id");
                    let kind: Option<String> = row.get("media_kind");
                    if let Some(kind) = kind.as_deref() {
                        if !MediaKind::parse(kind)
                            .is_some_and(|kind| complete_types.contains(&kind))
                            || type_generations
                                .get(kind)
                                .is_some_and(|committed| generation <= *committed)
                        {
                            continue;
                        }
                    } else if complete_types.len() != MediaKind::ALL.len() {
                        // Legacy unidentified members can only be removed by a
                        // full inventory until their type has been observed.
                        continue;
                    }
                    if !observed_mapping_ids.contains(&mapping_id) {
                        sqlx::query(
                            "DELETE FROM movie_catalog_sightings WHERE source_id = ? AND media_mapping_id = ? AND generation <= ?",
                        )
                        .bind(source_id)
                        .bind(mapping_id)
                        .bind(generation)
                        .execute(&mut *transaction)
                        .await?;
                    }
                }
                for kind in complete_types {
                    sqlx::query("INSERT INTO movie_catalog_type_generations (source_id, media_kind, committed_generation) VALUES (?, ?, ?) ON CONFLICT(source_id, media_kind) DO UPDATE SET committed_generation = MAX(committed_generation, excluded.committed_generation)")
                        .bind(source_id)
                        .bind(kind.as_str())
                        .bind(generation)
                        .execute(&mut *transaction)
                        .await?;
                }
                if complete_types.len() == MediaKind::ALL.len() {
                    sqlx::query(
                        "UPDATE movie_catalog_sources SET committed_generation = ? WHERE id = ?",
                    )
                    .bind(generation)
                    .bind(source_id)
                    .execute(&mut *transaction)
                    .await?;
                }
            }
        }

        sqlx::query("UPDATE movie_catalog_scopes SET committed_generation = MAX(committed_generation, ?) WHERE id = ?")
            .bind(generation)
            .bind(scope_id)
            .execute(&mut *transaction)
            .await?;

        if prune_missing_sources
            && complete_types.len() == MediaKind::ALL.len()
            && generation > scope_sources_generation
        {
            let current_source_keys = snapshots
                .iter()
                .map(|snapshot| snapshot.source_key.as_str())
                .collect::<HashSet<_>>();
            let source_rows =
                sqlx::query("SELECT id, source_key FROM movie_catalog_sources WHERE scope_id = ?")
                    .bind(scope_id)
                    .fetch_all(&mut *transaction)
                    .await?;
            for row in source_rows {
                let source_key: String = row.get("source_key");
                if !current_source_keys.contains(source_key.as_str()) {
                    let source_id: i64 = row.get("id");
                    sqlx::query("DELETE FROM movie_catalog_sources WHERE id = ? AND committed_generation <= ? AND NOT EXISTS (SELECT 1 FROM movie_catalog_sightings WHERE source_id = movie_catalog_sources.id AND generation > ?) AND NOT EXISTS (SELECT 1 FROM movie_catalog_type_generations WHERE source_id = movie_catalog_sources.id AND committed_generation > ?)")
                        .bind(source_id)
                        .bind(generation)
                        .bind(generation)
                        .bind(generation)
                        .execute(&mut *transaction)
                        .await?;
                }
            }
            sqlx::query("UPDATE movie_catalog_scopes SET sources_generation = ? WHERE id = ?")
                .bind(generation)
                .bind(scope_id)
                .execute(&mut *transaction)
                .await?;
        }

        let stable_groups = Self::rebuild_media_groups(&mut transaction, scope_id).await?;
        transaction.commit().await?;
        Ok(stable_groups)
    }

    pub(super) async fn media_scope_id(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        scope_key: &str,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query(
            "INSERT INTO movie_catalog_scopes (scope_key) VALUES (?) ON CONFLICT DO NOTHING",
        )
        .bind(scope_key)
        .execute(&mut **transaction)
        .await?;
        let (id,): (i64,) =
            sqlx::query_as("SELECT id FROM movie_catalog_scopes WHERE scope_key = ?")
                .bind(scope_key)
                .fetch_one(&mut **transaction)
                .await?;
        Ok(id)
    }

    pub(super) async fn media_source_id(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        scope_id: i64,
        source_key: &str,
        server_id: ServerId,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query(
            "INSERT INTO movie_catalog_sources (scope_id, source_key, server_id) VALUES (?, ?, ?) ON CONFLICT (scope_id, source_key) DO UPDATE SET server_id = excluded.server_id",
        )
        .bind(scope_id)
        .bind(source_key)
        .bind(server_id.as_i64())
        .execute(&mut **transaction)
        .await?;
        let (id,): (i64,) = sqlx::query_as(
            "SELECT id FROM movie_catalog_sources WHERE scope_id = ? AND source_key = ?",
        )
        .bind(scope_id)
        .bind(source_key)
        .fetch_one(&mut **transaction)
        .await?;
        Ok(id)
    }

    pub(super) async fn rebuild_media_groups(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        scope_id: i64,
    ) -> Result<HashMap<String, StableMediaGroup>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT
                mapping.id,
                mapping.virtual_media_id,
                mapping.server_id,
                member.group_id
            FROM movie_version_members member
            JOIN media_mappings mapping ON mapping.id = member.media_mapping_id
            WHERE member.scope_id = ?
              AND EXISTS (
                  SELECT 1 FROM movie_catalog_sightings sighting
                  WHERE sighting.scope_id = member.scope_id
                    AND sighting.media_mapping_id = member.media_mapping_id
              )
              AND EXISTS (
                  SELECT 1 FROM movie_version_aliases alias
                  WHERE alias.scope_id = member.scope_id
                    AND alias.media_mapping_id = member.media_mapping_id
              )
            "#,
        )
        .bind(scope_id)
        .fetch_all(&mut **transaction)
        .await?;
        let mut members = rows
            .into_iter()
            .map(|row| ActiveMediaMember {
                mapping_id: row.get("id"),
                virtual_media_id: row.get("virtual_media_id"),
                server_id: ServerId::new(row.get("server_id")),
                group_id: row.get("group_id"),
                aliases: BTreeSet::new(),
            })
            .collect::<Vec<_>>();
        let member_positions = members
            .iter()
            .enumerate()
            .map(|(index, member)| (member.mapping_id, index))
            .collect::<HashMap<_, _>>();
        let alias_rows = sqlx::query(
            r#"
            SELECT media_mapping_id, provider, provider_id
            FROM movie_version_aliases
            WHERE scope_id = ?
            "#,
        )
        .bind(scope_id)
        .fetch_all(&mut **transaction)
        .await?;
        for row in alias_rows {
            let mapping_id: i64 = row.get("media_mapping_id");
            let Some(&position) = member_positions.get(&mapping_id) else {
                continue;
            };
            let provider: String = row.get("provider");
            let provider_id: String = row.get("provider_id");
            let Some(alias) = MediaAlias::parse_storage(&provider, &provider_id) else {
                continue;
            };
            members[position].aliases.insert(alias);
        }

        // Source routes reference the member's group through a composite FK.
        sqlx::query(
            r#"
            DELETE FROM movie_version_sources
            WHERE scope_id = ?
              AND EXISTS (
                  SELECT 1 FROM movie_catalog_sightings sighting
                  WHERE sighting.scope_id = movie_version_sources.scope_id
                    AND sighting.media_mapping_id = movie_version_sources.member_mapping_id
              )
              AND NOT EXISTS (
                  SELECT 1 FROM movie_version_aliases alias
                  WHERE alias.scope_id = movie_version_sources.scope_id
                    AND alias.media_mapping_id = movie_version_sources.member_mapping_id
              )
            "#,
        )
        .bind(scope_id)
        .execute(&mut **transaction)
        .await?;

        sqlx::query(
            r#"
            UPDATE movie_version_members
            SET group_id = NULL
            WHERE scope_id = ?
              AND EXISTS (
                  SELECT 1 FROM movie_catalog_sightings sighting
                  WHERE sighting.scope_id = movie_version_members.scope_id
                    AND sighting.media_mapping_id = movie_version_members.media_mapping_id
              )
              AND NOT EXISTS (
                  SELECT 1 FROM movie_version_aliases alias
                  WHERE alias.scope_id = movie_version_members.scope_id
                    AND alias.media_mapping_id = movie_version_members.media_mapping_id
              )
            "#,
        )
        .bind(scope_id)
        .execute(&mut **transaction)
        .await?;

        let mut parents = (0..members.len()).collect::<Vec<_>>();
        let mut alias_owners = HashMap::new();
        for (index, member) in members.iter().enumerate() {
            for alias in &member.aliases {
                if let Some(owner) = alias_owners.insert(alias.clone(), index) {
                    union_indexes(&mut parents, owner, index);
                }
            }
        }
        let mut components = HashMap::<usize, Vec<usize>>::new();
        for index in 0..members.len() {
            let root = find_index(&mut parents, index);
            components.entry(root).or_default().push(index);
        }
        let mut components = components.into_values().collect::<Vec<_>>();
        components.sort_by_key(|component| {
            component
                .iter()
                .map(|index| members[*index].mapping_id)
                .min()
                .unwrap_or_default()
        });

        let group_alias_rows = sqlx::query(
            "SELECT group_id, provider, provider_id FROM movie_version_group_aliases WHERE scope_id = ?",
        )
        .bind(scope_id)
        .fetch_all(&mut **transaction)
        .await?;
        let mut historical_groups = HashMap::new();
        for row in group_alias_rows {
            let provider: String = row.get("provider");
            let provider_id: String = row.get("provider_id");
            let Some(alias) = MediaAlias::parse_storage(&provider, &provider_id) else {
                continue;
            };
            historical_groups.insert(alias, row.get::<i64, _>("group_id"));
        }
        let component_groups = components
            .iter()
            .map(|component| {
                let mut group_ids = component
                    .iter()
                    .filter_map(|index| members[*index].group_id)
                    .collect::<HashSet<_>>();
                for alias in component
                    .iter()
                    .flat_map(|index| members[*index].aliases.iter())
                {
                    if let Some(group_id) = historical_groups.get(alias) {
                        group_ids.insert(*group_id);
                    }
                }
                let mut group_ids = group_ids.into_iter().collect::<Vec<_>>();
                group_ids.sort_unstable();
                group_ids
            })
            .collect::<Vec<_>>();

        let mut group_components = HashMap::<i64, Vec<usize>>::new();
        for (component_index, group_ids) in component_groups.iter().enumerate() {
            for group_id in group_ids {
                group_components
                    .entry(*group_id)
                    .or_default()
                    .push(component_index);
            }
        }
        let split_groups = group_components
            .iter()
            .filter_map(|(group_id, component_indexes)| {
                (component_indexes.len() > 1).then_some(*group_id)
            })
            .collect::<HashSet<_>>();
        for group_id in &split_groups {
            sqlx::query(
                "UPDATE movie_version_groups SET ambiguous = 1 WHERE id = ? AND scope_id = ?",
            )
            .bind(group_id)
            .bind(scope_id)
            .execute(&mut **transaction)
            .await?;
        }

        let mut assignments = HashMap::new();
        for (component, existing_groups) in components.into_iter().zip(component_groups) {
            let group_id = if let Some(group_id) = existing_groups
                .iter()
                .copied()
                .find(|group_id| !split_groups.contains(group_id))
            {
                group_id
            } else {
                Self::create_media_group(transaction, scope_id).await?
            };

            for losing_group_id in existing_groups.iter().copied() {
                if losing_group_id == group_id || split_groups.contains(&losing_group_id) {
                    continue;
                }
                sqlx::query("UPDATE movie_version_group_ids SET canonical = 0 WHERE group_id = ?")
                    .bind(losing_group_id)
                    .execute(&mut **transaction)
                    .await?;
                sqlx::query("UPDATE movie_version_group_ids SET group_id = ? WHERE group_id = ?")
                    .bind(group_id)
                    .bind(losing_group_id)
                    .execute(&mut **transaction)
                    .await?;
            }

            for index in &component {
                if members[*index].group_id != Some(group_id) {
                    sqlx::query(
                        "DELETE FROM movie_version_sources WHERE scope_id = ? AND member_mapping_id = ?",
                    )
                    .bind(scope_id)
                    .bind(members[*index].mapping_id)
                    .execute(&mut **transaction)
                    .await?;
                }
                sqlx::query(
                    "UPDATE movie_version_members SET group_id = ? WHERE scope_id = ? AND media_mapping_id = ?",
                )
                .bind(group_id)
                .bind(scope_id)
                .bind(members[*index].mapping_id)
                .execute(&mut **transaction)
                .await?;
            }
            for losing_group_id in existing_groups.iter().copied() {
                if losing_group_id == group_id || split_groups.contains(&losing_group_id) {
                    continue;
                }
                sqlx::query(
                    "DELETE FROM movie_version_sources WHERE scope_id = ? AND group_id = ?",
                )
                .bind(scope_id)
                .bind(losing_group_id)
                .execute(&mut **transaction)
                .await?;
                sqlx::query(
                    "UPDATE movie_version_members SET group_id = ? WHERE scope_id = ? AND group_id = ?",
                )
                .bind(group_id)
                .bind(scope_id)
                .bind(losing_group_id)
                .execute(&mut **transaction)
                .await?;
                sqlx::query("DELETE FROM movie_version_groups WHERE id = ? AND scope_id = ?")
                    .bind(losing_group_id)
                    .bind(scope_id)
                    .execute(&mut **transaction)
                    .await?;
            }

            for old_group_id in existing_groups
                .iter()
                .copied()
                .chain(std::iter::once(group_id))
            {
                sqlx::query(
                    "DELETE FROM movie_version_group_aliases WHERE scope_id = ? AND group_id = ?",
                )
                .bind(scope_id)
                .bind(old_group_id)
                .execute(&mut **transaction)
                .await?;
            }
            let component_aliases = component
                .iter()
                .flat_map(|index| members[*index].aliases.iter())
                .collect::<BTreeSet<_>>();
            // Multiple encodings on one backend are versions only when every
            // member has the exact same numbered, provider-matched series identity.
            let numbered_episode = component_aliases.len() == 1
                && component_aliases.iter().next().is_some_and(|alias| {
                    alias.kind == crate::media::identity::MediaKind::Episode
                        && alias.provider == crate::media::identity::MediaProvider::SeriesGroup
                });
            for alias in component_aliases {
                sqlx::query(
                    r#"
                    INSERT INTO movie_version_group_aliases (
                        scope_id, group_id, provider, provider_id
                    ) VALUES (?, ?, ?, ?)
                    ON CONFLICT (scope_id, provider, provider_id) DO UPDATE SET
                        group_id = excluded.group_id
                    "#,
                )
                .bind(scope_id)
                .bind(group_id)
                .bind(alias.storage_provider())
                .bind(&alias.provider_id)
                .execute(&mut **transaction)
                .await?;
            }

            let server_count = component
                .iter()
                .map(|index| members[*index].server_id)
                .collect::<HashSet<_>>()
                .len();
            let ambiguous = server_count != component.len() && !numbered_episode;
            let publish = !ambiguous && component.len() > 1;
            sqlx::query(
                "UPDATE movie_version_groups SET ambiguous = ?, published = CASE WHEN published = 1 OR ? THEN 1 ELSE 0 END WHERE id = ?",
            )
            .bind(ambiguous)
            .bind(publish)
            .bind(group_id)
            .execute(&mut **transaction)
            .await?;
            let (virtual_media_id, published): (String, bool) = sqlx::query_as(
                r#"
                SELECT id.virtual_media_id, group_row.published
                FROM movie_version_group_ids id
                JOIN movie_version_groups group_row ON group_row.id = id.group_id
                WHERE id.group_id = ? AND id.canonical = 1
                "#,
            )
            .bind(group_id)
            .fetch_one(&mut **transaction)
            .await?;
            let stable_group = StableMediaGroup {
                virtual_media_id,
                active_member_count: component.len(),
                ambiguous,
                published,
            };
            for index in component {
                assignments.insert(
                    members[index].virtual_media_id.clone(),
                    stable_group.clone(),
                );
            }
        }
        Ok(assignments)
    }

    pub(super) async fn create_media_group(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        scope_id: i64,
    ) -> Result<i64, sqlx::Error> {
        let result =
            sqlx::query("INSERT INTO movie_version_groups (scope_id, created_at) VALUES (?, ?)")
                .bind(scope_id)
                .bind(chrono::Utc::now())
                .execute(&mut **transaction)
                .await?;
        let group_id = result.last_insert_rowid();
        sqlx::query(
            "INSERT INTO movie_version_group_ids (virtual_media_id, group_id, canonical, created_at) VALUES (?, ?, 1, ?)",
        )
        .bind(generate_token())
        .bind(group_id)
        .bind(chrono::Utc::now())
        .execute(&mut **transaction)
        .await?;
        Ok(group_id)
    }
}
