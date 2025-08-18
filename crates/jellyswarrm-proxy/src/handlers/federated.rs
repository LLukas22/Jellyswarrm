use axum::{
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use tokio::task::JoinSet;
use tracing::{debug, error};

use crate::{
    handlers::{
        common::{execute_json_request, process_media_item},
        items::get_items,
    },
    request_preprocessing::{apply_to_request, extract_request_infos, JellyfinAuthorization},
    AppState,
};

pub async fn get_items_from_all_servers_if_not_restricted(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::ItemsResponse>, StatusCode> {
    // Extract request information and sessions

    if let Some(query) = req.uri().query() {
        // Check if the request is for a specific series
        if query.contains("SeriesId") {
            return get_items(State(state), req).await;
        }
    }

    get_items_from_all_servers(State(state), req).await
}

pub async fn get_items_from_all_servers(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::ItemsResponse>, StatusCode> {
    let (original_request, _, _, sessions) =
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

            let result = match execute_json_request::<crate::models::ItemsResponse>(
                &state_clone.reqwest_client,
                request,
            )
            .await
            {
                Ok(items_response) => {
                    let mut processed_items = Vec::new();

                    let server_id = { state_clone.config.read().await.server_id.clone() };
                    for item in items_response.items {
                        match process_media_item(
                            item,
                            &state_clone.media_storage,
                            &server_clone,
                            true, // Change name to include server name
                            &server_id,
                        )
                        .await
                        {
                            Ok(processed_item) => processed_items.push(processed_item),
                            Err(e) => {
                                error!(
                                    "Failed to process media item from server '{}': {:?}",
                                    server_clone.name, e
                                );
                                return (index, None);
                            }
                        }
                    }

                    let item_count = processed_items.len();
                    debug!(
                        "Successfully retrieved {} items from server: {}",
                        item_count, server_clone.name
                    );
                    Some(processed_items)
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
    let mut indexed_results: Vec<(usize, Option<Vec<crate::models::MediaItem>>)> = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((index, items)) => indexed_results.push((index, items)),
            Err(e) => error!("Task failed: {:?}", e),
        }
    }

    // Sort results by original server order
    indexed_results.sort_by_key(|(index, _)| *index);

    // Extract items in original server order
    let mut server_items: Vec<Vec<crate::models::MediaItem>> = Vec::new();
    for (_, items) in indexed_results {
        if let Some(items) = items {
            server_items.push(items);
        }
    }

    // Interleave items from all servers
    let mut interleaved_items = Vec::new();
    let max_items = server_items
        .iter()
        .map(|items| items.len())
        .max()
        .unwrap_or(0);

    for i in 0..max_items {
        for server_item_list in &server_items {
            if let Some(item) = server_item_list.get(i) {
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

    Ok(Json(crate::models::ItemsResponse {
        items: interleaved_items,
        total_record_count: count as i32,
        start_index: 0,
    }))
}
