//! Reconcile fetched media observations and validated inventory evidence before catalog presentation.
use crate::application::error::ApplicationError;
use crate::models::enums::BaseItemKind;
use std::collections::BTreeSet;

use crate::application::services::MediaServices;
use crate::{
    application::request::input::OperationRequest,
    media::catalog::{MediaDedupPlan, TaggedMediaItem},
    media::identity::{MediaAlias, MediaKind, MediaObservation},
    media::scope::{MediaCatalogScope, MediaScopeKind},
    media::storage::{
        InventorySourceEvidence, InventorySourceStatus, InventoryUpdate, MediaCatalogSnapshot,
        ReconciliationMode,
    },
};

use super::{
    fetch::{fetch_catalog, fetch_show_catalog, FetchMode, FetchedCatalog},
    finalize_items_response,
    library_resolution::CatalogFetchTarget,
    postprocessing::{FederatedItems, ServerItems},
    request_policy::CatalogRequestPolicy,
};

async fn show_catalog_aliases(
    state: &MediaServices,
    items: &[crate::models::MediaItem],
    viewer: &str,
    series_group: Option<&str>,
    catalog_scope: &str,
) -> Result<Vec<BTreeSet<MediaAlias>>, ApplicationError> {
    let mut parents = std::collections::HashMap::new();
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        let mut aliases = MediaAlias::from_item(item);
        if matches!(item.item_type, BaseItemKind::Season | BaseItemKind::Episode) {
            let group = if let Some(group) = series_group {
                Some(group.to_string())
            } else if let Some(parent) = item.series_id.as_ref().or_else(|| {
                (item.item_type == BaseItemKind::Season)
                    .then_some(item.parent_id.as_ref())
                    .flatten()
            }) {
                if !parents.contains_key(parent) {
                    let group = state
                        .media_storage
                        .get_media_parent_group_id_for_scope(parent, viewer, Some(catalog_scope))
                        .await
                        .map_err(ApplicationError::Storage)?;
                    parents.insert(parent.clone(), group);
                }
                parents[parent].clone()
            } else {
                None
            };
            if let Some(alias) = group.as_deref().and_then(|group| {
                MediaAlias::for_season(item, group).or_else(|| MediaAlias::for_episode(item, group))
            }) {
                // Some backends reuse the show's IDs on its children. Once
                // the parent and coordinates are known, do not bridge different
                // seasons or episodes via those unreliable IDs.
                aliases = BTreeSet::from([alias]);
            }
        }
        result.push(aliases);
    }
    Ok(result)
}

enum CatalogContext {
    Library,
    Show { series_group: String },
}

impl CatalogContext {
    fn series_group(&self) -> Option<&str> {
        match self {
            Self::Library => None,
            Self::Show { series_group } => Some(series_group),
        }
    }
}

pub(crate) async fn get_virtual_library_items(
    state: &MediaServices,
    preprocessed: OperationRequest,
    catalog_scope_key: String,
    targets: Vec<CatalogFetchTarget>,
    skipped_targets: usize,
) -> Result<serde_json::Value, ApplicationError> {
    get_reconciled_catalog(
        state,
        preprocessed,
        catalog_scope_key,
        targets,
        skipped_targets,
        CatalogContext::Library,
    )
    .await
}

async fn get_reconciled_catalog(
    state: &MediaServices,
    preprocessed: OperationRequest,
    catalog_scope_key: String,
    targets: Vec<CatalogFetchTarget>,
    skipped_targets: usize,
    context: CatalogContext,
) -> Result<serde_json::Value, ApplicationError> {
    let snapshot = super::snapshots::SnapshotRequest::prepare(
        state,
        &preprocessed,
        &targets,
        &catalog_scope_key,
    )
    .await;
    let policy = snapshot.materialization_policy(&preprocessed);
    snapshot
        .serve(
            state,
            get_reconciled_catalog_uncached(
                state,
                preprocessed,
                catalog_scope_key,
                targets,
                skipped_targets,
                context,
                policy,
            ),
        )
        .await
}

