use axum::{
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use regex::Regex;
use std::sync::LazyLock;
use tokio::task::JoinSet;
use tracing::{debug, error, trace};

use crate::{
    handlers::{
        common::{execute_json_request, process_media_item},
        items::get_items,
    },
    models::enums::{BaseItemKind, CollectionType},
    request_preprocessing::{apply_to_request, extract_request_infos, JellyfinAuthorization},
    AppState,
};

static SERIES_OR_PARENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("(?i)(seriesid|parentid)").unwrap());

pub async fn get_items_from_all_servers_if_not_restricted(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    // Extract request information and sessions

    if let Some(query) = req.uri().query() {
        // Check if the request is for a specific series or folder
        if SERIES_OR_PARENT_RE.is_match(query) {
            if let Ok(parsed) = serde_urlencoded::from_str::<std::collections::HashMap<String, String>>(query) {
                if let Some(parent_id) = parsed
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("ParentId"))
                    .map(|(_, value)| value.clone())
                {
                    if state
                        .library_sync
                        .unified_library_by_virtual_id(&parent_id)
                        .await
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                        .is_some()
                    {
                        let mut query = crate::library_sync_service::UnifiedBrowseQuery::default();
                        query.parent_id = Some(parent_id);
                        if let Some(start_index) = parsed.get("StartIndex").and_then(|v| v.parse().ok()) {
                            query.start_index = Some(start_index);
                        }
                        if let Some(limit) = parsed.get("Limit").and_then(|v| v.parse().ok()) {
                            query.limit = Some(limit);
                        }
                        query.search_term = parsed.get("SearchTerm").cloned();
                        query.include_item_types = parsed.get("IncludeItemTypes").cloned();
                        query.sort_by = parsed.get("SortBy").cloned();
                        query.sort_order = parsed.get("SortOrder").cloned();
                        query.recursive = parsed
                            .get("Recursive")
                            .and_then(|v| v.parse::<bool>().ok());
                        query.user_id = parsed.get("UserId").cloned();

                        let library = state
                            .library_sync
                            .unified_library_by_virtual_id(query.parent_id.as_deref().unwrap_or_default())
                            .await
                            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                            .ok_or(StatusCode::NOT_FOUND)?;
                        let items = state
                            .library_sync
                            .browse_unified_library(&library, &query)
                            .await
                            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                        return Ok(Json(crate::models::ItemsResponseVariants::WithCount(items)));
                    }
                }
            }
            return get_items(State(state), req).await;
        }
    }

    get_items_from_all_servers(State(state), req).await
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

    // Create JoinSet for parallel execution
    let mut join_set = JoinSet::new();

    for (index, (session, server)) in sessions.into_iter().enumerate() {
        let request = match original_request.try_clone() {
            Some(req) => req,
            None => {
                error!("Failed to clone request for server: {}", server.name);
                continue;
            }
        };

        let auth = JellyfinAuthorization::Authorization(session.to_authorization());
        let mut request = request;
        let state_clone = state.clone();
        let server_clone = server.clone();
        let session_clone = session.clone();

        // Spawn task in JoinSet with server index
        join_set.spawn(async move {
            apply_to_request(
                &mut request,
                &server_clone,
                &Some(session_clone),
                &Some(auth),
                &state_clone,
            )
            .await;

            let result = match execute_json_request::<crate::models::ItemsResponseVariants>(
                &state_clone.reqwest_client,
                request,
            )
            .await
            {
                Ok(mut items_response) => {
                    let server_id = { state_clone.config.read().await.server_id.clone() };
                    for item in items_response.iter_mut_items() {
                        match process_media_item(
                            item.clone(),
                            &state_clone,
                            &server_clone,
                            true, // Change name to include server name
                            &server_id,
                        )
                        .await
                        {
                            Ok(processed_item) => *item = processed_item,
                            Err(e) => {
                                error!(
                                    "Failed to process media item from server '{}': {:?}",
                                    server_clone.name, e
                                );
                                return (index, None);
                            }
                        }
                    }

                    let item_count = items_response.len();
                    debug!(
                        "Successfully retrieved {} items from server: {}",
                        item_count, server_clone.name
                    );
                    trace!(
                        "Items from server '{}': {}",
                        server_clone.name,
                        serde_json::to_string(&items_response).unwrap_or_default()
                    );
                    Some(items_response)
                }
                Err(e) => {
                    error!(
                        "Failed to get items from server '{}': {:?}",
                        server_clone.name, e
                    );
                    None
                }
            };

            (index, result)
        });
    }

    // Wait for all tasks to complete and collect results with their original indices
    let mut indexed_results: Vec<(usize, Option<crate::models::ItemsResponseVariants>)> =
        Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((index, items)) => indexed_results.push((index, items)),
            Err(e) => error!("Task failed: {:?}", e),
        }
    }

    // Sort results by original server order
    indexed_results.sort_by_key(|(index, _)| *index);

    // Extract items in original server order
    let mut server_items: Vec<crate::models::ItemsResponseVariants> = Vec::new();
    for (_, items) in indexed_results {
        if let Some(items) = items {
            server_items.push(items);
        }
    }

    // Interleave items from all servers with Live TV filtering
    let mut interleaved_items = Vec::new();
    let mut live_tv_count = 0;
    let max_items = server_items
        .iter()
        .map(|items| items.len())
        .max()
        .unwrap_or(0);

    for i in 0..max_items {
        for server_item_list in &server_items {
            if let Some(item) = server_item_list.get(i) {
                // Skip additional Live TV items
                if let Some(collectiontype) = &item.collection_type {
                    if *collectiontype == CollectionType::LiveTv
                        && item.item_type == BaseItemKind::UserView
                    {
                        live_tv_count += 1;
                        if live_tv_count > 1 {
                            continue;
                        }
                    }
                }
                interleaved_items.push(item.clone());
            }
        }
    }

    if original_request.url().path().ends_with("/Views") || original_request.url().path().eq_ignore_ascii_case("/UserViews") {
        let user_id = original_request
            .url()
            .query_pairs()
            .find(|(k, _)| k.eq_ignore_ascii_case("UserId"))
            .map(|(_, v)| v.to_string());
        if let Ok(mut unified_views) = state.library_sync.unified_views(user_id.as_deref()).await {
            interleaved_items.append(&mut unified_views);
        }
    }

    let count = interleaved_items.len();
    debug!(
        "Returning {} interleaved items from {} servers",
        count,
        server_items.len()
    );

    trace!(
        "Items: {}",
        serde_json::to_string(&interleaved_items).unwrap_or_default()
    );

    if server_items
        .iter()
        .any(|items| matches!(items, crate::models::ItemsResponseVariants::WithCount(_)))
    {
        Ok(Json(crate::models::ItemsResponseVariants::WithCount(
            crate::models::ItemsResponseWithCount {
                items: interleaved_items,
                total_record_count: count as i32,
                start_index: 0,
            },
        )))
    } else {
        Ok(Json(crate::models::ItemsResponseVariants::Bare(
            interleaved_items,
        )))
    }
}
