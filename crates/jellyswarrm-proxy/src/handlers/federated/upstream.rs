use std::collections::HashSet;

use hyper::StatusCode;
use tokio::task::JoinSet;
use tracing::{debug, error, trace, warn};

use crate::{
    handlers::common::{execute_json_request, response_json_to_payload},
    models::{ItemsResponseVariants, ItemsResponseWithCount},
    processors::response_processor::ResponseProcessingProfile,
    request_preprocessing::{apply_to_request, JellyfinAuthorization},
    server_id::ServerId,
    server_storage::Server,
    user_authorization_service::AuthorizationSession,
    virtual_library_service::DiscoveredLibrary,
    AppState,
};

use super::{
    item_policy::presentable_library_collection_type,
    library_resolution::{is_library_root_request, CatalogFetchTarget},
    postprocessing::{Pagination, ResponseShape, ServerItems},
    request_policy::{
        normalize_upstream_pagination, replace_aggregate_parent_id, set_upstream_page,
        CatalogRequestPolicy, UPSTREAM_PAGE_SIZE,
    },
};
use crate::url_helper::replace_path_id;

pub(super) struct FetchedCatalog {
    pub(super) server_items: Vec<FetchedServerItems>,
    pub(super) failures: usize,
    pub(super) response_shape: ResponseShape,
}

#[derive(Clone, Copy)]
pub(super) enum FetchMode {
    Listing,
    VirtualLibrary,
    Inventory,
}

pub(super) struct FetchedServerItems {
    pub(super) server_items: ServerItems,
    pub(super) upstream_total: Option<i32>,
    pub(super) fully_fetched: bool,
    pub(super) source_parent_id: Option<String>,
}

/// Federates `/Shows/{seriesId}/Seasons|Episodes` for a collapsed (aggregate)
/// show by fanning out the path ID to each member's original series ID.
pub(super) async fn fetch_show_catalog(
    state: &AppState,
    original_request: &reqwest::Request,
    policy: &CatalogRequestPolicy,
    targets: Vec<CatalogFetchTarget>,
    mut failures: usize,
) -> Result<FetchedCatalog, StatusCode> {
    let mut join_set = JoinSet::new();
    let season_id = original_request
        .url()
        .query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case("SeasonId"))
        .map(|(_, id)| id.into_owned());
    let season_mappings = if let Some(id) = season_id.as_deref() {
        if let Some(group) = state
            .media_storage
            .get_media_version_group(id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        {
            Some(
                state
                    .media_storage
                    .get_media_version_members(group.id)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                    .into_iter()
                    .map(|member| member.mapping)
                    .collect::<Vec<_>>(),
            )
        } else {
            Some(
                state
                    .media_storage
                    .get_media_mapping_by_virtual(id)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                    .into_iter()
                    .collect(),
            )
        }
    } else {
        None
    };

    for (index, target) in targets.into_iter().enumerate() {
        let season_mapping = if let Some(mappings) = &season_mappings {
            let Some(mapping) = mappings
                .iter()
                .find(|mapping| mapping.server_id == target.server.id)
            else {
                continue;
            };
            Some(mapping)
        } else {
            None
        };
        let Some(mut request) = original_request.try_clone() else {
            error!("Failed to clone request for server: {}", target.server.name);
            failures += 1;
            continue;
        };
        if let Some(mapping) = season_mapping {
            let pairs = request
                .url()
                .query_pairs()
                .map(|(key, value)| {
                    let value = if key.eq_ignore_ascii_case("SeasonId") {
                        mapping.original_media_id.clone()
                    } else {
                        value.into_owned()
                    };
                    (key.into_owned(), value)
                })
                .collect::<Vec<_>>();
            request
                .url_mut()
                .query_pairs_mut()
                .clear()
                .extend_pairs(pairs);
        }
        policy.prepare_upstream(request.url_mut(), true);
        let source_parent_id = target.parent_id.clone();
        if let Some(parent_id) = target.parent_id.as_deref() {
            if let Some(replaced) = replace_path_id(request.url(), "Shows", parent_id) {
                *request.url_mut() = replaced;
            }
        }

        let state = state.clone();
        join_set.spawn(async move {
            let result =
                fetch_paged_items_from_server(index, state, request, target.session, target.server)
                    .await;
            (
                index,
                result.map(|mut fetched| {
                    fetched.source_parent_id = source_parent_id;
                    fetched
                }),
            )
        });
    }

    if join_set.is_empty() && failures == 0 {
        return Ok(FetchedCatalog {
            server_items: Vec::new(),
            failures: 0,
            response_shape: ResponseShape::Counted,
        });
    }
    let (indexed_results, failures) = collect_federated_results(join_set, failures).await?;
    if failures > 0 {
        warn!(
            "Returning partial federated show response after {} server failure(s)",
            failures
        );
    }
    let server_items = indexed_results
        .into_iter()
        .map(|(_, items)| items)
        .collect::<Vec<_>>();
    let response_shape = ResponseShape::from_responses(
        server_items
            .iter()
            .map(|items| &items.server_items.response),
    );

    Ok(FetchedCatalog {
        server_items,
        failures,
        response_shape,
    })
}