async fn get_reconciled_catalog_uncached(
    state: &MediaServices,
    preprocessed: OperationRequest,
    catalog_scope_key: String,
    targets: Vec<CatalogFetchTarget>,
    skipped_targets: usize,
    context: CatalogContext,
    mut policy: CatalogRequestPolicy,
) -> Result<super::snapshots::SnapshotResponse, ApplicationError> {
    let deduplicate_media = state.deduplicate_media_enabled().await;
    let reconciliation_generation = if deduplicate_media {
        ReconciliationMode::Enabled {
            generation: state
                .media_storage
                .begin_media_reconciliation()
                .await
                .map_err(ApplicationError::Storage)?,
        }
    } else {
        ReconciliationMode::Disabled
    };
    let original_request = preprocessed.original_request;
    policy.reconcile_playback = deduplicate_media;
    let series_group = context.series_group();
    let fetched = match &context {
        CatalogContext::Show { .. } => {
            fetch_show_catalog(state, &original_request, &policy, targets, skipped_targets).await?
        }
        CatalogContext::Library => {
            fetch_catalog(
                state,
                &original_request,
                &policy,
                targets,
                FetchMode::VirtualLibrary,
                skipped_targets,
            )
            .await?
        }
    };

    let viewer = preprocessed
        .context
        .access_scope()
        .map(|scope| scope.user_id().to_string())
        .unwrap_or_else(|| "anonymous".to_string());
    let report = fetched.report.clone();
    let response_shape = fetched.response_shape;
    let items = reconcile_fetched_catalog(
        state,
        ReconciliationContext {
            viewer: &viewer,
            catalog_scope_key: &catalog_scope_key,
            series_group,
            generation: reconciliation_generation,
        },
        &policy,
        fetched,
    )
    .await?;
    if let Some(start_id) = policy.episode_start_id.as_deref() {
        // Alternate/member episode IDs collapse to a different public ID. Use
        // this viewer's freshly reconciled catalog to locate the queue anchor.
        if let Some(aggregate) = state
            .media_storage
            .get_media_parent_group_id_for_scope(start_id, &viewer, Some(&catalog_scope_key))
            .await
            .map_err(ApplicationError::Storage)?
        {
            policy.episode_start_id = Some(aggregate);
        }
    }
    Ok(super::snapshots::SnapshotResponse {
        response: finalize_items_response(state, &original_request, items, &policy, response_shape)
            .await?,
        report,
    })
}

pub(super) struct ReconciliationContext<'a> {
    pub(crate) viewer: &'a str,
    pub(crate) catalog_scope_key: &'a str,
    pub(crate) series_group: Option<&'a str>,
    pub(crate) generation: ReconciliationMode,
}

