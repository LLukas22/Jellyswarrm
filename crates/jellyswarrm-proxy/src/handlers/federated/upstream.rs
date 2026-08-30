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
    library_resolution::CatalogFetchTarget,
    postprocessing::{Pagination, ResponseShape, ServerItems},
    request_policy::{
        ensure_duplicate_identity_field, ensure_global_sort_fields,
        is_upstream_limited_catalog_request, merged_library_max_pages,
        normalize_upstream_pagination, replace_parent_id, set_upstream_page, UPSTREAM_PAGE_SIZE,
    },
};

const MAX_PARALLEL_UPSTREAM_PAGES: usize = 8;

pub(super) struct FetchedCatalog {
    pub(super) server_items: Vec<FetchedServerItems>,
    pub(super) failures: usize,
    pub(super) response_shape: ResponseShape,
}

#[derive(Clone, Copy)]
pub(super) enum FetchMode {
    ClientWindow(Pagination),
    VirtualLibrary { pagination: Pagination },
    Inventory,
}

pub(super) struct FetchedServerItems {
    pub(super) server_items: ServerItems,
    pub(super) upstream_total: Option<i32>,
    pub(super) fully_fetched: bool,
    pub(super) source_parent_id: Option<String>,
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

struct WindowedItems {
    response: ItemsResponseVariants,
    upstream_total: Option<i32>,
    fully_fetched: bool,
}

pub(super) async fn fetch_catalog(
    state: &AppState,
    original_request: &reqwest::Request,
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
        ensure_global_sort_fields(request.url_mut());
        let source_parent_id = target.parent_id.clone();
        if let Some(parent_id) = target.parent_id.as_deref() {
            *request.url_mut() = replace_parent_id(request.url(), parent_id);
            ensure_duplicate_identity_field(request.url_mut());
        }

        let state = state.clone();
        join_set.spawn(async move {
            let result = match mode {
                FetchMode::ClientWindow(pagination) => fetch_items_from_server(
                    index,
                    state,
                    request,
                    target.session,
                    target.server,
                    pagination,
                    true,
                )
                .await
                .map(FetchedServerItems::complete),
                FetchMode::VirtualLibrary { pagination } => {
                    if is_upstream_limited_catalog_request(request.url()) {
                        fetch_items_from_server(
                            index,
                            state,
                            request,
                            target.session,
                            target.server,
                            pagination,
                            false,
                        )
                        .await
                        .map(FetchedServerItems::complete)
                    } else {
                        fetch_windowed_items_from_server(
                            index,
                            state,
                            request,
                            target.session,
                            target.server,
                            merged_library_max_pages(pagination),
                            false,
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
    should_change_name: bool,
) -> Result<ServerItems, StatusCode> {
    let proxy_api_key = JellyfinAuthorization::from_request(&request)
        .and_then(|auth| auth.token_ref().map(str::to_string));
    let ServerItems {
        mut response,
        server,
    } = fetch_raw_items_from_server(index, state.clone(), request, session, server, pagination)
        .await?;

    process_items_response_json(
        &mut response,
        &state,
        &server,
        should_change_name,
        proxy_api_key.as_deref(),
    )
    .await?;

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

async fn fetch_windowed_items_from_server(
    index: usize,
    state: AppState,
    request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
    max_pages: Option<usize>,
    should_change_name: bool,
) -> Result<FetchedServerItems, StatusCode> {
    let proxy_api_key = JellyfinAuthorization::from_request(&request)
        .and_then(|auth| auth.token_ref().map(str::to_string));
    let WindowedItems {
        mut response,
        upstream_total,
        fully_fetched,
    } = fetch_windowed_raw_items_from_server(
        index,
        state.clone(),
        request,
        session,
        server.clone(),
        max_pages,
    )
    .await?;
    process_items_response_json(
        &mut response,
        &state,
        &server,
        should_change_name,
        proxy_api_key.as_deref(),
    )
    .await?;
    Ok(FetchedServerItems {
        server_items: ServerItems { response, server },
        upstream_total,
        fully_fetched,
        source_parent_id: None,
    })
}

async fn fetch_windowed_raw_items_from_server(
    index: usize,
    state: AppState,
    request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
    max_pages: Option<usize>,
) -> Result<WindowedItems, StatusCode> {
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

    let mut fully_fetched = first_page_len < UPSTREAM_PAGE_SIZE;
    let max_pages = max_pages.or_else(|| {
        upstream_total.map(|total| (total.max(0) as usize).div_ceil(UPSTREAM_PAGE_SIZE).max(1))
    });
    let mut next_page = 1;
    while !fully_fetched && max_pages.is_none_or(|max_pages| next_page < max_pages) {
        let end_page = max_pages
            .map(|max_pages| (next_page + MAX_PARALLEL_UPSTREAM_PAGES).min(max_pages))
            .unwrap_or(next_page + MAX_PARALLEL_UPSTREAM_PAGES);
        let page_starts = (next_page..end_page)
            .map(|page| page * UPSTREAM_PAGE_SIZE)
            .collect::<Vec<_>>();
        let extra_pages = fetch_upstream_pages_parallel(
            index,
            state.clone(),
            &request,
            session.clone(),
            server.clone(),
            &page_starts,
        )
        .await?;
        next_page = end_page;

        if let Some((_, last_page)) = extra_pages.last() {
            fully_fetched = last_page.len() < UPSTREAM_PAGE_SIZE;
        }
        let mut found_new_item = false;
        for (_, page) in extra_pages {
            for item in page.into_items() {
                if seen_item_ids.insert(item.id.clone()) {
                    found_new_item = true;
                    all_items.push(item);
                }
            }
        }
        if !found_new_item {
            break;
        }
    }

    if let Some(total) = upstream_total {
        if all_items.len() >= total.max(0) as usize {
            fully_fetched = true;
        }
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

    Ok(WindowedItems {
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

async fn fetch_upstream_pages_parallel(
    index: usize,
    state: AppState,
    request: &reqwest::Request,
    session: AuthorizationSession,
    server: Server,
    page_starts: &[usize],
) -> Result<Vec<(usize, ItemsResponseVariants)>, StatusCode> {
    let mut pages = Vec::with_capacity(page_starts.len());
    for chunk in page_starts.chunks(MAX_PARALLEL_UPSTREAM_PAGES) {
        let mut join_set = JoinSet::new();
        for &start_index in chunk {
            let state = state.clone();
            let request = request
                .try_clone()
                .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
            let session = session.clone();
            let server = server.clone();
            join_set.spawn(async move {
                let page =
                    fetch_upstream_page_raw(index, state, request, session, server, start_index)
                        .await?;
                Ok::<_, StatusCode>((start_index, page))
            });
        }

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(Ok((start_index, page))) => pages.push((start_index, page)),
                Ok(Err(status)) => return Err(status),
                Err(error) => {
                    error!("Parallel upstream page fetch failed: {:?}", error);
                    return Err(StatusCode::INTERNAL_SERVER_ERROR);
                }
            }
        }
    }

    pages.sort_by_key(|(start_index, _)| *start_index);
    Ok(pages)
}

async fn fetch_raw_items_from_server(
    index: usize,
    state: AppState,
    mut request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
    pagination: Pagination,
) -> Result<ServerItems, StatusCode> {
    normalize_upstream_pagination(request.url_mut(), pagination);
    execute_raw_items_request(index, state, request, session, server).await
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
    track_discovered_libraries(&state, &server, &items_response).await;
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
    should_change_name: bool,
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
            should_change_name,
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
