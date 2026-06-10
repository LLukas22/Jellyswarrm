use axum::{extract::State, Json};
use hyper::StatusCode;
use std::collections::{HashMap, VecDeque};
use tokio::task::JoinSet;
use tracing::{debug, error, trace, warn};

use crate::{
    duplicate_policy::{deduplicate_tagged_items, DuplicatePolicy, DuplicatePolicyConfig, TaggedMediaItem},
    extractors::Preprocessed,
    handlers::{
        common::{execute_json_request, response_json_to_payload},
        items::get_items,
    },
    library_group_service::{normalize_library_id, LibraryAssignment},
    merged_library_service::MergedLibraryMember,
    models::{
        enums::{BaseItemKind, CollectionType},
        ItemsResponseVariants, ItemsResponseWithCount, MediaItem,
    },
    processors::response_processor::ResponseProcessingProfile,
    request_preprocessing::{apply_to_request, JellyfinAuthorization, PreprocessedRequest},
    server_storage::Server,
    user_authorization_service::AuthorizationSession,
    AppState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pagination {
    start_index: usize,
    limit: Option<usize>,
}

type NamedMediaItemGroup = (String, Vec<(MediaItem, Server)>);

fn extract_parent_id(url: &url::Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case("ParentId"))
        .map(|(_, value)| value.into_owned())
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

    if let Some(parent_id) = extract_parent_id(original_request.url()) {
        if is_single_virtual_library_parent(&state, &parent_id).await {
            return get_items(State(state), Preprocessed(preprocessed)).await;
        }
    }

    get_items_from_all_servers_preprocessed(&state, preprocessed).await
}