/// Shared refresh/interactive reconciliation. No incoming request, observation,
/// cache materialization or client pagination belongs in this stage.
pub(super) async fn reconcile_fetched_catalog(
    state: &MediaServices,
    context: ReconciliationContext<'_>,
    policy: &CatalogRequestPolicy,
    fetched: FetchedCatalog,
) -> Result<FederatedItems, ApplicationError> {
    let ReconciliationContext {
        viewer,
        catalog_scope_key,
        series_group,
        generation: reconciliation_generation,
    } = context;
    let deduplicate_media = matches!(
        reconciliation_generation,
        ReconciliationMode::Enabled { .. }
    );
    let FetchedCatalog {
        server_items,
        report,
        ..
    } = fetched;
    let evidence = report
        .outcomes()
        .iter()
        .map(|source| InventorySourceEvidence {
            source_key: format!(
                "{}:{}",
                source.server_id,
                source
                    .parent_id
                    .as_ref()
                    .map(|id| id.as_str())
                    .unwrap_or_default()
            ),
            server_id: source.server_id,
            status: match source.outcome {
                super::scan::FetchOutcome::Complete => InventorySourceStatus::Complete,
                super::scan::FetchOutcome::BoundedFeed => InventorySourceStatus::Bounded,
                super::scan::FetchOutcome::Partial(_) => InventorySourceStatus::Incomplete,
            },
        })
        .collect::<Vec<_>>();
    let authoritative_inventory = series_group.is_none() && !policy.inventory_types.is_empty();
    let mut snapshots = Vec::new();
    let mut tagged_items = Vec::new();
    let mut catalog_aliases = Vec::new();
    // A cold recursive listing can contain both the parent series and its
    // children. Publish provider-matched parents before deriving child aliases,
    // rather than requiring another request to establish the parent identity.
    // This additive pass shares the request generation and never prunes an
    // inventory; the full snapshots below remain authoritative for removals.
    if deduplicate_media
        && server_items.iter().any(|fetch| {
            fetch
                .server_items
                .response
                .items()
                .iter()
                .any(|item| matches!(item.item_type, BaseItemKind::Season | BaseItemKind::Episode))
        })
    {
        let parents = server_items
            .iter()
            .map(|fetch| MediaCatalogSnapshot {
                source_key: format!(
                    "{}:{}",
                    fetch.server_items.server.id,
                    fetch
                        .source_parent_id
                        .as_ref()
                        .map(|id| id.as_str())
                        .unwrap_or_default()
                ),
                server_id: fetch.server_items.server.id,
                complete: false,
                observations: fetch
                    .server_items
                    .response
                    .items()
                    .iter()
                    .filter(|item| item.item_type == BaseItemKind::Series)
                    .map(|item| MediaObservation {
                        virtual_media_id: item.id.clone(),
                        kind: MediaKind::Series,
                        aliases: MediaAlias::from_item(item),
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        if let ReconciliationMode::Enabled { generation } = reconciliation_generation {
            if parents
                .iter()
                .any(|snapshot| !snapshot.observations.is_empty())
            {
                state
                    .media_storage
                    .apply_inventory_update(
                        catalog_scope_key,
                        generation,
                        InventoryUpdate::Observations(parents),
                    )
                    .await
                    .map_err(ApplicationError::Storage)?;
            }
        }
    }
    for fetch in server_items {
        let ServerItems { response, server } = fetch.server_items;
        let items = response.into_items();
        if deduplicate_media {
            let aliases =
                show_catalog_aliases(state, &items, viewer, series_group, catalog_scope_key)
                    .await?;
            snapshots.push(MediaCatalogSnapshot {
                source_key: format!(
                    "{}:{}",
                    server.id,
                    fetch
                        .source_parent_id
                        .as_ref()
                        .map(|id| id.as_str())
                        .unwrap_or_default()
                ),
                server_id: server.id,
                complete: fetch.outcome == super::scan::FetchOutcome::Complete
                    && authoritative_inventory,
                observations: items
                    .iter()
                    .zip(&aliases)
                    .filter(|(item, _)| MediaKind::from_item_kind(&item.item_type).is_some())
                    .map(|(item, aliases)| MediaObservation {
                        virtual_media_id: item.id.clone(),
                        kind: MediaKind::from_item_kind(&item.item_type)
                            .expect("deduplicated type"),
                        aliases: aliases.clone(),
                    })
                    .collect(),
            });
            catalog_aliases.extend(aliases);
        }
        tagged_items.extend(items.into_iter().map(|item| TaggedMediaItem {
            item,
            server: server.clone(),
        }));
    }

    let mut items = if let ReconciliationMode::Enabled { generation } = reconciliation_generation {
        let plan = MediaDedupPlan::with_aliases(tagged_items, catalog_aliases);
        let update = if authoritative_inventory && !evidence.is_empty() {
            InventoryUpdate::validated_inventory(
                snapshots,
                policy.inventory_types.clone(),
                &evidence,
                report.failure_count(),
            )
            .map_err(|error| ApplicationError::Internal(error.into()))?
        } else {
            InventoryUpdate::Observations(snapshots)
        };
        let stable_group_ids = state
            .media_storage
            .apply_inventory_update(catalog_scope_key, generation, update)
            .await
            .map_err(ApplicationError::Storage)?;
        FederatedItems::from_merged_items(plan.collapse(&stable_group_ids))
    } else {
        FederatedItems::from_tagged_items(tagged_items)
    };
    for item in items.items_mut() {
        if deduplicate_media || series_group.is_some() {
            crate::application::details::sources::preserve_media_parent_groups(
                state,
                item,
                viewer,
                Some(catalog_scope_key),
            )
            .await?;
        }
        if let Some(group) = series_group {
            if matches!(item.item_type, BaseItemKind::Season | BaseItemKind::Episode) {
                if item.series_id.is_some() && item.parent_id == item.series_id {
                    item.parent_id = Some(group.to_string());
                }
                item.series_id = Some(group.to_string());
            }
        }
    }
    Ok(items)
}

/// Federates `/Shows/{aggregateId}/Seasons|Episodes` across the member
/// series of a collapsed show. Seasons and (Jellyfin v12+) episodes merge
/// using provider identities, with children identified by their matched parent
/// series and season/episode numbers when available.
pub(crate) async fn get_aggregate_show_items(
    state: &MediaServices,
    preprocessed: OperationRequest,
    aggregate_id: String,
    targets: Vec<CatalogFetchTarget>,
    skipped_targets: usize,
) -> Result<serde_json::Value, ApplicationError> {
    let viewer = preprocessed
        .context
        .access_scope()
        .map(|scope| scope.user_id().to_string())
        .unwrap_or_else(|| "anonymous".to_string());
    let catalog_scope_key = MediaCatalogScope {
        kind: MediaScopeKind::Aggregate,
        viewer: &viewer,
        resource_id: &aggregate_id,
    }
    .to_string();
    get_reconciled_catalog(
        state,
        preprocessed,
        catalog_scope_key,
        targets,
        skipped_targets,
        CatalogContext::Show {
            series_group: aggregate_id,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::catalog::request_policy::is_authoritative_media_inventory_request;
    use crate::{
        media::storage::MediaStorageService, persistence::MIGRATOR, servers::storage::Server,
    };

    #[tokio::test]
    async fn partial_inventories_retain_sightings_until_a_full_inventory_removes_them() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let row = sqlx::query("INSERT INTO servers (name, url, priority, created_at, updated_at) VALUES ('test', 'http://localhost:8096', 100, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) RETURNING *")
            .fetch_one(&pool).await.unwrap();
        let server = Server::from_row(row).unwrap();
        let storage = MediaStorageService::new(pool.clone());
        let mut observations = Vec::new();
        for (id, kind) in [
            ("movie", "Movie"),
            ("series", "Series"),
            ("season", "Season"),
            ("episode", "Episode"),
        ] {
            let mapping = storage
                .get_or_create_media_mapping(id, &server)
                .await
                .unwrap();
            let item: crate::models::MediaItem = serde_json::from_value(serde_json::json!({
                "Id": mapping.virtual_media_id, "Type": kind, "ProviderIds": {"Tmdb": "42"}
            }))
            .unwrap();
            observations.push(MediaObservation {
                virtual_media_id: mapping.virtual_media_id,
                kind: MediaKind::from_item_kind(&item.item_type).unwrap(),
                aliases: MediaAlias::from_item(&item),
            });
        }

        for (path, observed, expected) in [
            (
                "/Items?ParentId=show&Recursive=true",
                observations.clone(),
                4,
            ),
            (
                "/Items?ParentId=show&Recursive=true&IncludeItemTypes=Movie",
                vec![observations[0].clone()],
                4,
            ),
            (
                "/Items?ParentId=show&Recursive=true&IncludeItemTypes=Series",
                vec![observations[1].clone()],
                4,
            ),
            ("/Shows/show/Seasons", vec![observations[2].clone()], 4),
            ("/Shows/show/Episodes", vec![observations[3].clone()], 4),
            (
                "/Shows/show/Episodes?SeasonId=season&IsPlayed=true",
                vec![],
                4,
            ),
            (
                "/Items?ParentId=show&Recursive=true",
                vec![observations[0].clone()],
                1,
            ),
            ("/Items?ParentId=show&Recursive=true", vec![], 0),
        ] {
            let url = url::Url::parse(&format!("http://localhost{path}")).unwrap();
            let complete = is_authoritative_media_inventory_request(&url);
            let generation = storage.begin_media_reconciliation().await.unwrap();
            storage
                .reconcile_media_catalog(
                    "aggregate:show:user",
                    generation,
                    &[MediaCatalogSnapshot {
                        source_key: format!("{}:show", server.id),
                        server_id: server.id,
                        complete,
                        observations: observed,
                    }],
                    complete,
                )
                .await
                .unwrap();
            let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM movie_catalog_sightings")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(count, expected, "{path}");
        }
    }
}