impl FetchedServerItems {
    fn complete(server_items: ServerItems) -> Self {
        let upstream_total = match &server_items.response {
            ItemsResponseVariants::WithCount(response) => Some(response.total_record_count),
            ItemsResponseVariants::Bare(_) => None,
        };
        Self {
            server_items,
            upstream_total,
            fully_fetched: true,
            source_parent_id: None,
        }
    }
}

struct PagedItems {
    response: ItemsResponseVariants,
    upstream_total: Option<i32>,
    fully_fetched: bool,
}

pub(super) async fn fetch_catalog(
    state: &AppState,
    original_request: &reqwest::Request,
    policy: &CatalogRequestPolicy,
    targets: Vec<CatalogFetchTarget>,
    mode: FetchMode,
    mut failures: usize,
) -> Result<FetchedCatalog, StatusCode> {
    let mut join_set = JoinSet::new();

    for (index, target) in targets.into_iter().enumerate() {
        let Some(mut request) = original_request.try_clone() else {
            error!("Failed to clone request for server: {}", target.server.name);
            failures += 1;
            continue;
        };
        policy.prepare_upstream(
            request.url_mut(),
            target.parent_id.is_some() || matches!(mode, FetchMode::VirtualLibrary),
        );
        let source_parent_id = target.parent_id.clone();
        if let Some(parent_id) = target.parent_id.as_deref() {
            if let Some(resolved_id) = target.resolved_parent_id.as_deref() {
                *request.url_mut() =
                    replace_aggregate_parent_id(request.url(), resolved_id, parent_id);
            }
        }
        let upstream_limited = policy.upstream_limited;
        let library_inventory =
            is_library_root_request(request.url(), state.get_url_prefix().await.as_deref());
        let pagination = policy.pagination;
        let state = state.clone();
        join_set.spawn(async move {
            let result = match mode {
                FetchMode::Listing if upstream_limited || library_inventory => {
                    fetch_items_from_server(
                        index,
                        state,
                        request,
                        target.session,
                        target.server,
                        pagination,
                    )
                    .await
                    .map(FetchedServerItems::complete)
                }
                FetchMode::Listing | FetchMode::VirtualLibrary => {
                    if upstream_limited {
                        fetch_items_from_server(
                            index,
                            state,
                            request,
                            target.session,
                            target.server,
                            pagination,
                        )
                        .await
                        .map(FetchedServerItems::complete)
                    } else {
                        fetch_paged_items_from_server(
                            index,
                            state,
                            request,
                            target.session,
                            target.server,
                        )
                        .await
                    }
                }
                FetchMode::Inventory => fetch_raw_items_from_server(
                    index,
                    state,
                    request,
                    target.session,
                    target.server,
                    Pagination::unbounded(),
                )
                .await
                .map(FetchedServerItems::complete),
            };
            (
                index,
                result.map(|mut fetched| {
                    fetched.source_parent_id = source_parent_id;
                    fetched
                }),
            )
        });
    }

    let (indexed_results, failures) = collect_federated_results(join_set, failures).await?;
    if failures > 0 {
        warn!(
            "Returning partial federated response after {} server failure(s)",
            failures
        );
    }
    let server_items = indexed_results
        .into_iter()
        .map(|(_, items)| items)
        .collect::<Vec<_>>();
    let response_shape = ResponseShape::from_responses(
        server_items
            .iter()
            .map(|items| &items.server_items.response),
    );

    Ok(FetchedCatalog {
        server_items,
        failures,
        response_shape,
    })
}

