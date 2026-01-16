use axum::{
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use regex::Regex;
use std::collections::BTreeMap;
use std::sync::LazyLock;
use tokio::task::JoinSet;
use tracing::{debug, error, info, trace, warn};

use crate::{
    handlers::{
        common::{execute_json_request, extract_all_ids_from_items, process_media_item},
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
            return get_items(State(state), req).await;
        }
    }

    get_items_from_all_servers(State(state), req).await
}

pub async fn get_items_from_all_servers(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    let uri = req.uri().clone();
    info!("[FEDERATED] Fetching items from all servers for: {}", uri);

    let (original_request, _, user, sessions, _) =
        extract_request_infos(req, &state).await.map_err(|e| {
            error!("[FEDERATED] Failed to preprocess request: {}", e);
            StatusCode::BAD_REQUEST
        })?;

    if let Some(ref u) = user {
        info!("[FEDERATED] User: {} ({})", u.original_username, u.id);
    }

    let sessions = sessions.ok_or_else(|| {
        warn!("[FEDERATED] No sessions found - returning 401");
        StatusCode::UNAUTHORIZED
    })?;

    if sessions.is_empty() {
        warn!("[FEDERATED] Sessions list is empty - returning 401");
        return Err(StatusCode::UNAUTHORIZED);
    }

    info!("[FEDERATED] Querying {} backend server(s) in parallel", sessions.len());

    // Create JoinSet for parallel execution
    let mut join_set = JoinSet::new();

    for (index, (session, server)) in sessions.into_iter().enumerate() {
        info!("[FEDERATED] Adding server to query: {} ({})", server.name, server.url);
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
                    let server_url = server_clone.url.as_str();

                    // Pre-warm the media mapping cache with all IDs from items
                    // This reduces DB queries from O(n*m) to O(1) batch query
                    let items_slice = items_response.items_slice();
                    let all_ids = extract_all_ids_from_items(items_slice);
                    if !all_ids.is_empty() {
                        if let Err(e) = state_clone
                            .media_storage
                            .prewarm_cache_for_ids(&all_ids, server_url)
                            .await
                        {
                            debug!(
                                "Failed to prewarm cache for server '{}': {:?}",
                                server_clone.name, e
                            );
                            // Continue anyway - individual lookups will still work
                        }
                    }

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
                    info!(
                        "[FEDERATED] Server '{}' returned {} items",
                        server_clone.name, item_count
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

    // Use BTreeMap for automatic ordering (avoids manual sorting)
    let mut indexed_results: BTreeMap<usize, crate::models::ItemsResponseVariants> =
        BTreeMap::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((index, Some(items))) => {
                indexed_results.insert(index, items);
            }
            Ok((_, None)) => {} // Skip failed requests
            Err(e) => error!("Task failed: {:?}", e),
        }
    }

    // Extract items in original server order (already sorted by BTreeMap)
    let server_items: Vec<crate::models::ItemsResponseVariants> =
        indexed_results.into_values().collect();

    // Interleave items from all servers with Live TV filtering
    // Pre-allocate capacity to avoid reallocations
    let estimated_capacity = server_items.iter().map(|items| items.len()).sum::<usize>();
    let mut interleaved_items = Vec::with_capacity(estimated_capacity);
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
                let should_skip = if let Some(collectiontype) = &item.collection_type {
                    if *collectiontype == CollectionType::LiveTv
                        && item.item_type == BaseItemKind::UserView
                    {
                        live_tv_count += 1;
                        live_tv_count > 1
                    } else {
                        false
                    }
                } else {
                    false
                };

                if !should_skip {
                    interleaved_items.push(item.clone());
                }
            }
        }
    }

    let count = interleaved_items.len();
    info!(
        "[FEDERATED] Returning {} total items (merged from {} server responses)",
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
