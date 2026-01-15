use std::collections::HashMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use hyper::StatusCode;
use regex::Regex;
use tracing::{error, info};

use crate::models::enums::CollectionType;
use crate::{
    media_storage_service::MediaStorageService,
    models::{MediaItem, MediaSource},
    server_storage::Server,
    session_storage::PlaybackSession,
    AppState,
};

pub fn payload_from_request<T>(request: &reqwest::Request) -> Result<T, StatusCode>
where
    T: serde::de::DeserializeOwned,
{
    let bytes = request
        .body()
        .ok_or(StatusCode::BAD_REQUEST)?
        .as_bytes()
        .ok_or(StatusCode::BAD_REQUEST)?;
    match serde_json::from_slice::<T>(bytes) {
        Ok(val) => Ok(val),
        Err(e) => {
            if let Ok(body_str) = std::str::from_utf8(bytes) {
                error!("Failed to parse JSON body: {e}\nBody: {body_str}");
            } else {
                error!("Failed to parse JSON body: {e}\nBody (non-UTF8)");
            }
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

/// Execute a reqwest request and parse the JSON response with comprehensive error handling
pub async fn execute_json_request<T>(
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<T, StatusCode>
where
    T: serde::de::DeserializeOwned,
{
    let response = client
        .execute(request)
        .await
        .map_err(|e| {
            error!("Failed to execute request: {}", e);
            StatusCode::BAD_GATEWAY
        })?
        .error_for_status()
        .map_err(|e| {
            error!("Request failed with status: {}", e);
            StatusCode::UNAUTHORIZED
        })?;

    let response_text = response.text().await.map_err(|e| {
        error!("Failed to get response text: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    match serde_json::from_str::<T>(&response_text) {
        Ok(val) => Ok(val),
        Err(_original_error) => {
            // First, try to parse as generic JSON to get a pretty-printed version
            let pretty_json = match serde_json::from_str::<serde_json::Value>(&response_text) {
                Ok(val) => {
                    // JSON is structurally valid, pretty-print it
                    match serde_json::to_string_pretty(&val) {
                        Ok(pretty) => pretty,
                        Err(_) => response_text.clone(),
                    }
                }
                Err(_) => {
                    // JSON is completely invalid, use original
                    response_text.clone()
                }
            };

            // Now try to parse the pretty JSON as our target type to get better error info
            let parse_error = match serde_json::from_str::<T>(&pretty_json) {
                Ok(_) => _original_error, // Shouldn't happen, but use original error
                Err(e) => e,
            };

            // Optional file dump in debug builds
            if cfg!(debug_assertions) {
                let dump_dir = crate::config::DATA_DIR.join("json_dumps");
                if fs::create_dir_all(&dump_dir).is_ok() {
                    let ts = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    let filename = format!(
                        "json_parse_error_{}_{}.json",
                        ts,
                        std::any::type_name::<T>().replace("::", "_")
                    );
                    let path = dump_dir.join(filename);
                    if fs::write(&path, &pretty_json).is_ok() {
                        info!("Debug: JSON dump saved to {:?}", path);
                    }
                }
            }

            // Extract line/column info and show snippet
            let err_str = parse_error.to_string();
            let re = Regex::new(r"line\s*(\d+)\s*column\s*(\d+)").unwrap();
            if let Some(caps) = re.captures(&err_str) {
                if let (Some(line_m), Some(col_m)) = (caps.get(1), caps.get(2)) {
                    if let (Ok(line), Ok(col)) = (
                        line_m.as_str().parse::<usize>(),
                        col_m.as_str().parse::<usize>(),
                    ) {
                        // Show error with context snippet
                        let lines: Vec<&str> = pretty_json.lines().collect();
                        let line_idx = line.saturating_sub(1); // Convert to 0-based
                        let col_idx = col.saturating_sub(1); // Convert to 0-based

                        let mut snippet = String::new();
                        let context_before = 3;
                        let context_after = 3;
                        let start_idx = line_idx.saturating_sub(context_before);
                        let end_idx = std::cmp::min(lines.len(), line_idx + context_after + 1);

                        for i in start_idx..end_idx {
                            let line_num = i + 1;
                            let line_content = lines.get(i).unwrap_or(&"");

                            if i == line_idx {
                                // Error line
                                snippet.push_str(&format!(">>> {line_num:>4} | {line_content}\n"));
                                // Show caret pointing to error column
                                let visible_col =
                                    std::cmp::min(col_idx, line_content.chars().count());
                                let spaces = " ".repeat(visible_col);
                                snippet.push_str(&format!("         | {spaces}^ (column {col})\n"));
                            } else {
                                // Context line
                                snippet.push_str(&format!("    {line_num:>4} | {line_content}\n"));
                            }
                        }

                        error!(
                            "JSON parsing failed: {}\nAt line {}, column {}:\n{}",
                            parse_error, line, col, snippet
                        );
                        return Err(StatusCode::BAD_GATEWAY);
                    }
                }
            }

            // Fallback if no line/column info
            error!("JSON parsing failed: {}", parse_error);
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

pub async fn get_virtual_id(
    id: &str,
    media_storage: &MediaStorageService,
    server: &Server,
) -> Result<String, StatusCode> {
    let mapping = media_storage
        .get_or_create_media_mapping(id, server.url.as_str())
        .await
        .map_err(|e| {
            error!(
                "Failed to get virtual id for: `{}` on server: {}!/n Error: {}",
                id, server.name, e
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(mapping.virtual_media_id.clone())
}

/// Extract all IDs from a media item that need virtual ID resolution
/// This is used for cache prewarming to reduce database round-trips
pub fn extract_all_ids_from_item(item: &MediaItem) -> Vec<String> {
    let mut ids = Vec::new();

    // Main item ID
    ids.push(item.id.clone());

    // Optional IDs
    if let Some(ref id) = item.parent_id {
        ids.push(id.clone());
    }
    if let Some(ref id) = item.item_id {
        ids.push(id.clone());
    }
    if let Some(ref id) = item.etag {
        ids.push(id.clone());
    }
    if let Some(ref id) = item.series_id {
        ids.push(id.clone());
    }
    if let Some(ref id) = item.season_id {
        ids.push(id.clone());
    }
    if let Some(ref id) = item.display_preferences_id {
        ids.push(id.clone());
    }
    if let Some(ref id) = item.parent_logo_item_id {
        ids.push(id.clone());
    }
    if let Some(ref id) = item.parent_backdrop_item_id {
        ids.push(id.clone());
    }
    if let Some(ref id) = item.parent_logo_image_tag {
        ids.push(id.clone());
    }
    if let Some(ref id) = item.parent_thumb_item_id {
        ids.push(id.clone());
    }
    if let Some(ref id) = item.parent_thumb_image_tag {
        ids.push(id.clone());
    }
    if let Some(ref id) = item.series_primary_image_tag {
        ids.push(id.clone());
    }

    // Image tags (HashMap values)
    if let Some(ref image_tags) = item.image_tags {
        for tag_id in image_tags.values() {
            ids.push(tag_id.clone());
        }
    }

    // Backdrop image tags (Vec)
    if let Some(ref backdrop_tags) = item.backdrop_image_tags {
        ids.extend(backdrop_tags.iter().cloned());
    }

    // Parent backdrop image tags (Vec)
    if let Some(ref parent_backdrop_tags) = item.parent_backdrop_image_tags {
        ids.extend(parent_backdrop_tags.iter().cloned());
    }

    // Image blur hashes (nested HashMap - extract keys from inner maps)
    if let Some(ref blur_hashes) = item.image_blur_hashes {
        for hash_map in blur_hashes.values() {
            for hash_id in hash_map.keys() {
                ids.push(hash_id.clone());
            }
        }
    }

    // Chapter image tags
    if let Some(ref chapters) = item.chapters {
        for chapter in chapters {
            if let Some(ref tag) = chapter.image_tag {
                ids.push(tag.clone());
            }
        }
    }

    // Trickplay keys
    if let Some(ref trickplay) = item.trickplay {
        for id in trickplay.keys() {
            ids.push(id.clone());
        }
    }

    // Media source IDs
    if let Some(ref media_sources) = item.media_sources {
        for source in media_sources {
            ids.push(source.id.clone());
        }
    }

    ids
}

/// Extract all IDs from a list of media items for batch cache prewarming
pub fn extract_all_ids_from_items(items: &[MediaItem]) -> Vec<String> {
    let mut all_ids = Vec::new();
    for item in items {
        all_ids.extend(extract_all_ids_from_item(item));
    }
    // Deduplicate
    all_ids.sort();
    all_ids.dedup();
    all_ids
}

/// Processes a media item.
/// Replaces the original ids with vitual ids that map back to the original media item and server.
pub async fn process_media_item(
    item: MediaItem,
    state: &AppState,
    server: &Server,
    should_change_name: bool,
    server_id: &str,
) -> Result<MediaItem, StatusCode> {
    let mut item = item;

    let media_storage = &state.media_storage;

    let allowed_to_change_name = state.can_change_item_names().await;

    let can_change_name = if let Some(ref collection_type) = item.collection_type {
        match collection_type {
            CollectionType::LiveTv => false,
            _ => allowed_to_change_name,
        }
    } else {
        allowed_to_change_name
    };

    if can_change_name && should_change_name {
        if let Some(name) = &item.name {
            item.name = Some(format!("{} [{}]", name, server.name));
        }

        if let Some(series_name) = &item.series_name {
            item.series_name = Some(format!("{} [{}]", series_name, server.name));
        }
    }

    item.id = get_virtual_id(&item.id, media_storage, server).await?;

    if let Some(parent_id) = &item.parent_id {
        item.parent_id = Some(get_virtual_id(parent_id, media_storage, server).await?);
    }

    if let Some(original_id) = &item.item_id {
        item.item_id = Some(get_virtual_id(original_id, media_storage, server).await?);
    }

    if let Some(etag) = &item.etag {
        item.etag = Some(get_virtual_id(etag, media_storage, server).await?);
    }

    if let Some(series_id) = &item.series_id {
        item.series_id = Some(get_virtual_id(series_id, media_storage, server).await?);
    }

    if let Some(season_id) = &item.season_id {
        item.season_id = Some(get_virtual_id(season_id, media_storage, server).await?);
    }

    if let Some(preferences_id) = &item.display_preferences_id {
        item.display_preferences_id =
            Some(get_virtual_id(preferences_id, media_storage, server).await?);
    }

    if item.can_delete.is_some() {
        item.can_delete = Some(false);
    }

    if item.can_download.is_some() {
        item.can_download = Some(false);
    }

    if let Some(media_sources) = &mut item.media_sources {
        for source in media_sources.iter_mut() {
            *source = process_media_source(source.clone(), media_storage, server).await?;
        }
    }

    if let Some(parent_logo_item_id) = &item.parent_logo_item_id {
        item.parent_logo_item_id =
            Some(get_virtual_id(parent_logo_item_id, media_storage, server).await?);
    }

    if let Some(parent_backdrop_item_id) = &item.parent_backdrop_item_id {
        item.parent_backdrop_item_id =
            Some(get_virtual_id(parent_backdrop_item_id, media_storage, server).await?);
    }

    if let Some(parent_logo_image_tag) = &item.parent_logo_image_tag {
        item.parent_logo_image_tag =
            Some(get_virtual_id(parent_logo_image_tag, media_storage, server).await?);
    }

    if let Some(parent_thumb_item_id) = &item.parent_thumb_item_id {
        item.parent_thumb_item_id =
            Some(get_virtual_id(parent_thumb_item_id, media_storage, server).await?);
    }

    if let Some(parent_thumb_image_tag) = &item.parent_thumb_image_tag {
        item.parent_thumb_image_tag =
            Some(get_virtual_id(parent_thumb_image_tag, media_storage, server).await?);
    }

    if let Some(series_primary_image_tag) = &item.series_primary_image_tag {
        item.series_primary_image_tag =
            Some(get_virtual_id(series_primary_image_tag, media_storage, server).await?);
    }

    if let Some(image_tags) = &mut item.image_tags {
        let mut updated_tags = HashMap::new();
        for (tag_type, tag_id) in image_tags.iter() {
            let virtual_id = get_virtual_id(tag_id, media_storage, server).await?;
            updated_tags.insert(tag_type.clone(), virtual_id);
        }
        *image_tags = updated_tags;
    }

    if let Some(image_blur_hashes) = &mut item.image_blur_hashes {
        let mut updated_blur_hashes = HashMap::new();
        for (image_type, hash_map) in image_blur_hashes.iter() {
            let mut updated_hash_map = HashMap::new();
            for (hash_id, hash_value) in hash_map.iter() {
                let virtual_id = get_virtual_id(hash_id, media_storage, server).await?;
                updated_hash_map.insert(virtual_id, hash_value.clone());
            }
            updated_blur_hashes.insert(image_type.clone(), updated_hash_map);
        }
        *image_blur_hashes = updated_blur_hashes;
    }

    if let Some(backdrop_image_tags) = &mut item.backdrop_image_tags {
        let mut new_backdrop_tags = Vec::new();
        for tag in backdrop_image_tags.iter() {
            let virtual_id = get_virtual_id(tag, media_storage, server).await?;
            new_backdrop_tags.push(virtual_id);
        }
        item.backdrop_image_tags = Some(new_backdrop_tags);
    }

    if let Some(parent_backdrop_image_tags) = &mut item.parent_backdrop_image_tags {
        let mut new_parent_backdrop_image_tags = Vec::new();
        for tag in parent_backdrop_image_tags.iter() {
            let virtual_id = get_virtual_id(tag, media_storage, server).await?;
            new_parent_backdrop_image_tags.push(virtual_id);
        }
        item.parent_backdrop_image_tags = Some(new_parent_backdrop_image_tags);
    }

    if let Some(chapters) = &mut item.chapters {
        for chapter in chapters.iter_mut() {
            if let Some(image_tag) = &chapter.image_tag {
                chapter.image_tag = Some(get_virtual_id(image_tag, media_storage, server).await?);
            }
        }
    }

    if let Some(trickplay) = &mut item.trickplay {
        let mut updated_hash_map = HashMap::new();
        for (id, v) in trickplay.iter() {
            let virtual_id = get_virtual_id(id, media_storage, server).await?;
            updated_hash_map.insert(virtual_id, v.clone());
        }
        *trickplay = updated_hash_map;
    }

    if item.server_id.is_some() {
        item.server_id = Some(server_id.to_string());
    }

    Ok(item)
}

pub async fn process_media_source(
    item: MediaSource,
    media_storage: &MediaStorageService,
    server: &Server,
) -> Result<MediaSource, StatusCode> {
    let mut item = item;

    item.id = get_virtual_id(&item.id, media_storage, server).await?;
    // TODO check media streams

    Ok(item)
}

pub async fn track_play_session(
    item: &MediaSource,
    session_id: &str,
    server: &Server,
    state: &AppState,
) -> Result<(), StatusCode> {
    if let Some(transcoding_url) = &item.transcoding_url {
        let re = Regex::new(r"/videos/([^/]+)/").unwrap();
        let id = re
            .captures(transcoding_url)
            .and_then(|cap| cap.get(1))
            .map(|m| m.as_str())
            .unwrap_or_default();
        info!(
            "Tracking play session for item: {}, server: {}",
            id, server.name
        );
        state
            .play_sessions
            .add_session(PlaybackSession {
                item_id: id.to_string(),
                session_id: session_id.to_string(),
                server: server.clone(),
            })
            .await;
    }

    Ok(())
}
