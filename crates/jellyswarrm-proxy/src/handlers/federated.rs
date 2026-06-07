use axum::{extract::State, Json};
use hyper::StatusCode;
use std::collections::{HashMap, VecDeque};
use tokio::task::JoinSet;
use tracing::{debug, error, trace, warn};

use crate::{
    extractors::Preprocessed,
    handlers::{
        common::{execute_json_request, response_json_to_payload},
        items::get_items,
    },
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

    if state.merge_libraries_enabled().await {
        if let Some(parent_id) = extract_parent_id(original_request.url()) {
            match state.merged_library_service.resolve(&parent_id).await {
                Ok(Some((lib, members))) if !members.is_empty() => {
                    debug!(
                        "ParentId {} is merged library '{}' — fanning out to {} servers",
                        parent_id,
                        lib.collection_type,
                        members.len()
                    );
                    return get_items_for_merged_library(&state, preprocessed, members).await;
                }
                Ok(_) => {}
                Err(e) => {
                    error!("Failed to resolve merged library for {}: {}", parent_id, e);
                }
            }
        }
    }

    if has_query_key(original_request.url(), &["SeriesId", "ParentId"]) {
        return get_items(State(state), Preprocessed(preprocessed)).await;
    }

    get_items_from_all_servers_preprocessed(&state, preprocessed).await
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
    let mut join_set = JoinSet::new();
    let mut failures = 0;

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

        let mut request = match original_request.try_clone() {
            Some(request) => request,
            None => {
                error!("Failed to clone request for merged library fan-out");
                failures += 1;
                continue;
            }
        };

        *request.url_mut() = replace_parent_id(request.url(), &mapping.original_media_id);

        let state_clone = state.clone();
        join_set.spawn(async move {
            let result = fetch_items_from_server(
                index,
                state_clone,
                request,
                session,
                server,
                pagination,
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
    let mut all_items = indexed_results
        .into_iter()
        .flat_map(|(_, response)| response.into_items())
        .collect::<Vec<_>>();
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
    if state.merge_libraries_enabled().await {
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
    let mut non_lib_per_server: Vec<ItemsResponseVariants> = Vec::new();
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
                non_lib_per_server.push(ItemsResponseVariants::Bare(processed));
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

        let mut members = Vec::new();
        let mut template = None;
        let mut total_child_count = 0;

        for (item, server) in &group {
            total_child_count += item.child_count.unwrap_or(0);
            let processed =
                process_media_item_for_server(item.clone(), state, server, false).await?;
            members.push((server.url.to_string(), processed.id.clone()));
            if template.is_none() {
                template = Some(processed);
            }
        }

        if let Err(e) = state
            .merged_library_service
            .upsert_members(&merged.virtual_id, &members)
            .await
        {
            error!("Failed to upsert merged library members: {}", e);
        }

        if let Some(mut item) = template {
            item.id = merged.virtual_id.clone();
            item.name = Some(display_name);
            item.child_count = Some(total_child_count);
            library_items.push(item);
        }
    }

    library_items.sort_by(|a, b| {
        let left = a.name.as_deref().unwrap_or("");
        let right = b.name.as_deref().unwrap_or("");
        left.cmp(right)
    });

    let mut final_items = library_items;
    final_items.extend(interleave_items(non_lib_per_server));

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
}
