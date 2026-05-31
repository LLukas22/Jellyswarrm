use axum::{
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use std::collections::VecDeque;
use tokio::task::JoinSet;
use tracing::{debug, error, trace, warn};

use crate::{
    handlers::{
        common::{execute_json_request, process_items_response, process_media_item},
        items::get_items,
    },
    merged_library_service::MergedLibraryMember,
    models::{
        enums::{BaseItemKind, CollectionType},
        ItemsResponseVariants, ItemsResponseWithCount, MediaItem,
    },
    request_preprocessing::{apply_to_request, extract_request_infos, JellyfinAuthorization},
    server_storage::Server,
    user_authorization_service::AuthorizationSession,
    AppState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pagination {
    start_index: usize,
    limit: Option<usize>,
}

fn extract_parent_id(query: &str) -> Option<String> {
    url::Url::parse(&format!("http://x?{}", query))
        .ok()?
        .query_pairs()
        .find(|(k, _)| k.eq_ignore_ascii_case("parentid"))
        .map(|(_, v)| v.into_owned())
}

fn replace_parent_id(url: &url::Url, new_id: &str) -> url::Url {
    let new_query: String = url
        .query_pairs()
        .map(|(k, v)| {
            let val = if k.eq_ignore_ascii_case("parentid") {
                new_id.to_string()
            } else {
                v.into_owned()
            };
            format!(
                "{}={}",
                k,
                percent_encoding::utf8_percent_encode(&val, percent_encoding::NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&");

    let mut new_url = url.clone();
    new_url.set_query(if new_query.is_empty() {
        None
    } else {
        Some(&new_query)
    });
    new_url
}

pub async fn get_items_from_all_servers_if_not_restricted(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    if let Some(query) = req.uri().query() {
        if state.merge_libraries_enabled().await {
            if let Some(parent_id) = extract_parent_id(query) {
                match state.merged_library_service.resolve(&parent_id).await {
                    Ok(Some((lib, members))) if !members.is_empty() => {
                        debug!(
                            "ParentId {} is merged library '{}' — fanning out to {} servers",
                            parent_id,
                            lib.collection_type,
                            members.len()
                        );
                        return get_items_for_merged_library(State(state), req, members).await;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        error!("Failed to resolve merged library for {}: {}", parent_id, e);
                    }
                }
            }
        }
    }

    if has_query_key(req.uri(), &["SeriesId", "ParentId"]) {
        return get_items(State(state), req).await;
    }

    get_items_from_all_servers(State(state), req).await
}

async fn get_items_for_merged_library(
    State(state): State<AppState>,
    req: Request,
    members: Vec<MergedLibraryMember>,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    let (original_request, _, _, sessions, _) =
        extract_request_infos(req, &state).await.map_err(|e| {
            error!("Failed to preprocess merged-library request: {}", e);
            StatusCode::BAD_REQUEST
        })?;

    let sessions = sessions.ok_or(StatusCode::UNAUTHORIZED)?;
    if sessions.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let pagination = pagination_from_url(original_request.url());
    let server_id = { state.config.read().await.server_id.clone() };
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
            Some(v) => v,
            None => {
                error!(
                    "No media mapping found for virtual library {}",
                    member.virtual_library_id
                );
                continue;
            }
        };

        let session = sessions
            .iter()
            .find(|(_, session_server)| session_server.id == server.id)
            .map(|(sess, _)| sess.clone());

        let session = match session {
            Some(s) => s,
            None => {
                error!("No active session for server '{}' — skipping", server.name);
                continue;
            }
        };

        let mut request = match original_request.try_clone() {
            Some(r) => r,
            None => {
                error!("Failed to clone request for merged library fan-out");
                continue;
            }
        };

        let new_url = replace_parent_id(request.url(), &mapping.original_media_id);
        *request.url_mut() = new_url;

        let state_clone = state.clone();
        let server_id = server_id.clone();

        join_set.spawn(async move {
            let result = fetch_items_from_server(
                state_clone,
                request,
                session,
                server,
                server_id,
                pagination,
                false,
            )
            .await;
            (index, result)
        });
    }

    let mut indexed: Vec<(usize, ItemsResponseVariants)> = Vec::new();
    while let Some(res) = join_set.join_next().await {
        match res {
            Ok((index, Ok(items))) => indexed.push((index, items)),
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

    if indexed.is_empty() {
        error!("All merged library fan-out requests failed");
        return Err(StatusCode::BAD_GATEWAY);
    }

    if failures > 0 {
        warn!(
            "Returning partial merged library response after {} server failure(s)",
            failures
        );
    }

    indexed.sort_by_key(|(i, _)| *i);

    let server_responses: Vec<crate::models::ItemsResponseVariants> =
        indexed.into_iter().map(|(_, v)| v).collect();
    let mut all_items: Vec<crate::models::MediaItem> = server_responses
        .into_iter()
        .flat_map(|r| r.into_items())
        .collect();
    all_items.sort_by(|a, b| {
        let ak = a.sort_name.as_deref().or(a.name.as_deref()).unwrap_or("");
        let bk = b.sort_name.as_deref().or(b.name.as_deref()).unwrap_or("");
        ak.cmp(bk)
    });
    let (paged, total) = apply_pagination(all_items, pagination);
    Ok(Json(crate::models::ItemsResponseVariants::WithCount(
        ItemsResponseWithCount {
            items: paged,
            total_record_count: to_i32(total),
            start_index: to_i32(pagination.start_index),
        },
    )))
}

pub async fn get_items_from_all_servers(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    let (original_request, _, _, sessions, _) =
        extract_request_infos(req, &state).await.map_err(|e| {
            error!("Failed to preprocess request: {}", e);
            StatusCode::BAD_REQUEST
        })?;

    let sessions = sessions.ok_or(StatusCode::UNAUTHORIZED)?;
    if sessions.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let pagination = pagination_from_url(original_request.url());
    let cfg = state.config.read().await.clone();
    let server_id = cfg.server_id.clone();
    let merge_libraries = cfg.merge_libraries;
    drop(cfg);

    let mut join_set = JoinSet::new();
    let mut failures = 0;

    for (index, (session, server)) in sessions.into_iter().enumerate() {
        let mut request = match original_request.try_clone() {
            Some(r) => r,
            None => {
                error!("Failed to clone request for server: {}", server.name);
                failures += 1;
                continue;
            }
        };

        let auth = JellyfinAuthorization::Authorization(session.to_authorization());
        let state_clone = state.clone();

        join_set.spawn(async move {
            normalize_upstream_pagination(request.url_mut(), pagination);
            apply_to_request(
                &mut request,
                &server,
                &Some(session),
                &Some(auth),
                &state_clone,
            )
            .await;

            match execute_json_request::<crate::models::ItemsResponseVariants>(
                &state_clone.reqwest_client,
                request,
            )
            .await
            {
                Ok(resp) => {
                    debug!("Fetched {} raw items from '{}'", resp.len(), server.name);
                    trace!(
                        "Raw items from '{}': {}",
                        server.name,
                        serde_json::to_string(&resp).unwrap_or_default()
                    );
                    (index, Ok((resp, server)))
                }
                Err(e) => {
                    error!("Failed to fetch items from '{}': {:?}", server.name, e);
                    (index, Err(e))
                }
            }
        });
    }

    let mut indexed_raw: Vec<(usize, (ItemsResponseVariants, Server))> = Vec::new();
    while let Some(res) = join_set.join_next().await {
        match res {
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

    indexed_raw.sort_by_key(|(i, _)| *i);

    let server_raw: Vec<(
        crate::models::ItemsResponseVariants,
        crate::server_storage::Server,
    )> = indexed_raw.into_iter().map(|(_, v)| v).collect();

    let mut library_groups: std::collections::HashMap<
        String,
        Vec<(MediaItem, crate::server_storage::Server)>,
    > = std::collections::HashMap::new();
    let mut non_lib_per_server: Vec<crate::models::ItemsResponseVariants> = Vec::new();

    // Deduplicate LiveTv across servers — keep at most one entry.
    let mut live_tv_seen = false;
    for (raw_response, server) in server_raw {
        let mut non_lib: Vec<MediaItem> = Vec::new();

        for item in raw_response.into_items() {
            let is_mergeable = merge_libraries
                && matches!(
                    item.item_type,
                    BaseItemKind::UserView | BaseItemKind::CollectionFolder
                )
                && item
                    .collection_type
                    .as_ref()
                    .map(|ct| *ct != CollectionType::LiveTv)
                    .unwrap_or(false);

            if is_mergeable {
                // Group by (collection_type, normalized_name) so "Movies" on server A only
                // merges with "Movies" on server B, not an unrelated "Spectacles" folder.
                let ct_str = serde_json::to_string(item.collection_type.as_ref().unwrap())
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string();
                let name_lower = item.name.as_deref().unwrap_or("").to_lowercase();
                let ct_key = format!("{}:{}", ct_str, name_lower);
                library_groups
                    .entry(ct_key)
                    .or_default()
                    .push((item, server.clone()));
            } else {
                if let Some(ct) = &item.collection_type {
                    if *ct == CollectionType::LiveTv && item.item_type == BaseItemKind::UserView {
                        if live_tv_seen {
                            continue;
                        }
                        live_tv_seen = true;
                    }
                }
                non_lib.push(item);
            }
        }

        if !non_lib.is_empty() {
            let mut processed: Vec<MediaItem> = Vec::new();
            for item in non_lib {
                match process_media_item(item, &state, &server, true, &server_id, None).await {
                    Ok(p) => processed.push(p),
                    Err(e) => error!(
                        "Failed to process non-library item from '{}': {:?}",
                        server.name, e
                    ),
                }
            }
            if !processed.is_empty() {
                non_lib_per_server.push(crate::models::ItemsResponseVariants::Bare(processed));
            }
        }
    }

    let mut library_items: Vec<MediaItem> = Vec::new();

    for (ct_key, group) in library_groups {
        if group.len() == 1 {
            if let Some((item, server)) = group.into_iter().next() {
                match process_media_item(item, &state, &server, true, &server_id, None).await {
                    Ok(p) => library_items.push(p),
                    Err(e) => error!("Failed to process single-server library: {:?}", e),
                }
            }
        } else {
            let display_name = group[0].0.name.clone().unwrap_or_else(|| {
                ct_key
                    .split_once(':')
                    .map(|x| x.1.to_string())
                    .unwrap_or_else(|| ct_key.clone())
            });

            let merged = match state
                .merged_library_service
                .get_or_create(&ct_key, &display_name)
                .await
            {
                Ok(m) => m,
                Err(e) => {
                    error!(
                        "Failed to get/create merged library for '{}': {}",
                        ct_key, e
                    );
                    for (item, server) in group {
                        if let Ok(p) =
                            process_media_item(item, &state, &server, true, &server_id, None).await
                        {
                            library_items.push(p);
                        }
                    }
                    continue;
                }
            };

            let mut members: Vec<(String, String)> = Vec::new();
            let mut template: Option<MediaItem> = None;
            let mut total_child_count: i32 = 0;

            for (item, server) in &group {
                total_child_count += item.child_count.unwrap_or(0);
                match process_media_item(
                    item.clone(),
                    &state,
                    server,
                    false, // no "[Server]" suffix — merged folder has a clean name
                    &server_id,
                    None,
                )
                .await
                {
                    Ok(processed) => {
                        members.push((server.url.to_string(), processed.id.clone()));
                        if template.is_none() {
                            template = Some(processed);
                        }
                    }
                    Err(e) => error!(
                        "Failed to process library item for '{}': {:?}",
                        server.name, e
                    ),
                }
            }

            if let Err(e) = state
                .merged_library_service
                .upsert_members(&merged.virtual_id, &members)
                .await
            {
                error!("Failed to upsert merged library members: {}", e);
            }

            if let Some(mut tmpl) = template {
                tmpl.id = merged.virtual_id.clone();
                tmpl.name = Some(display_name);
                tmpl.child_count = Some(total_child_count);
                library_items.push(tmpl);
            }
        }
    }

    library_items.sort_by(|a, b| {
        let ak = a.name.as_deref().unwrap_or("");
        let bk = b.name.as_deref().unwrap_or("");
        ak.cmp(bk)
    });
    let mut final_items = library_items;
    final_items.extend(interleave_items(non_lib_per_server));

    let (paged_items, total_count) = apply_pagination(final_items, pagination);
    debug!(
        "Returning {} of {} federated items",
        paged_items.len(),
        total_count
    );

    Ok(Json(crate::models::ItemsResponseVariants::WithCount(
        ItemsResponseWithCount {
            items: paged_items,
            total_record_count: to_i32(total_count),
            start_index: to_i32(pagination.start_index),
        },
    )))
}

async fn fetch_items_from_server(
    state: AppState,
    mut request: reqwest::Request,
    session: AuthorizationSession,
    server: Server,
    server_id: String,
    pagination: Pagination,
    should_change_name: bool,
) -> Result<ItemsResponseVariants, StatusCode> {
    normalize_upstream_pagination(request.url_mut(), pagination);

    let auth = JellyfinAuthorization::Authorization(session.to_authorization());
    apply_to_request(&mut request, &server, &Some(session), &Some(auth), &state).await;

    let mut items_response =
        execute_json_request::<ItemsResponseVariants>(&state.reqwest_client, request)
            .await
            .inspect_err(|e| {
                error!("Failed to get items from server '{}': {:?}", server.name, e);
            })?;

    process_items_response(
        &mut items_response,
        &state,
        &server,
        should_change_name,
        &server_id,
        None,
    )
    .await
    .inspect_err(|e| {
        error!(
            "Failed to process media items from server '{}': {:?}",
            server.name, e
        );
    })?;

    debug!(
        "Successfully retrieved {} items from server: {}",
        items_response.len(),
        server.name
    );
    trace!(
        "Items from server '{}': {}",
        server.name,
        serde_json::to_string(&items_response).unwrap_or_default()
    );

    Ok(items_response)
}

fn has_query_key(uri: &axum::http::Uri, keys: &[&str]) -> bool {
    uri.query()
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
        let uri = "/Items?foo=ParentId".parse().unwrap();
        assert!(!has_query_key(&uri, &["ParentId"]));

        let uri = "/Items?parentid=abc".parse().unwrap();
        assert!(has_query_key(&uri, &["ParentId"]));
    }

    #[test]
    fn has_query_key_decodes_encoded_query_keys() {
        let uri = "/Items?Parent%49d=abc".parse().unwrap();

        assert!(has_query_key(&uri, &["ParentId"]));
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
