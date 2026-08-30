use axum::Json;
use hyper::StatusCode;
use tracing::error;

use crate::{
    media_storage_service::MovieCatalogSnapshot,
    models::enums::BaseItemKind,
    movie_catalog::{MovieDedupPlan, TaggedMediaItem},
    movie_identity::{MovieAlias, MovieObservation},
    request_preprocessing::PreprocessedRequest,
    AppState,
};

use super::{
    finalize_items_response,
    library_resolution::CatalogFetchTarget,
    postprocessing::{FederatedItems, Pagination, ServerItems},
    request_policy::is_authoritative_movie_inventory_request,
    upstream::{estimate_merged_library_total, fetch_catalog, FetchMode, FetchedCatalog},
};

pub(super) async fn get_virtual_library_items(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    catalog_scope_key: String,
    targets: Vec<CatalogFetchTarget>,
    skipped_targets: usize,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let deduplicate_movies = state.deduplicate_movies_enabled().await;
    let reconciliation_generation = if deduplicate_movies {
        Some(
            state
                .media_storage
                .begin_movie_reconciliation()
                .await
                .map_err(|error| {
                    error!("Failed to begin movie reconciliation: {error}");
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
    let authoritative_inventory = is_authoritative_movie_inventory_request(original_request.url());
    let mut snapshots = Vec::new();
    let mut tagged_items = Vec::new();
    for fetch in server_items {
        if let Some(total) = fetch.upstream_total {
            upstream_total_sum += total.max(0);
        }
        all_fully_fetched &= fetch.fully_fetched;
        let ServerItems { response, server } = fetch.server_items;
        let items = response.into_items();
        if deduplicate_movies {
            snapshots.push(MovieCatalogSnapshot {
                source_key: format!(
                    "{}:{}",
                    server.id,
                    fetch.source_parent_id.as_deref().unwrap_or_default()
                ),
                server_id: server.id,
                complete: fetch.fully_fetched && authoritative_inventory,
                observations: items
                    .iter()
                    .filter(|item| item.item_type == BaseItemKind::Movie)
                    .map(|item| MovieObservation {
                        virtual_media_id: item.id.clone(),
                        aliases: MovieAlias::from_item(item),
                    })
                    .collect(),
            });
        }
        tagged_items.extend(items.into_iter().map(|item| TaggedMediaItem {
            item,
            server: server.clone(),
        }));
    }

    let items = if deduplicate_movies {
        let plan = MovieDedupPlan::new(tagged_items);
        let stable_group_ids = state
            .media_storage
            .reconcile_movie_catalog(
                &catalog_scope_key,
                reconciliation_generation.expect("enabled reconciliation has a generation"),
                &snapshots,
                skipped_targets == 0 && failures == 0,
            )
            .await
            .map_err(|error| {
                error!("Failed to reconcile movie version groups: {error}");
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
