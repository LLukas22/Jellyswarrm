//! Handler for merged library queries.
//!
//! This module intercepts requests for merged library content and fetches
//! items from the configured source libraries across multiple servers.

use axum::{
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use crate::{
    deduplication::deduplicate_items,
    handlers::common::{execute_json_request, process_media_item},
    merged_library_storage::MergedLibrary,
    models::{ItemsResponseVariants, ItemsResponseWithCount, MediaItem},
    request_preprocessing::{apply_to_request, extract_request_infos, JellyfinAuthorization},
    AppState,
};

/// Check if a parent ID corresponds to a merged library
#[allow(dead_code)]
pub async fn is_merged_library_request(state: &AppState, parent_id: &str) -> bool {
    state
        .merged_library_storage
        .is_merged_library(parent_id)
        .await
        .unwrap_or(false)
}

/// Handle a request for items from a merged library
pub async fn get_merged_library_items(
    State(state): State<AppState>,
    req: Request,
    merged_library: MergedLibrary,
) -> Result<Json<ItemsResponseVariants>, StatusCode> {
    info!(
        "Fetching items for merged library '{}' ({})",
        merged_library.name, merged_library.virtual_id
    );

    // Get sessions for the current user
    let (original_request, _, _, sessions, _) =
        extract_request_infos(req, &state).await.map_err(|e| {
            error!("Failed to preprocess request: {}", e);
            StatusCode::BAD_REQUEST
        })?;

    let sessions = sessions.ok_or(StatusCode::UNAUTHORIZED)?;
    if sessions.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Get the sources for this merged library
    let sources = state
        .merged_library_storage
        .get_sources(merged_library.id)
        .await
        .map_err(|e| {
            error!("Failed to get merged library sources: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    if sources.is_empty() {
        warn!(
            "Merged library '{}' has no sources configured",
            merged_library.name
        );
        return Ok(Json(ItemsResponseVariants::WithCount(ItemsResponseWithCount {
            items: vec![],
            total_record_count: 0,
            start_index: 0,
        })));
    }

    debug!(
        "Merged library '{}' has {} sources",
        merged_library.name,
        sources.len()
    );

    // Create a map of server_id -> (session, server) for quick lookup
    let session_map: std::collections::HashMap<i64, _> = sessions
        .iter()
        .filter_map(|(session, server)| Some((server.id, (session.clone(), server.clone()))))
        .collect();

    // Create JoinSet for parallel execution
    let mut join_set = JoinSet::new();
    let sources_count = sources.len();

    for source in sources {
        // Check if we have a session for this server
        let (session, server) = match session_map.get(&source.server_id) {
            Some((s, srv)) => (s.clone(), srv.clone()),
            None => {
                warn!(
                    "No session found for server ID {} (source library {})",
                    source.server_id, source.library_id
                );
                continue;
            }
        };

        // Clone the original request and modify it for this source library
        let request = match original_request.try_clone() {
            Some(req) => req,
            None => {
                error!("Failed to clone request for source library: {}", source.library_id);
                continue;
            }
        };

        let state_clone = state.clone();
        let source_clone = source.clone();
        let library_id = source.library_id.clone();
        let priority = source.priority;

        join_set.spawn(async move {
            // Modify the request to query the specific source library
            let mut request = request;

            // Build the items query URL for this source library
            let items_url = format!(
                "{}/Items?ParentId={}&Recursive=true&IncludeItemTypes=Movie,Series,Episode,MusicAlbum,Audio&Fields=ProviderIds,Overview,Genres,People,MediaSources,Path",
                server.url.as_str().trim_end_matches('/'),
                library_id
            );

            // Update request URL
            let new_url = match url::Url::parse(&items_url) {
                Ok(url) => url,
                Err(e) => {
                    error!("Failed to parse URL for source library {}: {}", library_id, e);
                    return (source_clone.server_id, source_clone.library_id.clone(), priority, None);
                }
            };

            *request.url_mut() = new_url;

            // Apply authentication
            let auth = JellyfinAuthorization::Authorization(session.to_authorization());
            apply_to_request(
                &mut request,
                &server,
                &Some(session.clone()),
                &Some(auth),
                &state_clone,
            )
            .await;

            // Execute the request
            let result = match execute_json_request::<ItemsResponseVariants>(
                &state_clone.reqwest_client,
                request,
            )
            .await
            {
                Ok(mut items_response) => {
                    let server_id = { state_clone.config.read().await.server_id.clone() };

                    // Process each item
                    for item in items_response.iter_mut_items() {
                        match process_media_item(
                            item.clone(),
                            &state_clone,
                            &server,
                            true,
                            &server_id,
                        )
                        .await
                        {
                            Ok(processed_item) => *item = processed_item,
                            Err(e) => {
                                error!(
                                    "Failed to process media item from server '{}': {:?}",
                                    server.name, e
                                );
                            }
                        }
                    }

                    let count = items_response.len();
                    debug!(
                        "Retrieved {} items from source library {} on server {}",
                        count, library_id, server.name
                    );

                    Some((items_response, server.name.clone()))
                }
                Err(e) => {
                    error!(
                        "Failed to get items from source library {} on server '{}': {:?}",
                        library_id, server.name, e
                    );
                    None
                }
            };

            (source_clone.server_id, source_clone.library_id.clone(), priority, result)
        });
    }

    // Collect results
    let mut all_items: Vec<(MediaItem, i64, String, i32)> = Vec::new();

    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((server_id, _library_id, priority, Some((items_response, server_name)))) => {
                for item in items_response.into_items() {
                    all_items.push((item, server_id, server_name.clone(), priority));
                }
            }
            Ok((_, library_id, _, None)) => {
                debug!("No results from source library {}", library_id);
            }
            Err(e) => {
                error!("Task failed: {:?}", e);
            }
        }
    }

    info!(
        "Collected {} total items from {} sources for merged library '{}'",
        all_items.len(),
        sources_count,
        merged_library.name
    );

    // Apply deduplication
    let deduplicated = deduplicate_items(all_items, &merged_library.dedup_strategy);

    debug!(
        "After deduplication: {} unique items (strategy: {:?})",
        deduplicated.len(),
        merged_library.dedup_strategy
    );

    // Convert deduplicated items back to MediaItems
    let items: Vec<MediaItem> = deduplicated
        .into_iter()
        .map(|d| d.primary)
        .collect();

    let count = items.len();

    Ok(Json(ItemsResponseVariants::WithCount(ItemsResponseWithCount {
        items,
        total_record_count: count as i32,
        start_index: 0,
    })))
}

/// Extension trait for ItemsResponseVariants to consume into items
trait IntoItems {
    fn into_items(self) -> Vec<MediaItem>;
}

impl IntoItems for ItemsResponseVariants {
    fn into_items(self) -> Vec<MediaItem> {
        match self {
            ItemsResponseVariants::WithCount(w) => w.items,
            ItemsResponseVariants::Bare(v) => v,
        }
    }
}