async fn collect_federated_results<T: Send + 'static>(
    mut join_set: JoinSet<(usize, Result<T, StatusCode>)>,
    mut failures: usize,
) -> Result<(Vec<(usize, T)>, usize), StatusCode> {
    let mut indexed_results = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((index, Ok(items))) => indexed_results.push((index, items)),
            Ok((_, Err(e))) => {
                failures += 1;
                error!("Federated server request failed: {:?}", e);
            }
            Err(e) => {
                failures += 1;
                error!("Task failed: {:?}", e);
            }
        }
    }

    if indexed_results.is_empty() {
        error!("All federated server requests failed");
        return Err(StatusCode::BAD_GATEWAY);
    }

    indexed_results.sort_by_key(|(index, _)| *index);
    Ok((indexed_results, failures))
}

async fn fetch_items_from_server(
    index: usize,
    state: AppState,
    request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
    pagination: Pagination,
) -> Result<ServerItems, StatusCode> {
    let proxy_api_key = JellyfinAuthorization::from_request(&request)
        .and_then(|auth| auth.token_ref().map(str::to_string));
    let ServerItems {
        mut response,
        server,
    } = fetch_raw_items_from_server(index, state.clone(), request, session, server, pagination)
        .await?;

    process_items_response_json(&mut response, &state, &server, proxy_api_key.as_deref()).await?;

    debug!(
        "Successfully retrieved {} items from server: {}",
        response.len(),
        server.name
    );
    trace!(
        "Items from server '{}' at index {}: {}",
        server.name,
        index,
        serde_json::to_string(&response).unwrap_or_default()
    );

    Ok(ServerItems { response, server })
}

pub(super) fn estimate_merged_library_total(
    fetched_len: usize,
    upstream_total_sum: i32,
    all_fully_fetched: bool,
) -> usize {
    if all_fully_fetched {
        return fetched_len;
    }

    fetched_len.max(upstream_total_sum.max(0) as usize)
}

async fn fetch_paged_items_from_server(
    index: usize,
    state: AppState,
    request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
) -> Result<FetchedServerItems, StatusCode> {
    let proxy_api_key = JellyfinAuthorization::from_request(&request)
        .and_then(|auth| auth.token_ref().map(str::to_string));
    let PagedItems {
        mut response,
        upstream_total,
        fully_fetched,
    } = fetch_paged_raw_items_from_server(index, state.clone(), request, session, server.clone())
        .await?;
    process_items_response_json(&mut response, &state, &server, proxy_api_key.as_deref()).await?;
    Ok(FetchedServerItems {
        server_items: ServerItems { response, server },
        upstream_total,
        fully_fetched,
        source_parent_id: None,
    })
}

async fn fetch_paged_raw_items_from_server(
    index: usize,
    state: AppState,
    request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
) -> Result<PagedItems, StatusCode> {
    let first_page = fetch_upstream_page_raw(
        index,
        state.clone(),
        request
            .try_clone()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?,
        session.clone(),
        server.clone(),
        0,
    )
    .await?;

    let had_counted_response = matches!(&first_page, ItemsResponseVariants::WithCount(_));
    let upstream_total = match &first_page {
        ItemsResponseVariants::WithCount(response) => Some(response.total_record_count),
        ItemsResponseVariants::Bare(_) => None,
    };
    let first_page_len = first_page.len();
    let mut seen_item_ids = HashSet::new();
    let mut all_items = first_page
        .into_items()
        .into_iter()
        .filter(|item| seen_item_ids.insert(item.id.clone()))
        .collect::<Vec<_>>();

    // A short page can be a backend-imposed cap, not the end of the catalog.
    // Follow actual offsets and trust counted totals only when covered. For
    // uncounted catalogs, an empty page confirms the end.
    let mut offset = first_page_len;
    let mut fully_fetched = upstream_total
        .is_some_and(|total| all_items.len() >= total.max(0) as usize)
        || (upstream_total.is_none() && first_page_len == 0);
    let mut made_progress = first_page_len > 0;
    while !fully_fetched && made_progress {
        let page = fetch_upstream_page_raw(
            index,
            state.clone(),
            request
                .try_clone()
                .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?,
            session.clone(),
            server.clone(),
            offset,
        )
        .await?;
        let page_len = page.len();
        offset = offset.saturating_add(page_len);
        made_progress = false;
        for item in page.into_items() {
            if seen_item_ids.insert(item.id.clone()) {
                made_progress = true;
                all_items.push(item);
            }
        }
        fully_fetched = upstream_total
            .is_some_and(|total| all_items.len() >= total.max(0) as usize)
            || (upstream_total.is_none() && page_len == 0);
    }

    let response = if had_counted_response {
        ItemsResponseVariants::WithCount(ItemsResponseWithCount {
            items: all_items,
            total_record_count: 0,
            start_index: 0,
        })
    } else {
        ItemsResponseVariants::Bare(all_items)
    };
    track_discovered_libraries(&state, &server, &response).await;

    Ok(PagedItems {
        response,
        upstream_total,
        fully_fetched,
    })
}