async fn resolve_library_parent_members(
    state: &AppState,
    parent_id: &str,
) -> Result<Option<Vec<MergedLibraryMember>>, StatusCode> {
    if state.library_group_service.has_groups().await.unwrap_or(false) {
        match state.library_group_service.resolve(state, parent_id).await {
            Ok(Some((lib, members))) if !members.is_empty() => {
                debug!(
                    "ParentId {} is custom library group '{}' — fanning out to {} servers",
                    parent_id,
                    lib.name,
                    members.len()
                );
                return Ok(Some(members));
            }
            Ok(_) => {}
            Err(e) => {
                error!(
                    "Failed to resolve custom library group for {}: {}",
                    parent_id, e
                );
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    if state.merge_libraries_enabled().await {
        match state.merged_library_service.resolve(parent_id).await {
            Ok(Some((lib, members))) if !members.is_empty() => {
                debug!(
                    "ParentId {} is merged library '{}' — fanning out to {} servers",
                    parent_id,
                    lib.collection_type,
                    members.len()
                );
                return Ok(Some(members));
            }
            Ok(_) => {}
            Err(e) => {
                error!("Failed to resolve merged library for {}: {}", parent_id, e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    Ok(None)
}

async fn is_single_virtual_library_parent(state: &AppState, parent_id: &str) -> bool {
    let parent_id = normalize_library_id(parent_id);
    if state
        .library_group_service
        .get_group(&parent_id)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        return false;
    }
    if state
        .merged_library_service
        .get_by_virtual_id(&parent_id)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        return false;
    }
    state
        .media_storage
        .get_media_mapping_by_virtual(&parent_id)
        .await
        .ok()
        .flatten()
        .is_some()
}

async fn get_items_for_merged_library(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    members: Vec<MergedLibraryMember>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let original_request = preprocessed.original_request;
    let sessions = preprocessed.sessions.ok_or(StatusCode::UNAUTHORIZED)?;
    if sessions.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let pagination = pagination_from_url(original_request.url());
    let parent_id = extract_parent_id(original_request.url());
    let duplicate_config = resolve_duplicate_policy(state, parent_id.as_deref()).await;
    let mut join_set = JoinSet::new();
    let mut failures = 0;
    let mut member_servers: HashMap<usize, Server> = HashMap::new();

    for (index, member) in members.into_iter().enumerate() {
        let resolved = state
            .media_storage
            .get_media_mapping_with_server(&member.virtual_library_id)
            .await
            .map_err(|e| {
                error!(
                    "Failed to resolve member library {}: {}",
                    member.virtual_library_id, e
                );
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

        let (mapping, server) = match resolved {
            Some(value) => value,
            None => {
                error!(
                    "No media mapping found for virtual library {}",
                    member.virtual_library_id
                );
                failures += 1;
                continue;
            }
        };

        let session = sessions
            .iter()
            .find(|(_, session_server)| session_server.id == server.id)
            .map(|(session, _)| session.clone());

        let Some(session) = session else {
            error!("No active session for server '{}' — skipping", server.name);
            failures += 1;
            continue;
        };

        member_servers.insert(index, server.clone());

        let mut request = match original_request.try_clone() {
            Some(request) => request,
            None => {
                error!("Failed to clone request for merged library fan-out");
                failures += 1;
                continue;
            }
        };

        *request.url_mut() = replace_parent_id(request.url(), &mapping.original_media_id);
        ensure_dedup_fields(request.url_mut());

        let state_clone = state.clone();
        join_set.spawn(async move {
            let result = fetch_all_items_from_server(
                index,
                state_clone,
                request,
                session,
                server,
                false,
            )
            .await;
            (index, result)
        });
    }

    let mut indexed_results: Vec<(usize, ItemsResponseVariants)> = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((index, Ok(items))) => indexed_results.push((index, items)),
            Ok((_, Err(e))) => {
                failures += 1;
                error!("Merged library fan-out failed: {:?}", e);
            }
            Err(e) => {
                failures += 1;
                error!("Task join error in merged fan-out: {:?}", e);
            }
        }
    }

    if indexed_results.is_empty() {
        error!("All merged library fan-out requests failed");
        return Err(StatusCode::BAD_GATEWAY);
    }

    if failures > 0 {
        warn!(
            "Returning partial merged library response after {} server failure(s)",
            failures
        );
    }

    indexed_results.sort_by_key(|(index, _)| *index);
    let wrapped_response = indexed_results
        .iter()
        .any(|(_, items)| matches!(items, ItemsResponseVariants::WithCount(_)));
    let all_items: Vec<TaggedMediaItem> = indexed_results
        .into_iter()
        .flat_map(|(index, response)| {
            let Some(server) = member_servers.get(&index).cloned() else {
                error!("Missing server mapping for merged fan-out index {}", index);
                return Vec::new();
            };
            response
                .into_items()
                .into_iter()
                .map(move |item| TaggedMediaItem { item, server: server.clone() })
                .collect::<Vec<_>>()
        })
        .collect();

    let mut all_items = deduplicate_tagged_items(all_items, &duplicate_config);
    all_items.sort_by(|a, b| {
        let left = a.sort_name.as_deref().or(a.name.as_deref()).unwrap_or("");
        let right = b.sort_name.as_deref().or(b.name.as_deref()).unwrap_or("");
        left.cmp(right)
    });

    let (paged_items, total_count) = apply_pagination(all_items, pagination);
    items_response_to_json(items_response_from_shape(
        paged_items,
        total_count,
        pagination,
        wrapped_response,
    ))
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
    if let Some(parent_id) = extract_parent_id(preprocessed.original_request.url()) {
        match resolve_library_parent_members(state, &parent_id).await {
            Ok(Some(members)) => {
                return get_items_for_merged_library(state, preprocessed, members).await;
            }
            Ok(None) => {
                if is_single_virtual_library_parent(state, &parent_id).await {
                    return get_items(State(state.clone()), Preprocessed(preprocessed)).await;
                }
            }
            Err(status) => return Err(status),
        }
    }

    if state
        .library_group_service
        .has_groups()
        .await
        .unwrap_or(false)
    {
        get_items_from_all_servers_with_custom_library_groups(state, preprocessed).await
    } else if state.merge_libraries_enabled().await {
        get_items_from_all_servers_with_merged_libraries(state, preprocessed).await
    } else {
        get_items_from_all_servers_interleaved(state, preprocessed).await
    }
}

async fn get_items_from_all_servers_interleaved(
    state: &AppState,
    preprocessed: PreprocessedRequest,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let original_request = preprocessed.original_request;
    let sessions = preprocessed.sessions.ok_or(StatusCode::UNAUTHORIZED)?;
    if sessions.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let pagination = pagination_from_url(original_request.url());
    let mut join_set = JoinSet::new();
    let mut failures = 0;

    for (index, (session, server)) in sessions.into_iter().enumerate() {
        let request = match original_request.try_clone() {
            Some(request) => request,
            None => {
                error!("Failed to clone request for server: {}", server.name);
                failures += 1;
                continue;
            }
        };

        let state_clone = state.clone();
        join_set.spawn(async move {
            let result = fetch_items_from_server(
                index,
                state_clone,
                request,
                session,
                server,
                pagination,
                true,
            )
            .await;
            (index, result)
        });
    }

    let (indexed_results, failures) = collect_federated_results(join_set, failures).await?;
    let wrapped_response = indexed_results
        .iter()
        .any(|(_, items)| matches!(items, ItemsResponseVariants::WithCount(_)));
    let server_count = indexed_results.len();
    let server_items = indexed_results
        .into_iter()
        .map(|(_, items)| items)
        .collect::<Vec<_>>();
    let interleaved_items = interleave_items(server_items);
    let (paged_items, total_count) = apply_pagination(interleaved_items, pagination);

    debug!(
        "Returning {} of {} interleaved items from {} servers",
        paged_items.len(),
        total_count,
        server_count
    );

    trace!(
        "Items: {}",
        serde_json::to_string(&paged_items).unwrap_or_default()
    );

    let response = if wrapped_response {
        ItemsResponseVariants::WithCount(ItemsResponseWithCount {
            items: paged_items,
            total_record_count: to_i32(total_count),
            start_index: to_i32(pagination.start_index),
        })
    } else {
        ItemsResponseVariants::Bare(paged_items)
    };

    if failures > 0 {
        warn!(
            "Returning partial federated response after {} server failure(s)",
            failures
        );
    }

    items_response_to_json(response)
}

async fn get_items_from_all_servers_with_merged_libraries(
    state: &AppState,
    preprocessed: PreprocessedRequest,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let original_request = preprocessed.original_request;
    let sessions = preprocessed.sessions.ok_or(StatusCode::UNAUTHORIZED)?;
    if sessions.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let pagination = pagination_from_url(original_request.url());
    let mut join_set = JoinSet::new();
    let mut failures = 0;

    for (index, (session, server)) in sessions.into_iter().enumerate() {
        let request = match original_request.try_clone() {
            Some(request) => request,
            None => {
                error!("Failed to clone request for server: {}", server.name);
                failures += 1;
                continue;
            }
        };

        let state_clone = state.clone();
        join_set.spawn(async move {
            let result = fetch_raw_items_from_server(
                index,
                state_clone,
                request,
                session,
                server,
                pagination,
            )
            .await;
            (index, result)
        });
    }

    let mut indexed_raw: Vec<(usize, (ItemsResponseVariants, Server))> = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((index, Ok(raw))) => indexed_raw.push((index, raw)),
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

    if indexed_raw.is_empty() {
        error!("All federated server requests failed");
        return Err(StatusCode::BAD_GATEWAY);
    }

    if failures > 0 {
        warn!(
            "Returning partial federated response after {} server failure(s)",
            failures
        );
    }

    indexed_raw.sort_by_key(|(index, _)| *index);

    let server_raw = indexed_raw
        .into_iter()
        .map(|(_, raw)| raw)
        .collect::<Vec<_>>();
    let wrapped_response = server_raw
        .iter()
        .any(|(items, _)| matches!(items, ItemsResponseVariants::WithCount(_)));
    let mut library_groups: HashMap<String, Vec<(MediaItem, Server)>> = HashMap::new();
    let mut non_lib_per_server: Vec<(ItemsResponseVariants, Server)> = Vec::new();
    let mut live_tv_seen = false;

    for (raw_response, server) in server_raw {
        let mut non_library_items = Vec::new();

        for item in raw_response.into_items() {
            let is_mergeable = matches!(
                item.item_type,
                BaseItemKind::UserView | BaseItemKind::CollectionFolder
            ) && item
                .collection_type
                .as_ref()
                .is_some_and(|collection_type| *collection_type != CollectionType::LiveTv);

            if is_mergeable {
                let collection_type = serde_json::to_string(item.collection_type.as_ref().unwrap())
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string();
                let name = item.name.as_deref().unwrap_or("").to_lowercase();
                let key = format!("{}:{}", collection_type, name);
                library_groups
                    .entry(key)
                    .or_default()
                    .push((item, server.clone()));
            } else {
                if is_live_tv_user_view(&item) {
                    if live_tv_seen {
                        continue;
                    }
                    live_tv_seen = true;
                }
                non_library_items.push(item);
            }
        }

        if !non_library_items.is_empty() {
            let processed =
                process_media_items_for_server(non_library_items, state, &server, true).await?;
            if !processed.is_empty() {
                non_lib_per_server.push((ItemsResponseVariants::Bare(processed), server));
            }
        }
    }

    let mut library_items = Vec::new();
    for (key, group) in library_groups {
        if group.len() == 1 {
            if let Some((item, server)) = group.into_iter().next() {
                library_items
                    .push(process_media_item_for_server(item, state, &server, true).await?);
            }
            continue;
        }

        let display_name = group[0].0.name.clone().unwrap_or_else(|| {
            key.split_once(':')
                .map(|(_, name)| name.to_string())
                .unwrap_or_else(|| key.clone())
        });

        let merged = match state
            .merged_library_service
            .get_or_create(&key, &display_name)
            .await
        {
            Ok(merged) => merged,
            Err(e) => {
                error!("Failed to get/create merged library for '{}': {}", key, e);
                for (item, server) in group {
                    library_items
                        .push(process_media_item_for_server(item, state, &server, true).await?);
                }
                continue;
            }
        };

        let merged_item = build_merged_library_item(
            state,
            group,
            display_name,
            merged.virtual_id,
            true,
        )
        .await?;
        library_items.push(merged_item);
    }

    library_items.sort_by(|a, b| {
        let left = a.name.as_deref().unwrap_or("");
        let right = b.name.as_deref().unwrap_or("");
        left.cmp(right)
    });

    let mut final_items = library_items;
    final_items.extend(interleave_server_items(non_lib_per_server));

    let (paged_items, total_count) = apply_pagination(final_items, pagination);
    debug!(
        "Returning {} of {} federated items",
        paged_items.len(),
        total_count
    );

    items_response_to_json(items_response_from_shape(
        paged_items,
        total_count,
        pagination,
        wrapped_response,
    ))
}

async fn get_items_from_all_servers_with_custom_library_groups(
    state: &AppState,
    preprocessed: PreprocessedRequest,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let original_request = preprocessed.original_request;
    let sessions = preprocessed.sessions.ok_or(StatusCode::UNAUTHORIZED)?;
    if sessions.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let pagination = pagination_from_url(original_request.url());
    let mut join_set = JoinSet::new();
    let mut failures = 0;

    for (index, (session, server)) in sessions.into_iter().enumerate() {
        let request = match original_request.try_clone() {
            Some(request) => request,
            None => {
                error!("Failed to clone request for server: {}", server.name);
                failures += 1;
                continue;
            }
        };

        let state_clone = state.clone();
        join_set.spawn(async move {
            let result = fetch_raw_items_from_server(
                index,
                state_clone,
                request,
                session,
                server,
                pagination,
            )
            .await;
            (index, result)
        });
    }

    let mut indexed_raw: Vec<(usize, (ItemsResponseVariants, Server))> = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((index, Ok(raw))) => indexed_raw.push((index, raw)),
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

    if indexed_raw.is_empty() {
        error!("All federated server requests failed");
        return Err(StatusCode::BAD_GATEWAY);
    }

    if failures > 0 {
        warn!(
            "Returning partial federated response after {} server failure(s)",
            failures
        );
    }

    indexed_raw.sort_by_key(|(index, _)| *index);

    let server_raw = indexed_raw
        .into_iter()
        .map(|(_, raw)| raw)
        .collect::<Vec<_>>();
    let wrapped_response = server_raw
        .iter()
        .any(|(items, _)| matches!(items, ItemsResponseVariants::WithCount(_)));
    let custom_assignments = state
        .library_group_service
        .get_assignments()
        .await
        .unwrap_or_default();
    let mut custom_library_groups: HashMap<String, NamedMediaItemGroup> = HashMap::new();
    let mut library_groups: HashMap<String, Vec<(MediaItem, Server)>> = HashMap::new();
    let mut non_lib_per_server: Vec<(ItemsResponseVariants, Server)> = Vec::new();
    let mut live_tv_seen = false;

    for (raw_response, server) in server_raw {
        let mut non_library_items = Vec::new();

        for item in raw_response.into_items() {
            let is_mergeable = matches!(
                item.item_type,
                BaseItemKind::UserView | BaseItemKind::CollectionFolder
            ) && item
                .collection_type
                .as_ref()
                .is_some_and(|collection_type| *collection_type != CollectionType::LiveTv);

            if is_mergeable {
                let original_library_id = normalize_library_id(&item.id);
                if let Some(LibraryAssignment {
                    group_virtual_id,
                    group_name,
                }) = custom_assignments.get(&(server.id, original_library_id.clone()))
                {
                    custom_library_groups
                        .entry(group_virtual_id.clone())
                        .or_insert_with(|| (group_name.clone(), Vec::new()))
                        .1
                        .push((item, server.clone()));
                    continue;
                }

                library_groups
                    .entry(format!("single:{}:{}", server.id, original_library_id))
                    .or_default()
                    .push((item, server.clone()));
            } else {
                if is_live_tv_user_view(&item) {
                    if live_tv_seen {
                        continue;
                    }
                    live_tv_seen = true;
                }
                non_library_items.push(item);
            }
        }

        if !non_library_items.is_empty() {
            let processed =
                process_media_items_for_server(non_library_items, state, &server, true).await?;
            if !processed.is_empty() {
                non_lib_per_server.push((ItemsResponseVariants::Bare(processed), server));
            }
        }
    }

    let group_sort_order: HashMap<String, i32> = state
        .library_group_service
        .list_groups()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|group| (group.virtual_id.clone(), group.sort_order))
        .collect();
    let mut custom_group_entries: Vec<(String, NamedMediaItemGroup)> =
        custom_library_groups.into_iter().collect();
    custom_group_entries.sort_by(|left, right| {
        let left_order = group_sort_order.get(&left.0).copied().unwrap_or(0);
        let right_order = group_sort_order.get(&right.0).copied().unwrap_or(0);
        left_order
            .cmp(&right_order)
            .then_with(|| left.1.0.cmp(&right.1.0))
    });

    let mut library_join = JoinSet::new();
    for (group_virtual_id, (display_name, group)) in custom_group_entries {
        if group.is_empty() {
            continue;
        }
        let state = state.clone();
        library_join.spawn(async move {
            build_merged_library_item(&state, group, display_name, group_virtual_id, false).await
        });
    }
    for (_key, group) in library_groups {
        if let Some((item, server)) = group.into_iter().next() {
            let state = state.clone();
            library_join.spawn(async move {
                process_library_folder(&state, item, &server, true).await
            });
        }
    }

    let mut library_items = Vec::new();
    while let Some(result) = library_join.join_next().await {
        match result {
            Ok(Ok(item)) => library_items.push(item),
            Ok(Err(status)) => return Err(status),
            Err(e) => {
                error!("Library folder processing failed: {:?}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    library_items.sort_by(|a, b| {
        let left = a.name.as_deref().unwrap_or("");
        let right = b.name.as_deref().unwrap_or("");
        left.cmp(right)
    });

    let mut final_items = library_items;
    if is_playback_catalog_request(original_request.url()) {
        let duplicate_config =
            resolve_duplicate_policy(state, extract_parent_id(original_request.url()).as_deref())
                .await;
        let tagged: Vec<TaggedMediaItem> = non_lib_per_server
            .into_iter()
            .flat_map(|(items, server)| {
                items
                    .into_items()
                    .into_iter()
                    .map(move |item| TaggedMediaItem {
                        item,
                        server: server.clone(),
                    })
            })
            .collect();
        final_items.extend(deduplicate_tagged_items(tagged, &duplicate_config));
    } else {
        final_items.extend(interleave_server_items(non_lib_per_server));
    }

    let (paged_items, total_count) = apply_pagination(final_items, pagination);
    debug!(
        "Returning {} of {} federated items",
        paged_items.len(),
        total_count
    );

    items_response_to_json(items_response_from_shape(
        paged_items,
        total_count,
        pagination,
        wrapped_response,
    ))
}

fn items_response_from_shape(
    items: Vec<MediaItem>,
    total_count: usize,
    pagination: Pagination,
    wrapped_response: bool,
) -> ItemsResponseVariants {
    if wrapped_response {
        ItemsResponseVariants::WithCount(ItemsResponseWithCount {
            items,
            total_record_count: to_i32(total_count),
            start_index: to_i32(pagination.start_index),
        })
    } else {
        ItemsResponseVariants::Bare(items)
    }
}

async fn collect_federated_results(
    mut join_set: JoinSet<(usize, Result<ItemsResponseVariants, StatusCode>)>,
    mut failures: usize,
) -> Result<(Vec<(usize, ItemsResponseVariants)>, usize), StatusCode> {
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
) -> Result<ItemsResponseVariants, StatusCode> {
    let (mut items_response, server) =
        fetch_raw_items_from_server(index, state.clone(), request, session, server, pagination)
            .await?;

    process_items_response_json(&mut items_response, &state, &server, should_change_name).await?;

    debug!(
        "Successfully retrieved {} items from server: {}",
        items_response.len(),
        server.name
    );
    trace!(
        "Items from server '{}' at index {}: {}",
        server.name,
        index,
        serde_json::to_string(&items_response).unwrap_or_default()
    );

    Ok(items_response)
}

const UPSTREAM_PAGE_SIZE: usize = 100;

async fn fetch_all_items_from_server(
    index: usize,
    state: AppState,
    request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
    should_change_name: bool,
) -> Result<ItemsResponseVariants, StatusCode> {
    let (mut items_response, server) =
        fetch_all_raw_items_from_server(index, state.clone(), request, session, server).await?;
    process_items_response_json(&mut items_response, &state, &server, should_change_name).await?;
    Ok(items_response)
}

async fn fetch_all_raw_items_from_server(
    index: usize,
    state: AppState,
    request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
) -> Result<(ItemsResponseVariants, Server), StatusCode> {
    let mut all_items = Vec::new();
    let mut start_index = 0;
    let mut had_counted_response = false;

    loop {
        let mut page_request = request
            .try_clone()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
        set_upstream_page(page_request.url_mut(), start_index, UPSTREAM_PAGE_SIZE);

        let (page, _server) = fetch_raw_items_from_server(
            index,
            state.clone(),
            page_request,
            session.clone(),
            server.clone(),
            Pagination {
                start_index: 0,
                limit: None,
            },
        )
        .await?;

        let page_len = page.len();
        if matches!(&page, ItemsResponseVariants::WithCount(_)) {
            had_counted_response = true;
        }
        all_items.extend(page.into_items());

        if !should_continue_upstream_fetch(page_len, UPSTREAM_PAGE_SIZE) {
            break;
        }
        start_index += page_len;
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

    Ok((response, server))
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

fn should_continue_upstream_fetch(page_len: usize, page_size: usize) -> bool {
    page_len >= page_size
}

fn ensure_dedup_fields(url: &mut url::Url) {
    const REQUIRED_FIELDS: &[&str] = &["ChildCount", "ProviderIds"];
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
    for required_field in REQUIRED_FIELDS {
        if !fields
            .iter()
            .any(|field| field.eq_ignore_ascii_case(required_field))
        {
            fields.push((*required_field).to_string());
        }
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
) -> Result<(ItemsResponseVariants, Server), StatusCode> {
    normalize_upstream_pagination(request.url_mut(), pagination);

    let auth = JellyfinAuthorization::Authorization(session.to_authorization());
    apply_to_request(&mut request, &server, &Some(session), &Some(auth), &state).await;

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

    Ok((items_response, server))
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

async fn build_merged_library_item(
    state: &AppState,
    group: Vec<(MediaItem, Server)>,
    display_name: String,
    virtual_id: String,
    persist_members: bool,
) -> Result<MediaItem, StatusCode> {
    let mut members = Vec::new();
    let mut template = None;
    let mut total_child_count = 0;

    let primary_tag = group
        .first()
        .and_then(|(item, _)| item.image_tags.as_ref()?.get("Primary").cloned());

    for (item, server) in &group {
        total_child_count += item.child_count.unwrap_or(0);
        let processed = process_media_item_for_server(item.clone(), state, server, false).await?;
        members.push((server.url.to_string(), processed.id.clone()));
        if template.is_none() {
            template = Some(processed);
        }
    }

    if persist_members {
        if let Err(e) = state
            .merged_library_service
            .upsert_members(&virtual_id, &members)
            .await
        {
            error!("Failed to upsert merged library members: {}", e);
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
    Ok(item)
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
    attach_library_folder_image_source(
        &mut processed,
        &image_source_id,
        primary_tag.as_deref(),
    );
    Ok(processed)
}

fn attach_library_folder_image_source(
    item: &mut MediaItem,
    image_source_id: &str,
    primary_tag: Option<&str>,
) {
    let Some(primary_tag) = primary_tag
        .map(str::to_string)
        .or_else(|| {
            item.image_tags
                .as_ref()
                .and_then(|tags| tags.get("Primary").cloned())
        })
    else {
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

fn is_playback_catalog_request(url: &url::Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.contains("/nextup") || path.contains("/resume")
}

fn interleave_server_items(server_items: Vec<(ItemsResponseVariants, Server)>) -> Vec<MediaItem> {
    interleave_items(
        server_items
            .into_iter()
            .map(|(items, _)| items)
            .collect(),
    )
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

fn items_response_to_json(
    response: ItemsResponseVariants,
) -> Result<Json<serde_json::Value>, StatusCode> {
    serde_json::to_value(response).map(Json).map_err(|e| {
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

fn pagination_from_url(url: &url::Url) -> Pagination {
    let mut pagination = Pagination {
        start_index: 0,
        limit: None,
    };

    for (key, value) in url.query_pairs() {
        if key.eq_ignore_ascii_case("StartIndex") {
            if let Ok(start_index) = value.parse() {
                pagination.start_index = start_index;
            }
        } else if key.eq_ignore_ascii_case("Limit") {
            if let Ok(limit) = value.parse() {
                pagination.limit = Some(limit);
            }
        }
    }

    pagination
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

fn interleave_items(server_items: Vec<ItemsResponseVariants>) -> Vec<MediaItem> {
    let mut queues = server_items
        .into_iter()
        .map(|items| VecDeque::from(items.into_items()))
        .collect::<Vec<_>>();
    let mut interleaved_items = Vec::new();
    let mut has_live_tv_user_view = false;

    while queues.iter().any(|items| !items.is_empty()) {
        for queue in &mut queues {
            let Some(item) = queue.pop_front() else {
                continue;
            };

            if is_live_tv_user_view(&item) {
                if has_live_tv_user_view {
                    continue;
                }
                has_live_tv_user_view = true;
            }

            interleaved_items.push(item);
        }
    }

    interleaved_items
}

fn is_live_tv_user_view(item: &MediaItem) -> bool {
    item.collection_type == Some(CollectionType::LiveTv) && item.item_type == BaseItemKind::UserView
}

fn apply_pagination(items: Vec<MediaItem>, pagination: Pagination) -> (Vec<MediaItem>, usize) {
    let total_count = items.len();
    if pagination.start_index >= total_count {
        return (Vec::new(), total_count);
    }

    let items = items.into_iter().skip(pagination.start_index);
    let items = if let Some(limit) = pagination.limit {
        items.take(limit).collect()
    } else {
        items.collect()
    };

    (items, total_count)
}

fn to_i32(value: usize) -> i32 {
    value.min(i32::MAX as usize) as i32
}

async fn resolve_duplicate_policy(
    state: &AppState,
    parent_id: Option<&str>,
) -> DuplicatePolicyConfig {
    if let Some(parent_id) = parent_id {
        let parent_id = normalize_library_id(parent_id);
        if let Ok(Some(group)) = state.library_group_service.get_group(&parent_id).await {
            return DuplicatePolicyConfig {
                policy: group.duplicate_policy,
                preferred_server_id: group.preferred_server_id,
            };
        }
    }

    DuplicatePolicyConfig {
        policy: DuplicatePolicy::ServerPriority,
        preferred_server_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn pagination_from_url_defaults_when_absent() {
        let url = url::Url::parse("http://localhost/Items?Recursive=true").unwrap();

        assert_eq!(
            pagination_from_url(&url),
            Pagination {
                start_index: 0,
                limit: None,
            }
        );
    }

    #[test]
    fn pagination_from_url_is_case_insensitive() {
        let url = url::Url::parse("http://localhost/Items?startindex=12&limit=24").unwrap();

        assert_eq!(
            pagination_from_url(&url),
            Pagination {
                start_index: 12,
                limit: Some(24),
            }
        );
    }

    #[test]
    fn pagination_from_url_ignores_invalid_numbers() {
        let url = url::Url::parse("http://localhost/Items?StartIndex=nope&Limit=nope").unwrap();

        assert_eq!(
            pagination_from_url(&url),
            Pagination {
                start_index: 0,
                limit: None,
            }
        );
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
    fn apply_pagination_slices_after_interleave() {
        let items = vec![media_item("one", None), media_item("two", None)];
        let pagination = Pagination {
            start_index: 1,
            limit: Some(1),
        };

        let (items, total_count) = apply_pagination(items, pagination);

        assert_eq!(total_count, 2);
        assert_eq!(items[0].id, "two");
    }

    #[test]
    fn apply_pagination_returns_rest_when_limit_absent() {
        let items = vec![
            media_item("one", None),
            media_item("two", None),
            media_item("three", None),
        ];
        let pagination = Pagination {
            start_index: 1,
            limit: None,
        };

        let (items, total_count) = apply_pagination(items, pagination);
        let ids = item_ids(&items);

        assert_eq!(total_count, 3);
        assert_eq!(ids, vec!["two", "three"]);
    }

    #[test]
    fn apply_pagination_returns_empty_when_start_is_out_of_range() {
        let items = vec![media_item("one", None), media_item("two", None)];
        let pagination = Pagination {
            start_index: 2,
            limit: Some(10),
        };

        let (items, total_count) = apply_pagination(items, pagination);

        assert_eq!(total_count, 2);
        assert!(items.is_empty());
    }

    #[test]
    fn apply_pagination_allows_zero_limit() {
        let items = vec![media_item("one", None), media_item("two", None)];
        let pagination = Pagination {
            start_index: 0,
            limit: Some(0),
        };

        let (items, total_count) = apply_pagination(items, pagination);

        assert_eq!(total_count, 2);
        assert!(items.is_empty());
    }

    #[test]
    fn interleave_items_round_robins_uneven_server_lists() {
        let server_items = vec![
            ItemsResponseVariants::Bare(vec![media_item("a1", None), media_item("a2", None)]),
            ItemsResponseVariants::Bare(vec![
                media_item("b1", None),
                media_item("b2", None),
                media_item("b3", None),
            ]),
            ItemsResponseVariants::Bare(vec![media_item("c1", None)]),
        ];

        let items = interleave_items(server_items);

        assert_eq!(item_ids(&items), vec!["a1", "b1", "c1", "a2", "b2", "b3"]);
    }

    #[test]
    fn interleave_items_accepts_wrapped_responses() {
        let server_items = vec![
            ItemsResponseVariants::WithCount(ItemsResponseWithCount {
                items: vec![media_item("wrapped", None)],
                total_record_count: 1,
                start_index: 0,
            }),
            ItemsResponseVariants::Bare(vec![media_item("bare", None)]),
        ];

        let items = interleave_items(server_items);

        assert_eq!(item_ids(&items), vec!["wrapped", "bare"]);
    }

    #[test]
    fn interleave_items_keeps_only_one_live_tv_user_view() {
        let server_items = vec![
            ItemsResponseVariants::Bare(vec![
                media_item("live-one", Some("livetv")),
                media_item("movie-one", None),
            ]),
            ItemsResponseVariants::Bare(vec![
                media_item("live-two", Some("livetv")),
                media_item("movie-two", None),
            ]),
        ];

        let items = interleave_items(server_items);
        let ids = items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["live-one", "movie-one", "movie-two"]);
    }

    #[test]
    fn interleave_items_does_not_dedupe_non_user_view_live_tv_items() {
        let server_items = vec![
            ItemsResponseVariants::Bare(vec![typed_media_item(
                "channel-one",
                "LiveTvChannel",
                Some("livetv"),
            )]),
            ItemsResponseVariants::Bare(vec![typed_media_item(
                "channel-two",
                "LiveTvChannel",
                Some("livetv"),
            )]),
        ];

        let items = interleave_items(server_items);

        assert_eq!(item_ids(&items), vec!["channel-one", "channel-two"]);
    }

    #[test]
    fn interleave_then_paginate_matches_global_order() {
        let server_items = vec![
            ItemsResponseVariants::Bare(vec![media_item("a1", None), media_item("a2", None)]),
            ItemsResponseVariants::Bare(vec![media_item("b1", None), media_item("b2", None)]),
        ];
        let pagination = Pagination {
            start_index: 1,
            limit: Some(2),
        };

        let (items, total_count) = apply_pagination(interleave_items(server_items), pagination);

        assert_eq!(total_count, 4);
        assert_eq!(item_ids(&items), vec!["b1", "a2"]);
    }

    #[test]
    fn items_response_from_shape_keeps_bare_responses_bare() {
        let response = items_response_from_shape(
            vec![media_item("one", None)],
            1,
            Pagination {
                start_index: 0,
                limit: None,
            },
            false,
        );

        assert!(matches!(response, ItemsResponseVariants::Bare(_)));
    }

    #[test]
    fn items_response_from_shape_keeps_counted_responses_counted() {
        let response = items_response_from_shape(
            vec![media_item("one", None)],
            12,
            Pagination {
                start_index: 4,
                limit: Some(1),
            },
            true,
        );

        let ItemsResponseVariants::WithCount(response) = response else {
            panic!("expected counted response");
        };
        assert_eq!(response.total_record_count, 12);
        assert_eq!(response.start_index, 4);
    }

    fn query_pairs(url: &url::Url) -> Vec<(String, String)> {
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    fn item_ids(items: &[MediaItem]) -> Vec<&str> {
        items.iter().map(|item| item.id.as_str()).collect()
    }

    fn media_item(id: &str, collection_type: Option<&str>) -> MediaItem {
        typed_media_item(id, "UserView", collection_type)
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
}
