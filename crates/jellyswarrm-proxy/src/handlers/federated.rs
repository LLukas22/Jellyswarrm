use axum::{
    extract::{Path, Query, Request, State},
    response::{IntoResponse, Response},
    Json,
};
use hyper::StatusCode;
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;
use tokio::task::JoinSet;
use tracing::{debug, error, trace};

use crate::{
    handlers::{
        common::{execute_json_request, process_media_item},
        items::get_items,
    },
    models::{
        enums::{BaseItemKind, CollectionType},
        ItemsResponseVariants, ItemsResponseWithCount, MediaItem,
    },
    request_preprocessing::{apply_to_request, extract_request_infos, JellyfinAuthorization},
    unified_library_service::UnifiedLibraryAggregation,
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

pub async fn get_views_with_unified(
    State(state): State<AppState>,
    req: Request,
) -> Result<impl IntoResponse, StatusCode> {
    let covered_types = state
        .unified_library
        .get_covered_collection_types()
        .await
        .map_err(|e| {
            error!("Failed to get covered collection types: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let Json(federated) = get_items_from_all_servers(State(state.clone()), req).await?;

    if covered_types.is_empty() {
        return Ok(Json(federated));
    }

    let (mut items, was_with_count) = match federated {
        ItemsResponseVariants::WithCount(w) => (w.items, true),
        ItemsResponseVariants::Bare(v) => (v, false),
    };

    // Remove real library folders whose type is covered by a virtual group
    items.retain(|item| {
        if item.item_type == BaseItemKind::CollectionFolder {
            if let Some(ct) = &item.collection_type {
                return !covered_types.contains(ct);
            }
        }
        true
    });

    let stubs = state
        .unified_library
        .get_virtual_library_stubs()
        .await
        .map_err(|e| {
            error!("Failed to get virtual library stubs: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let proxy_server_id = state.config.read().await.server_id.clone();
    for stub in stubs {
        if let Ok(v) = serde_json::to_value(stub) {
            if let Ok(mut mi) = serde_json::from_value::<MediaItem>(v) {
                mi.server_id = Some(proxy_server_id.clone());
                items.push(mi);
            }
        }
    }

    let count = items.len() as i32;
    let response = if was_with_count {
        ItemsResponseVariants::WithCount(ItemsResponseWithCount {
            items,
            total_record_count: count,
            start_index: 0,
        })
    } else {
        ItemsResponseVariants::Bare(items)
    };

    Ok(Json(response))
}

pub async fn handle_items_with_virtual_library(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
    req: Request,
) -> Result<Response, StatusCode> {
    let parent_id = params.get("ParentId").cloned();
    let start_index = params
        .get("StartIndex")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    let limit = params.get("Limit").and_then(|s| s.parse::<usize>().ok());

    if let Some(pid) = parent_id {
        match state.unified_library.get_group_by_virtual_id(&pid).await {
            Ok(Some(group)) => {
                let (_, _, _, sessions, _) =
                    extract_request_infos(req, &state).await.map_err(|e| {
                        error!("Failed to extract request infos: {}", e);
                        StatusCode::BAD_REQUEST
                    })?;
                let sessions = sessions.ok_or(StatusCode::UNAUTHORIZED)?;
                if sessions.is_empty() {
                    return Err(StatusCode::UNAUTHORIZED);
                }
                let user_id = sessions[0].0.user_id.clone();

                let aggregation = state
                    .unified_library
                    .get_aggregated_items(&group, &user_id, start_index, limit)
                    .await
                    .unwrap_or_else(|_| UnifiedLibraryAggregation {
                        items: vec![],
                        total_count: 0,
                        offline_servers: vec![],
                        unmatched_count: 0,
                    });

                let proxy_server_id = state.config.read().await.server_id.clone();
                let media_items: Vec<MediaItem> = aggregation
                    .items
                    .into_iter()
                    .filter_map(|bi| {
                        serde_json::to_value(bi)
                            .ok()
                            .and_then(|v| serde_json::from_value(v).ok())
                    })
                    .map(|mut item: MediaItem| {
                        item.server_id = Some(proxy_server_id.clone());
                        item
                    })
                    .collect();

                let offline_servers = aggregation.offline_servers;
                let total_count = aggregation.total_count;

                let body = Json(ItemsResponseVariants::WithCount(ItemsResponseWithCount {
                    items: media_items,
                    total_record_count: total_count,
                    start_index: start_index as i32,
                }));

                let mut response = body.into_response();
                if !offline_servers.is_empty() {
                    if let Ok(val) = offline_servers.join(",").parse() {
                        response
                            .headers_mut()
                            .insert("X-Jellyswarrm-Offline-Servers", val);
                    }
                }
                return Ok(response);
            }
            Ok(None) => {}
            Err(e) => {
                error!("DB error looking up virtual library {}: {}", pid, e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    get_items_from_all_servers_if_not_restricted(State(state), req)
        .await
        .map(IntoResponse::into_response)
}

/// GET /Users/{user_id}/Items/{item_id}
/// Intercepts requests for virtual library items; falls back to upstream for real items.
pub async fn get_item_or_virtual_library(
    State(state): State<AppState>,
    Path((_, item_id)): Path<(String, String)>,
    req: Request,
) -> Result<Response, StatusCode> {
    match state.unified_library.get_group_by_virtual_id(&item_id).await {
        Ok(Some(group)) => {
            let collection_type_str = serde_json::to_value(&group.library_type)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default();

            let item = MediaItem {
                id: group.virtual_id,
                name: Some(group.name),
                item_type: BaseItemKind::CollectionFolder,
                collection_type: Some(group.library_type),
                is_folder: Some(true),
                server_id: None,
                item_id: None,
                series_id: None,
                series_name: None,
                season_id: None,
                etag: None,
                date_created: None,
                can_delete: Some(false),
                can_download: Some(false),
                sort_name: None,
                external_urls: None,
                path: None,
                enable_media_source_display: None,
                channel_id: None,
                provider_ids: None,
                parent_id: None,
                parent_logo_item_id: None,
                parent_backdrop_item_id: None,
                parent_backdrop_image_tags: None,
                parent_logo_image_tag: None,
                parent_thumb_item_id: None,
                parent_thumb_image_tag: None,
                user_data: None,
                child_count: None,
                display_preferences_id: None,
                tags: None,
                series_primary_image_tag: None,
                image_tags: None,
                backdrop_image_tags: None,
                image_blur_hashes: None,
                original_title: None,
                media_sources: None,
                media_streams: None,
                chapters: None,
                trickplay: None,
                extra: {
                    let mut m = std::collections::HashMap::new();
                    m.insert(
                        "CollectionType".to_string(),
                        serde_json::Value::String(collection_type_str),
                    );
                    m
                },
            };
            Ok(Json(item).into_response())
        }
        Ok(None) => crate::handlers::items::get_item(State(state), req)
            .await
            .map(IntoResponse::into_response),
        Err(e) => {
            error!("DB error looking up virtual item {}: {}", item_id, e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