async fn fetch_upstream_page_raw(
    index: usize,
    state: AppState,
    request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
    start_index: usize,
) -> Result<ItemsResponseVariants, StatusCode> {
    let mut request = request;
    set_upstream_page(request.url_mut(), start_index, UPSTREAM_PAGE_SIZE);
    execute_raw_items_request(index, state, request, session, server)
        .await
        .map(|items| items.response)
}

async fn fetch_raw_items_from_server(
    index: usize,
    state: AppState,
    mut request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
    pagination: Pagination,
) -> Result<ServerItems, StatusCode> {
    // Library discovery must cover the whole inventory, independently of the
    // client's display page. Some upstreams cap even an unbounded request.
    if is_library_root_request(request.url(), state.get_url_prefix().await.as_deref()) {
        return fetch_library_inventory(index, state, request, session, server).await;
    }
    normalize_upstream_pagination(request.url_mut(), pagination);
    let items = execute_raw_items_request(index, state.clone(), request, session, server).await?;
    track_discovered_libraries(&state, &items.server, &items.response).await;
    Ok(items)
}

async fn fetch_library_inventory(
    index: usize,
    state: AppState,
    mut request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
) -> Result<ServerItems, StatusCode> {
    // Keep the first request unpaginated for ordinary Jellyfin user Views.
    normalize_upstream_pagination(request.url_mut(), Pagination::unbounded());
    let mut pagination = jellyfin_api::library_pagination::LibraryPagination::default();
    let mut items = Vec::new();
    let mut counted = false;
    loop {
        let mut page_request = request
            .try_clone()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
        // Advance by the actual number received, not our requested page size.
        if pagination.fetched_count() > 0 {
            set_upstream_page(
                page_request.url_mut(),
                pagination.fetched_count(),
                UPSTREAM_PAGE_SIZE,
            );
        }
        let page = execute_raw_items_request(
            index,
            state.clone(),
            page_request,
            session.clone(),
            server.clone(),
        )
        .await?;
        let total = match &page.response {
            ItemsResponseVariants::WithCount(response) => {
                Some(response.total_record_count.max(0) as usize)
            }
            ItemsResponseVariants::Bare(_) => None,
        };
        counted |= total.is_some();
        let next = page.response.into_items();
        let complete = pagination
            .accept_page(total, next.iter().map(|item| item.id.as_str()))
            .map_err(|error| {
                warn!(
                    "Incomplete library inventory from server '{}': {error}",
                    server.name
                );
                StatusCode::BAD_GATEWAY
            })?
            .unwrap_or(true);
        items.extend(next);
        if complete {
            break;
        }
    }
    let response = if counted {
        ItemsResponseVariants::WithCount(ItemsResponseWithCount {
            total_record_count: items.len() as i32,
            start_index: 0,
            items,
        })
    } else {
        ItemsResponseVariants::Bare(items)
    };
    // Commit discoveries only after all pages succeeded. A failed continuation
    // must not make a partial inventory look authoritative.
    track_discovered_libraries(&state, &server, &response).await;
    Ok(ServerItems { response, server })
}

