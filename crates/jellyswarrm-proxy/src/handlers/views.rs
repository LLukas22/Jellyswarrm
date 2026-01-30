//! Handler for user views (library listings) with merged library injection.
//!
//! This module handles the `/Users/{userId}/Views` and `/UserViews` endpoints,
//! injecting merged libraries as CollectionFolder items alongside real server libraries.

use axum::{
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use std::collections::HashMap;
use tracing::{debug, info};

use crate::{
    handlers::federated::get_items_from_all_servers,
    merged_library_storage::MergedLibrary,
    models::{
        enums::{BaseItemKind, CollectionType as JellyfinCollectionType},
        ItemsResponseVariants, MediaItem, UserData,
    },
    AppState,
};

/// Get user views (libraries) with merged libraries injected.
///
/// This handler:
/// 1. Fetches views from all connected servers
/// 2. Fetches merged libraries visible to the user
/// 3. Creates MediaItem entries for each merged library
/// 4. Injects merged libraries at the beginning of the list
pub async fn get_user_views_with_merged_libraries(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<ItemsResponseVariants>, StatusCode> {
    // Get merged libraries first
    let merged_libraries = state
        .merged_library_storage
        .list_merged_libraries_for_user(None) // TODO: Get user ID from request
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to fetch merged libraries: {}", e);
            Vec::new()
        });

    debug!(
        "Found {} merged libraries to inject into views",
        merged_libraries.len()
    );

    // Get the server ID for MediaItem creation
    let server_id = { state.config.read().await.server_id.clone() };

    // Create MediaItems for merged libraries
    let merged_items: Vec<MediaItem> = merged_libraries
        .into_iter()
        .map(|lib| create_merged_library_view_item(lib, &server_id))
        .collect();

    // Get views from all servers
    let server_views = get_items_from_all_servers(State(state), req).await?;

    // Combine merged libraries with server views
    let combined = match server_views.0 {
        ItemsResponseVariants::WithCount(mut response) => {
            // Insert merged libraries at the beginning
            let merged_count = merged_items.len();
            for (i, item) in merged_items.into_iter().enumerate() {
                response.items.insert(i, item);
            }
            response.total_record_count += merged_count as i32;

            info!(
                "Returning {} views ({} merged + {} from servers)",
                response.total_record_count,
                merged_count,
                response.total_record_count - merged_count as i32
            );

            ItemsResponseVariants::WithCount(response)
        }
        ItemsResponseVariants::Bare(mut items) => {
            // Insert merged libraries at the beginning
            let merged_count = merged_items.len();
            for (i, item) in merged_items.into_iter().enumerate() {
                items.insert(i, item);
            }

            info!(
                "Returning {} views ({} merged + {} from servers)",
                items.len(),
                merged_count,
                items.len() - merged_count
            );

            ItemsResponseVariants::Bare(items)
        }
    };

    Ok(Json(combined))
}

/// Create a MediaItem representing a merged library as a CollectionFolder.
///
/// This creates a view item that looks like a native Jellyfin library to clients.
fn create_merged_library_view_item(library: MergedLibrary, server_id: &str) -> MediaItem {
    let collection_type = match library.collection_type {
        crate::merged_library_storage::CollectionType::Movies => JellyfinCollectionType::Movies,
        crate::merged_library_storage::CollectionType::TvShows => JellyfinCollectionType::TvShows,
        crate::merged_library_storage::CollectionType::Music => JellyfinCollectionType::Music,
        crate::merged_library_storage::CollectionType::Books => JellyfinCollectionType::Books,
        crate::merged_library_storage::CollectionType::Mixed => JellyfinCollectionType::Folders,
    };

    // Create a sort name from the library name (lowercase, no special chars)
    let sort_name = library
        .name
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>();

    // Create the UserData for the library
    let user_data = UserData {
        playback_position_ticks: 0,
        play_count: 0,
        is_favorite: false,
        played: false,
        key: library.virtual_id.clone(),
        item_id: "00000000000000000000000000000000".to_string(),
        played_percentage: None,
        last_played_date: None,
        unplayed_item_count: None,
    };

    MediaItem {
        name: Some(library.name),
        server_id: Some(server_id.to_string()),
        id: library.virtual_id.clone(),
        item_id: None,
        series_id: None,
        series_name: None,
        season_id: None,
        etag: None,
        date_created: Some(library.created_at.to_rfc3339()),
        can_delete: Some(false),
        can_download: Some(false),
        sort_name: Some(sort_name),
        external_urls: Some(vec![]),
        path: None,
        enable_media_source_display: Some(true),
        channel_id: None,
        provider_ids: Some(serde_json::json!({})),
        is_folder: Some(true),
        parent_id: None,
        parent_logo_item_id: None,
        parent_backdrop_item_id: None,
        parent_backdrop_image_tags: None,
        parent_logo_image_tag: None,
        parent_thumb_item_id: None,
        parent_thumb_image_tag: None,
        item_type: BaseItemKind::CollectionFolder,
        collection_type: Some(collection_type),
        user_data: Some(user_data),
        child_count: None, // Could be populated later with actual count
        display_preferences_id: Some(library.virtual_id),
        tags: Some(vec!["merged".to_string()]),
        series_primary_image_tag: None,
        image_tags: Some(HashMap::new()),
        backdrop_image_tags: Some(vec![]),
        image_blur_hashes: None,
        original_title: None,
        media_sources: None,
        media_streams: None,
        chapters: None,
        trickplay: None,
        extra: HashMap::new(),
    }
}
