use std::collections::{HashMap, HashSet};

use axum::{extract::State, Json};
use hyper::StatusCode;
use tokio::task::JoinSet;
use tracing::{debug, error, trace, warn};

use crate::{
    duplicate_handling::TaggedMediaItem,
    extractors::Preprocessed,
    handlers::{
        common::{execute_json_request, response_json_to_payload},
        items::get_items,
    },
    models::{
        enums::{BaseItemKind, CollectionType},
        ItemsResponseVariants, ItemsResponseWithCount, MediaItem,
    },
    processors::response_processor::ResponseProcessingProfile,
    request_preprocessing::{apply_to_request, JellyfinAuthorization, PreprocessedRequest},
    server_id::ServerId,
    server_storage::Server,
    user_authorization_service::AuthorizationSession,
    virtual_library_service::{
        compare_virtual_library_routes, normalize_library_id, DiscoveredLibrary,
        VirtualLibraryAccessScope, VirtualLibraryResolution,
    },
    AppState,
};

mod library_resolution;
mod postprocessing;

use library_resolution::{resolve_catalog_plan, CatalogFetchTarget, CatalogPlan};
use postprocessing::{FederatedItems, Pagination, ResponseShape, ServerItems};

struct FetchedCatalog {
    server_items: Vec<FetchedServerItems>,
    failures: usize,
    response_shape: ResponseShape,
}

struct AutomaticGroupPresentation {
    items: Vec<MediaItem>,
    discovered_members: Vec<(String, ServerId, String)>,
}

struct BuiltVirtualLibrary {
    item: MediaItem,
    members: Vec<(ServerId, String)>,
}

struct NamedMediaItemGroup {
    name: String,
    sort_order: i32,
    items: Vec<ServerMediaItem>,
}

struct LibraryRootInventory {
    configured_groups: HashMap<String, NamedMediaItemGroup>,
    unassigned_libraries: Vec<ServerMediaItem>,
    non_library_per_server: Vec<ServerItems>,
}

struct ServerMediaItem {
    item: MediaItem,
    server: Server,
}

fn replace_parent_id(url: &url::Url, new_id: &str) -> url::Url {
    let pairs = url
        .query_pairs()
        .map(|(key, value)| {
            let value = if key.eq_ignore_ascii_case("ParentId") {
                new_id.to_string()
            } else {
                value.into_owned()
            };
            (key.into_owned(), value)
        })
        .collect::<Vec<_>>();

    let mut new_url = url.clone();
    new_url.query_pairs_mut().clear().extend_pairs(pairs);
    new_url
}

pub async fn get_items_from_all_servers_if_not_restricted(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let original_request = &preprocessed.original_request;

    if has_query_key(original_request.url(), &["SeriesId"]) {
        return get_items(State(state), Preprocessed(preprocessed)).await;
    }

    get_items_from_all_servers_preprocessed(&state, preprocessed).await
}

pub async fn get_items_from_all_servers(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    get_items_from_all_servers_preprocessed(&state, preprocessed).await
}

async fn get_items_from_all_servers_preprocessed(
    state: &AppState,
    preprocessed: PreprocessedRequest,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let plan = resolve_catalog_plan(state, &preprocessed).await?;
    match plan {
        CatalogPlan::EmptyVirtual => {
            let response_shape = if preprocessed
                .original_request
                .url()
                .path()
                .to_ascii_lowercase()
                .ends_with("/latest")
            {
                ResponseShape::Bare
            } else {
                ResponseShape::Counted
            };
            finalize_items_response(
                FederatedItems::default(),
                preprocessed.original_request.url(),
                response_shape,
            )
        }
        CatalogPlan::SingleServer => {
            get_items(State(state.clone()), Preprocessed(preprocessed)).await
        }
        CatalogPlan::Virtual {
            targets,
            skipped_targets,
        } => get_virtual_library_items(state, preprocessed, targets, skipped_targets).await,
        CatalogPlan::Interleaved(targets) => {
            get_interleaved_root(state, preprocessed, targets).await
        }
        CatalogPlan::AutomaticRoot(targets) => {
            get_automatic_library_root(state, preprocessed, targets).await
        }
        CatalogPlan::ConfiguredRoot(targets) => {
            get_configured_library_root(state, preprocessed, targets).await
        }
    }
}

