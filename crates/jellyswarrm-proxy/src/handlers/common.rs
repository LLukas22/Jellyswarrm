use std::collections::HashMap;

use hyper::StatusCode;
use regex::Regex;
use tracing::{error, info};

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

    serde_json::from_str(&response_text).map_err(|e| {
        error!(
            "Failed to parse response JSON: {}. Response body: {}",
            e, response_text
        );
        StatusCode::BAD_GATEWAY
    })
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

/// Processes a media item.
/// Replaces the original ids with vitual ids that map back to the original media item and server.
pub async fn process_media_item(
    item: MediaItem,
    media_storage: &MediaStorageService,
    server: &Server,
    change_name: bool,
    server_id: &str,
) -> Result<MediaItem, StatusCode> {
    let mut item = item;

    if change_name {
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
