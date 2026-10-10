use axum::Json;
use hyper::StatusCode;
use std::collections::BTreeSet;
use tracing::error;

use crate::{
    media_catalog::{MediaDedupPlan, TaggedMediaItem},
    media_identity::{MediaAlias, MediaKind, MediaObservation},
    media_scope::{MediaCatalogScope, MediaScopeKind},
    media_storage_service::MediaCatalogSnapshot,
    request_preprocessing::PreprocessedRequest,
    AppState,
};

use super::{
    finalize_items_response,
    library_resolution::CatalogFetchTarget,
    postprocessing::{FederatedItems, ServerItems},
    request_policy::CatalogRequestPolicy,
    upstream::{fetch_catalog, fetch_show_catalog, FetchMode, FetchedCatalog},
};

async fn show_catalog_aliases(
    state: &AppState,
    items: &[crate::models::MediaItem],
    viewer: &str,
    series_group: Option<&str>,
    catalog_scope: &str,
) -> Result<Vec<BTreeSet<MediaAlias>>, StatusCode> {
    let mut parents = std::collections::HashMap::new();
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        let mut aliases = MediaAlias::from_item(item);
        if matches!(
            item.item_type,
            crate::models::enums::BaseItemKind::Season
                | crate::models::enums::BaseItemKind::Episode
        ) {
            let group = if let Some(group) = series_group {
                Some(group.to_string())
            } else if let Some(parent) = item.series_id.as_ref().or_else(|| {
                (item.item_type == crate::models::enums::BaseItemKind::Season)
                    .then_some(item.parent_id.as_ref())
                    .flatten()
            }) {
                if !parents.contains_key(parent) {
                    let group = state
                        .media_storage
                        .get_media_parent_group_id_for_scope(parent, viewer, Some(catalog_scope))
                        .await
                        .map_err(|error| {
                            error!("Failed to resolve show parent identity: {error}");
                            StatusCode::INTERNAL_SERVER_ERROR
                        })?;
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

pub(super) async fn get_virtual_library_items(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    catalog_scope_key: String,
    targets: Vec<CatalogFetchTarget>,
    skipped_targets: usize,
) -> Result<Json<serde_json::Value>, StatusCode> {
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
    state: &AppState,
    mut preprocessed: PreprocessedRequest,
    catalog_scope_key: String,
    targets: Vec<CatalogFetchTarget>,
    skipped_targets: usize,
    context: CatalogContext,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let snapshot = super::snapshots::SnapshotRequest::prepare(
        state,
        &mut preprocessed,
        &targets,
        &catalog_scope_key,
    )
    .await;
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
            ),
        )
        .await
}

async fn get_reconciled_catalog_uncached(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    catalog_scope_key: String,
    targets: Vec<CatalogFetchTarget>,
    skipped_targets: usize,
    context: CatalogContext,
) -> Result<super::snapshots::SnapshotResponse, StatusCode> {
    let deduplicate_media = state.deduplicate_media_enabled().await;
    let reconciliation_generation = if deduplicate_media {
        Some(
            state
                .media_storage
                .begin_media_reconciliation()
                .await
                .map_err(|error| {
                    error!("Failed to begin media reconciliation: {error}");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?,
        )
    } else {
        None
    };
    let original_request = preprocessed.original_request;
    let policy = CatalogRequestPolicy::from_url(original_request.url());
    let series_group = context.series_group();
    let FetchedCatalog {
        server_items,
        response_shape,
        failures,
    } = match &context {
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

    let authoritative_inventory = series_group.is_none() && policy.authoritative_inventory;
    let mut snapshots = Vec::new();
    let mut tagged_items = Vec::new();
    let viewer = preprocessed
        .access_scope
        .as_ref()
        .map(|scope| scope.user_id().to_string())
        .unwrap_or_else(|| "anonymous".to_string());
    let mut catalog_aliases = Vec::new();
    // A cold recursive listing can contain both the parent series and its
    // children. Publish provider-matched parents before deriving child aliases,
    // rather than requiring another request to establish the parent identity.
    // This additive pass shares the request generation and never prunes an
    // inventory; the full snapshots below remain authoritative for removals.
    if deduplicate_media
        && server_items.iter().any(|fetch| {
            fetch.server_items.response.items().iter().any(|item| {
                matches!(
                    item.item_type,
                    crate::models::enums::BaseItemKind::Season
                        | crate::models::enums::BaseItemKind::Episode
                )
            })
        })
    {
        let parents = server_items
            .iter()
            .map(|fetch| MediaCatalogSnapshot {
                source_key: format!(
                    "{}:{}",
                    fetch.server_items.server.id,
                    fetch.source_parent_id.as_deref().unwrap_or_default()
                ),
                server_id: fetch.server_items.server.id,
                complete: false,
                observations: fetch
                    .server_items
                    .response
                    .items()
                    .iter()
                    .filter(|item| item.item_type == crate::models::enums::BaseItemKind::Series)
                    .map(|item| MediaObservation {
                        virtual_media_id: item.id.clone(),
                        aliases: MediaAlias::from_item(item),
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        if parents
            .iter()
            .any(|snapshot| !snapshot.observations.is_empty())
        {
            state
                .media_storage
                .reconcile_media_catalog(
                    &catalog_scope_key,
                    reconciliation_generation.expect("enabled reconciliation has a generation"),
                    &parents,
                    false,
                )
                .await
                .map_err(|error| {
                    error!("Failed to reconcile parent series identities: {error}");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
        }
    }
    for fetch in server_items {
        let ServerItems { response, server } = fetch.server_items;
        let items = response.into_items();
        if deduplicate_media {
            let aliases =
                show_catalog_aliases(state, &items, &viewer, series_group, &catalog_scope_key)
                    .await?;
            snapshots.push(MediaCatalogSnapshot {
                source_key: format!(
                    "{}:{}",
                    server.id,
                    fetch.source_parent_id.as_deref().unwrap_or_default()
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

    let mut items = if deduplicate_media {
        let plan = MediaDedupPlan::with_aliases(tagged_items, catalog_aliases);
        let stable_group_ids = state
            .media_storage
            .reconcile_media_catalog(
                &catalog_scope_key,
                reconciliation_generation.expect("enabled reconciliation has a generation"),
                &snapshots,
                authoritative_inventory && failures == 0,
            )
            .await
            .map_err(|error| {
                error!("Failed to reconcile media version groups: {error}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        FederatedItems::from_merged_items(plan.collapse(&stable_group_ids))
    } else {
        FederatedItems::from_tagged_items(tagged_items)
    };
    for item in items.items_mut() {
        if deduplicate_media || series_group.is_some() {
            crate::handlers::media_versions::preserve_media_parent_groups_in_scope(
                state,
                item,
                &viewer,
                Some(&catalog_scope_key),
            )
            .await?;
        }
        if let Some(group) = series_group {
            if matches!(
                item.item_type,
                crate::models::enums::BaseItemKind::Season
                    | crate::models::enums::BaseItemKind::Episode
            ) {
                if item.series_id.is_some() && item.parent_id == item.series_id {
                    item.parent_id = Some(group.to_string());
                }
                item.series_id = Some(group.to_string());
            }
        }
    }
    Ok(super::snapshots::SnapshotResponse {
        response: finalize_items_response(state, &original_request, items, &policy, response_shape)
            .await?,
        complete: failures == 0,
    })
}

/// Federates `/Shows/{aggregateId}/Seasons|Episodes` across the member
/// series of a collapsed show. Seasons and (Jellyfin v12+) episodes merge
/// using provider identities, with children identified by their matched parent
/// series and season/episode numbers when available.
pub(super) async fn get_aggregate_show_items(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    aggregate_id: String,
    targets: Vec<CatalogFetchTarget>,
    skipped_targets: usize,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let viewer = preprocessed
        .access_scope
        .as_ref()
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
    use crate::handlers::federated::request_policy::is_authoritative_media_inventory_request;
    use crate::{
        config::MIGRATOR, media_storage_service::MediaStorageService, server_storage::Server,
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
            let item = serde_json::from_value(serde_json::json!({
                "Id": mapping.virtual_media_id, "Type": kind, "ProviderIds": {"Tmdb": "42"}
            }))
            .unwrap();
            observations.push(MediaObservation {
                virtual_media_id: mapping.virtual_media_id,
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