async fn execute_raw_items_request(
    index: usize,
    state: AppState,
    mut request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
) -> Result<ServerItems, StatusCode> {
    let auth = JellyfinAuthorization::Authorization(session.to_authorization());
    apply_to_request(
        &mut request,
        &server,
        &Some(session),
        &Some(auth),
        &state,
        None,
    )
    .await;

    let response = execute_json_request::<serde_json::Value>(&state.reqwest_client, request)
        .await
        .inspect_err(|e| {
            error!("Failed to get items from server '{}': {:?}", server.name, e);
        })?;

    let items_response: ItemsResponseVariants = response_json_to_payload(response)?;
    debug!(
        "Fetched {} raw items from server '{}' at index {}",
        items_response.len(),
        server.name,
        index
    );

    Ok(ServerItems {
        response: items_response,
        server,
    })
}

async fn track_discovered_libraries(
    state: &AppState,
    server: &Server,
    response: &ItemsResponseVariants,
) {
    let libraries = discovered_libraries_from_response(server.id, response);
    if let Err(error) = state
        .virtual_library_service
        .track_discovered_libraries(&libraries)
        .await
    {
        warn!(
            "Failed to track libraries observed on server '{}': {error}",
            server.name
        );
    }
}

fn discovered_libraries_from_response(
    server_id: ServerId,
    response: &ItemsResponseVariants,
) -> Vec<DiscoveredLibrary> {
    let items = match response {
        ItemsResponseVariants::WithCount(response) => &response.items,
        ItemsResponseVariants::Bare(items) => items,
    };
    items
        .iter()
        .filter_map(|item| {
            let collection_type = presentable_library_collection_type(item)?;
            let name = item.name.as_deref()?.trim();
            (!name.is_empty()).then(|| DiscoveredLibrary {
                server_id,
                original_library_id: item.id.clone(),
                name: name.to_string(),
                collection_type: collection_type.as_str().to_string(),
            })
        })
        .collect()
}

async fn process_items_response_json(
    response: &mut ItemsResponseVariants,
    state: &AppState,
    server: &Server,
    proxy_api_key: Option<&str>,
) -> Result<(), StatusCode> {
    let mut response_json = serde_json::to_value(&*response).map_err(|e| {
        error!("Failed to serialize items response JSON: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    state
        .process_response_json(
            &mut response_json,
            server,
            ResponseProcessingProfile::Media,
            proxy_api_key,
        )
        .await
        .inspect_err(|e| {
            error!(
                "Failed to process media items from server '{}': {:?}",
                server.name, e
            );
        })?;

    *response = response_json_to_payload(response_json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MediaItem;
    use serde_json::json;

    #[test]
    fn raw_library_responses_expose_real_libraries_for_tracking() {
        let mut library = typed_media_item("real-library-id", "CollectionFolder", Some("movies"));
        library.name = Some("Movies".to_string());
        let mut live_tv = typed_media_item("live-tv", "UserView", Some("livetv"));
        live_tv.name = Some("Live TV".to_string());
        let response = ItemsResponseVariants::Bare(vec![
            library,
            live_tv,
            typed_media_item("movie", "Movie", None),
        ]);

        assert_eq!(
            discovered_libraries_from_response(ServerId::new(7), &response),
            vec![DiscoveredLibrary {
                server_id: ServerId::new(7),
                original_library_id: "real-library-id".to_string(),
                name: "Movies".to_string(),
                collection_type: "movies".to_string(),
            }]
        );
    }

    #[test]
    fn merged_library_total_uses_exact_count_when_fully_fetched() {
        assert_eq!(estimate_merged_library_total(42, 500, true), 42);
    }

    #[test]
    fn merged_library_total_uses_upstream_total_when_windowed() {
        assert_eq!(estimate_merged_library_total(80, 1000, false), 1000);
    }

    fn typed_media_item(id: &str, item_type: &str, collection_type: Option<&str>) -> MediaItem {
        let mut item = json!({
            "Id": id,
            "Type": item_type,
        });

        if let Some(collection_type) = collection_type {
            item["CollectionType"] = json!(collection_type);
        }

        serde_json::from_value(item).unwrap()
    }
}
