use axum::Json;
use hyper::StatusCode;
use tracing::error;

use crate::{
    media_catalog::{MediaDedupPlan, TaggedMediaItem},
    media_identity::{MediaAlias, MediaKind, MediaObservation},
    media_storage_service::MediaCatalogSnapshot,
    request_preprocessing::PreprocessedRequest,
    AppState,
};

use super::{
    finalize_items_response,
    library_resolution::CatalogFetchTarget,
    postprocessing::{FederatedItems, Pagination, ServerItems},
    request_policy::is_authoritative_media_inventory_request,
    upstream::{
        estimate_merged_library_total, fetch_catalog, fetch_show_catalog, FetchMode, FetchedCatalog,
    },
};

pub(super) async fn get_virtual_library_items(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    catalog_scope_key: String,
    targets: Vec<CatalogFetchTarget>,
    skipped_targets: usize,
) -> Result<Json<serde_json::Value>, StatusCode> {
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
    let pagination = Pagination::from_url(original_request.url());
    let FetchedCatalog {
        server_items,
        failures,
        response_shape,
    } = fetch_catalog(
        state,
        &original_request,
        targets,
        FetchMode::VirtualLibrary { pagination },
        skipped_targets,
    )
    .await?;

    let mut upstream_total_sum = 0i32;
    let mut all_fully_fetched = true;
    let authoritative_inventory = is_authoritative_media_inventory_request(original_request.url());
    let mut snapshots = Vec::new();
    let mut tagged_items = Vec::new();
    for fetch in server_items {
        if let Some(total) = fetch.upstream_total {
            upstream_total_sum += total.max(0);
        }
        all_fully_fetched &= fetch.fully_fetched;
        let ServerItems { response, server } = fetch.server_items;
        let items = response.into_items();
        if deduplicate_media {
            snapshots.push(MediaCatalogSnapshot {
                source_key: format!(
                    "{}:{}",
                    server.id,
                    fetch.source_parent_id.as_deref().unwrap_or_default()
                ),
                server_id: server.id,
                complete: fetch.fully_fetched && authoritative_inventory,
                observations: items
                    .iter()
                    .filter(|item| MediaKind::from_item_kind(&item.item_type).is_some())
                    .map(|item| MediaObservation {
                        virtual_media_id: item.id.clone(),
                        aliases: MediaAlias::from_item(item),
                    })
                    .collect(),
            });
        }
        tagged_items.extend(items.into_iter().map(|item| TaggedMediaItem {
            item,
            server: server.clone(),
        }));
    }

    let items = if deduplicate_media {
        let plan = MediaDedupPlan::new(tagged_items);
        let stable_group_ids = state
            .media_storage
            .reconcile_media_catalog(
                &catalog_scope_key,
                reconciliation_generation.expect("enabled reconciliation has a generation"),
                &snapshots,
                authoritative_inventory && skipped_targets == 0 && failures == 0,
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
    let total_count =
        estimate_merged_library_total(items.len(), upstream_total_sum, all_fully_fetched);

    finalize_items_response(
        items.with_reported_total(total_count),
        original_request.url(),
        response_shape,
    )
}

/// Federates `/Shows/{aggregateId}/Seasons|Episodes` across the member
/// series of a collapsed show. Seasons and (Jellyfin v12+) episodes merge
/// with the same provider-ID reconciliation used for movies at the library
/// level; the scope is keyed by the aggregate so season/episode numbers never
/// collide across different shows.
pub(super) async fn get_aggregate_show_items(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    aggregate_id: String,
    targets: Vec<CatalogFetchTarget>,
    skipped_targets: usize,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let deduplicate = state.deduplicate_media_enabled().await;
    let reconciliation_generation = if deduplicate {
        Some(
            state
                .media_storage
                .begin_media_reconciliation()
                .await
                .map_err(|error| {
                    error!("Failed to begin show reconciliation: {error}");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?,
        )
    } else {
        None
    };
    let original_request = preprocessed.original_request;
    let viewer = preprocessed
        .access_scope
        .as_ref()
        .map(|scope| scope.user_id().to_string())
        .unwrap_or_else(|| "anonymous".to_string());
    let catalog_scope_key = format!("aggregate:{aggregate_id}:{viewer}");
    let FetchedCatalog {
        server_items,
        failures: _,
        response_shape,
    } = fetch_show_catalog(state, &original_request, targets, skipped_targets).await?;

    let mut upstream_total_sum = 0i32;
    let mut all_fully_fetched = true;
    let mut snapshots = Vec::new();
    let mut tagged_items = Vec::new();
    for fetch in server_items {
        if let Some(total) = fetch.upstream_total {
            upstream_total_sum += total.max(0);
        }
        all_fully_fetched &= fetch.fully_fetched;
        let ServerItems { response, server } = fetch.server_items;
        let items = response.into_items();
        if deduplicate {
            snapshots.push(MediaCatalogSnapshot {
                source_key: format!(
                    "{}:{}",
                    server.id,
                    fetch.source_parent_id.as_deref().unwrap_or_default()
                ),
                server_id: server.id,
                // Seasons, episodes and filtered listings share this source.
                // A show-child response cannot replace the whole inventory.
                complete: false,
                observations: items
                    .iter()
                    .filter(|item| MediaKind::from_item_kind(&item.item_type).is_some())
                    .map(|item| MediaObservation {
                        virtual_media_id: item.id.clone(),
                        aliases: MediaAlias::from_item(item),
                    })
                    .collect(),
            });
        }
        tagged_items.extend(items.into_iter().map(|item| TaggedMediaItem {
            item,
            server: server.clone(),
        }));
    }

    for tagged in &mut tagged_items {
        crate::handlers::media_versions::preserve_media_parent_groups(
            state,
            &mut tagged.item,
            &viewer,
        )
        .await?;
        if matches!(
            tagged.item.item_type,
            crate::models::enums::BaseItemKind::Season
                | crate::models::enums::BaseItemKind::Episode
        ) {
            if tagged.item.series_id.is_some() && tagged.item.parent_id == tagged.item.series_id {
                tagged.item.parent_id = Some(aggregate_id.clone());
            }
            tagged.item.series_id = Some(aggregate_id.clone());
        }
    }

    let items = if deduplicate {
        let plan = MediaDedupPlan::new(tagged_items);
        let stable_group_ids = state
            .media_storage
            .reconcile_media_catalog(
                &catalog_scope_key,
                reconciliation_generation.expect("enabled reconciliation has a generation"),
                &snapshots,
                false,
            )
            .await
            .map_err(|error| {
                error!("Failed to reconcile show version groups: {error}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        FederatedItems::from_merged_items(plan.collapse(&stable_group_ids))
    } else {
        FederatedItems::from_tagged_items(tagged_items)
    };
    let total_count =
        estimate_merged_library_total(items.len(), upstream_total_sum, all_fully_fetched);

    finalize_items_response(
        items.with_reported_total(total_count),
        original_request.url(),
        response_shape,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
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