async fn get_virtual_library_items(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    targets: Vec<CatalogFetchTarget>,
    skipped_targets: usize,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let original_request = preprocessed.original_request;
    let pagination = Pagination::from_url(original_request.url());
    let FetchedCatalog {
        server_items,
        response_shape,
        ..
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
    let tagged_items: Vec<TaggedMediaItem> = server_items
        .into_iter()
        .flat_map(|fetch| {
            if let Some(total) = fetch.upstream_total {
                upstream_total_sum += total.max(0);
            }
            all_fully_fetched &= fetch.fully_fetched;

            let ServerItems { response, server } = fetch.server_items;
            response
                .into_items()
                .into_iter()
                .map(move |item| TaggedMediaItem {
                    item,
                    server: server.clone(),
                })
                .collect::<Vec<_>>()
        })
        .collect();

    let items = FederatedItems::from_tagged_items(tagged_items);
    let total_count =
        estimate_merged_library_total(items.len(), upstream_total_sum, all_fully_fetched);

    finalize_items_response(
        items.with_reported_total(total_count),
        original_request.url(),
        response_shape,
    )
}

async fn get_interleaved_root(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    targets: Vec<CatalogFetchTarget>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let original_request = preprocessed.original_request;
    let pagination = Pagination::from_url(original_request.url());
    let FetchedCatalog {
        server_items,
        response_shape,
        ..
    } = fetch_catalog(
        state,
        &original_request,
        targets,
        FetchMode::ClientWindow(pagination),
        0,
    )
    .await?;
    let server_count = server_items.len();
    let responses = server_items
        .into_iter()
        .map(|items| items.server_items.response)
        .collect::<Vec<_>>();
    let items = FederatedItems::interleaved(responses);

    debug!("Combined items from {server_count} servers");

    finalize_items_response(items, original_request.url(), response_shape)
}

#[derive(Clone, Copy)]
enum FetchMode {
    ClientWindow(Pagination),
    VirtualLibrary { pagination: Pagination },
    Inventory,
}

async fn fetch_catalog(
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
        if let Some(parent_id) = target.parent_id {
            *request.url_mut() = replace_parent_id(request.url(), &parent_id);
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
            (index, result)
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

async fn process_library_group_individually(
    state: &AppState,
    group: Vec<ServerMediaItem>,
) -> Result<Vec<MediaItem>, StatusCode> {
    let mut items = Vec::with_capacity(group.len());
    for ServerMediaItem { item, server } in group {
        items.push(process_media_item_for_server(item, state, &server, true).await?);
    }
    Ok(items)
}

async fn present_automatic_library_group(
    state: &AppState,
    key: String,
    group: Vec<ServerMediaItem>,
    access_scope: &VirtualLibraryAccessScope,
    complete_refresh: bool,
) -> Result<AutomaticGroupPresentation, StatusCode> {
    if group.len() == 1 {
        if !complete_refresh {
            if let Some(automatic) = state
                .virtual_library_service
                .get_automatic_library_by_collection_type(&key)
                .await
                .map_err(|error| {
                    error!("Failed to load automatic library: {error}");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?
            {
                let has_accessible_members = matches!(
                    state
                        .virtual_library_service
                        .resolve(&automatic.virtual_id, Some(access_scope))
                        .await
                        .map_err(|error| {
                            error!("Failed to resolve automatic library snapshot: {error}");
                            StatusCode::INTERNAL_SERVER_ERROR
                        })?,
                    VirtualLibraryResolution::Resolved(_)
                );
                if has_accessible_members {
                    let display_name = group
                        .first()
                        .and_then(|source| source.item.name.clone())
                        .unwrap_or_else(|| key.clone());
                    let built = build_virtual_library_item(
                        state,
                        group,
                        display_name,
                        automatic.virtual_id.clone(),
                    )
                    .await?;
                    let discovered_members = built
                        .members
                        .into_iter()
                        .map(|(server_id, member_id)| {
                            (automatic.virtual_id.clone(), server_id, member_id)
                        })
                        .collect();
                    return Ok(AutomaticGroupPresentation {
                        items: vec![built.item],
                        discovered_members,
                    });
                }
            }
        }

        return Ok(AutomaticGroupPresentation {
            items: process_library_group_individually(state, group).await?,
            discovered_members: Vec::new(),
        });
    }

    let display_name = group
        .first()
        .and_then(|source| source.item.name.clone())
        .unwrap_or_else(|| {
            key.split_once(':')
                .map(|(_, name)| name.to_string())
                .unwrap_or_else(|| key.clone())
        });
    let automatic = match state
        .virtual_library_service
        .get_or_create_automatic_library(&key, &display_name)
        .await
    {
        Ok(automatic) => automatic,
        Err(error) => {
            error!("Failed to get/create merged library for '{key}': {error}");
            return Ok(AutomaticGroupPresentation {
                items: process_library_group_individually(state, group).await?,
                discovered_members: Vec::new(),
            });
        }
    };
    let built =
        build_virtual_library_item(state, group, display_name, automatic.virtual_id.clone())
            .await?;
    let discovered_members = built
        .members
        .into_iter()
        .map(|(server_id, member_id)| (automatic.virtual_id.clone(), server_id, member_id))
        .collect();
    Ok(AutomaticGroupPresentation {
        items: vec![built.item],
        discovered_members,
    })
}

async fn partition_library_root_inventory(
    state: &AppState,
    server_items: Vec<FetchedServerItems>,
) -> Result<LibraryRootInventory, StatusCode> {
    let assignments = state
        .virtual_library_service
        .get_assignments()
        .await
        .map_err(|error| {
            error!("Failed to load configured library assignments: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let mut configured_groups: HashMap<String, NamedMediaItemGroup> = HashMap::new();
    let mut unassigned_libraries = Vec::new();
    let mut non_library_per_server = Vec::new();
    let mut live_tv_seen = false;

    for fetched in server_items {
        let ServerItems { response, server } = fetched.server_items;
        let mut non_library_items = Vec::new();

        for item in response.into_items() {
            if let Some(collection_type) = presentable_library_collection_type(&item) {
                let original_library_id = normalize_library_id(&item.id);
                if let Some(assignment) = assignments.get(&(server.id, original_library_id)) {
                    if assignment
                        .collection_type
                        .as_deref()
                        .is_some_and(|group_type| {
                            group_type.eq_ignore_ascii_case(collection_type.as_str())
                        })
                    {
                        configured_groups
                            .entry(assignment.group_virtual_id.clone())
                            .or_insert_with(|| NamedMediaItemGroup {
                                name: assignment.group_name.clone(),
                                sort_order: assignment.group_sort_order,
                                items: Vec::new(),
                            })
                            .items
                            .push(ServerMediaItem {
                                item,
                                server: server.clone(),
                            });
                    } else {
                        warn!(
                            "Ignoring type-incompatible library assignment for '{}' on server {}",
                            item.name.as_deref().unwrap_or(&item.id),
                            server.id
                        );
                        unassigned_libraries.push(ServerMediaItem {
                            item,
                            server: server.clone(),
                        });
                    }
                } else {
                    unassigned_libraries.push(ServerMediaItem {
                        item,
                        server: server.clone(),
                    });
                }
                continue;
            }

            if is_live_tv_user_view(&item) {
                if live_tv_seen {
                    continue;
                }
                live_tv_seen = true;
            }
            non_library_items.push(item);
        }

        if !non_library_items.is_empty() {
            let processed =
                process_media_items_for_server(non_library_items, state, &server, true).await?;
            if !processed.is_empty() {
                non_library_per_server.push(ServerItems {
                    response: ItemsResponseVariants::Bare(processed),
                    server,
                });
            }
        }
    }

    Ok(LibraryRootInventory {
        configured_groups,
        unassigned_libraries,
        non_library_per_server,
    })
}

async fn get_automatic_library_root(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    targets: Vec<CatalogFetchTarget>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let PreprocessedRequest {
        original_request,
        access_scope,
        ..
    } = preprocessed;
    let access_scope = access_scope.ok_or(StatusCode::UNAUTHORIZED)?;
    let refresh_generation = state
        .virtual_library_service
        .begin_automatic_library_refresh(&access_scope)
        .await;

    let FetchedCatalog {
        server_items,
        failures,
        response_shape,
    } = fetch_catalog(state, &original_request, targets, FetchMode::Inventory, 0).await?;
    let refreshed_server_ids = server_items
        .iter()
        .map(|items| items.server_items.server.id)
        .collect::<Vec<_>>();
    let LibraryRootInventory {
        configured_groups,
        unassigned_libraries,
        non_library_per_server,
    } = partition_library_root_inventory(state, server_items).await?;
    let mut library_groups: HashMap<String, Vec<ServerMediaItem>> = HashMap::new();
    for source in unassigned_libraries {
        let Some(key) = automatic_library_key(&source.item) else {
            continue;
        };
        library_groups.entry(key).or_default().push(source);
    }

    let mut library_items = present_configured_library_groups(state, configured_groups).await?;
    let mut discovered_members = Vec::new();
    let mut automatic_groups = library_groups.into_iter().collect::<Vec<_>>();
    automatic_groups.sort_by(|left, right| left.0.cmp(&right.0));
    for (key, group) in automatic_groups {
        let presentation =
            present_automatic_library_group(state, key, group, &access_scope, failures == 0)
                .await?;
        library_items.extend(presentation.items);
        discovered_members.extend(presentation.discovered_members);
    }

    state
        .virtual_library_service
        .reconcile_automatic_library_members(
            &access_scope,
            refresh_generation,
            &refreshed_server_ids,
            &discovered_members,
        )
        .await
        .map_err(|e| {
            error!("Failed to reconcile automatic library members: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let items = FederatedItems::new(library_items).merge_interleaved(non_library_per_server);
    finalize_items_response(items, original_request.url(), response_shape)
}

async fn present_configured_library_groups(
    state: &AppState,
    configured_library_groups: HashMap<String, NamedMediaItemGroup>,
) -> Result<Vec<MediaItem>, StatusCode> {
    let mut group_entries = configured_library_groups.into_iter().collect::<Vec<_>>();
    group_entries.sort_by(|left, right| {
        left.1
            .sort_order
            .cmp(&right.1.sort_order)
            .then_with(|| left.1.name.cmp(&right.1.name))
            .then_with(|| left.0.cmp(&right.0))
    });

    let mut library_join = JoinSet::new();
    for (index, (group_virtual_id, group)) in group_entries.into_iter().enumerate() {
        let state = state.clone();
        library_join.spawn(async move {
            let item =
                build_virtual_library_item(&state, group.items, group.name, group_virtual_id)
                    .await
                    .map(|built| built.item);
            (index, item)
        });
    }

    let mut indexed_library_items = Vec::new();
    while let Some(result) = library_join.join_next().await {
        match result {
            Ok((index, Ok(item))) => indexed_library_items.push((index, item)),
            Ok((_, Err(status))) => return Err(status),
            Err(error) => {
                error!("Library folder processing failed: {error:?}");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }
    indexed_library_items.sort_by_key(|(index, _)| *index);
    Ok(indexed_library_items
        .into_iter()
        .map(|(_, item)| item)
        .collect())
}

async fn get_configured_library_root(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    targets: Vec<CatalogFetchTarget>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let original_request = preprocessed.original_request;
    let FetchedCatalog {
        server_items,
        response_shape,
        ..
    } = fetch_catalog(state, &original_request, targets, FetchMode::Inventory, 0).await?;
    let LibraryRootInventory {
        configured_groups,
        unassigned_libraries,
        non_library_per_server,
    } = partition_library_root_inventory(state, server_items).await?;
    let mut library_items = present_configured_library_groups(state, configured_groups).await?;
    let mut single_groups = HashMap::new();
    for source in unassigned_libraries {
        let key = format!(
            "single:{}:{}",
            source.server.id,
            normalize_library_id(&source.item.id)
        );
        single_groups.entry(key).or_insert(source);
    }
    let mut single_groups = single_groups.into_iter().collect::<Vec<_>>();
    single_groups.sort_by(|left, right| left.0.cmp(&right.0));
    for (_key, ServerMediaItem { item, server }) in single_groups {
        library_items.push(process_library_folder(state, item, &server, true).await?);
    }

    let items = FederatedItems::new(library_items).merge_interleaved(non_library_per_server);

    finalize_items_response(items, original_request.url(), response_shape)
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
    let ServerItems {
        mut response,
        server,
    } = fetch_raw_items_from_server(index, state.clone(), request, session, server, pagination)
        .await?;

    process_items_response_json(&mut response, &state, &server, should_change_name).await?;

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

const UPSTREAM_PAGE_SIZE: usize = 100;
const MAX_PARALLEL_UPSTREAM_PAGES: usize = 8;

struct FetchedServerItems {
    server_items: ServerItems,
    upstream_total: Option<i32>,
    fully_fetched: bool,
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
        }
    }
}

struct WindowedItems {
    response: ItemsResponseVariants,
    upstream_total: Option<i32>,
    fully_fetched: bool,
}

fn is_upstream_limited_catalog_request(url: &url::Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.contains("/latest") || path.contains("/suggestions")
}

fn merged_library_max_pages(pagination: Pagination) -> Option<usize> {
    pagination.limit.map(|client_limit| {
        let window_end = pagination.start_index.saturating_add(client_limit);
        window_end
            .saturating_mul(3)
            .div_ceil(2)
            .div_ceil(UPSTREAM_PAGE_SIZE)
            .max(1)
    })
}

fn estimate_merged_library_total(
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
    process_items_response_json(&mut response, &state, &server, should_change_name).await?;
    Ok(FetchedServerItems {
        server_items: ServerItems { response, server },
        upstream_total,
        fully_fetched,
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
    let mut all_items = first_page.into_items();
    let mut seen_item_ids = all_items
        .iter()
        .map(|item| item.id.clone())
        .collect::<HashSet<_>>();

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
            let page_items = page.into_items();
            for item in &page_items {
                found_new_item |= seen_item_ids.insert(item.id.clone());
            }
            all_items.extend(page_items);
        }
        if !found_new_item {
            fully_fetched = true;
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

fn set_upstream_page(url: &mut url::Url, start_index: usize, limit: usize) {
    let pairs = url
        .query_pairs()
        .filter(|(key, _)| !is_pagination_key(key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();

    let mut query = url.query_pairs_mut();
    query.clear().extend_pairs(pairs);
    query
        .append_pair("StartIndex", &start_index.to_string())
        .append_pair("Limit", &limit.to_string());
}

fn ensure_duplicate_identity_field(url: &mut url::Url) {
    const REQUIRED_FIELD: &str = "ProviderIds";
    let pairs = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    let mut fields = pairs
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("Fields"))
        .map(|(_, value)| {
            value
                .split(',')
                .map(str::trim)
                .filter(|field| !field.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !fields
        .iter()
        .any(|field| field.eq_ignore_ascii_case(REQUIRED_FIELD))
    {
        fields.push(REQUIRED_FIELD.to_string());
    }
    let fields_value = fields.join(",");
    let mut wrote_fields = false;
    let mut query = url.query_pairs_mut();
    query.clear();
    for (key, value) in pairs {
        if key.eq_ignore_ascii_case("Fields") {
            query.append_pair("Fields", &fields_value);
            wrote_fields = true;
        } else {
            query.append_pair(&key, &value);
        }
    }
    if !wrote_fields {
        query.append_pair("Fields", &fields_value);
    }
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
            None,
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

async fn process_media_items_for_server(
    items: Vec<MediaItem>,
    state: &AppState,
    server: &Server,
    should_change_name: bool,
) -> Result<Vec<MediaItem>, StatusCode> {
    let mut processed = Vec::with_capacity(items.len());
    for item in items {
        processed
            .push(process_media_item_for_server(item, state, server, should_change_name).await?);
    }
    Ok(processed)
}

async fn build_virtual_library_item(
    state: &AppState,
    group: Vec<ServerMediaItem>,
    display_name: String,
    virtual_id: String,
) -> Result<BuiltVirtualLibrary, StatusCode> {
    let mut members = Vec::new();
    let mut template = None;
    let mut total_child_count = 0;

    let image_source_index =
        preferred_library_source_index(&group).ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let primary_tag = group[image_source_index]
        .item
        .image_tags
        .as_ref()
        .and_then(|tags| tags.get("Primary").cloned());

    for (index, ServerMediaItem { item, server }) in group.into_iter().enumerate() {
        total_child_count += item.child_count.unwrap_or(0);
        let processed = process_media_item_for_server(item, state, &server, false).await?;
        members.push((server.id, processed.id.clone()));
        if index == image_source_index {
            template = Some(processed);
        }
    }

    let mut item = template.ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let image_source_id = item.id.clone();
    item.id = virtual_id.clone();
    item.display_preferences_id = Some(virtual_id);
    item.name = Some(display_name.clone());
    item.sort_name = Some(display_name.to_lowercase());
    item.child_count = Some(total_child_count);
    attach_library_folder_image_source(&mut item, &image_source_id, primary_tag.as_deref());
    Ok(BuiltVirtualLibrary { item, members })
}

fn preferred_library_source_index(group: &[ServerMediaItem]) -> Option<usize> {
    group
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| {
            compare_virtual_library_routes(
                &left.server,
                &left.item.id,
                &right.server,
                &right.item.id,
            )
        })
        .map(|(index, _)| index)
}

async fn process_library_folder(
    state: &AppState,
    item: MediaItem,
    server: &Server,
    should_change_name: bool,
) -> Result<MediaItem, StatusCode> {
    let primary_tag = item
        .image_tags
        .as_ref()
        .and_then(|tags| tags.get("Primary").cloned());
    let mut processed =
        process_media_item_for_server(item, state, server, should_change_name).await?;
    let image_source_id = processed.id.clone();
    attach_library_folder_image_source(&mut processed, &image_source_id, primary_tag.as_deref());
    Ok(processed)
}

fn attach_library_folder_image_source(
    item: &mut MediaItem,
    image_source_id: &str,
    primary_tag: Option<&str>,
) {
    let Some(primary_tag) = primary_tag.map(str::to_string).or_else(|| {
        item.image_tags
            .as_ref()
            .and_then(|tags| tags.get("Primary").cloned())
    }) else {
        return;
    };

    if let Some(image_tags) = item.image_tags.as_mut() {
        image_tags.remove("Primary");
        if image_tags.is_empty() {
            item.image_tags = None;
        }
    }

    item.extra.insert(
        "PrimaryImageItemId".to_string(),
        serde_json::Value::String(image_source_id.to_string()),
    );
    item.extra.insert(
        "PrimaryImageTag".to_string(),
        serde_json::Value::String(primary_tag.clone()),
    );
    item.image_tags = Some(HashMap::from([("Primary".to_string(), primary_tag)]));
}

async fn process_media_item_for_server(
    item: MediaItem,
    state: &AppState,
    server: &Server,
    should_change_name: bool,
) -> Result<MediaItem, StatusCode> {
    let mut item_json = serde_json::to_value(item).map_err(|e| {
        error!("Failed to serialize media item JSON: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    state
        .process_response_json(
            &mut item_json,
            server,
            ResponseProcessingProfile::Media,
            should_change_name,
            None,
        )
        .await?;

    response_json_to_payload(item_json)
}

fn finalize_items_response(
    items: FederatedItems,
    url: &url::Url,
    response_shape: ResponseShape,
) -> Result<Json<serde_json::Value>, StatusCode> {
    serde_json::to_value(items.into_response(url, response_shape))
        .map(Json)
        .map_err(|e| {
            error!("Failed to serialize federated items response: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

fn has_query_key(url: &url::Url, keys: &[&str]) -> bool {
    url.query()
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes()).any(|(key, _)| {
                keys.iter()
                    .any(|expected_key| key.eq_ignore_ascii_case(expected_key))
            })
        })
        .unwrap_or(false)
}

fn normalize_upstream_pagination(url: &mut url::Url, pagination: Pagination) {
    let pairs = url
        .query_pairs()
        .filter(|(key, _)| !is_pagination_key(key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();

    let upstream_limit = pagination
        .limit
        .map(|limit| pagination.start_index.saturating_add(limit));

    let mut query = url.query_pairs_mut();
    query.clear().extend_pairs(pairs);
    if let Some(upstream_limit) = upstream_limit {
        query.append_pair("Limit", &upstream_limit.to_string());
    }
}

fn is_pagination_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("StartIndex") || key.eq_ignore_ascii_case("Limit")
}

fn is_live_tv_user_view(item: &MediaItem) -> bool {
    item.collection_type == Some(CollectionType::LiveTv) && item.item_type == BaseItemKind::UserView
}

fn automatic_library_key(item: &MediaItem) -> Option<String> {
    let collection_type = presentable_library_collection_type(item)?;
    let name = item.name.as_deref().unwrap_or("").to_lowercase();
    Some(format!("{}:{name}", collection_type.as_str()))
}

fn presentable_library_collection_type(item: &MediaItem) -> Option<&CollectionType> {
    let is_library = matches!(
        item.item_type,
        BaseItemKind::UserView | BaseItemKind::CollectionFolder
    );
    item.collection_type
        .as_ref()
        .filter(|collection_type| is_library && **collection_type != CollectionType::LiveTv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::MediaStreamingMode, server_url::ServerUrl};
    use serde_json::json;

    #[test]
    fn has_query_key_matches_keys_not_values() {
        let url = url::Url::parse("http://localhost/Items?foo=ParentId").unwrap();
        assert!(!has_query_key(&url, &["ParentId"]));

        let url = url::Url::parse("http://localhost/Items?parentid=abc").unwrap();
        assert!(has_query_key(&url, &["ParentId"]));
    }

    #[test]
    fn has_query_key_decodes_encoded_query_keys() {
        let url = url::Url::parse("http://localhost/Items?Parent%49d=abc").unwrap();

        assert!(has_query_key(&url, &["ParentId"]));
    }

    #[test]
    fn unassigned_library_uses_automatic_grouping() {
        let mut item = typed_media_item("library-id", "CollectionFolder", Some("movies"));
        item.name = Some("Anime".to_string());

        assert_eq!(
            automatic_library_key(&item).as_deref(),
            Some("movies:anime")
        );
    }

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
    fn is_upstream_limited_catalog_request_detects_latest_and_suggestions() {
        let latest =
            url::Url::parse("http://localhost/Users/u/Items/Latest?Limit=16&ParentId=abc").unwrap();
        let suggestions =
            url::Url::parse("http://localhost/Users/u/Items/Suggestions?Limit=12").unwrap();
        let browse =
            url::Url::parse("http://localhost/Users/u/Items?Limit=100&ParentId=abc").unwrap();

        assert!(is_upstream_limited_catalog_request(&latest));
        assert!(is_upstream_limited_catalog_request(&suggestions));
        assert!(!is_upstream_limited_catalog_request(&browse));
    }

    #[test]
    fn merged_library_max_pages_scales_with_client_window() {
        assert_eq!(
            merged_library_max_pages(Pagination {
                start_index: 0,
                limit: Some(100),
            }),
            Some(2)
        );
        assert_eq!(
            merged_library_max_pages(Pagination {
                start_index: 100,
                limit: Some(100),
            }),
            Some(3)
        );
        assert_eq!(
            merged_library_max_pages(Pagination {
                start_index: 0,
                limit: None,
            }),
            None
        );
        assert_eq!(
            merged_library_max_pages(Pagination {
                start_index: 1_200,
                limit: Some(100),
            }),
            Some(20)
        );
    }

    #[test]
    fn estimate_merged_library_total_uses_exact_count_when_fully_fetched() {
        assert_eq!(estimate_merged_library_total(42, 500, true), 42);
    }

    #[test]
    fn estimate_merged_library_total_uses_upstream_total_when_windowed() {
        assert_eq!(estimate_merged_library_total(80, 1000, false), 1000);
    }

    #[test]
    fn normalize_upstream_pagination_fetches_enough_for_global_page() {
        let mut url = url::Url::parse(
            "http://localhost/Items?StartIndex=20&Limit=10&Recursive=true&Fields=Genres",
        )
        .unwrap();

        normalize_upstream_pagination(
            &mut url,
            Pagination {
                start_index: 20,
                limit: Some(10),
            },
        );

        let pairs = query_pairs(&url);
        assert_eq!(
            pairs,
            vec![
                ("Recursive".to_string(), "true".to_string()),
                ("Fields".to_string(), "Genres".to_string()),
                ("Limit".to_string(), "30".to_string()),
            ]
        );
    }

    #[test]
    fn normalize_upstream_pagination_removes_start_index_when_unbounded() {
        let mut url =
            url::Url::parse("http://localhost/Items?StartIndex=20&Recursive=true").unwrap();

        normalize_upstream_pagination(
            &mut url,
            Pagination {
                start_index: 20,
                limit: None,
            },
        );

        assert_eq!(
            query_pairs(&url),
            vec![("Recursive".to_string(), "true".to_string())]
        );
    }

    #[test]
    fn normalize_upstream_pagination_uses_saturating_limit_math() {
        let mut url = url::Url::parse("http://localhost/Items?Limit=1").unwrap();

        normalize_upstream_pagination(
            &mut url,
            Pagination {
                start_index: usize::MAX,
                limit: Some(10),
            },
        );

        assert_eq!(
            query_pairs(&url),
            vec![("Limit".to_string(), usize::MAX.to_string())]
        );
    }

    #[test]
    fn set_upstream_page_preserves_non_pagination_parameters() {
        let mut url = url::Url::parse(
            "http://localhost/Items?StartIndex=20&Limit=10&Recursive=true&Fields=Genres",
        )
        .unwrap();

        set_upstream_page(&mut url, 300, 100);

        assert_eq!(
            query_pairs(&url),
            vec![
                ("Recursive".to_string(), "true".to_string()),
                ("Fields".to_string(), "Genres".to_string()),
                ("StartIndex".to_string(), "300".to_string()),
                ("Limit".to_string(), "100".to_string()),
            ]
        );
    }

    #[test]
    fn duplicate_identity_field_only_adds_provider_ids() {
        let mut url = url::Url::parse("http://localhost/Items?Fields=Genres").unwrap();

        ensure_duplicate_identity_field(&mut url);

        assert_eq!(
            query_pairs(&url),
            vec![("Fields".to_string(), "Genres,ProviderIds".to_string())]
        );
    }

    fn query_pairs(url: &url::Url) -> Vec<(String, String)> {
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
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

    #[test]
    fn attach_library_folder_image_source_points_client_at_source_library() {
        let mut item = typed_media_item("group-id", "CollectionFolder", Some("movies"));
        item.image_tags = Some(std::collections::HashMap::from([(
            "Primary".to_string(),
            "tag-123".to_string(),
        )]));

        super::attach_library_folder_image_source(&mut item, "source-library-id", Some("tag-123"));

        assert_eq!(
            item.image_tags
                .as_ref()
                .and_then(|tags| tags.get("Primary"))
                .map(String::as_str),
            Some("tag-123")
        );
        assert_eq!(
            item.extra.get("PrimaryImageItemId"),
            Some(&json!("source-library-id"))
        );
        assert_eq!(item.extra.get("PrimaryImageTag"), Some(&json!("tag-123")));
    }

    #[test]
    fn merged_library_image_uses_same_server_priority_as_request_routing() {
        let source = |id: &str, image_tag: &str, server_id: i64, priority: i32| {
            let mut item = typed_media_item(id, "CollectionFolder", Some("movies"));
            item.image_tags = Some(HashMap::from([(
                "Primary".to_string(),
                image_tag.to_string(),
            )]));
            ServerMediaItem {
                item,
                server: Server {
                    id: ServerId::new(server_id),
                    name: format!("Server {server_id}"),
                    url: ServerUrl::parse(&format!("http://server-{server_id}.example")).unwrap(),
                    priority,
                    media_streaming_mode: MediaStreamingMode::Redirect,
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                },
            }
        };
        let group = vec![
            source("fast-response", "wrong-tag", 1, 100),
            source("preferred", "routable-tag", 2, 200),
        ];

        let selected = preferred_library_source_index(&group).unwrap();

        assert_eq!(group[selected].item.id, "preferred");
        assert_eq!(
            group[selected]
                .item
                .image_tags
                .as_ref()
                .and_then(|tags| tags.get("Primary"))
                .map(String::as_str),
            Some("routable-tag")
        );
    }
}
